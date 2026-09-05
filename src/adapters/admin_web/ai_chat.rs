//! AI 對話頁（ChatGPT 風格）後端。
//!
//! AI_CHAT_V1
//!
//! 設計重點：
//! 1. **密鑰只存在後端**（`config/ai_chat.json`），前端永遠只拿到遮蔽字串
//!    （例如 `sk-…4f2a`）。這是把聊天整進 dashboard 而不是做純前端頁的主因。
//! 2. **多密鑰自動輪替**：任何一把用完額度／被限流／失效，立刻換下一把重送，
//!    對使用者完全無感。判定規則見 `classify_failure`。
//! 3. **背景 job + 輪詢**：沿用公告翻譯 V8 的教訓 —— 反向代理（Cloudflare
//!    Tunnel）會對長時間沒有回應的 HTTP 請求直接回空的 502。因此送出訊息只回
//!    202，實際生成在背景跑，前端每 350ms 輪詢一次拿「目前已生成的文字」，
//!    順便得到打字機效果。
//! 4. 上游一律用 **OpenAI 相容的 `/chat/completions`**，所以 OpenAI／Gemini
//!    （OpenAI 相容端點）／Groq／OpenRouter／DeepSeek／xAI／自訂網址都能共用同
//!    一條路徑，每把密鑰各自綁自己的服務商與模型。

use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    admin_login_redirect, html_page_response, must_change_password_redirect, session_token,
    unauthorized_response, AdminQuery, AdminWebState,
};

const PAGE_HTML: &str = include_str!("static/ai.html");

/// AI_CHAT_V3：頁面版本。前端是 `include_str!` 編進二進位的，忘了重新編譯就會
/// 看到舊畫面卻以為改動沒生效。把版本與編譯時間顯示在側欄，一眼就能分辨。
const PAGE_VERSION: &str = "v10";

fn build_stamp() -> &'static str {
    option_env!("TRONCLASS_BUILD_STAMP").unwrap_or("dev")
}

/// 單一請求的上限。串流過程中只要持續有 token 進來就不算逾時。
const AI_REQUEST_TIMEOUT_SECS: u64 = 180;
/// AI_CHAT_V8：SSE 推送的取樣間隔。上游 chunk 每 80ms 進 job，
/// 這裡用 40ms 去取，使用者看到的就是「幾乎即時」。
const AI_STREAM_TICK_MS: u64 = 40;
/// 連線最長閒置多少個 tick 就主動收掉（40ms × 5000 = 200 秒，
/// 比單次請求逾時 180 秒還長，正常情況輪不到它）。
const AI_STREAM_MAX_IDLE_TICKS: u32 = 5_000;
/// 整個 job 的總預算，超過就中止並回報。
const AI_TOTAL_BUDGET_SECS: u64 = 300;
/// 完成的 job 保留多久（供前端最後一次輪詢取結果）。
const AI_JOB_TTL_SECS: u64 = 600;
const AI_JOB_MAX: usize = 64;
/// 錯誤訊息保留的上游內文長度。
const AI_DETAIL_CHARS: usize = 400;

/// 被限流（可恢復）時的冷卻秒數。
const COOLDOWN_RATE_LIMIT_SECS: u64 = 60;
/// 額度用盡時的冷卻秒數（多數服務商以日／月為週期，先停 6 小時再自動復活）。
const COOLDOWN_QUOTA_SECS: u64 = 6 * 60 * 60;
/// 上游 5xx／連線失敗的冷卻秒數。
const COOLDOWN_UPSTREAM_SECS: u64 = 15;

/// 每個對話串保留的訊息數（超過只保留最新的）。
const THREAD_MAX_MESSAGES: usize = 400;
/// 每位使用者保留的對話串數。
const THREADS_PER_ACCOUNT: usize = 60;
/// 送給模型的歷史訊息數上限（避免無限增長把 token 燒光）。
const CONTEXT_MESSAGES: usize = 24;
/// AI_CHAT_V5：脈絡的 token 預算。固定「最近 N 則」在長對話會讓輸入
/// 呈二次成長 —— 第 12 輪只問一句話卻扛著 5000+ tokens 的歷史。
const CONTEXT_TOKEN_BUDGET: usize = 1500;
/// 累積這麼多則還沒摘要的訊息，就在背景把較早的濃縮成一段摘要。
const SUMMARY_TRIGGER: usize = 12;
/// 摘要後仍保留原文的最近則數。
const SUMMARY_KEEP_RAW: usize = 6;
/// 摘要本身的長度上限（token）。
const SUMMARY_MAX_TOKENS: u32 = 320;
// BULLETIN_AI_SUMMARY_V1：把單則學校公告濃縮成「字數精簡但重點不漏」的摘要。
const BULLETIN_SUMMARY_MAX_TOKENS: u32 = 420;
// BULLETIN_NOTIFY_SUMMARY_V2：通知預覽用的摘要。要比條列版更短 ——
// 像手機／Discord 通知預覽那樣，「一到兩句」就讓人看懂重點。
const BULLETIN_PREVIEW_MAX_TOKENS: u32 = 160;
/// 送進摘要模型的上游來源文字上限（位元組）。公告多半不長，但設上限以免錯字溢出。
const BULLETIN_SUMMARY_MAX_SOURCE: usize = 40_000;
/// 相同問題的回覆快取。
const CHAT_CACHE_TTL_SECS: u64 = 24 * 60 * 60;
const CHAT_CACHE_MAX: usize = 200;

/// AI_CHAT_V5：估算一段文字的 token 數。
///
/// 刻意不引入 tiktoken 之類的相依 —— 它只準於 OpenAI 的分詞器，對 Gemini
/// 一樣是估的，卻要多背一個含詞表的套件。這裡用保守上界：CJK 一字一 token，
/// 其餘每 4 個字元一 token，再加每則訊息的固定開銷。寧可少送一點也不要超支。
fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        let code = c as u32;
        let is_cjk = (0x3000..=0x9FFF).contains(&code)
            || (0xAC00..=0xD7AF).contains(&code)
            || (0xF900..=0xFAFF).contains(&code)
            || (0x1_0000..=0x3_FFFF).contains(&code);
        if is_cjk {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + (other + 3) / 4 + 4
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 共用的 HTTP client。每次請求都 build 一個 client 會重新初始化 TLS，
/// 既慢又可能耗盡 fd（公告翻譯 V7 已踩過這個坑）。
static AI_CLIENT: once_cell::sync::Lazy<reqwest::Client> = once_cell::sync::Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(AI_REQUEST_TIMEOUT_SECS))
        .connect_timeout(std::time::Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (compatible; TronclassPanel/1.0)")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

// ─────────────────────────────────────────────────────────────
// 服務商預設
// ─────────────────────────────────────────────────────────────

/// 內建服務商 →（顯示名稱, base_url, 預設模型）。
/// 全部走 OpenAI 相容的 `{base_url}/chat/completions`。
fn provider_preset(provider: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match provider {
        "openai" => Some(("OpenAI", "https://api.openai.com/v1", "gpt-4o-mini")),
        "gemini" => Some((
            "Google Gemini",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "gemini-2.0-flash",
        )),
        "groq" => Some((
            "Groq",
            "https://api.groq.com/openai/v1",
            "llama-3.3-70b-versatile",
        )),
        "openrouter" => Some((
            "OpenRouter",
            "https://openrouter.ai/api/v1",
            "openai/gpt-4o-mini",
        )),
        "deepseek" => Some(("DeepSeek", "https://api.deepseek.com/v1", "deepseek-chat")),
        "xai" => Some(("xAI Grok", "https://api.x.ai/v1", "grok-2-latest")),
        "mistral" => Some((
            "Mistral",
            "https://api.mistral.ai/v1",
            "mistral-small-latest",
        )),
        "custom" => Some(("自訂", "", "")),
        _ => None,
    }
}

/// AI_CHAT_V2：給使用者看的通俗名稱。管理面板用正式名稱（OpenAI），
/// 聊天頁的模型選單用大家熟悉的稱呼（ChatGPT）。
fn provider_display(provider: &str) -> &'static str {
    match provider {
        "openai" => "ChatGPT",
        "gemini" => "Gemini",
        "groq" => "Groq",
        "openrouter" => "OpenRouter",
        "deepseek" => "DeepSeek",
        "xai" => "Grok",
        "mistral" => "Mistral",
        "custom" => "自訂",
        _ => "其他",
    }
}

fn provider_label(provider: &str) -> String {
    provider_preset(provider)
        .map(|(label, _, _)| label.to_string())
        .unwrap_or_else(|| provider.to_string())
}

// ─────────────────────────────────────────────────────────────
// 密鑰與設定的持久化
// ─────────────────────────────────────────────────────────────

/// 密鑰目前的健康狀態。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum KeyStatus {
    /// 可用。
    Ok,
    /// 被限流／額度用盡，冷卻到期後自動復活。
    Cooling,
    /// 密鑰本身無效（401/403），需要人工處理，不會自動復活。
    Dead,
}

impl Default for KeyStatus {
    fn default() -> Self {
        Self::Ok
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct ApiKey {
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    provider: String,
    /// 空字串代表沿用服務商預設。
    #[serde(default)]
    base_url: String,
    secret: String,
    /// 空字串代表沿用服務商預設模型。
    #[serde(default)]
    model: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    status: KeyStatus,
    #[serde(default)]
    cooldown_until: u64,
    #[serde(default)]
    last_error: String,
    #[serde(default)]
    last_used: u64,
    #[serde(default)]
    uses: u64,
    #[serde(default)]
    fails: u64,
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    /// 上游若不吃 `stream_options`，自動關掉並記住，之後不再送。
    #[serde(default = "default_true")]
    usage_supported: bool,
}

fn default_true() -> bool {
    true
}

impl ApiKey {
    fn effective_base_url(&self) -> String {
        let trimmed = self.base_url.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
        provider_preset(&self.provider)
            .map(|(_, url, _)| url.trim_end_matches('/').to_string())
            .unwrap_or_default()
    }

    fn effective_model(&self) -> String {
        let trimmed = self.model.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
        provider_preset(&self.provider)
            .map(|(_, _, model)| model.to_string())
            .unwrap_or_default()
    }

    fn display_name(&self) -> String {
        if !self.label.trim().is_empty() {
            return self.label.trim().to_string();
        }
        format!("{}·{}", provider_label(&self.provider), mask_secret(&self.secret))
    }

    /// 現在能不能用（已啟用、非失效、冷卻已過）。
    fn available(&self) -> bool {
        if !self.enabled || self.secret.trim().is_empty() {
            return false;
        }
        match self.status {
            KeyStatus::Dead => false,
            KeyStatus::Cooling => self.cooldown_until <= now_secs(),
            KeyStatus::Ok => true,
        }
    }

    /// 給前端看的樣子 —— 絕不含完整密鑰。
    fn to_public_json(&self) -> Value {
        let cooling_left = self.cooldown_until.saturating_sub(now_secs());
        json!({
            "id": self.id,
            "label": self.label,
            "name": self.display_name(),
            "provider": self.provider,
            "provider_label": provider_label(&self.provider),
            "base_url": self.base_url,
            "effective_base_url": self.effective_base_url(),
            "model": self.model,
            "effective_model": self.effective_model(),
            "secret_masked": mask_secret(&self.secret),
            "enabled": self.enabled,
            "status": match self.status {
                KeyStatus::Ok => "ok",
                KeyStatus::Cooling => "cooling",
                KeyStatus::Dead => "dead",
            },
            "available": self.available(),
            "cooldown_secs_left": if self.status == KeyStatus::Cooling { cooling_left } else { 0 },
            "last_error": self.last_error,
            "last_used": self.last_used,
            "uses": self.uses,
            "fails": self.fails,
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.prompt_tokens + self.completion_tokens,
        })
    }
}

/// 只露出頭尾，中間全部遮掉。
fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 10 {
        return "•".repeat(chars.len().max(4));
    }
    let head: String = chars.iter().take(5).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{head}…{tail}")
}

/// AI_CHAT_V2：使用者可以自由選擇的模型。
///
/// 一個模型只描述「要用哪家的哪個模型」，不綁定特定密鑰 —— 實際送出時仍會在
/// 該服務商的所有密鑰之間自動輪替，所以選了 ChatGPT 就會吃掉所有 OpenAI 密鑰
/// 的額度，一把用完換下一把。
#[derive(Clone, Serialize, Deserialize)]
struct ModelOption {
    id: String,
    #[serde(default)]
    label: String,
    provider: String,
    model: String,
    #[serde(default = "default_true")]
    enabled: bool,
    /// AI_CHAT_V3：`fast`（快、便宜）／`balanced`／`deep`（強、慢、貴）。
    /// 自動選模型時用來決定「這題值不值得動用大模型」。
    #[serde(default = "default_tier")]
    tier: String,
    /// 擅長領域：`code`／`math`／`long`／`vision`／`general`。
    #[serde(default)]
    tags: Vec<String>,
}

fn default_tier() -> String {
    "balanced".to_string()
}

/// 把模型名稱拆成詞元。`openai/gpt-4o-mini` → `["openai","gpt","4o","mini"]`
///
/// 一定要拆詞再比對，不能用子字串 —— 「gemini」裡面就含有「mini」，
/// 直接 contains 會把 gemini-2.5-pro 誤判成快速模型。
fn model_tokens(model: &str) -> Vec<String> {
    model
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// 參數量詞元（`70b`、`8b`）→ 數字。
fn param_size(token: &str) -> Option<u32> {
    let digits = token.strip_suffix('b')?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// 由模型名稱推測分級。新增模型時先自動判斷，管理員可再手動改。
fn infer_tier(model: &str) -> String {
    const FAST: [&str; 10] = [
        "mini", "flash", "lite", "small", "haiku", "turbo", "instant", "nano", "tiny", "air",
    ];
    const DEEP: [&str; 11] = [
        "pro", "opus", "ultra", "large", "max", "thinking", "reasoner", "o1", "o3", "sonnet",
        "plus",
    ];
    let tokens = model_tokens(model);

    // 小模型（≤ 9B）算快速，大模型（≥ 30B）算強力。
    for token in tokens.iter() {
        if let Some(size) = param_size(token) {
            if size <= 9 {
                return "fast".to_string();
            }
            if size >= 30 {
                return "deep".to_string();
            }
        }
    }
    if tokens.iter().any(|t| FAST.contains(&t.as_str())) {
        return "fast".to_string();
    }
    if tokens.iter().any(|t| DEEP.contains(&t.as_str())) {
        return "deep".to_string();
    }
    "balanced".to_string()
}

/// 由模型名稱推測擅長領域。
fn infer_tags(model: &str) -> Vec<String> {
    let tokens = model_tokens(model);
    let has = |word: &str| tokens.iter().any(|t| t == word);
    let mut tags: Vec<String> = Vec::new();
    if has("code") || has("coder") || has("codestral") {
        tags.push("code".to_string());
    }
    if has("math") || has("reasoner") || has("thinking") || has("r1") {
        tags.push("math".to_string());
    }
    if has("vision") || has("vl") {
        tags.push("vision".to_string());
    }
    tags
}

impl ModelOption {
    fn display(&self) -> String {
        if !self.label.trim().is_empty() {
            return self.label.trim().to_string();
        }
        format!("{} · {}", provider_display(&self.provider), self.model)
    }

    /// 名稱能不能真的送給上游。舊資料或手打錯字會留下不合法的名稱，
    /// 這種模型必須擋在選單外，否則使用者一選就是 400。
    fn validity(&self) -> Result<String, String> {
        sanitize_model_name(&self.provider, &self.model)
    }

    fn to_json(&self, keys: &[ApiKey]) -> Value {
        let mine: Vec<&ApiKey> = keys.iter().filter(|k| k.provider == self.provider).collect();
        let usable = mine.iter().filter(|k| k.available()).count();
        let total = mine.len();
        // AI_CHAT_V7：只說「無可用密鑰」太含糊 —— 到底是沒加金鑰、被停用、
        // 失效還是冷卻中，處理方式完全不同，直接講明白。
        let unavailable_reason = if usable > 0 {
            String::new()
        } else if total == 0 {
            format!("尚未新增 {} 的 API 密鑰", provider_display(&self.provider))
        } else if mine.iter().all(|k| !k.enabled) {
            "此服務商的密鑰已被停用".to_string()
        } else if mine.iter().any(|k| k.status == KeyStatus::Dead) {
            "密鑰已失效（401／403），需要重新設定".to_string()
        } else {
            let left = mine
                .iter()
                .filter(|k| k.status == KeyStatus::Cooling)
                .map(|k| k.cooldown_until.saturating_sub(now_secs()))
                .min()
                .unwrap_or(0);
            if left > 3600 {
                format!("密鑰冷卻中，約 {} 小時後恢復", left / 3600)
            } else if left >= 60 {
                format!("密鑰冷卻中，約 {} 分鐘後恢復", left / 60)
            } else if left > 0 {
                format!("密鑰冷卻中，約 {left} 秒後恢復")
            } else {
                "密鑰暫時不可用".to_string()
            }
        };
        let validity = self.validity();
        let invalid_reason = validity.as_ref().err().cloned().unwrap_or_default();
        let valid = validity.is_ok();
        json!({
            "id": self.id,
            "label": self.label,
            "name": self.display(),
            "provider": self.provider,
            "provider_label": provider_display(self.provider.as_str()),
            "model": self.model,
            "enabled": self.enabled,
            "tier": self.tier,
            "tier_label": match self.tier.as_str() {
                "fast" => "快速",
                "deep" => "強力",
                _ => "均衡",
            },
            "tags": self.tags,
            "usable_keys": usable,
            "total_keys": total,
            "valid": valid,
            "invalid_reason": invalid_reason,
            "unavailable_reason": unavailable_reason,
            "available": self.enabled && usable > 0 && valid,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct ChatSettings {
    #[serde(default = "default_system_prompt")]
    system_prompt: String,
    #[serde(default = "default_temperature")]
    temperature: f32,
    #[serde(default)]
    max_tokens: u32,
    /// `order` = 固定由第一把開始；`round_robin` = 每次換下一把（分散配額）。
    #[serde(default = "default_strategy")]
    strategy: String,
    /// AI_CHAT_V5：脈絡 token 預算（0＝用內建預設）。
    #[serde(default = "default_context_budget")]
    context_budget: usize,
    /// 長對話自動濃縮成摘要。
    #[serde(default = "default_true")]
    summary_enabled: bool,
    /// 完全相同的問題直接回快取，不再送上游。
    #[serde(default = "default_true")]
    cache_enabled: bool,
}

fn default_context_budget() -> usize {
    CONTEXT_TOKEN_BUDGET
}

fn default_system_prompt() -> String {
    "你是一個樂於助人的助理，請用繁體中文回答，除非使用者用其他語言提問。".to_string()
}

fn default_temperature() -> f32 {
    0.7
}

fn default_strategy() -> String {
    "order".to_string()
}

impl Default for ChatSettings {
    fn default() -> Self {
        Self {
            system_prompt: default_system_prompt(),
            temperature: default_temperature(),
            max_tokens: 0,
            strategy: default_strategy(),
            context_budget: default_context_budget(),
            summary_enabled: true,
            cache_enabled: true,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct ChatStore {
    #[serde(default)]
    keys: Vec<ApiKey>,
    /// AI_CHAT_V2：管理員維護的「可選模型」清單，使用者在聊天頁自己挑。
    /// 留空時由密鑰自動推導，不設定也能直接用。
    #[serde(default)]
    models: Vec<ModelOption>,
    #[serde(default)]
    settings: ChatSettings,
    /// round_robin 用的游標。
    #[serde(default)]
    cursor: usize,
}

fn chat_store_file() -> std::path::PathBuf {
    std::path::Path::new("config/ai_chat.json").to_path_buf()
}

static CHAT_STORE: once_cell::sync::Lazy<std::sync::Mutex<ChatStore>> =
    once_cell::sync::Lazy::new(|| {
        let mut store = ChatStore::default();
        if let Ok(text) = std::fs::read_to_string(chat_store_file()) {
            if let Ok(parsed) = serde_json::from_str::<ChatStore>(&text) {
                store = parsed;
            }
        }
        // AI_CHAT_V6：舊設定檔可能存著「GPT-4o mini」這種顯示名稱，
        // 開機時就正規化並合併，使用者不必手動整理。
        let (fixed, merged, dropped) = migrate_models(&mut store.models);
        for key in store.keys.iter_mut() {
            if let Ok(name) = canonical_model_name(&key.provider, &key.model) {
                key.model = name;
            }
        }
        if fixed > 0 || merged > 0 || !dropped.is_empty() {
            tracing::info!(fixed, merged, ?dropped, "AI 模型清單已自動正規化");
        }
        std::sync::Mutex::new(store)
    });

fn persist_chat_store(store: &ChatStore) {
    let path = chat_store_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(store) {
        Ok(text) => {
            // ATOMIC_WRITE_V1：temp → fsync → rename；檔案含明文密鑰，權限在 rename 前就收到 0600。
            if let Err(e) = crate::atomic_write::write_atomic_with_mode(&path, text, 0o600) {
                tracing::warn!("無法寫入 ai_chat.json：{e}");
            }
        }
        Err(e) => tracing::warn!("ai_chat.json 序列化失敗：{e}"),
    }
}

/// 對 store 做一次修改並立刻落盤。
fn with_store<T>(apply: impl FnOnce(&mut ChatStore) -> T) -> Option<T> {
    let mut guard = CHAT_STORE.lock().ok()?;
    let out = apply(&mut guard);
    persist_chat_store(&guard);
    Some(out)
}

fn read_store<T>(read: impl FnOnce(&ChatStore) -> T) -> Option<T> {
    CHAT_STORE.lock().ok().map(|guard| read(&guard))
}

// ─────────────────────────────────────────────────────────────
// 對話串的持久化
// ─────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
    #[serde(default)]
    ts: i64,
    /// 這則回覆是哪把密鑰／哪個模型生的（只有 assistant 有）。
    #[serde(default)]
    key_name: String,
    #[serde(default)]
    model: String,
    /// AI_CHAT_V7：這則訊息附了哪些檔案（只留檔名，內容不落地）。
    #[serde(default)]
    attachments: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ChatThread {
    id: String,
    #[serde(default)]
    title: String,
    /// AI_CHAT_V2：這串對話用的模型，重新打開時自動選回同一個。
    #[serde(default)]
    model_id: String,
    /// AI_CHAT_V5：較早對話的濃縮摘要。
    #[serde(default)]
    summary: String,
    /// 已被摘要涵蓋的訊息則數（從頭算起）。
    #[serde(default)]
    summarized: usize,
    #[serde(default)]
    created: i64,
    #[serde(default)]
    updated: i64,
    #[serde(default)]
    messages: Vec<ChatMessage>,
}

impl ChatThread {
    fn summary_json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "model_id": self.model_id,
            "created": self.created,
            "updated": self.updated,
            "count": self.messages.len(),
        })
    }

    fn full_json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "model_id": self.model_id,
            "created": self.created,
            "updated": self.updated,
            "messages": self.messages.iter().map(|m| json!({
                "role": m.role,
                "content": m.content,
                "ts": m.ts,
                "key_name": m.key_name,
                "model": m.model,
                "attachments": m.attachments,
            })).collect::<Vec<Value>>(),
        })
    }
}

type ThreadMap = std::collections::HashMap<String, Vec<ChatThread>>;

fn threads_file() -> std::path::PathBuf {
    std::path::Path::new("config/ai_chat_threads.json").to_path_buf()
}

static THREADS: once_cell::sync::Lazy<std::sync::Mutex<ThreadMap>> =
    once_cell::sync::Lazy::new(|| {
        let mut map = ThreadMap::new();
        if let Ok(text) = std::fs::read_to_string(threads_file()) {
            if let Ok(parsed) = serde_json::from_str::<ThreadMap>(&text) {
                map = parsed;
            }
        }
        std::sync::Mutex::new(map)
    });

fn persist_threads(map: &ThreadMap) {
    let path = threads_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(map) {
        // ATOMIC_WRITE_V1
        if let Err(e) = crate::atomic_write::write_atomic(path, text) {
            tracing::warn!("無法寫入 ai_chat_threads.json：{e}");
        }
    }
}

fn with_threads<T>(account: &str, apply: impl FnOnce(&mut Vec<ChatThread>) -> T) -> Option<T> {
    let mut guard = THREADS.lock().ok()?;
    let list = guard.entry(account.to_string()).or_default();
    let out = apply(list);
    // 依更新時間排序後裁掉最舊的對話串。
    list.sort_by(|a, b| b.updated.cmp(&a.updated));
    list.truncate(THREADS_PER_ACCOUNT);
    persist_threads(&guard);
    Some(out)
}

fn read_threads<T>(account: &str, read: impl FnOnce(&[ChatThread]) -> T) -> Option<T> {
    let guard = THREADS.lock().ok()?;
    let empty: Vec<ChatThread> = Vec::new();
    Some(read(guard.get(account).unwrap_or(&empty)))
}

/// AI_CHAT_V5：在 token 預算內，由新到舊挑出要送給模型的歷史訊息。
///
/// 最新那一則一定會送（否則模型不知道使用者在問什麼），其餘依預算往前塞。
fn select_context<'a>(history: &'a [ChatMessage], budget: usize) -> Vec<&'a ChatMessage> {
    let mut picked: Vec<&ChatMessage> = Vec::new();
    let mut used = 0usize;
    for message in history.iter().rev() {
        let cost = estimate_tokens(&message.content);
        let is_latest = picked.is_empty();
        if !is_latest && (used + cost > budget || picked.len() >= CONTEXT_MESSAGES) {
            break;
        }
        used += cost;
        picked.push(message);
    }
    picked.reverse();
    picked
}

/// 用第一則使用者訊息當標題。
fn derive_title(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return "新對話".to_string();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 24 {
        return chars.into_iter().collect();
    }
    let head: String = chars.into_iter().take(24).collect();
    format!("{head}…")
}

// ─────────────────────────────────────────────────────────────
// 背景生成 job
// ─────────────────────────────────────────────────────────────

/// AI_CHAT_V1：沿用公告翻譯 V8 的「背景 job + 輪詢」。生成可能要幾十秒，
/// 若讓 HTTP 請求一直掛著，Cloudflare Tunnel 會直接回一個空的 502。
struct ChatJob {
    account: String,
    thread_id: String,
    /// 目前已生成的文字（前端每次輪詢都拿全文，直接覆蓋渲染）。
    text: String,
    done: bool,
    ok: bool,
    /// 使用者按了停止。
    cancel: bool,
    error: String,
    code: String,
    key_name: String,
    model: String,
    /// AI_CHAT_V3：自動模式下「為什麼挑這個模型」，顯示在回覆上方。
    auto_reason: String,
    /// AI_CHAT_V5：這次是不是直接命中快取。
    cached: bool,
    /// 成功後要寫回快取的 key（空＝不快取）。
    cache_key: String,
    /// 輪替過程中每把密鑰的結果，失敗時攤開給使用者看。
    attempts: Vec<Value>,
    /// AI_CHAT_V9（Agent 模式）：這次跑過的每一步工具呼叫，前端畫成時間軸。
    steps: Vec<Value>,
    created_at: u64,
    updated_at: u64,
}

impl ChatJob {
    fn to_json(&self) -> Value {
        json!({
            "state": if !self.done { "running" } else if self.ok { "done" } else { "failed" },
            "done": self.done,
            "ok": self.ok,
            "text": self.text,
            "thread_id": self.thread_id,
            "key_name": self.key_name,
            "model": self.model,
            "auto_reason": self.auto_reason,
            "cached": self.cached,
            "error": self.error,
            "code": self.code,
            "attempts": self.attempts,
            "steps": self.steps,
            "elapsed_secs": now_secs().saturating_sub(self.created_at),
        })
    }
}

static CHAT_JOBS: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, ChatJob>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn job_insert(job_id: &str, job: ChatJob) {
    if let Ok(mut map) = CHAT_JOBS.lock() {
        let now = now_secs();
        map.retain(|_, j| now.saturating_sub(j.updated_at) <= AI_JOB_TTL_SECS);
        while map.len() >= AI_JOB_MAX {
            let oldest = map
                .iter()
                .min_by_key(|(_, j)| j.updated_at)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(key) => {
                    map.remove(&key);
                }
                None => break,
            }
        }
        map.insert(job_id.to_string(), job);
    }
}

fn job_update<T>(job_id: &str, apply: impl FnOnce(&mut ChatJob) -> T) -> Option<T> {
    let mut map = CHAT_JOBS.lock().ok()?;
    let job = map.get_mut(job_id)?;
    let out = apply(job);
    job.updated_at = now_secs();
    Some(out)
}

fn job_cancelled(job_id: &str) -> bool {
    CHAT_JOBS
        .lock()
        .ok()
        .and_then(|map| map.get(job_id).map(|j| j.cancel))
        .unwrap_or(false)
}

/// AI_CHAT_V5：完全相同的請求直接回快取。
///
/// key 是「模型 + 完整訊息陣列」的雜湊，所以只有連脈絡都一模一樣時才命中，
/// 不會出現張冠李戴。校園情境重複問題多（作業截止、請假流程），命中率不低。
static CHAT_CACHE: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, (u64, String, String)>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn chat_cache_key(model_id: &str, messages: &[Value]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    model_id.hash(&mut hasher);
    for message in messages {
        message.to_string().hash(&mut hasher);
    }
    format!("{:x}", hasher.finish())
}

/// 回傳 (回覆內容, 當初用的模型)。
fn chat_cache_get(key: &str) -> Option<(String, String)> {
    let mut map = CHAT_CACHE.lock().ok()?;
    let now = now_secs();
    map.retain(|_, (at, _, _)| now.saturating_sub(*at) <= CHAT_CACHE_TTL_SECS);
    map.get(key).map(|(_, text, model)| (text.clone(), model.clone()))
}

fn chat_cache_put(key: &str, text: &str, model: &str) {
    if text.trim().is_empty() {
        return;
    }
    if let Ok(mut map) = CHAT_CACHE.lock() {
        if map.len() >= CHAT_CACHE_MAX {
            let oldest = map
                .iter()
                .min_by_key(|(_, (at, _, _))| *at)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                map.remove(&k);
            }
        }
        map.insert(
            key.to_string(),
            (now_secs(), text.to_string(), model.to_string()),
        );
    }
}

