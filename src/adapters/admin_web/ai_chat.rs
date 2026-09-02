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
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
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
const PAGE_VERSION: &str = "v3";

fn build_stamp() -> &'static str {
    option_env!("TRONCLASS_BUILD_STAMP").unwrap_or("dev")
}

/// 單一請求的上限。串流過程中只要持續有 token 進來就不算逾時。
const AI_REQUEST_TIMEOUT_SECS: u64 = 180;
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

    fn to_json(&self, keys: &[ApiKey]) -> Value {
        let usable = keys
            .iter()
            .filter(|k| k.provider == self.provider && k.available())
            .count();
        let total = keys.iter().filter(|k| k.provider == self.provider).count();
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
            "available": self.enabled && usable > 0,
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
        std::sync::Mutex::new(store)
    });

fn persist_chat_store(store: &ChatStore) {
    let path = chat_store_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(store) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                tracing::warn!("無法寫入 ai_chat.json：{e}");
                return;
            }
            // 檔案含明文密鑰，權限收到 0600。
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
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
        if let Err(e) = std::fs::write(path, text) {
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
/// 由密鑰推導出模型清單（每把密鑰的預設模型各一項）。
fn derive_models_from_keys(keys: &[ApiKey]) -> Vec<ModelOption> {
    let mut out: Vec<ModelOption> = Vec::new();
    for key in keys.iter() {
        let model = key.effective_model();
        if model.is_empty() {
            continue;
        }
        let id = format!("{}:{}", key.provider, model);
        if out.iter().any(|m| m.id == id) {
            continue;
        }
        out.push(ModelOption {
            tier: infer_tier(&model),
            tags: infer_tags(&model),
            id,
            label: String::new(),
            provider: key.provider.clone(),
            model,
            enabled: true,
        });
    }
    out
}

/// AI_CHAT_V4：把一批模型名稱登記成使用者可選的模型。
///
/// 關鍵細節：目前若還是「自動推導」狀態（`store.models` 是空的），必須**先把
/// 推導結果固化**再寫入，否則一旦清單變成非空，其他密鑰推導出來的模型會整批
/// 消失 —— 使用者會覺得「我只是新增一把金鑰，別的模型怎麼不見了」。
fn register_models(store: &mut ChatStore, provider: &str, models: &[String]) -> usize {
    let wanted: Vec<String> = models
        .iter()
        .filter_map(|m| sanitize_model_name(provider, m).ok())
        .collect();
    if wanted.is_empty() {
        return 0;
    }
    if store.models.is_empty() {
        store.models = derive_models_from_keys(&store.keys);
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

fn model_options() -> Vec<ModelOption> {
    read_store(|store| {
        if !store.models.is_empty() {
            return store.models.clone();
        }
        derive_models_from_keys(&store.keys)
    })
    .unwrap_or_default()
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
fn resolve_model(id: &str, prompt: &str) -> (Option<ModelOption>, String) {
    let options = model_options();
    let keys = read_store(|store| store.keys.clone()).unwrap_or_default();
    let wanted = id.trim();

    if !wanted.is_empty() && wanted != AUTO_MODEL_ID {
        if let Some(found) = options.iter().find(|m| m.id == wanted && m.enabled) {
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
            format!("HTTP {status}（模型 {model}）：{detail}")
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
            key.model = listed
                .first()
                .cloned()
                .unwrap_or_else(|| body.model.trim().to_string());
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
/// OpenRouter 一定要帶（`openai/gpt-4o`），Gemini 帶了就會被拒。
fn provider_allows_slash(provider: &str) -> bool {
    matches!(provider, "openrouter" | "custom")
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

/// 依服務商清洗並驗證模型名稱。回傳 `Err` 時附上人看得懂的原因。
fn sanitize_model_name(provider: &str, raw: &str) -> Result<String, String> {
    let mut name = normalize_model_id(raw);
    if name.is_empty() {
        return Err("模型名稱是空的".to_string());
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
            let model = if body.model.trim().is_empty() {
                provider_preset(&provider)
                    .map(|(_, _, m)| m.to_string())
                    .unwrap_or_default()
            } else {
                body.model.trim().to_string()
            };
            if model.is_empty() {
                return Err("請填入模型名稱".to_string());
            }
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
            // 一行一個模型名稱，或由「抓取模型」勾選後送過來。
            for raw in body.models.iter() {
                let model = normalize_model_id(raw);
                if model.is_empty() {
                    continue;
                }
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
        "adopt" => {
            // 把目前自動推導的清單「固化」成可編輯的顯式清單。
            if !store.models.is_empty() {
                return Err("清單已經是自訂狀態".to_string());
            }
            let out = derive_models_from_keys(&store.keys);
            if out.is_empty() {
                return Err("目前沒有密鑰可以推導模型".to_string());
            }
            let count = out.len();
            store.models = out;
            Ok(format!("已依現有密鑰建立 {count} 個模型，接下來可自行增減"))
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

    let text = body.message.trim().to_string();
    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "訊息是空的", "code": "AI_EMPTY_MESSAGE" })),
        )
            .into_response();
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
    let (selection, auto_reason) = resolve_model(&body.model_id, &text);
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
            content: text.clone(),
            ts: now_millis(),
            key_name: String::new(),
            model: String::new(),
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
        tokio::spawn(async move {
            run_chat_job(job_id, messages, settings, selection).await;
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

/// 輪詢：拿目前已生成的全文。
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