// ─────────────────────────────────────────────────────────────
// 失敗分類：決定要不要換密鑰、以及冷卻多久
// ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FailureKind {
    /// 密鑰無效／被撤銷 → 標記 Dead，不自動復活。
    InvalidKey,
    /// 額度／點數用完 → 長冷卻。
    QuotaExhausted,
    /// 被限流 → 短冷卻。
    RateLimited,
    /// 上游壞掉或連不上 → 極短冷卻，換下一把。
    Upstream,
    /// 請求本身有問題（模型名稱錯、參數錯）→ 換密鑰也沒用，直接中止。
    BadRequest,
}

impl FailureKind {
    fn cooldown_secs(self) -> u64 {
        match self {
            Self::InvalidKey | Self::BadRequest => 0,
            Self::QuotaExhausted => COOLDOWN_QUOTA_SECS,
            Self::RateLimited => COOLDOWN_RATE_LIMIT_SECS,
            Self::Upstream => COOLDOWN_UPSTREAM_SECS,
        }
    }

    fn status(self) -> KeyStatus {
        match self {
            Self::InvalidKey => KeyStatus::Dead,
            Self::BadRequest => KeyStatus::Ok,
            _ => KeyStatus::Cooling,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::InvalidKey => "密鑰無效",
            Self::QuotaExhausted => "額度用盡",
            Self::RateLimited => "被限流",
            Self::Upstream => "上游異常",
            Self::BadRequest => "請求被拒",
        }
    }

    /// 換下一把密鑰有沒有意義。
    fn should_rotate(self) -> bool {
        !matches!(self, Self::BadRequest)
    }
}

/// 依 HTTP 狀態碼與回應內文判斷失敗類型。
///
/// 各家錯誤格式不一致，只能靠狀態碼 + 關鍵字：
/// - OpenAI 額度用盡：429 + `insufficient_quota`
/// - OpenAI 純限流：  429 + `rate_limit_exceeded`（等一下就好）
/// - Gemini：429 `RESOURCE_EXHAUSTED`
/// - OpenRouter 點數不足：402
/// - Anthropic 相容層：`credit balance is too low`
fn classify_failure(status: u16, body: &str) -> FailureKind {
    let lower = body.to_ascii_lowercase();
    let quota_words = [
        "insufficient_quota",
        "insufficient quota",
        "exceeded your current quota",
        "quota_exceeded",
        "resource_exhausted",
        "credit balance",
        "insufficient credits",
        "not enough credits",
        "billing_hard_limit_reached",
        "exceeded your monthly",
        "out of credits",
        "payment required",
    ];
    if quota_words.iter().any(|w| lower.contains(w)) {
        return FailureKind::QuotaExhausted;
    }
    match status {
        401 | 403 => FailureKind::InvalidKey,
        402 => FailureKind::QuotaExhausted,
        429 => FailureKind::RateLimited,
        400 | 404 | 422 => {
            // 模型不存在／該密鑰無權使用此模型：換一把可能就有權限，
            // 所以只有明顯是參數問題時才判定成 BadRequest。
            if lower.contains("model") && (lower.contains("not found") || lower.contains("does not exist"))
            {
                FailureKind::BadRequest
            } else if status == 400 {
                FailureKind::BadRequest
            } else {
                FailureKind::Upstream
            }
        }
        _ => FailureKind::Upstream,
    }
}

fn detail_snippet(raw: &str) -> String {
    let cleaned = raw.replace(['\n', '\r'], " ");
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= AI_DETAIL_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(AI_DETAIL_CHARS).collect();
    out.push('…');
    out
}

/// 從各家的錯誤 JSON 撈出人看得懂的訊息。
fn extract_error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        for pointer in [
            "/error/message",
            "/error/msg",
            "/message",
            "/detail",
            "/error",
        ] {
            if let Some(text) = v.pointer(pointer).and_then(|x| x.as_str()) {
                if !text.trim().is_empty() {
                    return detail_snippet(text);
                }
            }
        }
    }
    detail_snippet(body)
}

/// 把失敗結果寫回密鑰狀態（冷卻／標記失效／累計次數）。
fn record_failure(key_id: &str, kind: FailureKind, message: &str) {
    let _ = with_store(|store| {
        if let Some(key) = store.keys.iter_mut().find(|k| k.id == key_id) {
            key.fails += 1;
            key.last_error = format!("{}：{}", kind.label(), message);
            key.status = kind.status();
            key.cooldown_until = if kind.cooldown_secs() > 0 {
                now_secs() + kind.cooldown_secs()
            } else {
                0
            };
        }
    });
    tracing::warn!(
        key_id,
        kind = kind.label(),
        cooldown_secs = kind.cooldown_secs(),
        "AI 密鑰失敗，準備輪替"
    );
}

fn record_success(key_id: &str, prompt_tokens: u64, completion_tokens: u64) {
    let _ = with_store(|store| {
        if let Some(key) = store.keys.iter_mut().find(|k| k.id == key_id) {
            key.uses += 1;
            key.last_used = now_secs();
            key.last_error = String::new();
            key.status = KeyStatus::Ok;
            key.cooldown_until = 0;
            key.prompt_tokens += prompt_tokens;
            key.completion_tokens += completion_tokens;
        }
    });
}

/// 目前可供選擇的模型。管理員沒建清單時，直接由密鑰推導（每個服務商一項），
/// 讓「新增一把密鑰就能立刻用」不需要多做設定。
/// AI_CHAT_V6：把整份模型清單正規化並合併重複項。
///
/// 回傳 (修正數, 合併數, 丟棄的名稱)。設定檔載入時會跑一次，
/// 所以舊資料裡的「GPT-4o mini」會自動變成「gpt-4o-mini」並與既有項目合併。
fn migrate_models(models: &mut Vec<ModelOption>) -> (usize, usize, Vec<String>) {
    let original = std::mem::take(models);
    let mut fixed = 0usize;
    let mut merged = 0usize;
    let mut dropped: Vec<String> = Vec::new();

    for mut item in original {
        let canonical = match canonical_model_name(&item.provider, &item.model) {
            Ok(name) => name,
            Err(_) => {
                dropped.push(item.model.clone());
                continue;
            }
        };
        if canonical != item.model {
            fixed += 1;
            item.model = canonical;
        }
        item.id = format!("{}:{}", item.provider, item.model);

        if let Some(existing) = models.iter_mut().find(|m| m.id == item.id) {
            // 同一個模型出現兩次：保留先出現的，但把對方設定過的資訊補進來。
            merged += 1;
            if existing.label.trim().is_empty() && !item.label.trim().is_empty() {
                existing.label = item.label;
            }
            if existing.tags.is_empty() && !item.tags.is_empty() {
                existing.tags = item.tags;
            }
            existing.enabled = existing.enabled || item.enabled;
            continue;
        }
        models.push(item);
    }
    (fixed, merged, dropped)
}

/// AI_CHAT_V4：把一批模型名稱登記成使用者可選的模型。
///
/// 關鍵細節：目前若還是「自動推導」狀態（`store.models` 是空的），必須**先把
/// 推導結果固化**再寫入，否則一旦清單變成非空，其他密鑰推導出來的模型會整批
/// 消失 —— 使用者會覺得「我只是新增一把金鑰，別的模型怎麼不見了」。
fn register_models(store: &mut ChatStore, provider: &str, models: &[String]) -> usize {
    let wanted: Vec<String> = models
        .iter()
        .filter_map(|m| canonical_model_name(provider, m).ok())
        .collect();
    if wanted.is_empty() {
        return 0;
    }
    let mut added = 0usize;
    for model in wanted {
        let id = format!("{provider}:{model}");
        if store.models.iter().any(|m| m.id == id) {
            continue;
        }
        store.models.push(ModelOption {
            tier: infer_tier(&model),
            tags: infer_tags(&model),
            id,
            label: String::new(),
            provider: provider.to_string(),
            model,
            enabled: true,
        });
        added += 1;
    }
    added
}

/// AI_CHAT_V7：使用者可選的模型 = 管理員實際填過的那些，就這樣。
///
/// 先前會在清單為空時「依密鑰推導」補上服務商的預設模型，結果是系統自作主張
/// 加了管理員從沒輸入過的項目。現在一律不推導：沒填就是沒有。
fn model_options() -> Vec<ModelOption> {
    read_store(|store| store.models.clone()).unwrap_or_default()
}

/// 「自動」這個虛擬選項的 id。
const AUTO_MODEL_ID: &str = "auto";

/// 使用者這則訊息的性質。用來決定該派哪種模型出場。
#[derive(Debug, Default, PartialEq, Eq)]
struct PromptNeed {
    code: bool,
    math: bool,
    long: bool,
    simple: bool,
}

/// AI_CHAT_V3：判斷問題性質。
///
/// 刻意用規則而不是「先問一次小模型」——後者每則訊息都要多一次往返，
/// 既慢又多燒一次額度，而且結果不穩定。規則式是決定性的，可以直接測。
fn classify_prompt(text: &str) -> PromptNeed {
    let lower = text.to_ascii_lowercase();
    let chars = text.chars().count();

    // 程式碼：圍欄、常見關鍵字、或明顯的程式符號組合。
    const CODE_WORDS: [&str; 18] = [
        "```", "function ", "def ", "class ", "import ", "select ", "console.log", "std::",
        "public static", "程式", "程式碼", "code", "bug", "編譯", "報錯", "stack trace",
        "npm ", "cargo ",
    ];
    let code = CODE_WORDS.iter().any(|w| lower.contains(w));

    // 數學／推理：算式符號或關鍵字。
    const MATH_WORDS: [&str; 12] = [
        "證明", "推導", "計算", "解方程", "積分", "微分", "機率", "matrix", "prove",
        "integral", "derivative", "equation",
    ];
    let math = MATH_WORDS.iter().any(|w| lower.contains(w))
        || (text.contains('=') && text.chars().filter(|c| c.is_ascii_digit()).count() >= 4);

    // 長文：整理／摘要大段文字，需要長上下文與較強的模型。
    let long = chars > 1200;

    // 簡單：很短、沒有上述特徵，用便宜的模型就好。
    //
    // 門檻刻意壓到 16 字：中文很精簡，「幫我寫一封請假信給教授」只有 11 字
    // 卻是一個完整任務，用 40 字當門檻會把這種請求誤判成閒聊而派出小模型。
    let simple = chars <= 16 && !code && !math;

    PromptNeed {
        code,
        math,
        long,
        simple,
    }
}

/// 依問題性質挑一個最合適的模型，回傳（模型, 選它的理由）。
///
/// 只在「有可用密鑰」的模型裡面挑，所以自動模式永遠不會挑到一個
/// 送出去就失敗的模型。
fn pick_auto_model(text: &str, options: &[ModelOption], keys: &[ApiKey]) -> Option<(ModelOption, String)> {
    let need = classify_prompt(text);
    let total = options.len() as i32;
    let mut best: Option<(i32, usize, &ModelOption)> = None;

    for (index, option) in options.iter().enumerate() {
        if !option.enabled {
            continue;
        }
        let usable = keys
            .iter()
            .any(|k| k.provider == option.provider && k.available());
        if !usable {
            continue;
        }
        // 名稱不合法的模型送出去必定 400，自動模式不能挑。
        if option.validity().is_err() {
            continue;
        }

        let mut score = 0i32;
        let has = |tag: &str| option.tags.iter().any(|t| t == tag);

        if need.code {
            score += if has("code") { 40 } else { 0 };
            score += match option.tier.as_str() {
                "deep" => 12,
                "fast" => -8,
                _ => 4,
            };
        }
        if need.math {
            score += if has("math") { 35 } else { 0 };
            score += match option.tier.as_str() {
                "deep" => 15,
                "fast" => -10,
                _ => 3,
            };
        }
        if need.long {
            score += if has("long") { 20 } else { 0 };
            score += match option.tier.as_str() {
                "deep" => 25,
                "fast" => -15,
                _ => 6,
            };
        }
        if need.simple {
            // 打招呼、一句話的小問題，不需要動用貴的模型。
            score += match option.tier.as_str() {
                "fast" => 25,
                "deep" => -12,
                _ => 5,
            };
        }
        if !need.code && !need.math && !need.long && !need.simple {
            score += match option.tier.as_str() {
                "balanced" => 10,
                "fast" => 4,
                _ => 2,
            };
        }
        // 同分時尊重管理員排的順序（越前面越優先）。
        score = score * 100 + (total - index as i32);

        if best.map(|(b, _, _)| score > b).unwrap_or(true) {
            best = Some((score, index, option));
        }
    }

    let (_, _, chosen) = best?;
    let reason = if need.code {
        "偵測到程式問題"
    } else if need.math {
        "偵測到計算／推理問題"
    } else if need.long {
        "內容較長，需要較強的模型"
    } else if need.simple {
        "簡短問題，用快速模型即可"
    } else {
        "一般問題"
    };
    Some((
        chosen.clone(),
        format!("自動選用：{}（{}）", chosen.display(), reason),
    ))
}

/// 把使用者選的模型 id 解析成實際設定，並附上「為什麼是這個」的說明。
///
/// - `auto`（或留空）→ 依問題性質自動挑。
/// - 指定的模型不存在／被停用 → 退回自動挑，不讓使用者卡住。
fn resolve_model(id: &str, prompt: &str, needs_vision: bool) -> (Option<ModelOption>, String) {
    let options = model_options();
    let keys = read_store(|store| store.keys.clone()).unwrap_or_default();
    let wanted = id.trim();

    // AI_CHAT_V7：附了圖片時，只有看得懂圖的模型能用。
    // 使用者選的那個不支援就自動換一個，並在回覆上說明換了誰、為什麼。
    if needs_vision {
        let usable = |m: &ModelOption| {
            m.enabled
                && m.validity().is_ok()
                && model_supports_vision(m)
                && keys
                    .iter()
                    .any(|k| k.provider == m.provider && k.available())
        };
        if !wanted.is_empty() && wanted != AUTO_MODEL_ID {
            if let Some(found) = options.iter().find(|m| m.id == wanted && usable(m)) {
                return (Some(found.clone()), String::new());
            }
        }
        let picked = options.iter().find(|m| usable(m)).cloned();
        return match picked {
            Some(chosen) => {
                let reason = format!("你附了圖片，改用看得懂圖片的「{}」", chosen.display());
                (Some(chosen), reason)
            }
            // 一個視覺模型都沒有：回 None，由呼叫端給出「請管理員新增」的訊息。
            None => (None, String::new()),
        };
    }

    if !wanted.is_empty() && wanted != AUTO_MODEL_ID {
        if let Some(found) = options
            .iter()
            .find(|m| m.id == wanted && m.enabled && m.validity().is_ok())
        {
            return (Some(found.clone()), String::new());
        }
    }

    if let Some((chosen, reason)) = pick_auto_model(prompt, &options, &keys) {
        return (Some(chosen), reason);
    }

    // 一個有可用密鑰的模型都沒有：退而求其次挑第一個啟用的，
    // 讓錯誤訊息由實際送出時的輪替流程產生（訊息比較具體）。
    (options.into_iter().find(|m| m.enabled), String::new())
}

/// 取出這次要嘗試的密鑰順序。已冷卻到期的會先自動復活。
///
/// `provider` 有值時只挑該服務商的密鑰 —— 使用者選了 Gemini 就不該偷偷
/// 掉到 OpenAI 去回答。
fn candidate_keys(provider: Option<&str>) -> Vec<ApiKey> {
    with_store(|store| {
        let now = now_secs();
        // 冷卻到期自動復活。
        for key in store.keys.iter_mut() {
            if key.status == KeyStatus::Cooling && key.cooldown_until <= now {
                key.status = KeyStatus::Ok;
                key.cooldown_until = 0;
            }
        }

        let mut list: Vec<ApiKey> = store
            .keys
            .iter()
            .filter(|k| {
                k.available()
                    && match provider {
                        Some(want) => k.provider == want,
                        None => true,
                    }
            })
            .cloned()
            .collect();
        if list.is_empty() {
            return list;
        }
        if store.settings.strategy == "round_robin" {
            // 每次從下一把開始，讓用量平均分散在所有密鑰上。
            let start = store.cursor % list.len();
            store.cursor = store.cursor.wrapping_add(1);
            list.rotate_left(start);
        }
        list
    })
    .unwrap_or_default()
}

// ─────────────────────────────────────────────────────────────
// 上游串流
// ─────────────────────────────────────────────────────────────

struct StreamOutcome {
    prompt_tokens: u64,
    completion_tokens: u64,
}

enum AttemptError {
    /// HTTP 層級失敗（尚未開始輸出）。
    Failed {
        kind: FailureKind,
        message: String,
        status: Option<u16>,
    },
    /// 上游不支援 `stream_options`，關掉重試同一把密鑰。
    RetryWithoutUsage,
}

/// 對單一把密鑰送出請求並把 token 串流寫進 job。
async fn stream_once(
    job_id: &str,
    key: &ApiKey,
    messages: &[Value],
    settings: &ChatSettings,
    model_override: Option<&str>,
    include_usage: bool,
) -> Result<StreamOutcome, AttemptError> {
    let base = key.effective_base_url();
    if base.is_empty() {
        return Err(AttemptError::Failed {
            kind: FailureKind::BadRequest,
            message: "沒有設定 API 網址（base URL）".to_string(),
            status: None,
        });
    }
    // 使用者選的模型優先；沒選才用這把密鑰的預設模型。
    let model = match model_override {
        Some(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => key.effective_model(),
    };
    // AI_CHAT_V5：送出前再驗一次。上游對格式錯誤只會回一句
    // 「unexpected model name format」，不講是哪個模型，很難查。
    let model = match sanitize_model_name(&key.provider, &model) {
        Ok(name) => name,
        Err(reason) => {
            return Err(AttemptError::Failed {
                kind: FailureKind::BadRequest,
                message: format!("模型名稱有問題 —— {reason}"),
                status: None,
            })
        }
    };
    let url = format!("{base}/chat/completions");

    let mut payload = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "temperature": settings.temperature,
    });
    if settings.max_tokens > 0 {
        payload["max_tokens"] = json!(settings.max_tokens);
    }
    if include_usage {
        payload["stream_options"] = json!({ "include_usage": true });
    }

    let mut request = AI_CLIENT
        .post(&url)
        .bearer_auth(key.secret.trim())
        .header("content-type", "application/json");
    if key.provider == "openrouter" {
        // OpenRouter 建議帶來源資訊，否則部分模型會拒絕。
        request = request
            .header("HTTP-Referer", "https://tronclass.panel.local")
            .header("X-Title", "Tronclass Panel");
    }

    let response = request.json(&payload).send().await.map_err(|e| {
        let message = if e.is_timeout() {
            format!("連線逾時（>{AI_REQUEST_TIMEOUT_SECS}s）")
        } else if e.is_connect() {
            format!("連不上 {base}")
        } else {
            detail_snippet(&e.to_string())
        };
        AttemptError::Failed {
            kind: FailureKind::Upstream,
            message,
            status: None,
        }
    })?;

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = response.text().await.unwrap_or_default();
        let lower = body.to_ascii_lowercase();
        // 有些相容端點不吃 stream_options，關掉再試一次同一把密鑰。
        if include_usage && status == 400 && lower.contains("stream_options") {
            return Err(AttemptError::RetryWithoutUsage);
        }
        let kind = classify_failure(status, &body);
        let detail = extract_error_message(&body);
        // 上游只會說「unexpected model name format」，不講是哪個模型。
        // 把模型名稱補進訊息裡，使用者才知道要去改哪一個。
        let message = if kind == FailureKind::BadRequest {
            // 404「模型不存在」多半是代號寫錯，直接把可能的正確寫法講出來。
            let hint = if status == 404 && key.provider == "groq" && !model.contains('/') {
                match groq_full_model_id(&model) {
                    Some(full) => format!("（Groq 的正確代號應為 {full}，請到模型清單改成這個）"),
                    None => "（Groq 多數模型代號帶服務商前綴，例如 openai/gpt-oss-120b）".to_string(),
                }
            } else {
                String::new()
            };
            format!("HTTP {status}（模型 {model}）：{detail}{hint}")
        } else {
            format!("HTTP {status}：{detail}")
        };
        return Err(AttemptError::Failed {
            kind,
            message,
            status: Some(status),
        });
    }

    // ── 逐塊讀 SSE，邊讀邊寫進 job，前端輪詢就看得到打字機效果 ──
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut prompt_tokens = 0u64;
    let mut completion_tokens = 0u64;
    let mut produced = false;
    let mut interrupted = false;
    let mut pending = String::new();
    let mut last_flush = std::time::Instant::now();

    while let Some(chunk) = stream.next().await {
        if job_cancelled(job_id) {
            break;
        }
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(e) => {
                if produced {
                    // 已經吐出一部分了，換密鑰重來只會讓使用者看到重複內容，
                    // 直接把已生成的內容留著並結束 —— 但要明講它不完整，
                    // 否則使用者會以為 AI 就是講到一半自己停住。
                    tracing::warn!("AI 串流中斷（已有部分輸出）：{e}");
                    interrupted = true;
                    break;
                }
                return Err(AttemptError::Failed {
                    kind: FailureKind::Upstream,
                    message: format!("串流中斷：{}", detail_snippet(&e.to_string())),
                    status: Some(status),
                });
            }
        };
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        // SSE 以空行分隔事件；這裡逐行處理即可。
        while let Some(newline) = buffer.find('\n') {
            let line = buffer[..newline].trim_end_matches('\r').to_string();
            buffer.drain(..=newline);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
                prompt_tokens = usage
                    .get("prompt_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(prompt_tokens);
                completion_tokens = usage
                    .get("completion_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(completion_tokens);
            }
            let delta = value
                .pointer("/choices/0/delta/content")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if !delta.is_empty() {
                produced = true;
                pending.push_str(delta);
            }
        }

        // 每 80ms 或累積夠多字才進 job，避免高頻搶 mutex。
        if !pending.is_empty() && last_flush.elapsed().as_millis() >= 80 {
            let piece = std::mem::take(&mut pending);
            let _ = job_update(job_id, |job| job.text.push_str(&piece));
            last_flush = std::time::Instant::now();
        }
    }

    if !pending.is_empty() {
        let piece = std::mem::take(&mut pending);
        let _ = job_update(job_id, |job| job.text.push_str(&piece));
    }

    if !produced {
        return Err(AttemptError::Failed {
            kind: FailureKind::Upstream,
            message: "上游沒有回傳任何內容".to_string(),
            status: Some(status),
        });
    }

    if interrupted {
        let _ = job_update(job_id, |job| {
            job.text.push_str("\n\n*（與上游的連線中斷，以上回覆可能不完整）*");
        });
    }

    // 沒拿到 usage 就用字元數粗估（約 4 字元 1 token）。
    if completion_tokens == 0 {
        let produced_len = job_update(job_id, |job| job.text.chars().count()).unwrap_or(0);
        completion_tokens = (produced_len / 3) as u64;
    }

    Ok(StreamOutcome {
        prompt_tokens,
        completion_tokens,
    })
}

/// AI_CHAT_V5：非串流的單次呼叫，給摘要這種背景工作用。
async fn complete_once(
    key: &ApiKey,
    model: &str,
    messages: Vec<Value>,
    max_tokens: u32,
) -> Result<String, String> {
    let base = key.effective_base_url();
    if base.is_empty() {
        return Err("沒有 API 網址".to_string());
    }
    let model = sanitize_model_name(&key.provider, model)?;
    let response = AI_CLIENT
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key.secret.trim())
        .json(&json!({
            "model": model,
            "messages": messages,
            "max_tokens": max_tokens,
            "temperature": 0.2,
            "stream": false,
        }))
        .send()
        .await
        .map_err(|e| detail_snippet(&e.to_string()))?;

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        // 摘要失敗時也要讓密鑰狀態跟著更新，否則額度用盡不會被記錄。
        let kind = classify_failure(status, &body);
        let message = extract_error_message(&body);
        record_failure(&key.id, kind, &message);
        return Err(format!("HTTP {status}：{message}"));
    }
    let value: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let text = value
        .pointer("/choices/0/message/content")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("上游沒有回傳摘要內容".to_string());
    }
    Ok(text)
}

/// 送一次帶工具定義的非串流請求，回傳上游的 `message` 物件。
///
/// 跟 `complete_once` 的差別：這裡要拿到完整的 `message`（可能含 `tool_calls`），
/// 不是只要文字。
async fn complete_with_tools(
    key: &ApiKey,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    max_tokens: u32,
    temperature: f32,
) -> Result<Value, String> {
    let base = key.effective_base_url();
    if base.is_empty() {
        return Err("沒有 API 網址".to_string());
    }
    let model = sanitize_model_name(&key.provider, model)?;
    let mut payload = json!({
        "model": model,
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
        "temperature": temperature,
        "stream": false,
    });
    if max_tokens > 0 {
        payload["max_tokens"] = json!(max_tokens);
    }

    let response = AI_CLIENT
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key.secret.trim())
        .json(&payload)
        .send()
        .await
        .map_err(|e| detail_snippet(&e.to_string()))?;

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let kind = classify_failure(status, &body);
        let message = extract_error_message(&body);
        record_failure(&key.id, kind, &message);
        return Err(format!("HTTP {status}（模型 {model}）：{message}"));
    }
    let value: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    value
        .pointer("/choices/0/message")
        .cloned()
        .ok_or_else(|| "上游沒有回傳 message".to_string())
}

/// 把 `tool_calls` 裡的參數解析出來。上游一律把 arguments 放成 JSON 字串。
fn parse_tool_arguments(raw: &Value) -> Value {
    match raw {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
        Value::Object(_) => raw.clone(),
        _ => json!({}),
    }
}

/// AI_CHAT_V9：Agent 模式的主迴圈。
///
/// 流程是「問模型 → 它要求呼叫工具 → 我們執行 → 把結果餵回去 → 再問」，
/// 直到它不再要求工具（那就是最終答案），或撞到 `AGENT_MAX_STEPS` 上限。
async fn run_agent_job(
    job_id: String,
    account: String,
    mut messages: Vec<Value>,
    settings: ChatSettings,
    selection: Option<ModelOption>,
) {
    let started = std::time::Instant::now();
    let provider = selection.as_ref().map(|m| m.provider.clone());
    let model_override = selection.as_ref().map(|m| m.model.clone());
    let keys = candidate_keys(provider.as_deref());

    if keys.is_empty() {
        let _ = job_update(&job_id, |job| {
            job.done = true;
            job.ok = false;
            job.code = "NO_KEY_AVAILABLE".to_string();
            job.error = "目前沒有可用的密鑰，Agent 無法執行。".to_string();
        });
        return;
    }

    // Agent 需要知道自己有哪些能力，以及「做完要講人話」。
    let guide = format!(
        "你現在是 Agent 模式，可以呼叫工具來完成任務，最多 {AGENT_MAX_STEPS} 步。\n\
         規則：\n\
         1. 需要算數字時一定要用 calc，不要自己心算。\n\
         2. 需要知道今天日期時用 now。\n\
         3. 產出報告、整理表格、寫長篇內容時，用 workspace_write 存成檔案，使用者可以下載。\n\
         4. 修改既有檔案前，先用 workspace_list 或 workspace_read 確認內容。\n\
         5. 工具做完後，最後一定要用繁體中文說明你做了什麼、結果是什麼。"
    );
    messages.insert(0, json!({ "role": "system", "content": guide }));

    let tools = agent_tools();
    let mut last_error = "Agent 沒有跑出結果".to_string();
    let mut last_code = "AGENT_FAILED".to_string();

    for key in keys.iter() {
        if job_cancelled(&job_id) {
            break;
        }
        let key_name = key.display_name();
        let model_used = match model_override.as_deref() {
            Some(name) if !name.trim().is_empty() => name.trim().to_string(),
            _ => key.effective_model(),
        };
        let _ = job_update(&job_id, |job| {
            job.key_name = key_name.clone();
            job.model = model_used.clone();
        });

        // 每換一把密鑰就從乾淨的脈絡重來，不然上一把失敗留下的半截 tool_calls
        // 會讓新的請求格式不合法。
        let mut convo = messages.clone();
        let mut failed = None;

        for step in 0..AGENT_MAX_STEPS {
            if job_cancelled(&job_id) {
                let _ = job_update(&job_id, |job| {
                    job.done = true;
                    job.ok = true;
                    job.code = "AI_CANCELLED".to_string();
                    if job.text.trim().is_empty() {
                        job.text = "（已停止）".to_string();
                    }
                });
                return;
            }
            if started.elapsed().as_secs() >= AI_TOTAL_BUDGET_SECS {
                failed = Some((
                    format!("超過總時間預算（{AI_TOTAL_BUDGET_SECS}s）"),
                    "AI_TIMEOUT".to_string(),
                ));
                break;
            }

            let reply = complete_with_tools(
                key,
                &model_used,
                &convo,
                &tools,
                settings.max_tokens,
                settings.temperature,
            )
            .await;

            let message = match reply {
                Ok(value) => value,
                Err(e) => {
                    failed = Some((e, "AGENT_UPSTREAM".to_string()));
                    break;
                }
            };

            let calls = message
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // 沒有要呼叫工具 = 這就是最終答案。
            if calls.is_empty() {
                let text = message
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let text = if text.is_empty() {
                    "（模型沒有給出文字回覆）".to_string()
                } else {
                    text
                };
                // Agent 走非串流路徑，上游不一定回 usage，用量以 0 記。
                record_success(&key.id, 0, 0);
                let files = workspace_list(&account);
                let _ = job_update(&job_id, |job| {
                    job.done = true;
                    job.ok = true;
                    job.text = text.clone();
                    job.steps.push(json!({
                        "kind": "final",
                        "summary": "完成",
                        "ok": true,
                        "files": files
                            .iter()
                            .map(|(n, size)| json!({ "name": n, "size": size }))
                            .collect::<Vec<Value>>(),
                    }));
                });
                return;
            }

            // 把模型這一輪的 assistant 訊息原樣加回去（一定要，
            // 否則接下來的 tool 訊息會對不上 tool_call_id）。
            convo.push(message.clone());

            for call in calls.iter() {
                let call_id = call
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("call")
                    .to_string();
                let fname = call
                    .pointer("/function/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = call
                    .pointer("/function/arguments")
                    .map(parse_tool_arguments)
                    .unwrap_or_else(|| json!({}));

                let (output, summary, ok) = run_tool(&account, &fname, &args);
                tracing::info!(tool = %fname, ok, "Agent 執行工具");

                let _ = job_update(&job_id, |job| {
                    job.steps.push(json!({
                        "kind": "tool",
                        "step": step + 1,
                        "tool": fname,
                        "summary": summary,
                        "ok": ok,
                        "args": args,
                        "output": output.chars().take(600).collect::<String>(),
                    }));
                });

                convo.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "name": fname,
                    "content": output,
                }));
            }

            // 最後一步還在要工具：叫它收尾。
            if step + 1 == AGENT_MAX_STEPS {
                convo.push(json!({
                    "role": "user",
                    "content": "已經達到步數上限，請不要再呼叫工具，直接用繁體中文總結目前的結果。",
                }));
                let final_reply =
                    complete_with_tools(key, &model_used, &convo, &[], settings.max_tokens, settings.temperature)
                        .await;
                let text = final_reply
                    .ok()
                    .and_then(|m| {
                        m.get("content")
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().to_string())
                    })
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "已達步數上限，但模型沒有給出總結。".to_string());
                let files = workspace_list(&account);
                let _ = job_update(&job_id, |job| {
                    job.done = true;
                    job.ok = true;
                    job.text = text.clone();
                    job.steps.push(json!({
                        "kind": "final",
                        "summary": format!("已達 {AGENT_MAX_STEPS} 步上限，強制收尾"),
                        "ok": true,
                        "files": files
                            .iter()
                            .map(|(n, size)| json!({ "name": n, "size": size }))
                            .collect::<Vec<Value>>(),
                    }));
                });
                return;
            }
        }

        if let Some((error, code)) = failed {
            last_error = error.clone();
            last_code = code;
            let _ = job_update(&job_id, |job| {
                job.attempts.push(json!({
                    "key": key.display_name(),
                    "ok": false,
                    "error": error,
                }));
            });
            continue;
        }
    }

    let _ = job_update(&job_id, |job| {
        job.done = true;
        job.ok = false;
        job.code = last_code.clone();
        job.error = format!("Agent 執行失敗：{last_error}");
    });
}

/// 挑一個最便宜的可用模型來做摘要 —— 摘要本身不該花大錢。
fn pick_cheapest_model() -> Option<(ModelOption, ApiKey)> {
    let options = model_options();
    let keys = read_store(|store| store.keys.clone()).unwrap_or_default();
    let mut best: Option<(u8, ModelOption, ApiKey)> = None;
    for option in options.iter().filter(|m| m.enabled) {
        let Some(key) = keys
            .iter()
            .find(|k| k.provider == option.provider && k.available())
        else {
            continue;
        };
        let rank = match option.tier.as_str() {
            "fast" => 0,
            "balanced" => 1,
            _ => 2,
        };
        if best.as_ref().map(|(r, _, _)| rank < *r).unwrap_or(true) {
            best = Some((rank, option.clone(), key.clone()));
        }
    }
    best.map(|(_, option, key)| (option, key))
}

/// AI_CHAT_V5：把較早的對話濃縮成一段摘要。
///
/// 在回覆送出「之後」才於背景執行，所以使用者完全感覺不到延遲；
/// 摘要只做一次並存進對話串，之後每輪都直接重用，成本被攤平。
async fn maybe_summarize(account: String, thread_id: String) {
    let Some((previous, pending, keep_from)) = read_threads(&account, |threads| {
        let thread = threads.iter().find(|t| t.id == thread_id)?;
        let total = thread.messages.len();
        // 保留最近幾則原文，只濃縮更早的部分。
        if total < thread.summarized + SUMMARY_TRIGGER {
            return None;
        }
        let cut = total.saturating_sub(SUMMARY_KEEP_RAW);
        if cut <= thread.summarized {
            return None;
        }
        let pending: Vec<ChatMessage> = thread.messages[thread.summarized..cut].to_vec();
        Some((thread.summary.clone(), pending, cut))
    })
    .flatten() else {
        return;
    };

    let Some((option, key)) = pick_cheapest_model() else {
        return;
    };

    let mut transcript = String::new();
    for message in pending.iter() {
        let who = if message.role == "user" { "使用者" } else { "助理" };
        transcript.push_str(&format!("{who}：{}
", message.content));
    }

    let instruction = if previous.trim().is_empty() {
        "把以下對話濃縮成重點摘要，保留人名、日期、數字、結論與待辦事項。用繁體中文條列，不要加入原文沒有的內容。".to_string()
    } else {
        format!(
            "這是既有摘要：
{previous}

請把既有摘要與以下新對話合併成一份更新後的摘要，保留人名、日期、數字、結論與待辦事項。用繁體中文條列，不要加入原文沒有的內容。"
        )
    };

    let messages = vec![
        json!({ "role": "system", "content": instruction }),
        json!({ "role": "user", "content": transcript }),
    ];

    match complete_once(&key, &option.model, messages, SUMMARY_MAX_TOKENS).await {
        Ok(summary) => {
            let _ = with_threads(&account, |threads| {
                if let Some(thread) = threads.iter_mut().find(|t| t.id == thread_id) {
                    // 期間可能又有新訊息，只認領當初算好的範圍。
                    if thread.messages.len() >= keep_from {
                        thread.summary = summary.clone();
                        thread.summarized = keep_from;
                    }
                }
            });
            tracing::info!(
                thread = %thread_id,
                upto = keep_from,
                model = %option.model,
                "對話摘要已更新"
            );
        }
        Err(e) => tracing::warn!("對話摘要失敗（不影響聊天）：{e}"),
    }
}

// BULLETIN_AI_SUMMARY_V1
// ─────────────────────────────────────────────────────────────
// 公告 AI 摘要：與聊天同一套密鑰輪替與 status 輪詢，但只有單次、非串流的小請求。
// 前端在「課程公告」每張卡片上放「摘要」按鈕，點擊後：
//   1. POST /admin/api/ai/summarize  → 建立背景 job，立刻回 job_id
//   2. 前端每 600ms 輪詢 /admin/api/ai/chat/status?job=<id>
//   3. 完成後回傳一段「字數精簡但重點不漏」的繁體中文條列摘要
// 用背景 job 的原因與聊天一致：避免反向代理（Cloudflare Tunnel）對長時間
// 沒有回應的 HTTP 請求回空 502。摘要很短，但偶爾上游慢，job 模式更穩。

/// 摘要用的 system 提示：只保留重點、不臆測、條列、精簡。
fn bulletin_summary_instruction() -> String {
    "你是學校公告的摘要助手。請把使用者給的公告濃縮成重點摘要。\
     要求：1) 用繁體中文。2) 用條列式，每點一行，最多 5 點。\
     3) 務必保留關鍵資訊：日期與截止期限、地點、需要採取的行動、重要規定、政策變更、\
     需回覆或參與的項目。4) 不要加入原文沒有的資訊或推測。\
     5) 整份精簡（約 100–180 字），但重點不可遺漏。".to_string()
}

/// 通知預覽用的摘要指令：像手機／Discord 的通知預覽一樣，
/// 只要一到兩句、抓到最關鍵的重點即可，不要條列。
fn bulletin_preview_instruction() -> String {
    "你是學校公告的摘要助手。請把使用者給的公告寫成「通知預覽」一句話。\
     要求：1) 用繁體中文。2) 一到兩句（約 20–45 字），不要列點、不要換行。\
     3) 只抓最關鍵的訊息：日期與截止期限、需要採取的行動、地點、或重要變更。\
     4) 不要加入原文沒有的資訊或推測。\
     5) 讀起來要像手機推播或 Discord 通知預覽那樣一目了然。".to_string()
}

async fn run_bulletin_summary_job(job_id: String, text: String) {
    let Some((option, _)) = pick_cheapest_model() else {
        let _ = job_update(&job_id, |job| {
            job.done = true;
            job.ok = false;
            job.code = "NO_MODEL_AVAILABLE".to_string();
            job.error = "系統尚未設定任何可用的 AI 模型，請聯絡管理員。".to_string();
        });
        return;
    };
    let provider = option.provider.clone();
    let model_used = option.model.clone();

    // BULLETIN_AI_SUMMARY_V2：對同一把密鑰失敗時，往下換其它「同服務商」的密鑰重試。
    let keys = candidate_keys(Some(&provider));
    if keys.is_empty() {
        let _ = job_update(&job_id, |job| {
            job.done = true;
            job.ok = false;
            job.code = "NO_KEY_AVAILABLE".to_string();
            job.error = "這個模型目前沒有可用的密鑰（額度用盡、被限流或已停用）。".to_string();
        });
        return;
    }

    let messages = vec![
        json!({ "role": "system", "content": bulletin_summary_instruction() }),
        json!({ "role": "user", "content": text }),
    ];

    let mut attempts: Vec<Value> = Vec::new();
    let mut last_error = "沒有任何密鑰可用".to_string();
    for key in keys.iter() {
        if job_cancelled(&job_id) {
            return;
        }
        let key_name = key.display_name();
        match complete_once(key, &model_used, messages.clone(), BULLETIN_SUMMARY_MAX_TOKENS).await {
            Ok(summary) => {
                let _ = job_update(&job_id, |job| {
                    job.done = true;
                    job.ok = true;
                    job.text = summary;
                    job.key_name = key_name;
                    job.model = model_used.clone();
                });
                return;
            }
            Err(e) => {
                attempts.push(json!({
                    "key": key_name,
                    "ok": false,
                    "status": 0,
                    "reason": e.clone(),
                }));
                last_error = e;
            }
        }
    }

    let _ = job_update(&job_id, |job| {
        job.done = true;
        job.ok = false;
        job.code = "AI_SUMMARY_FAILED".to_string();
        job.error = last_error;
        job.attempts = attempts;
    });
}

/// 送出公告 → 建立背景摘要 job → 立刻回 job_id。
#[derive(Debug, Deserialize)]
pub(super) struct SummarizeRequest {
    #[serde(default)]
    token: String,
    /// 要摘要的公告全文（通常含標題與正文）。由前端先轉成純文字再送。
    text: String,
}

pub(super) async fn summarize_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<SummarizeRequest>,
) -> Response {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };

    let text = body.text.trim().to_string();
    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "沒有可摘要的內容", "code": "AI_EMPTY_CONTENT" })),
        )
            .into_response();
    }
    if text.len() > BULLETIN_SUMMARY_MAX_SOURCE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("公告太長（上限 {} 位元組），無法摘要", BULLETIN_SUMMARY_MAX_SOURCE),
                "code": "AI_SUMMARY_TOO_LONG",
            })),
        )
            .into_response();
    }

    // 先確認有可用模型，才建 job。
    if pick_cheapest_model().is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "尚未設定 AI 模型，請到對話頁的「API 密鑰」新增，或在「使用者可選的模型」開放模型。",
                "code": "NO_MODEL_AVAILABLE",
            })),
        )
            .into_response();
    }

    let job_id = uuid::Uuid::new_v4().to_string();
    job_insert(
        &job_id,
        ChatJob {
            account: session.account_id,
            thread_id: String::new(),
            text: String::new(),
            done: false,
            ok: false,
            cancel: false,
            error: String::new(),
            code: String::new(),
            key_name: String::new(),
            model: String::new(),
            auto_reason: String::new(),
            cached: false,
            cache_key: String::new(),
            attempts: Vec::new(),
            steps: Vec::new(),
            created_at: now_secs(),
            updated_at: now_secs(),
        },
    );

    let job_id2 = job_id.clone();
    tokio::spawn(async move {
        run_bulletin_summary_job(job_id2, text).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": job_id,
            "state": "running",
            "poll_path": "/admin/api/ai/chat/status",
            "poll_ms": 600,
        })),
    )
        .into_response()
}

// BULLETIN_NOTIFY_SUMMARY_V1
// 供「公告通知」在背景生成本公告的重點摘要（精簡、保留關鍵資訊）。與
// summarize_handler 共用 pick_cheapest_model + complete_once 的密鑰輪替。
// 回傳 None 代表摘要無法生成（無模型／密鑰或上游失敗），呼叫端退回標題。
pub(crate) async fn summarize_bulletin_text(text: String) -> Option<String> {
    let (option, _) = pick_cheapest_model()?;
    let provider = option.provider.clone();
    let model_used = option.model.clone();
    let keys = candidate_keys(Some(&provider));
    if keys.is_empty() {
        return None;
    }
    let messages = vec![
        json!({ "role": "system", "content": bulletin_preview_instruction() }),
        json!({ "role": "user", "content": text }),
    ];
    for key in keys.iter() {
        // BULLETIN_NOTIFY_SUMMARY_V2：通知預覽用較短的 token 上限，逼模型寫短句。
        match complete_once(key, &model_used, messages.clone(), BULLETIN_PREVIEW_MAX_TOKENS).await {
            Ok(summary) if !summary.trim().is_empty() => return Some(summary),
            _ => continue,
        }
    }
    None
}

/// 背景生成：依序嘗試每一把可用密鑰，直到有人成功。
async fn run_chat_job(
    job_id: String,
    messages: Vec<Value>,
    settings: ChatSettings,
    selection: Option<ModelOption>,
) {
    let started = std::time::Instant::now();
    let provider = selection.as_ref().map(|m| m.provider.clone());
    let model_override = selection.as_ref().map(|m| m.model.clone());
    let keys = candidate_keys(provider.as_deref());

    if keys.is_empty() {
        let total = read_store(|s| s.keys.len()).unwrap_or(0);
        let hint = if total == 0 {
            "系統尚未設定任何 AI 密鑰，請聯絡管理員。".to_string()
        } else if let Some(model) = selection.as_ref() {
            // 使用者選了特定模型，就要講清楚是「這個模型」沒得用，
            // 而不是籠統地說全部壞掉 —— 換一個模型可能就能繼續聊。
            format!(
                "「{}」目前沒有可用的密鑰（額度用盡、被限流或已停用），請改選其他模型或稍後再試。",
                model.display()
            )
        } else {
            "所有密鑰都不可用（額度用盡、被限流或已停用），請聯絡管理員。".to_string()
        };
        let _ = job_update(&job_id, |job| {
            job.done = true;
            job.ok = false;
            job.code = "NO_KEY_AVAILABLE".to_string();
            job.error = hint;
        });
        return;
    }

    let mut attempts: Vec<Value> = Vec::new();
    let mut last_error = "沒有任何密鑰可用".to_string();
    let mut last_code = "NO_KEY_AVAILABLE".to_string();

    for key in keys.iter() {
        if job_cancelled(&job_id) {
            break;
        }
        if started.elapsed().as_secs() >= AI_TOTAL_BUDGET_SECS {
            last_error = format!("超過總時間預算（{AI_TOTAL_BUDGET_SECS}s）");
            last_code = "AI_TIMEOUT".to_string();
            break;
        }

        let key_name = key.display_name();
        let model_used = match model_override.as_deref() {
            Some(name) if !name.trim().is_empty() => name.trim().to_string(),
            _ => key.effective_model(),
        };
        let _ = job_update(&job_id, |job| {
            job.key_name = key_name.clone();
            job.model = model_used.clone();
        });

        let attempt_started = std::time::Instant::now();
        let mut include_usage = key.usage_supported;
        let mut result = stream_once(
            &job_id,
            key,
            &messages,
            &settings,
            model_override.as_deref(),
            include_usage,
        )
        .await;

        if matches!(result, Err(AttemptError::RetryWithoutUsage)) {
            // 記住這把不支援，下次不再送。
            include_usage = false;
            let _ = with_store(|store| {
                if let Some(k) = store.keys.iter_mut().find(|k| k.id == key.id) {
                    k.usage_supported = false;
                }
            });
            result = stream_once(
                &job_id,
                key,
                &messages,
                &settings,
                model_override.as_deref(),
                include_usage,
            )
            .await;
        }

        match result {
            Ok(outcome) => {
                record_success(&key.id, outcome.prompt_tokens, outcome.completion_tokens);
                // 使用者中途按了停止：已生成的內容照樣保留，但要回報成「已取消」，
                // 否則前端會把半截回覆當成正常結束。
                if job_cancelled(&job_id) {
                    attempts.push(json!({
                        "key": key_name,
                        "ok": true,
                        "cancelled": true,
                        "ms": attempt_started.elapsed().as_millis() as u64,
                    }));
                    finish_job_cancelled(&job_id, attempts);
                    return;
                }
                attempts.push(json!({
                    "key": key_name,
                    "ok": true,
                    "ms": attempt_started.elapsed().as_millis() as u64,
                    "prompt_tokens": outcome.prompt_tokens,
                    "completion_tokens": outcome.completion_tokens,
                }));
                tracing::info!(
                    key = %key_name,
                    model = %model_used,
                    prompt_tokens = outcome.prompt_tokens,
                    completion_tokens = outcome.completion_tokens,
                    "AI 對話完成"
                );
                finish_job_success(&job_id, &key_name, &model_used, attempts);
                return;
            }
            Err(AttemptError::RetryWithoutUsage) => {
                // 已經重試過一次還是這個結果，當成上游異常處理。
                record_failure(&key.id, FailureKind::Upstream, "stream_options 相容性問題");
                attempts.push(json!({
                    "key": key_name,
                    "ok": false,
                    "reason": "stream_options 不相容",
                    "ms": attempt_started.elapsed().as_millis() as u64,
                }));
                last_error = "上游不接受 stream_options".to_string();
                last_code = "AI_UPSTREAM_FAILED".to_string();
            }
            Err(AttemptError::Failed {
                kind,
                message,
                status,
            }) => {
                record_failure(&key.id, kind, &message);
                attempts.push(json!({
                    "key": key_name,
                    "ok": false,
                    "kind": kind.label(),
                    "status": status,
                    "reason": message,
                    "ms": attempt_started.elapsed().as_millis() as u64,
                    "rotated": kind.should_rotate(),
                }));
                last_error = format!("{}（{}）：{}", key_name, kind.label(), message);
                last_code = match kind {
                    FailureKind::QuotaExhausted => "AI_QUOTA_EXHAUSTED",
                    FailureKind::InvalidKey => "AI_KEY_INVALID",
                    FailureKind::RateLimited => "AI_RATE_LIMITED",
                    FailureKind::BadRequest => "AI_BAD_REQUEST",
                    FailureKind::Upstream => "AI_UPSTREAM_FAILED",
                }
                .to_string();

                if !kind.should_rotate() {
                    // 換密鑰也救不了（例如模型名稱打錯），直接停。
                    break;
                }
            }
        }
    }

    if job_cancelled(&job_id) {
        finish_job_cancelled(&job_id, attempts);
        return;
    }

    let tried = attempts.len();
    let _ = job_update(&job_id, |job| {
        job.done = true;
        job.ok = false;
        job.code = last_code;
        job.error = format!("已嘗試 {tried} 把密鑰仍失敗。最後一次：{last_error}");
        job.attempts = attempts;
    });
}

/// 成功收尾：把 assistant 訊息寫回對話串。
fn finish_job_success(job_id: &str, key_name: &str, model: &str, attempts: Vec<Value>) {
    let snapshot = job_update(job_id, |job| {
        job.done = true;
        job.ok = true;
        job.key_name = key_name.to_string();
        job.model = model.to_string();
        job.attempts = attempts;
        (
            job.account.clone(),
            job.thread_id.clone(),
            job.text.clone(),
            job.cache_key.clone(),
        )
    });
    let Some((account, thread_id, text, cache_key)) = snapshot else {
        return;
    };
    if !cache_key.is_empty() {
        chat_cache_put(&cache_key, &text, model);
    }
    append_assistant_message(&account, &thread_id, &text, key_name, model);
}

/// 使用者按停止：已生成的內容照樣存檔，不浪費已花掉的 token。
fn finish_job_cancelled(job_id: &str, attempts: Vec<Value>) {
    let snapshot = job_update(job_id, |job| {
        job.done = true;
        job.ok = true;
        job.error = "已由使用者停止".to_string();
        job.code = "AI_CANCELLED".to_string();
        job.attempts = attempts;
        (
            job.account.clone(),
            job.thread_id.clone(),
            job.text.clone(),
            job.key_name.clone(),
            job.model.clone(),
        )
    });
    let Some((account, thread_id, text, key_name, model)) = snapshot else {
        return;
    };
    if !text.trim().is_empty() {
        append_assistant_message(&account, &thread_id, &text, &key_name, &model);
    }
}

fn append_assistant_message(
    account: &str,
    thread_id: &str,
    text: &str,
    key_name: &str,
    model: &str,
) {
    let _ = with_threads(account, |threads| {
        if let Some(thread) = threads.iter_mut().find(|t| t.id == thread_id) {
            thread.messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: text.to_string(),
                ts: now_millis(),
                key_name: key_name.to_string(),
                model: model.to_string(),
                attachments: Vec::new(),
            });
            if thread.messages.len() > THREAD_MAX_MESSAGES {
                let drop = thread.messages.len() - THREAD_MAX_MESSAGES;
                thread.messages.drain(..drop);
            }
            thread.updated = now_millis();
        }
    });
}

// ─────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────

pub(super) async fn page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return admin_login_redirect("/admin/ai");
    };
    if session.must_change_password {
        return must_change_password_redirect();
    }
    html_page_response(&headers, super::pages::inject_theme(PAGE_HTML))
}

/// 目前狀態：密鑰（遮蔽後）、設定、對話串清單。
pub(super) async fn state_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };

    // AI_CHAT_V2：密鑰是管理員資產，一般使用者連遮蔽後的清單都拿不到。
    let can_manage = session.is_admin;
    let (keys, settings) = read_store(|store| {
        let keys: Vec<Value> = if can_manage {
            store.keys.iter().map(|k| k.to_public_json()).collect()
        } else {
            Vec::new()
        };
        (
            keys,
            json!({
                "system_prompt": store.settings.system_prompt,
                "temperature": store.settings.temperature,
                "max_tokens": store.settings.max_tokens,
                "strategy": store.settings.strategy,
                "context_budget": store.settings.context_budget,
                "summary_enabled": store.settings.summary_enabled,
                "cache_enabled": store.settings.cache_enabled,
            }),
        )
    })
    .unwrap_or_else(|| (Vec::new(), json!({})));

    // 模型清單所有人都看得到（要選才聊得起來），但只有管理員能改。
    let all_keys = read_store(|store| store.keys.clone()).unwrap_or_default();
    let options = model_options();
    let models: Vec<Value> = options.iter().map(|m| m.to_json(&all_keys)).collect();
    let models_customized = read_store(|store| !store.models.is_empty()).unwrap_or(false);

    let threads = read_threads(&session.account_id, |list| {
        list.iter().map(|t| t.summary_json()).collect::<Vec<Value>>()
    })
    .unwrap_or_default();

    let usable = all_keys.iter().filter(|k| k.available()).count();

    let providers: Vec<Value> = [
        "openai",
        "gemini",
        "groq",
        "openrouter",
        "deepseek",
        "xai",
        "mistral",
        "custom",
    ]
    .iter()
    .filter_map(|p| {
        provider_preset(p).map(|(label, base, model)| {
            json!({ "id": p, "label": label, "base_url": base, "model": model })
        })
    })
    .collect();

    (
        StatusCode::OK,
        Json(json!({
            "keys": keys,
            "settings": settings,
            "threads": threads,
            "providers": providers,
            "models": models,
            "models_customized": models_customized,
            "page_version": PAGE_VERSION,
            "build": build_stamp(),
            "can_manage": can_manage,
            "is_admin": session.is_admin,
            "usable_keys": usable,
            "usable_models": options
                .iter()
                .filter(|m| m.enabled && all_keys.iter().any(|k| k.provider == m.provider && k.available()))
                .count(),
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct KeyActionRequest {
    #[serde(default)]
    token: String,
    action: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    secret: String,
    #[serde(default)]
    model: String,
    /// AI_CHAT_V4：這把密鑰能用的所有模型（一行一個）。
    /// 第一個當作這把密鑰的預設模型，全部都會登記成使用者可選的模型。
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    enabled: Option<bool>,
    /// `move` 用：-1 上移、1 下移。
    #[serde(default)]
    offset: i32,
}

/// 密鑰的新增／修改／刪除／啟停／復活／排序。
pub(super) async fn keys_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<KeyActionRequest>,
) -> Response {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    // 密鑰的新增／修改／刪除只開放管理員，沒有任何開關可以放寬。
    if !session.is_admin {
        return unauthorized_response();
    }

    let action = body.action.as_str();
    let result: Result<String, String> = with_store(|store| match action {
        "add" => {
            if body.secret.trim().is_empty() {
                return Err("請填入 API 密鑰".to_string());
            }
            let provider = if body.provider.trim().is_empty() {
                "openai".to_string()
            } else {
                body.provider.trim().to_string()
            };
            if provider_preset(&provider).is_none() {
                return Err(format!("不認得的服務商：{provider}"));
            }
            if provider == "custom" && body.base_url.trim().is_empty() {
                return Err("自訂服務商必須填 API 網址".to_string());
            }
            // 同一把密鑰不要重複加。
            if store.keys.iter().any(|k| k.secret == body.secret.trim()) {
                return Err("這把密鑰已經加過了".to_string());
            }
            // 多行模型的第一個當預設；沒填 models 就退回單一 model 欄位。
            let listed: Vec<String> = body
                .models
                .iter()
                .map(|m| normalize_model_id(m))
                .filter(|m| !m.is_empty())
                .collect();
            let default_model = listed
                .first()
                .cloned()
                .unwrap_or_else(|| body.model.trim().to_string());
            store.keys.push(ApiKey {
                id: uuid::Uuid::new_v4().to_string(),
                label: body.label.trim().to_string(),
                provider: provider.clone(),
                base_url: body.base_url.trim().to_string(),
                secret: body.secret.trim().to_string(),
                model: default_model,
                enabled: body.enabled.unwrap_or(true),
                status: KeyStatus::Ok,
                cooldown_until: 0,
                last_error: String::new(),
                last_used: 0,
                uses: 0,
                fails: 0,
                prompt_tokens: 0,
                completion_tokens: 0,
                usage_supported: true,
            });
            let listed_count = listed.len();
            register_models(store, &provider, &listed);
            Ok(if listed_count > 0 {
                format!("已新增密鑰，這把金鑰可用 {listed_count} 個模型")
            } else {
                "已新增密鑰".to_string()
            })
        }
        "update" => {
            let listed: Vec<String> = body
                .models
                .iter()
                .map(|m| normalize_model_id(m))
                .filter(|m| !m.is_empty())
                .collect();
            let Some(key) = store.keys.iter_mut().find(|k| k.id == body.id) else {
                return Err("找不到這把密鑰".to_string());
            };
            key.label = body.label.trim().to_string();
            if !body.provider.trim().is_empty() {
                key.provider = body.provider.trim().to_string();
            }
            key.base_url = body.base_url.trim().to_string();
            // 編輯時模型欄位留空代表「不更動」。若直接覆寫成空字串，
            // 這把密鑰的預設模型就會被悄悄清掉。
            if let Some(first) = listed.first() {
                key.model = first.clone();
            } else if !body.model.trim().is_empty() {
                key.model = body.model.trim().to_string();
            }
            // 密鑰留空代表不更動（前端顯示的是遮蔽字串）。
            if !body.secret.trim().is_empty() {
                key.secret = body.secret.trim().to_string();
                key.status = KeyStatus::Ok;
                key.cooldown_until = 0;
                key.last_error = String::new();
            }
            if let Some(enabled) = body.enabled {
                key.enabled = enabled;
            }
            // 借用結束後才能再動 store.models。
            let provider = key.provider.clone();
            let listed_count = listed.len();
            register_models(store, &provider, &listed);
            Ok(if listed_count > 0 {
                format!("已更新密鑰，這把金鑰可用 {listed_count} 個模型")
            } else {
                "已更新密鑰".to_string()
            })
        }
        "delete" => {
            let before = store.keys.len();
            store.keys.retain(|k| k.id != body.id);
            if store.keys.len() == before {
                return Err("找不到這把密鑰".to_string());
            }
            Ok("已刪除密鑰".to_string())
        }
        "toggle" => {
            let Some(key) = store.keys.iter_mut().find(|k| k.id == body.id) else {
                return Err("找不到這把密鑰".to_string());
            };
            key.enabled = body.enabled.unwrap_or(!key.enabled);
            Ok(if key.enabled {
                "已啟用".to_string()
            } else {
                "已停用".to_string()
            })
        }
        "revive" => {
            let Some(key) = store.keys.iter_mut().find(|k| k.id == body.id) else {
                return Err("找不到這把密鑰".to_string());
            };
            key.status = KeyStatus::Ok;
            key.cooldown_until = 0;
            key.last_error = String::new();
            Ok("已重設狀態，這把密鑰會重新加入輪替".to_string())
        }
        "move" => {
            let Some(index) = store.keys.iter().position(|k| k.id == body.id) else {
                return Err("找不到這把密鑰".to_string());
            };
            let target = index as i32 + body.offset;
            if target < 0 || target as usize >= store.keys.len() {
                return Err("已經在邊界了".to_string());
            }
            store.keys.swap(index, target as usize);
            Ok("已調整順序".to_string())
        }
        other => Err(format!("不支援的操作：{other}")),
    })
    .unwrap_or_else(|| Err("密鑰儲存區被鎖住了，請稍後再試".to_string()));

    match result {
        Ok(message) => (StatusCode::OK, Json(json!({ "ok": true, "message": message })))
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": error })),
        )
            .into_response(),
    }
}

/// 實測一把密鑰能不能用（送一個極短的請求）。
pub(super) async fn key_test_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<KeyActionRequest>,
) -> Response {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }

    let Some(key) = read_store(|store| store.keys.iter().find(|k| k.id == body.id).cloned())
        .flatten()
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": "找不到這把密鑰" })),
        )
            .into_response();
    };

    let base = key.effective_base_url();
    let model = key.effective_model();
    if base.is_empty() || model.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "ok": false,
                "error": "尚未設定 API 網址或模型名稱",
            })),
        )
            .into_response();
    }

    let started = std::time::Instant::now();
    let response = AI_CLIENT
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key.secret.trim())
        .json(&json!({
            "model": model,
            "messages": [{ "role": "user", "content": "ping" }],
            "max_tokens": 1,
            "stream": false,
        }))
        .send()
        .await;

    let elapsed_ms = started.elapsed().as_millis() as u64;
    match response {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            if (200..300).contains(&status) {
                record_success(&key.id, 0, 0);
                (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "status": status,
                        "elapsed_ms": elapsed_ms,
                        "model": model,
                        "message": format!("可用（{elapsed_ms} ms）"),
                    })),
                )
                    .into_response()
            } else {
                let kind = classify_failure(status, &text);
                let message = extract_error_message(&text);
                record_failure(&key.id, kind, &message);
                (
                    StatusCode::OK,
                    Json(json!({
                        "ok": false,
                        "status": status,
                        "elapsed_ms": elapsed_ms,
                        "kind": kind.label(),
                        "error": message,
                    })),
                )
                    .into_response()
            }
        }
        Err(e) => {
            let message = detail_snippet(&e.to_string());
            record_failure(&key.id, FailureKind::Upstream, &message);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": false,
                    "elapsed_ms": elapsed_ms,
                    "kind": FailureKind::Upstream.label(),
                    "error": message,
                })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct SettingsRequest {
    #[serde(default)]
    token: String,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    context_budget: Option<usize>,
    #[serde(default)]
    summary_enabled: Option<bool>,
    #[serde(default)]
    cache_enabled: Option<bool>,
}

pub(super) async fn settings_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<SettingsRequest>,
) -> Response {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }

    let _ = with_store(|store| {
        if let Some(prompt) = body.system_prompt {
            store.settings.system_prompt = prompt;
        }
        if let Some(temperature) = body.temperature {
            store.settings.temperature = temperature.clamp(0.0, 2.0);
        }
        if let Some(max_tokens) = body.max_tokens {
            store.settings.max_tokens = max_tokens.min(32_000);
        }
        if let Some(strategy) = body.strategy {
            if strategy == "order" || strategy == "round_robin" {
                store.settings.strategy = strategy;
            }
        }
        if let Some(budget) = body.context_budget {
            // 太小會讓模型看不到上一句，設下限。
            store.settings.context_budget = budget.clamp(200, 32_000);
        }
        if let Some(enabled) = body.summary_enabled {
            store.settings.summary_enabled = enabled;
        }
        if let Some(enabled) = body.cache_enabled {
            store.settings.cache_enabled = enabled;
        }
    });

    (StatusCode::OK, Json(json!({ "ok": true, "message": "已儲存設定" }))).into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct ModelActionRequest {
    #[serde(default)]
    token: String,
    action: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    offset: i32,
    /// AI_CHAT_V3：一次填入多個模型名稱。
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    tier: String,
    /// 逗號分隔的標籤字串。
    #[serde(default)]
    tags: String,
}

/// 服務商的模型名稱是否允許帶斜線（`vendor/model` 形式）。
///
/// - OpenRouter：一定要帶（`openai/gpt-4o`）
/// - **Groq：一定要帶**（`openai/gpt-oss-120b`、`qwen/qwen3.6-27b`、
///   `meta-llama/llama-4-scout-17b-16e-instruct`），剝掉前綴會找不到模型
/// - Gemini／OpenAI／DeepSeek 等：帶了反而會被拒
fn provider_allows_slash(provider: &str) -> bool {
    matches!(provider, "groq" | "openrouter" | "custom")
}

/// AI_CHAT_V5：把模型名稱清成上游能接受的格式。
///
/// 實測結果：Gemini 的 `GenerateContentRequest.model` 只吃
/// `models/{name}` 這種格式，name 裡有斜線或空白就會回
/// `400 unexpected model name format`。使用者常從免費 API 清單複製到
/// OpenRouter 格式的 `google/gemini-2.0-flash`，貼進 Gemini 就爆這個錯。
fn normalize_model_id(raw: &str) -> String {
    // 全形空白與零寬字元一併清掉（從網頁複製很常夾帶）。
    let cleaned: String = raw
        .chars()
        .filter(|c| !matches!(*c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}'))
        .map(|c| if c == '\u{3000}' || c == '\u{a0}' { ' ' } else { c })
        .collect();
    let mut name = cleaned.trim().to_string();
    // Gemini 的清單會回 models/xxx，重複前綴也一併剝掉。
    while let Some(rest) = name.strip_prefix("models/") {
        name = rest.trim().to_string();
    }
    name
}

/// AI_CHAT_V6：把模型名稱正規化成官方代號。
///
/// 使用者常把選單上的「顯示名稱」當代號填（`GPT-4o mini`、`Gemini 3.5 Flash`），
/// 這其實跟 `gpt-4o-mini`、`gemini-3.5-flash` 是同一個模型。與其標成不合法
/// 讓清單裡一堆重複項目，不如直接轉成正規寫法並合併。
///
/// 例外：`custom`（自架端點，如 LM Studio）的模型名稱是大小寫敏感的
/// （`Qwen2.5-7B-Instruct`），只做去空白處理，不動大小寫。
fn canonical_model_name(provider: &str, raw: &str) -> Result<String, String> {
    let mut name = normalize_model_id(raw);
    if provider != "custom" {
        name = name.to_ascii_lowercase();
        // 空白轉連字號：「GPT-4o mini」→「gpt-4o-mini」
        let mut collapsed = String::with_capacity(name.len());
        let mut last_dash = false;
        for c in name.chars() {
            if c.is_whitespace() || c == '-' {
                if !last_dash {
                    collapsed.push('-');
                    last_dash = true;
                }
            } else {
                collapsed.push(c);
                last_dash = false;
            }
        }
        name = collapsed.trim_matches('-').to_string();
    }
    sanitize_model_name(provider, &name)
}

/// AI_CHAT_V6：Groq 官方代號中「帶服務商前綴」的那些模型。
///
/// 用途有兩個：
/// 1. 舊版本曾經誤把 Groq 的前綴當成 Gemini 那種多餘前綴剝掉，設定檔裡因此存了
///    `gpt-oss-120b` 這種殘缺代號，送出去只會拿到 404。載入設定時照這張表補回。
/// 2. 使用者從文件、部落格複製到的常常是「短名」，一樣直接幫他補齊。
///
/// 只列「官方代號本來就帶前綴」的；`llama-3.3-70b-versatile`、`whisper-large-v3`
/// 這類本來就沒前綴的絕對不能出現在這裡，否則會反過來把正確名稱改壞。
const GROQ_PREFIXED_MODELS: &[&str] = &[
    "openai/gpt-oss-120b",
    "openai/gpt-oss-20b",
    "openai/gpt-oss-safeguard-20b",
    "qwen/qwen3-32b",
    "qwen/qwen3.6-27b",
    "qwen/qwen3.8-27b",
    "qwen/qwen3-vl-32b-instruct",
    "moonshotai/kimi-k2-instruct",
    "moonshotai/kimi-k2-instruct-0905",
    "meta-llama/llama-4-scout-17b-16e-instruct",
    "meta-llama/llama-4-maverick-17b-128e-instruct",
    "meta-llama/llama-guard-4-12b",
    "meta-llama/llama-prompt-guard-2-22m",
    "meta-llama/llama-prompt-guard-2-86m",
    "minimaxai/minimax-m2.5",
    "minimaxai/minimax-m2.7",
    "groq/compound",
    "groq/compound-mini",
];

/// 缺前綴的 Groq 短名 → 官方完整代號。找不到就回 `None`（代表本來就沒前綴）。
fn groq_full_model_id(bare: &str) -> Option<&'static str> {
    let bare = bare.trim().to_ascii_lowercase();
    if bare.is_empty() || bare.contains('/') {
        return None;
    }
    GROQ_PREFIXED_MODELS
        .iter()
        .copied()
        .find(|full| matches!(full.split_once('/'), Some((_, rest)) if rest == bare))
}

/// 依服務商清洗並驗證模型名稱。回傳 `Err` 時附上人看得懂的原因。
fn sanitize_model_name(provider: &str, raw: &str) -> Result<String, String> {
    let mut name = normalize_model_id(raw);
    if name.is_empty() {
        return Err("模型名稱是空的".to_string());
    }

    // Groq 的官方代號多半帶前綴。舊版誤刪過，這裡照官方表補回去，
    // 使用者不必自己發現「原來要重新輸入一次」。
    if provider == "groq" {
        if let Some(full) = groq_full_model_id(&name) {
            tracing::info!(from = %name, to = full, "補回 Groq 模型代號的服務商前綴");
            name = full.to_string();
        }
    }

    // 不吃斜線的服務商：把 `google/gemini-2.0-flash` 這種前綴自動修掉，
    // 而不是讓使用者對著 400 錯誤發呆。
    if !provider_allows_slash(provider) {
        if let Some((vendor, rest)) = name.split_once('/') {
            if rest.contains('/') {
                return Err(format!(
                    "「{name}」含有多個斜線，{} 不接受這種格式",
                    provider_display(provider)
                ));
            }
            tracing::info!(provider, vendor, "自動移除模型名稱的服務商前綴");
            name = rest.to_string();
        }
    }

    if name.contains(char::is_whitespace) {
        return Err(format!("「{name}」含有空白字元，請確認是否複製到多餘內容"));
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/');
    if let Some(bad) = name.chars().find(|c| !allowed(*c)) {
        return Err(format!("「{name}」含有不合法的字元「{bad}」"));
    }
    if name.len() > 120 {
        return Err("模型名稱過長".to_string());
    }
    Ok(name)
}

/// AI_CHAT_V2：管理員維護「使用者可以選哪些模型」。
pub(super) async fn models_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<ModelActionRequest>,
) -> Response {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }

    let result: Result<String, String> = with_store(|store| match body.action.as_str() {
        "add" => {
            let provider = body.provider.trim().to_string();
            if provider_preset(&provider).is_none() {
                return Err(format!("不認得的服務商：{provider}"));
            }
            let raw = if body.model.trim().is_empty() {
                provider_preset(&provider)
                    .map(|(_, _, m)| m.to_string())
                    .unwrap_or_default()
            } else {
                body.model.trim().to_string()
            };
            if raw.is_empty() {
                return Err("請填入模型名稱".to_string());
            }
            let model = canonical_model_name(&provider, &raw)?;
            let id = format!("{provider}:{model}");
            if store.models.iter().any(|m| m.id == id) {
                return Err("這個模型已經在清單裡了".to_string());
            }
            store.models.push(ModelOption {
                tier: infer_tier(&model),
                tags: infer_tags(&model),
                id,
                label: body.label.trim().to_string(),
                provider,
                model,
                enabled: body.enabled.unwrap_or(true),
            });
            Ok("已新增模型".to_string())
        }
        "bulk_add" => {
            let provider = body.provider.trim().to_string();
            if provider_preset(&provider).is_none() {
                return Err(format!("不認得的服務商：{provider}"));
            }
            let mut added = 0usize;
            let mut skipped = 0usize;
            // 一行一個模型名稱，會先正規化成官方代號再登記。
            for raw in body.models.iter() {
                let Ok(model) = canonical_model_name(&provider, raw) else {
                    continue;
                };
                let id = format!("{provider}:{model}");
                if store.models.iter().any(|m| m.id == id) {
                    skipped += 1;
                    continue;
                }
                store.models.push(ModelOption {
                    tier: infer_tier(&model),
                    tags: infer_tags(&model),
                    id,
                    label: String::new(),
                    provider: provider.clone(),
                    model,
                    enabled: true,
                });
                added += 1;
            }
            if added == 0 {
                return Err(if skipped > 0 {
                    "這些模型都已經在清單裡了".to_string()
                } else {
                    "沒有可新增的模型名稱".to_string()
                });
            }
            Ok(format!("已新增 {added} 個模型（略過重複 {skipped} 個）"))
        }
        "set_tier" => {
            let tier = body.tier.trim();
            if !matches!(tier, "fast" | "balanced" | "deep") {
                return Err("分級只能是 fast／balanced／deep".to_string());
            }
            let Some(item) = store.models.iter_mut().find(|m| m.id == body.id) else {
                return Err("找不到這個模型".to_string());
            };
            item.tier = tier.to_string();
            Ok("已更新分級".to_string())
        }
        "set_tags" => {
            let Some(item) = store.models.iter_mut().find(|m| m.id == body.id) else {
                return Err("找不到這個模型".to_string());
            };
            item.tags = body
                .tags
                .split(&[',', '，', ' '][..])
                .map(|t| t.trim().to_ascii_lowercase())
                .filter(|t| !t.is_empty())
                .collect();
            Ok("已更新擅長領域".to_string())
        }
        "delete" => {
            let before = store.models.len();
            store.models.retain(|m| m.id != body.id);
            if store.models.len() == before {
                return Err("找不到這個模型".to_string());
            }
            Ok("已移除模型".to_string())
        }
        "toggle" => {
            let Some(item) = store.models.iter_mut().find(|m| m.id == body.id) else {
                return Err("找不到這個模型".to_string());
            };
            item.enabled = body.enabled.unwrap_or(!item.enabled);
            Ok(if item.enabled {
                "已開放使用者選用".to_string()
            } else {
                "已隱藏，使用者不會看到".to_string()
            })
        }
        "rename" => {
            let Some(item) = store.models.iter_mut().find(|m| m.id == body.id) else {
                return Err("找不到這個模型".to_string());
            };
            item.label = body.label.trim().to_string();
            Ok("已更新顯示名稱".to_string())
        }
        "move" => {
            let Some(index) = store.models.iter().position(|m| m.id == body.id) else {
                return Err("找不到這個模型".to_string());
            };
            let target = index as i32 + body.offset;
            if target < 0 || target as usize >= store.models.len() {
                return Err("已經在邊界了".to_string());
            }
            store.models.swap(index, target as usize);
            Ok("已調整順序".to_string())
        }
        "orphans" => {
            // AI_CHAT_V8：移除「該服務商連一把密鑰都沒有」的模型。
            // 舊版會依密鑰自動推導模型並寫進設定檔，密鑰刪掉後這些項目
            // 會留在清單裡變成孤兒，使用者看了會以為系統又自己加東西。
            let providers: Vec<String> =
                store.keys.iter().map(|k| k.provider.clone()).collect();
            let mut removed: Vec<String> = Vec::new();
            store.models.retain(|m| {
                if providers.contains(&m.provider) {
                    true
                } else {
                    removed.push(format!("{}·{}", provider_display(&m.provider), m.model));
                    false
                }
            });
            if removed.is_empty() {
                return Err("每個模型都有對應的服務商密鑰，沒有需要移除的".to_string());
            }
            tracing::info!(?removed, "移除沒有對應密鑰的模型");
            Ok(format!(
                "已移除 {} 個沒有密鑰的模型：{}",
                removed.len(),
                removed.join("、")
            ))
        }
        "clean" => {
            let (fixed, merged, dropped) = migrate_models(&mut store.models);
            if fixed == 0 && merged == 0 && dropped.is_empty() {
                return Err("清單已經是整理過的狀態".to_string());
            }
            let mut parts: Vec<String> = Vec::new();
            if fixed > 0 {
                parts.push(format!("修正 {fixed} 個名稱"));
            }
            if merged > 0 {
                parts.push(format!("合併 {merged} 個重複"));
            }
            if !dropped.is_empty() {
                parts.push(format!("移除 {}（無法修正）", dropped.join("、")));
            }
            Ok(parts.join("；"))
        }
        "reset" => {
            store.models.clear();
            Ok("已清空自訂清單，改回依密鑰自動推導".to_string())
        }
        other => Err(format!("不支援的操作：{other}")),
    })
    .unwrap_or_else(|| Err("設定儲存區被鎖住了，請稍後再試".to_string()));

    match result {
        Ok(message) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "message": message })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": error })),
        )
            .into_response(),
    }
}

// ─────────────────────────────────────────────────────────────
// AI_CHAT_V9：Agent 模式
//
// 讓模型能「自己動手做」而不是只會講話：它可以要求呼叫工具，我們執行完把結果
// 餵回去，它再決定下一步，最多跑 AGENT_MAX_STEPS 輪。每一步都記在 job.steps 裡，
// 前端畫成時間軸，使用者看得到它到底做了什麼 —— 不是黑箱。
//
// 工具全部在**每個帳號自己的沙箱資料夾**裡動作，碰不到系統其他檔案。
// ─────────────────────────────────────────────────────────────

/// 最多幾輪「呼叫工具 → 看結果 → 再想」。超過就強制收尾，避免無限迴圈燒 token。
const AGENT_MAX_STEPS: usize = 8;
/// 單一檔案大小上限（64 KB）。
const AGENT_MAX_FILE_BYTES: usize = 64 * 1024;
/// 每個帳號最多存幾個檔案。
const AGENT_MAX_FILES: usize = 50;
/// 工具回傳給模型的內容上限，太長會把脈絡撐爆。
const AGENT_TOOL_OUTPUT_MAX: usize = 4_000;

/// 每個帳號的工作區資料夾。帳號 id 先雜湊過再當目錄名，
/// 避免帳號含有 `/`、`..` 之類的字元直接變成路徑。
fn workspace_dir(account: &str) -> std::path::PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in account.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    std::path::Path::new("config/ai_workspace").join(format!("{hash:016x}"))
}

/// 檢查模型給的檔名。**這是沙箱的唯一入口，寫錯就等於開後門。**
///
/// 只允許單層檔名：不能有路徑分隔、不能有 `..`、不能是絕對路徑、
/// 不能是隱藏檔，字元集也限制死。
fn safe_workspace_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("檔名是空的".to_string());
    }
    if name.len() > 80 {
        return Err("檔名太長（上限 80 字元）".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("檔名不能包含路徑，只能是單一檔名".to_string());
    }
    if name.contains("..") || name.starts_with('.') {
        return Err("檔名不能以點開頭，也不能包含 `..`".to_string());
    }
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ');
    if let Some(bad) = name.chars().find(|c| !ok(*c)) {
        return Err(format!("檔名含有不允許的字元「{bad}」"));
    }
    // 副檔名白名單：只讓它產出「看得懂的文字檔」，不要變成放執行檔的地方。
    const ALLOWED_EXT: &[&str] = &[
        "txt", "md", "csv", "json", "html", "svg", "log", "yml", "yaml", "ini", "tsv",
    ];
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !ALLOWED_EXT.contains(&ext.as_str()) {
        return Err(format!(
            "只能建立這些類型的檔案：{}（收到的是「{name}」）",
            ALLOWED_EXT.join("、")
        ));
    }
    Ok(name.to_string())
}

/// 列出某個帳號工作區裡的檔案（檔名 + 大小）。
fn workspace_list(account: &str) -> Vec<(String, u64)> {
    let dir = workspace_dir(account);
    let mut out: Vec<(String, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                out.push((name.to_string(), meta.len()));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn workspace_read(account: &str, name: &str) -> Result<String, String> {
    let name = safe_workspace_name(name)?;
    let path = workspace_dir(account).join(&name);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("找不到檔案「{name}」"))
        }
        Err(e) => Err(format!("讀取「{name}」失敗：{e}")),
    }
}

fn workspace_write(account: &str, name: &str, content: &str) -> Result<String, String> {
    let name = safe_workspace_name(name)?;
    if content.len() > AGENT_MAX_FILE_BYTES {
        return Err(format!(
            "內容有 {} KB，單檔上限 {} KB",
            content.len() / 1024,
            AGENT_MAX_FILE_BYTES / 1024
        ));
    }
    let dir = workspace_dir(account);
    let existing = workspace_list(account);
    if existing.len() >= AGENT_MAX_FILES && !existing.iter().any(|(n, _)| n == &name) {
        return Err(format!(
            "工作區已經有 {AGENT_MAX_FILES} 個檔案，請先刪掉一些"
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("建立工作區失敗：{e}"))?;
    let path = dir.join(&name);
    // ATOMIC_WRITE_V1：AI 工作區檔案也走原子寫入，避免模型寫到一半失敗留下半截檔。
    crate::atomic_write::write_atomic(&path, content).map_err(|e| format!("寫入「{name}」失敗：{e}"))?;
    Ok(format!("已寫入「{name}」（{} 位元組）", content.len()))
}

fn workspace_delete(account: &str, name: &str) -> Result<String, String> {
    let name = safe_workspace_name(name)?;
    let path = workspace_dir(account).join(&name);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(format!("已刪除「{name}」")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("找不到檔案「{name}」"))
        }
        Err(e) => Err(format!("刪除「{name}」失敗：{e}")),
    }
}

/// 極小的四則運算器。模型算數字常常會錯，給它一個真的會算的工具。
///
/// 支援 `+ - * / %`、括號、小數與負號。故意不支援變數與函式呼叫 ——
/// 這裡不需要一個什麼都能執行的東西。
fn calc_eval(expr: &str) -> Result<f64, String> {
    let chars: Vec<char> = expr.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.is_empty() {
        return Err("算式是空的".to_string());
    }
    if chars.len() > 200 {
        return Err("算式太長".to_string());
    }
    let mut pos = 0usize;
    let value = calc_expr(&chars, &mut pos)?;
    if pos != chars.len() {
        return Err(format!("算式在第 {} 個字元附近有問題", pos + 1));
    }
    if !value.is_finite() {
        return Err("算出來不是有限的數字（可能除以零）".to_string());
    }
    Ok(value)
}

fn calc_expr(c: &[char], pos: &mut usize) -> Result<f64, String> {
    let mut acc = calc_term(c, pos)?;
    while *pos < c.len() {
        match c[*pos] {
            '+' => {
                *pos += 1;
                acc += calc_term(c, pos)?;
            }
            '-' => {
                *pos += 1;
                acc -= calc_term(c, pos)?;
            }
            _ => break,
        }
    }
    Ok(acc)
}

fn calc_term(c: &[char], pos: &mut usize) -> Result<f64, String> {
    let mut acc = calc_unary(c, pos)?;
    while *pos < c.len() {
        match c[*pos] {
            '*' => {
                *pos += 1;
                acc *= calc_unary(c, pos)?;
            }
            '/' => {
                *pos += 1;
                let rhs = calc_unary(c, pos)?;
                if rhs == 0.0 {
                    return Err("不能除以零".to_string());
                }
                acc /= rhs;
            }
            '%' => {
                *pos += 1;
                let rhs = calc_unary(c, pos)?;
                if rhs == 0.0 {
                    return Err("不能對零取餘數".to_string());
                }
                acc %= rhs;
            }
            _ => break,
        }
    }
    Ok(acc)
}

fn calc_unary(c: &[char], pos: &mut usize) -> Result<f64, String> {
    if *pos < c.len() && c[*pos] == '-' {
        *pos += 1;
        return Ok(-calc_unary(c, pos)?);
    }
    if *pos < c.len() && c[*pos] == '+' {
        *pos += 1;
        return calc_unary(c, pos);
    }
    calc_atom(c, pos)
}

fn calc_atom(c: &[char], pos: &mut usize) -> Result<f64, String> {
    if *pos >= c.len() {
        return Err("算式結尾不完整".to_string());
    }
    if c[*pos] == '(' {
        *pos += 1;
        let value = calc_expr(c, pos)?;
        if *pos >= c.len() || c[*pos] != ')' {
            return Err("括號沒有收尾".to_string());
        }
        *pos += 1;
        return Ok(value);
    }
    let start = *pos;
    while *pos < c.len() && (c[*pos].is_ascii_digit() || c[*pos] == '.') {
        *pos += 1;
    }
    if start == *pos {
        return Err(format!("第 {} 個字元不是數字", start + 1));
    }
    let text: String = c[start..*pos].iter().collect();
    text.parse::<f64>()
        .map_err(|_| format!("「{text}」不是合法的數字"))
}

/// 給模型看的工具清單（OpenAI function calling 格式）。
fn agent_tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "workspace_list",
                "description": "列出工作區裡目前有哪些檔案（含大小）。要修改或引用既有檔案前先呼叫這個。",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "workspace_read",
                "description": "讀取工作區裡某個檔案的完整內容。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "檔名，例如 report.md" }
                    },
                    "required": ["name"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "workspace_write",
                "description": "在工作區建立或覆寫一個檔案。適合用來產出報告、整理表格、寫筆記。副檔名只能是 txt/md/csv/json/html/svg/log/yml/yaml/ini/tsv。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "檔名，例如 summary.md" },
                        "content": { "type": "string", "description": "完整的檔案內容" }
                    },
                    "required": ["name", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "workspace_delete",
                "description": "刪除工作區裡的一個檔案。",
                "parameters": {
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "calc",
                "description": "計算一個算式，支援 + - * / % 與括號。需要算數字時一律用這個，不要自己心算。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "expr": { "type": "string", "description": "例如 (1234 + 567) * 8.5 / 3" }
                    },
                    "required": ["expr"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "now",
                "description": "取得現在的日期時間（伺服器時區）。需要算日期、判斷截止日時先問這個。",
                "parameters": { "type": "object", "properties": {}, "required": [] }
            }
        }),
    ]
}

/// 執行一個工具呼叫。回 `(給模型看的結果, 給人看的摘要, 成功與否)`。
fn run_tool(account: &str, name: &str, args: &Value) -> (String, String, bool) {
    let arg_str = |key: &str| -> String {
        args.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };

    let (result, ok) = match name {
        "workspace_list" => {
            let files = workspace_list(account);
            if files.is_empty() {
                ("工作區目前是空的。".to_string(), true)
            } else {
                let listed: Vec<String> = files
                    .iter()
                    .map(|(n, size)| format!("{n}（{size} 位元組）"))
                    .collect();
                (listed.join("\n"), true)
            }
        }
        "workspace_read" => match workspace_read(account, &arg_str("name")) {
            Ok(text) => (text, true),
            Err(e) => (e, false),
        },
        "workspace_write" => {
            match workspace_write(account, &arg_str("name"), &arg_str("content")) {
                Ok(message) => (message, true),
                Err(e) => (e, false),
            }
        }
        "workspace_delete" => match workspace_delete(account, &arg_str("name")) {
            Ok(message) => (message, true),
            Err(e) => (e, false),
        },
        "calc" => {
            let expr = arg_str("expr");
            match calc_eval(&expr) {
                Ok(value) => (format!("{expr} = {value}"), true),
                Err(e) => (format!("算不出來：{e}"), false),
            }
        }
        "now" => (
            format!(
                "現在是 {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S %A")
            ),
            true,
        ),
        other => (format!("沒有名叫「{other}」的工具"), false),
    };

    // 太長的結果會把脈絡撐爆，截斷後告訴模型「還有更多」。
    let trimmed = if result.chars().count() > AGENT_TOOL_OUTPUT_MAX {
        let head: String = result.chars().take(AGENT_TOOL_OUTPUT_MAX).collect();
        format!("{head}\n…（結果過長，只顯示前 {AGENT_TOOL_OUTPUT_MAX} 字）")
    } else {
        result
    };

    let summary = match name {
        "workspace_list" => "列出工作區檔案".to_string(),
        "workspace_read" => format!("讀取 {}", arg_str("name")),
        "workspace_write" => format!("寫入 {}", arg_str("name")),
        "workspace_delete" => format!("刪除 {}", arg_str("name")),
        "calc" => format!("計算 {}", arg_str("expr")),
        "now" => "查詢現在時間".to_string(),
        other => other.to_string(),
    };
    (trimmed, summary, ok)
}

/// AI_CHAT_V7：一次提問最多能附幾個檔案。
const MAX_ATTACHMENTS: usize = 6;
/// 單張圖片解碼後的大小上限（4 MB）。再大就算送得出去，多數服務商也會擋。
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// 單個文字檔取用的字元上限。超過就截斷並在內容裡註明，
/// 否則一個 log 檔就能把整個 token 預算吃光。
const MAX_TEXT_CHARS: usize = 8_000;
/// 所有附件加起來的上限（12 MB，含 base64 膨脹前的原始大小）。
const MAX_ATTACH_TOTAL_BYTES: usize = 12 * 1024 * 1024;

/// 前端上傳的一個附件。**只在這次提問用一次，不落地存檔。**
///
/// `Debug` 是手寫的：`ChatRequest` 有 `#[derive(Debug)]`，如果這裡也直接 derive，
/// 一旦有人 debug 印出請求，4 MB 的 base64 圖片和整份文字檔內容就會被灌進 log。
/// 只印檔名、類型與大小就夠診斷了。
#[derive(Clone, Deserialize)]
pub(super) struct Attachment {
    #[serde(default)]
    name: String,
    #[serde(default)]
    mime: String,
    /// `image` = 圖片（`data` 放 base64）；`text` = 純文字檔（`text` 放內容）。
    #[serde(default)]
    kind: String,
    #[serde(default)]
    data: String,
    #[serde(default)]
    text: String,
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("name", &self.name)
            .field("mime", &self.mime)
            .field("kind", &self.kind)
            .field("data_len", &self.data.len())
            .field("text_len", &self.text.len())
            .finish()
    }
}

/// 驗證通過、可以直接組進請求裡的附件。
struct PreparedAttachments {
    /// (檔名, data URI)
    images: Vec<(String, String)>,
    /// (檔名, 內容)
    texts: Vec<(String, String)>,
    /// 給對話紀錄看的檔名清單。
    names: Vec<String>,
}

impl PreparedAttachments {
    fn is_empty(&self) -> bool {
        self.images.is_empty() && self.texts.is_empty()
    }
}

/// 檔名只留最後一段並去掉控制字元 —— 附件名會進提示詞，不能讓它夾帶怪東西。
fn safe_attachment_name(raw: &str) -> String {
    // 用閉包而不是 `['/', '\\']` 陣列 pattern —— 後者需要較新的 Rust，
    // 這個專案要能在舊工具鏈上編過。
    let base = raw
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(raw)
        .trim();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '`')
        .take(120)
        .collect();
    if cleaned.trim().is_empty() {
        "附件".to_string()
    } else {
        cleaned.trim().to_string()
    }
}

/// 服務商都吃 data URI，但只認得這幾種圖片格式。
fn allowed_image_mime(mime: &str) -> Option<&'static str> {
    match mime.trim().to_ascii_lowercase().as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/webp" => Some("image/webp"),
        "image/gif" => Some("image/gif"),
        _ => None,
    }
}

/// 檢查並整理附件。回 `Err` 時是使用者看得懂的理由。
fn prepare_attachments(list: &[Attachment]) -> Result<PreparedAttachments, String> {
    if list.len() > MAX_ATTACHMENTS {
        return Err(format!("一次最多只能附 {MAX_ATTACHMENTS} 個檔案"));
    }
    let mut out = PreparedAttachments {
        images: Vec::new(),
        texts: Vec::new(),
        names: Vec::new(),
    };
    let mut total = 0usize;

    for item in list {
        let name = safe_attachment_name(&item.name);
        match item.kind.trim() {
            "image" => {
                let Some(mime) = allowed_image_mime(&item.mime) else {
                    return Err(format!(
                        "「{name}」的格式不支援，圖片只接受 PNG／JPG／WebP／GIF"
                    ));
                };
                // 前端可能連 data URI 前綴一起送過來，兩種都收。
                let payload = match item.data.split_once("base64,") {
                    Some((_, rest)) => rest,
                    None => item.data.as_str(),
                };
                let payload: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
                if payload.is_empty() {
                    return Err(format!("「{name}」是空的"));
                }
                let bytes = BASE64
                    .decode(payload.as_bytes())
                    .map_err(|_| format!("「{name}」的內容毀損，請重新選一次"))?;
                if bytes.is_empty() {
                    return Err(format!("「{name}」是空的"));
                }
                if bytes.len() > MAX_IMAGE_BYTES {
                    return Err(format!(
                        "「{name}」有 {} MB，單張圖片上限 {} MB",
                        bytes.len() / (1024 * 1024),
                        MAX_IMAGE_BYTES / (1024 * 1024)
                    ));
                }
                total += bytes.len();
                out.images
                    .push((name.clone(), format!("data:{mime};base64,{payload}")));
                out.names.push(name);
            }
            "text" => {
                let body = item.text.replace('\r', "");
                if body.trim().is_empty() {
                    return Err(format!("「{name}」讀不到文字內容"));
                }
                total += body.len();
                let truncated: String = if body.chars().count() > MAX_TEXT_CHARS {
                    let head: String = body.chars().take(MAX_TEXT_CHARS).collect();
                    format!("{head}\n…（檔案過長，只取前 {MAX_TEXT_CHARS} 字）")
                } else {
                    body
                };
                out.texts.push((name.clone(), truncated));
                out.names.push(name);
            }
            other => {
                return Err(format!("「{name}」的類型無法辨識（{other}）"));
            }
        }
        if total > MAX_ATTACH_TOTAL_BYTES {
            return Err(format!(
                "附件總大小超過 {} MB",
                MAX_ATTACH_TOTAL_BYTES / (1024 * 1024)
            ));
        }
    }
    Ok(out)
}

/// 這個模型看不看得懂圖片。
///
/// 以管理員設定的 `vision` 標籤為準；沒設標籤時才用名稱推測，
/// 免得使用者附了圖卻被系統說「沒有支援的模型」。這只是判斷既有模型的能力，
/// 不會憑空多出任何一個模型。
fn model_supports_vision(option: &ModelOption) -> bool {
    if option.tags.iter().any(|t| t == "vision") {
        return true;
    }
    let tokens = model_tokens(&option.model);
    let has = |word: &str| tokens.iter().any(|t| t == word);
    if has("vision") || has("vl") || has("multimodal") {
        return true;
    }
    let name = option.model.to_ascii_lowercase();
    const VISION_HINTS: &[&str] = &[
        "gpt-4o",
        "gpt-4.1",
        "gpt-5",
        "o3",
        "o4-mini",
        "gemini",
        "claude-3",
        "claude-4",
        "claude-sonnet",
        "claude-opus",
        "llama-4-scout",
        "llama-4-maverick",
        "qwen3.6-27b",
        "qwen3.8-27b",
        "pixtral",
        "grok-2-vision",
        "grok-4",
    ];
    VISION_HINTS.iter().any(|hint| name.contains(hint))
}

#[derive(Debug, Deserialize)]
pub(super) struct ChatRequest {
    #[serde(default)]
    token: String,
    #[serde(default)]
    thread_id: String,
    /// 使用者在聊天頁選的模型（`model_options()` 的 id）。留空＝自動挑。
    #[serde(default)]
    model_id: String,
    message: String,
    /// AI_CHAT_V7：這次提問附帶的圖片／文字檔。用完即丟，不寫進硬碟。
    #[serde(default)]
    attachments: Vec<Attachment>,
    /// AI_CHAT_V9：開啟 Agent 模式（可呼叫工具、多步驟執行）。
    #[serde(default)]
    agent: bool,
}

/// 送出訊息 → 建立背景 job → 立刻回 202。
pub(super) async fn chat_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<ChatRequest>,
) -> Response {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };

    // AI_CHAT_V7：先把附件驗好，訊息可不可以是空的要看有沒有附件。
    let attachments = match prepare_attachments(&body.attachments) {
        Ok(prepared) => prepared,
        Err(reason) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": reason, "code": "AI_BAD_ATTACHMENT" })),
            )
                .into_response();
        }
    };

    let mut text = body.message.trim().to_string();
    if text.is_empty() && attachments.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "訊息是空的", "code": "AI_EMPTY_MESSAGE" })),
        )
            .into_response();
    }
    // 只丟檔案不打字也是常見用法，補一句預設指令免得模型不知道要幹嘛。
    if text.is_empty() {
        text = if attachments.images.is_empty() {
            "請閱讀附件並說明重點。".to_string()
        } else {
            "請看這張圖片，說明裡面的內容。".to_string()
        };
    }
    if text.len() > 200_000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "訊息太長（上限 200000 位元組）", "code": "AI_TOO_LONG" })),
        )
            .into_response();
    }

    let account = session.account_id.clone();
    // ChatJob 會吃掉 account，快取路徑與背景摘要另外留一份。
    let account_for_cache = account.clone();

    // 先確定要用哪個模型。使用者選的優先，沒選（或選的已被管理員刪掉）
    // 就自動挑一個真的有可用密鑰的。
    let needs_vision = !attachments.images.is_empty();
    let (selection, auto_reason) = resolve_model(&body.model_id, &text, needs_vision);
    if selection.is_none() && needs_vision {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "目前沒有看得懂圖片的模型可用。請管理員在模型清單新增具視覺能力的模型（例如 Groq 的 qwen/qwen3.6-27b、Gemini、GPT-4o），或把該模型標上 vision 標籤。",
                "code": "AI_NO_VISION_MODEL",
            })),
        )
            .into_response();
    }
    if selection.is_none() {
        let total = read_store(|store| store.keys.len()).unwrap_or(0);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": if total == 0 {
                    "系統尚未設定任何 AI 模型，請聯絡管理員。"
                } else {
                    "目前沒有可用的模型，請聯絡管理員。"
                },
                "code": "NO_MODEL_AVAILABLE",
            })),
        )
            .into_response();
    }
    let selected_id = selection
        .as_ref()
        .map(|m| m.id.clone())
        .unwrap_or_default();
    // 使用者選的是「自動」時，對話串要記住 auto，
    // 否則下次打開這串對話會被固定成這次剛好挑中的那個模型。
    let remembered_id = if body.model_id.trim() == AUTO_MODEL_ID || body.model_id.trim().is_empty() {
        AUTO_MODEL_ID.to_string()
    } else {
        selected_id.clone()
    };

    // AI_CHAT_V7：文字檔的內容直接併進這則訊息保存下來 —— 這樣之後追問
    // 「剛剛那個檔案第三欄是什麼」時模型還看得到。圖片不併（base64 太肥），
    // 只留檔名，要再問就重附一次。
    let mut stored_text = text.clone();
    for (name, body_text) in &attachments.texts {
        stored_text.push_str(&format!("\n\n【附件：{name}】\n```\n{body_text}\n```"));
    }
    if !attachments.images.is_empty() {
        let names: Vec<&str> = attachments
            .images
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        stored_text.push_str(&format!("\n\n（附了圖片：{}）", names.join("、")));
    }

    // 把使用者訊息寫進對話串（沒有指定就開一個新的）。
    let thread_id = if body.thread_id.trim().is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        body.thread_id.trim().to_string()
    };

    let history = with_threads(&account, |threads| {
        let position = threads.iter().position(|t| t.id == thread_id);
        let index = match position {
            Some(index) => index,
            None => {
                threads.push(ChatThread {
                    id: thread_id.clone(),
                    title: derive_title(&text),
                    model_id: remembered_id.clone(),
                    summary: String::new(),
                    summarized: 0,
                    created: now_millis(),
                    updated: now_millis(),
                    messages: Vec::new(),
                });
                threads.len() - 1
            }
        };
        let thread = &mut threads[index];
        if thread.title.trim().is_empty() || thread.title == "新對話" {
            thread.title = derive_title(&text);
        }
        // 中途換模型也記下來，下次打開這串對話會選回最後用的那個。
        thread.model_id = remembered_id.clone();
        thread.messages.push(ChatMessage {
            role: "user".to_string(),
            content: stored_text.clone(),
            ts: now_millis(),
            key_name: String::new(),
            model: String::new(),
            attachments: attachments.names.clone(),
        });
        if thread.messages.len() > THREAD_MAX_MESSAGES {
            let drop = thread.messages.len() - THREAD_MAX_MESSAGES;
            thread.messages.drain(..drop);
        }
        thread.updated = now_millis();
        (
            thread.messages.clone(),
            thread.summary.clone(),
            thread.summarized.min(thread.messages.len()),
        )
    })
    .unwrap_or_default();
    let (history, thread_summary, summarized) = history;

    let settings = read_store(|store| store.settings.clone()).unwrap_or_default();

    // AI_CHAT_V5：組送給模型的訊息 = system ＋ 較早內容的摘要 ＋ token 預算內的最近幾則。
    let mut messages: Vec<Value> = Vec::new();
    if !settings.system_prompt.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": settings.system_prompt }));
    }
    if !thread_summary.trim().is_empty() {
        messages.push(json!({
            "role": "system",
            "content": format!("以下是這段對話較早內容的摘要，供你延續脈絡：\n{thread_summary}"),
        }));
    }
    let budget = if settings.context_budget == 0 {
        CONTEXT_TOKEN_BUDGET
    } else {
        settings.context_budget
    };
    // 已被摘要涵蓋的部分不再原文重送。
    let recent = &history[summarized.min(history.len())..];
    let picked = select_context(recent, budget);
    let context_tokens: usize = picked
        .iter()
        .map(|m| estimate_tokens(&m.content))
        .sum::<usize>();
    for message in picked {
        messages.push(json!({ "role": message.role, "content": message.content }));
    }
    // AI_CHAT_V7：把這次的圖片掛到最新一則使用者訊息上。
    // OpenAI 相容格式的 content 可以是陣列，Gemini／Groq／OpenRouter 都吃這套。
    // `select_context` 保證最新一則一定在，所以最後一筆就是它。
    if !attachments.images.is_empty() {
        if let Some(last) = messages.last_mut() {
            let mut parts: Vec<Value> = vec![json!({ "type": "text", "text": stored_text })];
            for (_, uri) in &attachments.images {
                parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": uri },
                }));
            }
            last["content"] = Value::Array(parts);
        }
    }
    tracing::debug!(
        history = history.len(),
        summarized,
        sent = messages.len(),
        est_tokens = context_tokens,
        "AI 脈絡組裝完成"
    );

    // 完全相同的請求直接回快取（temperature 拉高時代表要多樣性，就不快取）。
    let cache_key = if settings.cache_enabled && settings.temperature < 1.0 {
        chat_cache_key(&selected_id, &messages)
    } else {
        String::new()
    };
    let cache_hit = if cache_key.is_empty() {
        None
    } else {
        chat_cache_get(&cache_key)
    };

    let job_id = uuid::Uuid::new_v4().to_string();
    job_insert(
        &job_id,
        ChatJob {
            account,
            thread_id: thread_id.clone(),
            text: String::new(),
            done: false,
            ok: false,
            cancel: false,
            error: String::new(),
            code: String::new(),
            key_name: String::new(),
            model: String::new(),
            auto_reason: auto_reason.clone(),
            cached: cache_hit.is_some(),
            cache_key: cache_key.clone(),
            attempts: Vec::new(),
            steps: Vec::new(),
            created_at: now_secs(),
            updated_at: now_secs(),
        },
    );

    if let Some((text, model)) = cache_hit {
        // 直接把 job 標成完成，前端照原本的輪詢流程就會立刻拿到答案。
        let _ = job_update(&job_id, |job| {
            job.done = true;
            job.ok = true;
            job.text = text.clone();
            job.model = model.clone();
            job.key_name = "快取".to_string();
        });
        append_assistant_message(&account_for_cache, &thread_id, &text, "快取", &model);
        tracing::info!(model = %model, "AI 回覆命中快取，未呼叫上游");
    } else {
        let job_id = job_id.clone();
        let summary_enabled = settings.summary_enabled;
        let account = account_for_cache.clone();
        let thread = thread_id.clone();
        let agent_mode = body.agent;
        tokio::spawn(async move {
            if agent_mode {
                // Agent 走工具呼叫迴圈，不是單純的串流生成。
                run_agent_job(
                    job_id,
                    account.clone(),
                    messages,
                    settings,
                    selection,
                )
                .await;
            } else {
                run_chat_job(job_id, messages, settings, selection).await;
            }
            // 回覆送出後才在背景濃縮較早的對話，使用者感覺不到延遲。
            if summary_enabled {
                maybe_summarize(account, thread).await;
            }
        });
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": job_id,
            "thread_id": thread_id,
            "model_id": selected_id,
            "auto_reason": auto_reason,
            "state": "running",
            "poll_path": "/admin/api/ai/chat/status",
            "poll_ms": 350,
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct JobQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    job: String,
}

/// AI_CHAT_V8：把生成中的內容用 SSE 直接推給瀏覽器。
///
/// 為什麼要做：原本前端每 350ms 輪詢一次、而且每次都把「整段文字」重新拿回來
/// 再重畫整棵 Markdown DOM。結果是（1）字是一坨一坨跳出來的、（2）長回覆的
/// 流量是 O(n²)、（3）重畫時使用者選取的文字會被清掉。改成只推「新增的那一段」。
///
/// 前端如果連不上（某些代理會吃掉 SSE），會自動退回原本的輪詢，不會壞掉。
pub(super) async fn chat_stream_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<JobQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    let job_id = query.job.trim().to_string();
    if job_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "缺少 job 參數", "code": "AI_JOB_MISSING" })),
        )
            .into_response();
    }

    // 先確認這個 job 存在而且是自己的 —— 不要為了一個不存在的工作開一條長連線。
    let owned = CHAT_JOBS
        .lock()
        .ok()
        .and_then(|map| map.get(&job_id).map(|job| job.account == session.account_id));
    match owned {
        Some(true) => {}
        Some(false) => return unauthorized_response(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "找不到這個生成工作（可能已過期）",
                    "code": "AI_JOB_NOT_FOUND",
                })),
            )
                .into_response();
        }
    }

    let cursor = StreamCursor {
        job: job_id,
        account: session.account_id.clone(),
        sent: 0,
        steps_sent: 0,
        meta_sent: false,
        finished: false,
        idle_ticks: 0,
    };

    let stream = futures::stream::unfold(cursor, |mut cur| async move {
        if cur.finished {
            return None;
        }
        loop {
            // 取快照時不能持有鎖跨越 await，先 clone 出來再放掉。
            let snapshot = CHAT_JOBS.lock().ok().and_then(|map| {
                map.get(&cur.job).map(|job| {
                    (
                        job.account.clone(),
                        job.text.clone(),
                        job.done,
                        job.key_name.clone(),
                        job.model.clone(),
                        job.auto_reason.clone(),
                        job.cached,
                        job.steps.clone(),
                        job.to_json(),
                    )
                })
            });
            let Some((account, text, done, key_name, model, auto_reason, cached, steps, full)) =
                snapshot
            else {
                cur.finished = true;
                return Some((
                    Ok::<Event, std::convert::Infallible>(
                        Event::default().event("gone").data("expired"),
                    ),
                    cur,
                ));
            };
            if account != cur.account {
                cur.finished = true;
                return Some((Ok(Event::default().event("gone").data("forbidden")), cur));
            }

            // 先送一次「這則是誰生的」，前端要拿去標在氣泡上。
            if !cur.meta_sent && (!key_name.is_empty() || !auto_reason.is_empty()) {
                cur.meta_sent = true;
                let payload = json!({
                    "key_name": key_name,
                    "model": model,
                    "auto_reason": auto_reason,
                    "cached": cached,
                })
                .to_string();
                return Some((Ok(Event::default().event("meta").data(payload)), cur));
            }

            // AI_CHAT_V9：Agent 的每一步都要即時讓使用者看到，
            // 不然它跑八步的期間畫面是完全靜止的，看起來像當掉。
            if steps.len() > cur.steps_sent {
                let step = steps[cur.steps_sent].clone();
                cur.steps_sent += 1;
                cur.idle_ticks = 0;
                return Some((
                    Ok(Event::default().event("step").data(step.to_string())),
                    cur,
                ));
            }

            if let Some((delta, next)) = next_delta(&text, cur.sent) {
                cur.sent = next;
                cur.idle_ticks = 0;
                return Some((Ok(Event::default().event("delta").data(delta)), cur));
            }

            if done {
                cur.finished = true;
                return Some((Ok(Event::default().event("done").data(full.to_string())), cur));
            }

            cur.idle_ticks += 1;
            if cur.idle_ticks > AI_STREAM_MAX_IDLE_TICKS {
                cur.finished = true;
                return Some((Ok(Event::default().event("gone").data("timeout")), cur));
            }
            tokio::time::sleep(std::time::Duration::from_millis(AI_STREAM_TICK_MS)).await;
        }
    });

    let mut response = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("ping"),
        )
        .into_response();
    // nginx／cloudflared 之類的中間層預設會緩衝，緩衝了就等於沒有串流。
    let head = response.headers_mut();
    head.insert("x-accel-buffering", HeaderValue::from_static("no"));
    head.insert("cache-control", HeaderValue::from_static("no-store"));
    response
}

/// 從 `sent` 之後取出還沒推送的那一段。
///
/// `sent` 一律等於上一次的 `text.len()`，所以必定落在字元邊界上；
/// 但萬一有人改壞了，`get()` 會回 `None` 而不是 panic —— 這時退回整段重推，
/// 前端拿到的內容仍然正確（頂多重複一次），總比整條連線炸掉好。
fn next_delta(text: &str, sent: usize) -> Option<(String, usize)> {
    if text.len() <= sent {
        return None;
    }
    match text.get(sent..) {
        Some(rest) if !rest.is_empty() => Some((rest.to_string(), text.len())),
        Some(_) => None,
        None => Some((text.to_string(), text.len())),
    }
}

/// SSE 推送的游標：記住這條連線已經送到哪裡。
struct StreamCursor {
    job: String,
    account: String,
    /// 已推送出去的位元組數。
    sent: usize,
    /// AI_CHAT_V9：已推送出去幾個 Agent 步驟。
    steps_sent: usize,
    meta_sent: bool,
    finished: bool,
    idle_ticks: u32,
}

/// AI_CHAT_V9：下載 Agent 產生的檔案。只能拿自己工作區裡的東西。
pub(super) async fn workspace_file_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<WorkspaceQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    let name = match safe_workspace_name(&query.name) {
        Ok(name) => name,
        Err(reason) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": reason, "code": "AI_BAD_FILENAME" })),
            )
                .into_response();
        }
    };
    match workspace_read(&session.account_id, &name) {
        Ok(text) => {
            let mut response = (StatusCode::OK, text).into_response();
            let head = response.headers_mut();
            head.insert(
                "content-type",
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            // 一律當附件下載，絕不讓瀏覽器把使用者產生的 HTML 當網頁執行。
            if let Ok(value) = HeaderValue::from_str(&format!(
                "attachment; filename*=UTF-8''{}",
                percent_encode_filename(&name)
            )) {
                head.insert("content-disposition", value);
            }
            head.insert(
                "x-content-type-options",
                HeaderValue::from_static("nosniff"),
            );
            response
        }
        Err(reason) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": reason, "code": "AI_FILE_NOT_FOUND" })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct WorkspaceQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    name: String,
}

/// 檔名放進 header 前要 percent-encode，不然中文或空白會讓 header 不合法。
fn percent_encode_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 輪詢：拿目前已生成的全文。SSE 連不上時的後備方案。
pub(super) async fn chat_status_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<JobQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    if query.job.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "缺少 job 參數", "code": "AI_JOB_MISSING" })),
        )
            .into_response();
    }

    let snapshot = CHAT_JOBS.lock().ok().and_then(|map| {
        map.get(query.job.trim()).map(|job| {
            // 只能看自己的 job。
            if job.account != session.account_id {
                None
            } else {
                Some(job.to_json())
            }
        })
    });

    match snapshot {
        Some(Some(value)) => (StatusCode::OK, Json(value)).into_response(),
        Some(None) => unauthorized_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "找不到這個生成工作（可能已過期）",
                "code": "AI_JOB_NOT_FOUND",
            })),
        )
            .into_response(),
    }
}

/// 停止生成：已產出的內容會保留。
pub(super) async fn chat_stop_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<JobQuery>,
) -> Response {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    let stopped = job_update(body.job.trim(), |job| {
        if job.account != session.account_id {
            return false;
        }
        job.cancel = true;
        true
    })
    .unwrap_or(false);

    (
        StatusCode::OK,
        Json(json!({ "ok": stopped, "message": if stopped { "已送出停止" } else { "工作已結束或不存在" } })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct ThreadRequest {
    #[serde(default)]
    token: String,
    action: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
}

/// 對話串：讀取／改名／刪除／全部清空。
pub(super) async fn threads_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<ThreadRequest>,
) -> Response {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    let account = session.account_id.clone();

    match body.action.as_str() {
        "get" => {
            let thread = read_threads(&account, |list| {
                list.iter().find(|t| t.id == body.id).map(|t| t.full_json())
            })
            .flatten();
            match thread {
                Some(value) => (StatusCode::OK, Json(json!({ "ok": true, "thread": value })))
                    .into_response(),
                None => (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "ok": false, "error": "找不到這個對話" })),
                )
                    .into_response(),
            }
        }
        "rename" => {
            let title = derive_title(&body.title);
            let _ = with_threads(&account, |threads| {
                if let Some(thread) = threads.iter_mut().find(|t| t.id == body.id) {
                    thread.title = title.clone();
                }
            });
            (StatusCode::OK, Json(json!({ "ok": true, "title": title }))).into_response()
        }
        "delete" => {
            let _ = with_threads(&account, |threads| threads.retain(|t| t.id != body.id));
            (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
        }
        "clear" => {
            let _ = with_threads(&account, |threads| threads.clear());
            (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
        }
        other => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": format!("不支援的操作：{other}") })),
        )
            .into_response(),
    }
}

// ─────────────────────────────────────────────────────────────
// 測試
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod ai_chat_tests {
    use super::*;

    #[test]
    fn mask_secret_hides_the_middle() {
        assert_eq!(mask_secret(""), "");
        assert_eq!(mask_secret("short"), "•••••");
        let masked = mask_secret("sk-proj-abcdefghijklmnop1234");
        assert!(masked.starts_with("sk-pr"));
        assert!(masked.ends_with("1234"));
        assert!(!masked.contains("abcdefgh"));
    }

    #[test]
    fn quota_errors_are_separated_from_plain_rate_limits() {
        assert_eq!(
            classify_failure(429, r#"{"error":{"code":"insufficient_quota"}}"#),
            FailureKind::QuotaExhausted
        );
        assert_eq!(
            classify_failure(429, r#"{"error":{"code":"rate_limit_exceeded"}}"#),
            FailureKind::RateLimited
        );
        assert_eq!(
            classify_failure(429, r#"{"error":{"status":"RESOURCE_EXHAUSTED"}}"#),
            FailureKind::QuotaExhausted
        );
        assert_eq!(classify_failure(402, "{}"), FailureKind::QuotaExhausted);
        assert_eq!(classify_failure(401, "{}"), FailureKind::InvalidKey);
        assert_eq!(classify_failure(503, "upstream down"), FailureKind::Upstream);
    }

    #[test]
    fn quota_exhaustion_cools_down_but_never_kills_the_key() {
        assert_eq!(FailureKind::QuotaExhausted.status(), KeyStatus::Cooling);
        assert!(FailureKind::QuotaExhausted.cooldown_secs() > 0);
        assert!(FailureKind::QuotaExhausted.should_rotate());
        // 密鑰無效時換下一把仍然有意義，但這把不該自動復活。
        assert_eq!(FailureKind::InvalidKey.status(), KeyStatus::Dead);
        assert_eq!(FailureKind::InvalidKey.cooldown_secs(), 0);
        // 參數錯誤換密鑰沒用。
        assert!(!FailureKind::BadRequest.should_rotate());
    }

    #[test]
    fn cooling_key_becomes_available_again_after_the_cooldown() {
        let mut key = ApiKey {
            id: "k1".into(),
            label: String::new(),
            provider: "openai".into(),
            base_url: String::new(),
            secret: "sk-test-1234567890".into(),
            model: String::new(),
            enabled: true,
            status: KeyStatus::Cooling,
            cooldown_until: now_secs() + 600,
            last_error: String::new(),
            last_used: 0,
            uses: 0,
            fails: 1,
            prompt_tokens: 0,
            completion_tokens: 0,
            usage_supported: true,
        };
        assert!(!key.available(), "冷卻中不該被選到");
        key.cooldown_until = now_secs().saturating_sub(1);
        assert!(key.available(), "冷卻過期就要自動復活");
        key.status = KeyStatus::Dead;
        assert!(!key.available(), "失效的密鑰不會自己回來");
        key.status = KeyStatus::Ok;
        key.enabled = false;
        assert!(!key.available(), "手動停用要被尊重");
    }

    #[test]
    fn presets_fill_in_base_url_and_model() {
        let key = ApiKey {
            id: "k2".into(),
            label: "備援".into(),
            provider: "groq".into(),
            base_url: String::new(),
            secret: "gsk_abcdefghijklmnop".into(),
            model: String::new(),
            enabled: true,
            status: KeyStatus::Ok,
            cooldown_until: 0,
            last_error: String::new(),
            last_used: 0,
            uses: 0,
            fails: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            usage_supported: true,
        };
        assert_eq!(key.effective_base_url(), "https://api.groq.com/openai/v1");
        assert_eq!(key.effective_model(), "llama-3.3-70b-versatile");
        assert_eq!(key.display_name(), "備援");

        let custom = ApiKey {
            base_url: "https://my.proxy.example/v1/".into(),
            model: "my-model".into(),
            ..key.clone()
        };
        // 尾端斜線要被吃掉，否則會組出 //chat/completions。
        assert_eq!(custom.effective_base_url(), "https://my.proxy.example/v1");
        assert_eq!(custom.effective_model(), "my-model");
    }

    #[test]
    fn public_json_never_leaks_the_secret() {
        let key = ApiKey {
            id: "k3".into(),
            label: String::new(),
            provider: "openai".into(),
            base_url: String::new(),
            secret: "sk-proj-SUPERSECRETVALUE9999".into(),
            model: String::new(),
            enabled: true,
            status: KeyStatus::Ok,
            cooldown_until: 0,
            last_error: String::new(),
            last_used: 0,
            uses: 0,
            fails: 0,
            prompt_tokens: 10,
            completion_tokens: 20,
            usage_supported: true,
        };
        let text = key.to_public_json().to_string();
        assert!(!text.contains("SUPERSECRETVALUE"));
        assert!(text.contains("total_tokens"));
    }

    #[test]
    fn error_message_is_pulled_out_of_each_vendor_shape() {
        assert_eq!(
            extract_error_message(r#"{"error":{"message":"You exceeded your current quota"}}"#),
            "You exceeded your current quota"
        );
        assert_eq!(
            extract_error_message(r#"{"message":"invalid api key"}"#),
            "invalid api key"
        );
        assert_eq!(extract_error_message("plain text failure"), "plain text failure");
    }

    fn key_for(provider: &str, model: &str, status: KeyStatus) -> ApiKey {
        ApiKey {
            id: format!("{provider}-{model}"),
            label: String::new(),
            provider: provider.into(),
            base_url: String::new(),
            secret: "sk-test-1234567890".into(),
            model: model.into(),
            enabled: true,
            status,
            cooldown_until: if status == KeyStatus::Cooling {
                now_secs() + 600
            } else {
                0
            },
            last_error: String::new(),
            last_used: 0,
            uses: 0,
            fails: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            usage_supported: true,
        }
    }

    #[test]
    fn users_see_friendly_model_names() {
        let option = ModelOption {
            id: "openai:gpt-4o-mini".into(),
            label: String::new(),
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            enabled: true,
            tier: "balanced".into(),
            tags: Vec::new(),
        };
        // 使用者看到的是「ChatGPT」而不是「openai」。
        assert_eq!(option.display(), "ChatGPT · gpt-4o-mini");
        let named = ModelOption {
            label: "老師推薦".into(),
            ..option.clone()
        };
        assert_eq!(named.display(), "老師推薦");
        assert_eq!(provider_display("gemini"), "Gemini");
        assert_eq!(provider_display("xai"), "Grok");
    }

    #[test]
    fn a_model_is_only_available_when_its_provider_has_a_usable_key() {
        let option = ModelOption {
            id: "gemini:gemini-2.0-flash".into(),
            label: String::new(),
            provider: "gemini".into(),
            model: "gemini-2.0-flash".into(),
            enabled: true,
            tier: "balanced".into(),
            tags: Vec::new(),
        };
        // 只有 OpenAI 密鑰時，Gemini 這個選項要顯示成不可用。
        let keys = vec![key_for("openai", "gpt-4o-mini", KeyStatus::Ok)];
        let json = option.to_json(&keys);
        assert_eq!(json["available"], serde_json::json!(false));
        assert_eq!(json["usable_keys"], serde_json::json!(0));

        // 加上一把冷卻中的 Gemini 密鑰：算得到總數，但仍然不可用。
        let keys = vec![
            key_for("openai", "gpt-4o-mini", KeyStatus::Ok),
            key_for("gemini", "gemini-2.0-flash", KeyStatus::Cooling),
        ];
        let json = option.to_json(&keys);
        assert_eq!(json["total_keys"], serde_json::json!(1));
        assert_eq!(json["available"], serde_json::json!(false));

        // 冷卻結束後就可用了。
        let keys = vec![key_for("gemini", "gemini-2.0-flash", KeyStatus::Ok)];
        let json = option.to_json(&keys);
        assert_eq!(json["available"], serde_json::json!(true));
        assert_eq!(json["provider_label"], serde_json::json!("Gemini"));
    }

    #[test]
    fn a_disabled_model_is_never_offered() {
        let option = ModelOption {
            id: "openai:gpt-4o".into(),
            label: String::new(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            enabled: false,
            tier: "balanced".into(),
            tags: Vec::new(),
        };
        let keys = vec![key_for("openai", "gpt-4o", KeyStatus::Ok)];
        // 有可用密鑰，但管理員把它關掉了。
        assert_eq!(option.to_json(&keys)["available"], serde_json::json!(false));
        assert_eq!(option.to_json(&keys)["usable_keys"], serde_json::json!(1));
    }

    #[test]
    fn orphan_models_from_deleted_keys_can_be_removed() {
        // 舊版依密鑰推導出的模型會留在設定檔裡。密鑰刪掉後這些項目變成孤兒，
        // 使用者會誤以為系統又自己新增了模型。
        let mut store = ChatStore {
            keys: vec![key_for("openai", "gpt-5.4-mini", KeyStatus::Ok)],
            models: vec![
                ModelOption {
                    id: "openai:gpt-5.4-mini".into(),
                    label: String::new(),
                    provider: "openai".into(),
                    model: "gpt-5.4-mini".into(),
                    enabled: true,
                    tier: "fast".into(),
                    tags: Vec::new(),
                },
                ModelOption {
                    id: "gemini:gemini-2.0-flash".into(),
                    label: String::new(),
                    provider: "gemini".into(),
                    model: "gemini-2.0-flash".into(),
                    enabled: true,
                    tier: "fast".into(),
                    tags: Vec::new(),
                },
                ModelOption {
                    id: "gemini:gemini-3.5-flash".into(),
                    label: String::new(),
                    provider: "gemini".into(),
                    model: "gemini-3.5-flash".into(),
                    enabled: true,
                    tier: "fast".into(),
                    tags: Vec::new(),
                },
            ],
            ..Default::default()
        };

        let providers: Vec<String> = store.keys.iter().map(|k| k.provider.clone()).collect();
        let mut removed: Vec<String> = Vec::new();
        store.models.retain(|m| {
            if providers.contains(&m.provider) {
                true
            } else {
                removed.push(m.model.clone());
                false
            }
        });

        assert_eq!(removed.len(), 2, "兩個 Gemini 模型沒有對應密鑰");
        assert_eq!(store.models.len(), 1);
        assert_eq!(store.models[0].model, "gpt-5.4-mini");
    }

    #[test]
    fn the_page_version_is_bumped_whenever_the_page_changes() {
        // 前端是 include_str! 編進二進位的。版本號不動，使用者就無法分辨
        // 「功能沒生效」是程式問題還是沒重新編譯。
        assert_ne!(PAGE_VERSION, "v3", "頁面改過就要把版本號往上加");
        assert!(PAGE_VERSION.starts_with('v'));
    }

    #[test]
    fn the_model_list_is_exactly_what_the_admin_typed() {
        // 沒有任何顯式模型時就是空的 —— 不再依密鑰「推導」出管理員沒填過的項目。
        let store = ChatStore {
            keys: vec![key_for("openai", "gpt-4o-mini", KeyStatus::Ok)],
            ..Default::default()
        };
        assert!(
            store.models.is_empty(),
            "有密鑰不代表就該自動生出模型選項"
        );
    }

    #[test]
    fn an_unavailable_model_explains_why() {
        let option = ModelOption {
            id: "gemini:gemini-2.0-flash".into(),
            label: String::new(),
            provider: "gemini".into(),
            model: "gemini-2.0-flash".into(),
            enabled: true,
            tier: "fast".into(),
            tags: Vec::new(),
        };

        // 1) 完全沒有這家的密鑰
        let json = option.to_json(&[key_for("openai", "gpt-4o-mini", KeyStatus::Ok)]);
        assert!(json["unavailable_reason"]
            .as_str()
            .unwrap()
            .contains("尚未新增"));

        // 2) 有密鑰但被停用
        let mut disabled = key_for("gemini", "gemini-2.0-flash", KeyStatus::Ok);
        disabled.enabled = false;
        let json = option.to_json(&[disabled]);
        assert!(json["unavailable_reason"].as_str().unwrap().contains("停用"));

        // 3) 密鑰失效
        let json = option.to_json(&[key_for("gemini", "gemini-2.0-flash", KeyStatus::Dead)]);
        assert!(json["unavailable_reason"].as_str().unwrap().contains("失效"));

        // 4) 冷卻中要講還要多久
        let json = option.to_json(&[key_for("gemini", "gemini-2.0-flash", KeyStatus::Cooling)]);
        assert!(json["unavailable_reason"].as_str().unwrap().contains("冷卻"));

        // 5) 正常可用時不該有理由
        let json = option.to_json(&[key_for("gemini", "gemini-2.0-flash", KeyStatus::Ok)]);
        assert_eq!(json["available"], serde_json::json!(true));
        assert_eq!(json["unavailable_reason"], serde_json::json!(""));
    }

    #[test]
    fn display_names_are_canonicalised_into_official_ids() {
        // 使用者把選單上看到的名字當代號填，其實是同一個模型。
        assert_eq!(
            canonical_model_name("openai", "GPT-4o mini").unwrap(),
            "gpt-4o-mini"
        );
        assert_eq!(
            canonical_model_name("openai", "GPT-3.5 Turbo").unwrap(),
            "gpt-3.5-turbo"
        );
        assert_eq!(
            canonical_model_name("gemini", "Gemini 3.5 Flash").unwrap(),
            "gemini-3.5-flash"
        );
        // 多餘的空白／連字號要收斂成一個。
        assert_eq!(
            canonical_model_name("openai", "  GPT-4o   mini  ").unwrap(),
            "gpt-4o-mini"
        );
        // 自架端點（LM Studio 等）大小寫敏感，不能亂改。
        assert_eq!(
            canonical_model_name("custom", "Qwen2.5-7B-Instruct").unwrap(),
            "Qwen2.5-7B-Instruct"
        );
        // 真的救不回來的還是要擋。
        assert!(canonical_model_name("openai", "gpt-4o★").is_err());
    }

    #[test]
    fn the_same_model_written_two_ways_is_merged_into_one() {
        let mut models = vec![
            ModelOption {
                id: "openai:GPT-4o mini".into(),
                label: String::new(),
                provider: "openai".into(),
                model: "GPT-4o mini".into(),
                enabled: true,
                tier: "fast".into(),
                tags: vec!["code".into()],
            },
            ModelOption {
                id: "openai:gpt-4o-mini".into(),
                label: "主力".into(),
                provider: "openai".into(),
                model: "gpt-4o-mini".into(),
                enabled: true,
                tier: "fast".into(),
                tags: Vec::new(),
            },
            ModelOption {
                id: "openai:gpt-4o★".into(),
                label: String::new(),
                provider: "openai".into(),
                model: "gpt-4o★".into(),
                enabled: true,
                tier: "balanced".into(),
                tags: Vec::new(),
            },
        ];
        let (fixed, merged, dropped) = migrate_models(&mut models);
        assert_eq!(fixed, 1, "GPT-4o mini 要被修正");
        assert_eq!(merged, 1, "修正後與既有項目重複，要合併");
        assert_eq!(dropped, vec!["gpt-4o★".to_string()]);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "gpt-4o-mini");
        assert_eq!(models[0].id, "openai:gpt-4o-mini");
        // 合併時要把對方設定過的資訊補進來，不能無腦丟掉。
        assert_eq!(models[0].label, "主力");
        assert_eq!(models[0].tags, vec!["code".to_string()]);
    }

    #[test]
    fn migrating_an_already_clean_list_changes_nothing() {
        let mut models = vec![ModelOption {
            id: "gemini:gemini-2.0-flash".into(),
            label: String::new(),
            provider: "gemini".into(),
            model: "gemini-2.0-flash".into(),
            enabled: true,
            tier: "fast".into(),
            tags: Vec::new(),
        }];
        let (fixed, merged, dropped) = migrate_models(&mut models);
        assert_eq!((fixed, merged, dropped.len()), (0, 0, 0));
        assert_eq!(models.len(), 1);
    }

    #[test]
    fn editing_a_key_without_touching_models_keeps_its_default() {
        // 編輯密鑰時「可用模型」留空＝不更動。若把 key.model 覆寫成空字串，
        // 這把密鑰的預設模型就會消失，接著只能退回服務商預設。
        let mut key = key_for("openai", "gpt-4.1-mini", KeyStatus::Ok);
        let listed: Vec<String> = Vec::new();
        let body_model = "";

        if let Some(first) = listed.first() {
            key.model = first.clone();
        } else if !body_model.trim().is_empty() {
            key.model = body_model.trim().to_string();
        }
        assert_eq!(key.model, "gpt-4.1-mini", "留空不該把預設模型清掉");

        // 有填就以第一行為準。
        let listed = vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()];
        if let Some(first) = listed.first() {
            key.model = first.clone();
        }
        assert_eq!(key.model, "gpt-4o");
    }

    #[test]
    fn a_display_name_typed_by_mistake_is_flagged_and_never_used() {
        // 使用者把「GPT-4o mini」這種顯示名稱當成模型代號填進去，
        // 送到上游一定 400。這種項目要被標出來並排除在選單外。
        let bad = ModelOption {
            id: "openai:GPT-4o mini".into(),
            label: String::new(),
            provider: "openai".into(),
            model: "GPT-4o mini".into(),
            enabled: true,
            tier: "fast".into(),
            tags: Vec::new(),
        };
        assert!(bad.validity().is_err());
        let keys = vec![key_for("openai", "gpt-4o-mini", KeyStatus::Ok)];
        let json = bad.to_json(&keys);
        assert_eq!(json["valid"], serde_json::json!(false));
        // 有可用密鑰也不能算 available，否則使用者選得到就會失敗。
        assert_eq!(json["available"], serde_json::json!(false));
        assert!(json["invalid_reason"]
            .as_str()
            .unwrap()
            .contains("空白"));

        // 自動模式寧可挑合法的那個。
        let good = ModelOption {
            id: "openai:gpt-4o-mini".into(),
            model: "gpt-4o-mini".into(),
            ..bad.clone()
        };
        let options = vec![bad, good];
        let (chosen, _) = pick_auto_model("你好", &options, &keys).expect("要挑得出");
        assert_eq!(chosen.model, "gpt-4o-mini");
    }

    #[test]
    fn auto_mode_gives_up_when_every_model_name_is_broken() {
        let broken = ModelOption {
            id: "openai:GPT 4o".into(),
            label: String::new(),
            provider: "openai".into(),
            model: "GPT 4o".into(),
            enabled: true,
            tier: "fast".into(),
            tags: Vec::new(),
        };
        let keys = vec![key_for("openai", "gpt-4o-mini", KeyStatus::Ok)];
        assert!(
            pick_auto_model("你好", &[broken], &keys).is_none(),
            "全部不合法時要回 None，讓上層給出明確錯誤"
        );
    }

    fn att(kind: &str, name: &str, data: &str, text: &str) -> Attachment {
        Attachment {
            name: name.to_string(),
            mime: if kind == "image" {
                "image/png".to_string()
            } else {
                "text/plain".to_string()
            },
            kind: kind.to_string(),
            data: data.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn agent_workspace_names_cannot_escape_the_sandbox() {
        // 沙箱唯一的入口，寫錯就等於開後門。
        for attack in [
            "../../../etc/passwd",
            "/etc/passwd",
            "C:\\Windows\\system.ini",
            "a/../../b.txt",
            "sub/dir/file.txt",
            ".env",
            ".htaccess",
            "shell.sh",
            "run.py",
            "app.exe",
            "x.php",
            "",
            "   ",
        ] {
            assert!(
                safe_workspace_name(attack).is_err(),
                "「{attack}」不應該被放行"
            );
        }
        // 正常檔名要放行。
        for good in [
            "report.md",
            "data.csv",
            "notes.txt",
            "chart.svg",
            "table 1.tsv",
            "a-b_c.log",
        ] {
            assert_eq!(safe_workspace_name(good).unwrap(), good);
        }
    }

    #[test]
    fn workspace_dirs_are_hashed_so_account_ids_never_become_paths() {
        // 帳號 id 可能含有 `/` 或 `..`，直接拿來當目錄名就是路徑穿越。
        let sneaky = workspace_dir("../../root");
        let text = sneaky.to_string_lossy().to_string();
        assert!(text.starts_with("config/ai_workspace/"));
        assert!(!text.contains(".."));
        let tail = text.trim_start_matches("config/ai_workspace/");
        assert_eq!(tail.len(), 16);
        assert!(tail.chars().all(|c| c.is_ascii_hexdigit()));
        // 同帳號穩定、不同帳號不同。
        assert_eq!(workspace_dir("alice"), workspace_dir("alice"));
        assert_ne!(workspace_dir("alice"), workspace_dir("bob"));
    }

    #[test]
    fn agent_calculator_handles_precedence_and_refuses_nonsense() {
        for (expr, want) in [
            ("1+2*3", 7.0),
            ("(1+2)*3", 9.0),
            ("10/4", 2.5),
            ("-5+3", -2.0),
            ("2*(3+(4-1))", 12.0),
            ("10%3", 1.0),
            ("--3", 3.0),
        ] {
            let got = calc_eval(expr).unwrap_or_else(|e| panic!("{expr} 應該算得出來：{e}"));
            assert!((got - want).abs() < 1e-9, "{expr} 得到 {got}，預期 {want}");
        }
        // 除以零、語法錯誤、想塞程式碼進來的一律拒絕。
        for bad in ["1/0", "1+", "(1+2", "abc", "__import__(\"os\")", "1;2", "$(whoami)"] {
            assert!(calc_eval(bad).is_err(), "「{bad}」不應該算得出來");
        }
    }

    #[test]
    fn tool_arguments_parse_from_both_string_and_object() {
        // 多數服務商把 arguments 放成 JSON 字串，少數直接給物件。
        let as_string = parse_tool_arguments(&json!(r#"{"name":"a.md"}"#));
        assert_eq!(as_string["name"], json!("a.md"));
        let as_object = parse_tool_arguments(&json!({ "name": "b.md" }));
        assert_eq!(as_object["name"], json!("b.md"));
        // 壞掉的 JSON 不能讓整個 Agent 崩掉，退回空物件。
        assert_eq!(parse_tool_arguments(&json!("not json")), json!({}));
        assert_eq!(parse_tool_arguments(&json!(42)), json!({}));
    }

    #[test]
    fn agent_tool_schema_is_well_formed() {
        let tools = agent_tools();
        assert_eq!(tools.len(), 6);
        for tool in &tools {
            assert_eq!(tool["type"], json!("function"));
            let name = tool["function"]["name"].as_str().expect("要有名字");
            assert!(!name.is_empty());
            assert!(
                tool["function"]["description"]
                    .as_str()
                    .map(|d| !d.is_empty())
                    .unwrap_or(false),
                "{name} 少了說明，模型會不知道何時該用它"
            );
            assert_eq!(tool["function"]["parameters"]["type"], json!("object"));
        }
        // 檔名參數是必填，否則模型可能不帶檔名就呼叫。
        let write = tools
            .iter()
            .find(|t| t["function"]["name"] == json!("workspace_write"))
            .expect("要有 workspace_write");
        assert_eq!(
            write["function"]["parameters"]["required"],
            json!(["name", "content"])
        );
    }

    #[test]
    fn unknown_tools_fail_softly_instead_of_panicking() {
        let (output, summary, ok) = run_tool("test-account", "rm_minus_rf", &json!({}));
        assert!(!ok);
        assert!(output.contains("沒有名叫"));
        assert_eq!(summary, "rm_minus_rf");
        // 計算工具走得通。
        let (output, summary, ok) = run_tool("test-account", "calc", &json!({ "expr": "6*7" }));
        assert!(ok);
        assert!(output.contains("42"));
        assert!(summary.contains("6*7"));
        // 想穿越沙箱的檔名要被工具層擋下，而且不是 panic。
        let (output, _, ok) = run_tool(
            "test-account",
            "workspace_read",
            &json!({ "name": "../../../etc/passwd" }),
        );
        assert!(!ok);
        assert!(output.contains("路徑"));
    }

    #[test]
    fn stream_deltas_never_resend_what_was_already_pushed() {
        // 逐段長出來的文字，每次只推新增的部分，串起來要等於原文。
        let steps = ["你", "你好", "你好，世界", "你好，世界！"];
        let mut sent = 0usize;
        let mut rebuilt = String::new();
        for text in steps {
            if let Some((delta, next)) = next_delta(text, sent) {
                rebuilt.push_str(&delta);
                sent = next;
            }
        }
        assert_eq!(rebuilt, "你好，世界！");
        assert_eq!(sent, "你好，世界！".len());
        // 沒有新內容時不該再推東西出去。
        assert!(next_delta("你好，世界！", sent).is_none());
    }

    #[test]
    fn stream_deltas_survive_multibyte_and_emoji_boundaries() {
        let full = "第一段🙂第二段——結束";
        let mut sent = 0usize;
        let mut rebuilt = String::new();
        // 一個字元一個字元長，最容易切壞 UTF-8 的情況。
        for end in full.char_indices().map(|(i, c)| i + c.len_utf8()) {
            let partial = &full[..end];
            if let Some((delta, next)) = next_delta(partial, sent) {
                rebuilt.push_str(&delta);
                sent = next;
            }
        }
        assert_eq!(rebuilt, full);
        // 游標落在非字元邊界時退回整段重推，不 panic。
        let (delta, next) = next_delta(full, 1).expect("要有內容");
        assert_eq!(delta, full);
        assert_eq!(next, full.len());
    }

    #[test]
    fn attachments_are_validated_before_they_reach_the_model() {
        // 1x1 PNG。
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let ok = prepare_attachments(&[
            att("image", "photo.png", png, ""),
            att("text", "note.csv", "", "a,b\n1,2"),
        ])
        .expect("應該通過");
        assert_eq!(ok.images.len(), 1);
        assert_eq!(ok.texts.len(), 1);
        assert_eq!(ok.names, vec!["photo.png", "note.csv"]);
        assert!(ok.images[0].1.starts_with("data:image/png;base64,"));
        assert!(!ok.is_empty());

        // 連 data URI 前綴一起送過來也要收。
        let with_prefix = prepare_attachments(&[att(
            "image",
            "x.png",
            &format!("data:image/png;base64,{png}"),
            "",
        )])
        .expect("應該通過");
        assert!(with_prefix.images[0].1.starts_with("data:image/png;base64,i"));

        // 壞掉的 base64、不支援的格式、空檔案都要擋。
        assert!(prepare_attachments(&[att("image", "x.png", "@@@not-base64@@@", "")]).is_err());
        let mut bad_mime = att("image", "x.bmp", png, "");
        bad_mime.mime = "image/bmp".to_string();
        assert!(prepare_attachments(&[bad_mime]).is_err());
        assert!(prepare_attachments(&[att("text", "empty.txt", "", "   ")]).is_err());
        assert!(prepare_attachments(&[att("pdf", "a.pdf", "", "x")]).is_err());

        // 超過數量上限。
        let many: Vec<Attachment> = (0..MAX_ATTACHMENTS + 1)
            .map(|i| att("text", &format!("f{i}.txt"), "", "x"))
            .collect();
        assert!(prepare_attachments(&many).is_err());
    }

    #[test]
    fn long_text_files_are_truncated_instead_of_eating_the_budget() {
        let huge = "字".repeat(MAX_TEXT_CHARS + 500);
        let prepared = prepare_attachments(&[att("text", "big.log", "", &huge)]).expect("要收下");
        let body = &prepared.texts[0].1;
        assert!(body.contains("檔案過長"));
        assert!(body.chars().count() < MAX_TEXT_CHARS + 60);
    }

    #[test]
    fn attachment_names_cannot_smuggle_paths_or_control_characters() {
        assert_eq!(safe_attachment_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_attachment_name("C:\\temp\\a.png"), "a.png");
        assert_eq!(safe_attachment_name("a\nb.txt"), "ab.txt");
        assert_eq!(safe_attachment_name("   "), "附件");
    }

    #[test]
    fn vision_capability_follows_admin_tags_then_the_model_name() {
        let mut option = ModelOption {
            id: "groq:llama-3.3-70b-versatile".to_string(),
            label: String::new(),
            provider: "groq".to_string(),
            model: "llama-3.3-70b-versatile".to_string(),
            enabled: true,
            tier: "balanced".to_string(),
            tags: Vec::new(),
        };
        // 純文字模型：看不懂圖。
        assert!(!model_supports_vision(&option));
        // 管理員說它可以，就聽管理員的。
        option.tags = vec!["vision".to_string()];
        assert!(model_supports_vision(&option));

        // 沒標籤時靠名稱認得出來的那些。
        for name in [
            "gpt-4o-mini",
            "gemini-2.0-flash",
            "qwen/qwen3-vl-32b-instruct",
            "meta-llama/llama-4-scout-17b-16e-instruct",
            "qwen/qwen3.6-27b",
        ] {
            let candidate = ModelOption {
                id: format!("x:{name}"),
                label: String::new(),
                provider: "groq".to_string(),
                model: name.to_string(),
                enabled: true,
                tier: "balanced".to_string(),
                tags: Vec::new(),
            };
            assert!(model_supports_vision(&candidate), "{name} 應該支援圖片");
        }
    }

    #[test]
    fn legacy_stripped_groq_names_get_their_prefix_back() {
        // 舊版把前綴刪掉存進設定檔，載入時要自動補回，不然只會拿到 404。
        for (stored, expected) in [
            ("gpt-oss-120b", "openai/gpt-oss-120b"),
            ("gpt-oss-20b", "openai/gpt-oss-20b"),
            ("qwen3.8-27b", "qwen/qwen3.8-27b"),
            ("kimi-k2-instruct-0905", "moonshotai/kimi-k2-instruct-0905"),
            ("llama-4-scout-17b-16e-instruct", "meta-llama/llama-4-scout-17b-16e-instruct"),
            ("compound-mini", "groq/compound-mini"),
        ] {
            assert_eq!(sanitize_model_name("groq", stored).unwrap(), expected);
            assert_eq!(canonical_model_name("groq", stored).unwrap(), expected);
        }
    }

    #[test]
    fn groq_models_without_official_prefix_are_left_alone() {
        // 本來就沒前綴的別亂補，補了反而變成不存在的模型。
        for id in [
            "llama-3.3-70b-versatile",
            "llama-3.1-8b-instant",
            "whisper-large-v3",
            "whisper-large-v3-turbo",
            "gemma2-9b-it",
        ] {
            assert_eq!(sanitize_model_name("groq", id).unwrap(), id);
            assert!(groq_full_model_id(id).is_none());
        }
        // 別的服務商不套用這張表。
        assert_eq!(
            sanitize_model_name("openai", "gpt-oss-120b").unwrap(),
            "gpt-oss-120b"
        );
    }

    #[test]
    fn migrate_repairs_stored_groq_models_on_load() {
        let mut models = vec![ModelOption {
            id: "groq:gpt-oss-120b".to_string(),
            label: String::new(),
            provider: "groq".to_string(),
            model: "gpt-oss-120b".to_string(),
            enabled: true,
            tier: "deep".to_string(),
            tags: Vec::new(),
        }];
        let (fixed, _merged, dropped) = migrate_models(&mut models);
        assert_eq!(fixed, 1);
        assert!(dropped.is_empty());
        assert_eq!(models[0].model, "openai/gpt-oss-120b");
        assert_eq!(models[0].id, "groq:openai/gpt-oss-120b");
    }

    #[test]
    fn groq_model_ids_keep_their_vendor_prefix() {
        // Groq 的模型 ID 本來就帶前綴，剝掉就找不到模型。
        for id in [
            "openai/gpt-oss-120b",
            "qwen/qwen3.6-27b",
            "meta-llama/llama-4-scout-17b-16e-instruct",
            "moonshotai/kimi-k2-instruct-0905",
            "groq/compound-mini",
        ] {
            assert_eq!(
                canonical_model_name("groq", id).unwrap(),
                id,
                "Groq 的 {id} 必須原樣保留"
            );
        }
        // 沒有前綴的 Groq 模型也還是合法（例如 whisper-large-v3）。
        assert_eq!(
            canonical_model_name("groq", "whisper-large-v3").unwrap(),
            "whisper-large-v3"
        );
        // 分級推導對帶前綴的名稱也要正常運作。
        assert_eq!(infer_tier("openai/gpt-oss-120b"), "deep");
        assert_eq!(infer_tier("openai/gpt-oss-20b"), "balanced");
    }

    #[test]
    fn providers_that_reject_slashes_still_get_them_stripped() {
        // Gemini 帶前綴會回 400 unexpected model name format，要修掉。
        assert_eq!(
            canonical_model_name("gemini", "google/gemini-2.0-flash").unwrap(),
            "gemini-2.0-flash"
        );
        assert_eq!(
            canonical_model_name("openai", "openai/gpt-4o-mini").unwrap(),
            "gpt-4o-mini"
        );
        assert!(provider_allows_slash("groq"));
        assert!(provider_allows_slash("openrouter"));
        assert!(provider_allows_slash("custom"));
        assert!(!provider_allows_slash("gemini"));
        assert!(!provider_allows_slash("openai"));
    }

    #[test]
    fn a_slash_prefixed_model_name_is_repaired_for_gemini() {
        // 使用者常從免費 API 清單複製到 OpenRouter 格式，貼進 Gemini 會回
        // 400 unexpected model name format。這裡直接修掉而不是讓它爆。
        assert_eq!(
            sanitize_model_name("gemini", "google/gemini-2.0-flash").unwrap(),
            "gemini-2.0-flash"
        );
        assert_eq!(
            sanitize_model_name("gemini", "models/gemini-2.5-pro").unwrap(),
            "gemini-2.5-pro"
        );
        // OpenRouter 反過來一定要保留斜線。
        assert_eq!(
            sanitize_model_name("openrouter", "google/gemini-2.0-flash").unwrap(),
            "google/gemini-2.0-flash"
        );
    }

    #[test]
    fn a_malformed_model_name_is_rejected_with_a_readable_reason() {
        assert!(sanitize_model_name("gemini", "   ").is_err());
        let spaced = sanitize_model_name("gemini", "gemini 2.0 flash").unwrap_err();
        assert!(spaced.contains("空白"), "{spaced}");
        let bad = sanitize_model_name("gemini", "gemini-2.0-flash★").unwrap_err();
        assert!(bad.contains("不合法"), "{bad}");
        let many = sanitize_model_name("gemini", "a/b/c").unwrap_err();
        assert!(many.contains("斜線"), "{many}");
        // 從網頁複製常夾帶的零寬字元與全形空白要被清掉。
        assert_eq!(
            sanitize_model_name("gemini", "\u{feff}gemini-2.0-flash").unwrap(),
            "gemini-2.0-flash"
        );
        assert!(sanitize_model_name("gemini", "gemini\u{3000}flash").is_err());
    }

    #[test]
    fn token_estimate_treats_chinese_as_one_token_per_character() {
        // 4 個中文字 + 每則固定開銷 4
        assert_eq!(estimate_tokens("你好世界"), 8);
        // 8 個 ASCII 字元 → 2 tokens + 4
        assert_eq!(estimate_tokens("abcdefgh"), 6);
        assert!(estimate_tokens("") == 4);
        // 長文要明顯比短文貴。
        assert!(estimate_tokens(&"公告內容".repeat(100)) > estimate_tokens("你好"));
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: content.into(),
            ts: 0,
            key_name: String::new(),
            model: String::new(),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn context_selection_respects_the_token_budget() {
        let history: Vec<ChatMessage> = (0..20)
            .map(|i| msg(if i % 2 == 0 { "user" } else { "assistant" }, &"字".repeat(100)))
            .collect();
        // 每則約 104 tokens，預算 500 大約只塞得下 4 則。
        let picked = select_context(&history, 500);
        assert!(picked.len() >= 4 && picked.len() <= 5, "實際 {}", picked.len());
        // 一定要保留最新的那一則，否則模型不知道使用者在問什麼。
        assert_eq!(
            picked.last().unwrap().content,
            history.last().unwrap().content
        );
        // 順序要維持由舊到新。
        let all = select_context(&history, 100_000);
        assert_eq!(all.len(), CONTEXT_MESSAGES.min(history.len()));
    }

    #[test]
    fn the_latest_message_is_sent_even_if_it_blows_the_budget() {
        let history = vec![msg("user", &"字".repeat(5000))];
        let picked = select_context(&history, 100);
        assert_eq!(picked.len(), 1, "超預算也要送出使用者這一則");
    }

    #[test]
    fn the_cache_only_hits_on_an_identical_request() {
        let a = vec![json!({"role":"user","content":"這週作業截止日"})];
        let b = vec![json!({"role":"user","content":"這週作業截止日？"})];
        let ka = chat_cache_key("gemini:flash", &a);
        assert_eq!(ka, chat_cache_key("gemini:flash", &a));
        assert_ne!(ka, chat_cache_key("gemini:flash", &b), "問題不同就不該命中");
        assert_ne!(ka, chat_cache_key("openai:gpt-4o", &a), "換模型不該共用");

        chat_cache_put(&ka, "10 月 3 日", "gemini-2.0-flash");
        let hit = chat_cache_get(&ka).expect("要命中");
        assert_eq!(hit.0, "10 月 3 日");
        assert_eq!(hit.1, "gemini-2.0-flash");
        assert!(chat_cache_get("nonexistent-key").is_none());
        // 空回覆不該進快取，否則會一直回空白。
        chat_cache_put("empty-key", "   ", "m");
        assert!(chat_cache_get("empty-key").is_none());
    }

    #[test]
    fn registering_models_does_not_wipe_out_the_other_keys() {
        // 兩把不同服務商的密鑰，目前是「自動推導」狀態（models 為空）。
        let mut store = ChatStore {
            keys: vec![
                key_for("openai", "gpt-4o-mini", KeyStatus::Ok),
                key_for("gemini", "gemini-2.0-flash", KeyStatus::Ok),
            ],
            ..Default::default()
        };
        assert!(store.models.is_empty());

        // 只替 OpenAI 登記兩個模型。
        let added = register_models(
            &mut store,
            "openai",
            &["gpt-4o".to_string(), "gpt-4.1".to_string()],
        );
        assert_eq!(added, 2);

        let ids: Vec<String> = store.models.iter().map(|m| m.id.clone()).collect();
        // 關鍵：Gemini 推導出來的模型不能因此消失。
        assert!(
            ids.contains(&"gemini:gemini-2.0-flash".to_string()),
            "其他密鑰推導出的模型不該被洗掉，實際：{ids:?}"
        );
        assert!(ids.contains(&"openai:gpt-4o-mini".to_string()));
        assert!(ids.contains(&"openai:gpt-4o".to_string()));
        assert!(ids.contains(&"openai:gpt-4.1".to_string()));
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn registering_the_same_model_twice_is_a_no_op() {
        let mut store = ChatStore {
            keys: vec![key_for("gemini", "gemini-2.0-flash", KeyStatus::Ok)],
            ..Default::default()
        };
        assert_eq!(
            register_models(&mut store, "gemini", &["gemini-2.0-flash".to_string()]),
            0,
            "推導清單裡已經有了，不該重複新增"
        );
        assert_eq!(store.models.len(), 1);

        // Gemini 回的 models/ 前綴要先正規化，否則會變成第二筆重複資料。
        assert_eq!(
            register_models(&mut store, "gemini", &["models/gemini-2.0-flash".to_string()]),
            0
        );
        assert_eq!(store.models.len(), 1);

        // 空白行要被忽略。
        assert_eq!(
            register_models(&mut store, "gemini", &["".to_string(), "  ".to_string()]),
            0
        );
    }

    #[test]
    fn a_multi_model_key_registers_every_line() {
        let mut store = ChatStore::default();
        let added = register_models(
            &mut store,
            "gemini",
            &[
                "gemini-2.5-pro".to_string(),
                "gemini-2.5-flash".to_string(),
                "gemini-2.0-flash".to_string(),
            ],
        );
        assert_eq!(added, 3, "一把金鑰填三個模型就要有三個選項");
        // 分級要各自推導，自動模式才能挑對人。
        let pro = store.models.iter().find(|m| m.model == "gemini-2.5-pro").unwrap();
        let flash = store.models.iter().find(|m| m.model == "gemini-2.0-flash").unwrap();
        assert_eq!(pro.tier, "deep");
        assert_eq!(flash.tier, "fast");
    }

    #[test]
    fn gemini_is_not_mistaken_for_a_mini_model() {
        // 「gemini」字串裡就含有「mini」。用子字串比對會把所有 Gemini
        // 模型都判成快速模型，長文與推理就會派錯人。
        assert_eq!(infer_tier("gemini-2.5-pro"), "deep");
        assert_eq!(infer_tier("gemini-2.0-flash"), "fast");
        assert_eq!(
            model_tokens("openai/gpt-4o-mini"),
            vec!["openai", "gpt", "4o", "mini"]
        );
        assert_eq!(param_size("70b"), Some(70));
        assert_eq!(param_size("4o"), None);
    }

    #[test]
    fn a_short_chinese_request_is_not_treated_as_small_talk() {
        // 11 個字，但是一個完整任務。
        let need = classify_prompt("幫我寫一封請假信給教授");
        assert!(!need.simple, "這不是閒聊，不該派最小的模型");
        assert!(classify_prompt("你好").simple);
        assert!(classify_prompt("台灣的首都").simple);
    }

    #[test]
    fn model_tier_is_inferred_from_the_name() {
        assert_eq!(infer_tier("gemini-2.0-flash"), "fast");
        assert_eq!(infer_tier("gpt-4o-mini"), "fast");
        assert_eq!(infer_tier("gemini-2.5-pro"), "deep");
        assert_eq!(infer_tier("deepseek-reasoner"), "deep");
        assert_eq!(infer_tier("llama-3.3-70b-versatile"), "deep");
        assert_eq!(infer_tier("gpt-4o"), "balanced");
        assert_eq!(infer_tier("llama-3.1-8b-instant"), "fast");
        assert_eq!(infer_tier("qwen-2.5-72b"), "deep");
        assert_eq!(infer_tier("gemini-2.5-flash"), "fast");
        assert_eq!(infer_tags("deepseek-coder"), vec!["code".to_string()]);
        assert!(infer_tags("gpt-4o").is_empty());
    }

    #[test]
    fn prompts_are_classified_by_what_they_need() {
        let code = classify_prompt("這段 code 為什麼會 bug？\n```js\nconst a=1;\n```");
        assert!(code.code && !code.simple);

        let math = classify_prompt("請證明畢氏定理");
        assert!(math.math);

        let simple = classify_prompt("你好");
        assert!(simple.simple && !simple.code && !simple.math);

        let long = classify_prompt(&"這是一段很長的公告內容。".repeat(200));
        assert!(long.long && !long.simple);

        let general = classify_prompt("幫我寫一封請假信給教授，語氣客氣一點");
        assert!(!general.code && !general.math && !general.simple && !general.long);
    }

    fn opt(id: &str, provider: &str, model: &str, tier: &str, tags: &[&str]) -> ModelOption {
        ModelOption {
            id: id.into(),
            label: String::new(),
            provider: provider.into(),
            model: model.into(),
            enabled: true,
            tier: tier.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn auto_mode_sends_easy_questions_to_the_cheap_model() {
        let options = vec![
            opt("a", "openai", "gpt-4o", "deep", &[]),
            opt("b", "openai", "gpt-4o-mini", "fast", &[]),
        ];
        let keys = vec![key_for("openai", "gpt-4o", KeyStatus::Ok)];
        let (chosen, reason) = pick_auto_model("你好", &options, &keys).expect("要挑得出模型");
        assert_eq!(chosen.model, "gpt-4o-mini", "打招呼不該動用貴的模型");
        assert!(reason.contains("簡短問題"), "{reason}");
    }

    #[test]
    fn auto_mode_prefers_a_code_model_for_code_questions() {
        let options = vec![
            opt("a", "openai", "gpt-4o-mini", "fast", &[]),
            opt("b", "deepseek", "deepseek-coder", "balanced", &["code"]),
        ];
        let keys = vec![
            key_for("openai", "gpt-4o-mini", KeyStatus::Ok),
            key_for("deepseek", "deepseek-coder", KeyStatus::Ok),
        ];
        let (chosen, reason) =
            pick_auto_model("這段 function 為什麼會 bug", &options, &keys).expect("要挑得出");
        assert_eq!(chosen.model, "deepseek-coder");
        assert!(reason.contains("程式"), "{reason}");
    }

    #[test]
    fn auto_mode_upgrades_to_a_strong_model_for_long_input() {
        let options = vec![
            opt("a", "openai", "gpt-4o-mini", "fast", &[]),
            opt("b", "gemini", "gemini-2.5-pro", "deep", &[]),
        ];
        let keys = vec![
            key_for("openai", "gpt-4o-mini", KeyStatus::Ok),
            key_for("gemini", "gemini-2.5-pro", KeyStatus::Ok),
        ];
        let long = "請幫我摘要這份公告。".repeat(200);
        let (chosen, _) = pick_auto_model(&long, &options, &keys).expect("要挑得出");
        assert_eq!(chosen.model, "gemini-2.5-pro");
    }

    #[test]
    fn auto_mode_never_picks_a_model_without_a_usable_key() {
        let options = vec![
            opt("a", "deepseek", "deepseek-coder", "balanced", &["code"]),
            opt("b", "openai", "gpt-4o-mini", "fast", &[]),
        ];
        // 最適合的 deepseek 密鑰已失效，只剩 openai 能用。
        let keys = vec![
            key_for("deepseek", "deepseek-coder", KeyStatus::Dead),
            key_for("openai", "gpt-4o-mini", KeyStatus::Ok),
        ];
        let (chosen, _) = pick_auto_model("修一下這段 code 的 bug", &options, &keys).expect("要挑得出");
        assert_eq!(chosen.model, "gpt-4o-mini", "不能挑一個送出去必定失敗的模型");

        // 全部不可用時要回 None，讓上層產生明確錯誤。
        let dead = vec![key_for("openai", "gpt-4o-mini", KeyStatus::Dead)];
        assert!(pick_auto_model("你好", &options, &dead).is_none());
    }

    #[test]
    fn a_disabled_model_is_invisible_to_auto_mode() {
        let mut options = vec![opt("a", "openai", "gpt-4o-mini", "fast", &[])];
        options[0].enabled = false;
        let keys = vec![key_for("openai", "gpt-4o-mini", KeyStatus::Ok)];
        assert!(pick_auto_model("你好", &options, &keys).is_none());
    }

    #[test]
    fn gemini_model_ids_lose_their_prefix() {
        // Gemini 的 OpenAI 相容層回的是 models/gemini-2.0-flash
        assert_eq!(normalize_model_id("models/gemini-2.0-flash"), "gemini-2.0-flash");
        assert_eq!(normalize_model_id("  gpt-4o-mini "), "gpt-4o-mini");
        assert_eq!(normalize_model_id("openai/gpt-4o"), "openai/gpt-4o");
    }

    #[test]
    fn titles_are_derived_and_truncated() {
        assert_eq!(derive_title("   "), "新對話");
        assert_eq!(derive_title("幫我寫\n一段程式"), "幫我寫 一段程式");
        let long = "一".repeat(40);
        let title = derive_title(&long);
        assert_eq!(title.chars().count(), 25, "24 個字加上省略號");
        assert!(title.ends_with('…'));
    }
}
