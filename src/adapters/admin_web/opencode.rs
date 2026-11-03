//! opencode AI coding agent 後端模組。
//!
//! OPENCODE_INTEGRATION_V1
//! OPENCODE_AGENT_ONLY_V2（Turn 8）：AI 頁面只走 opencode（不再經過一般聊天）。
//!
//! 設計重點：
//! 1. **Sidecar 代理**：面板透過 Rust 後端代理 opencode server (127.0.0.1:4097)
//! 2. **權限控管**：僅管理員可設定與管理模型，僅 allowlist 帳號可使用
//! 3. **密鑰加密**：NVIDIA API Key 用 StoreSecret 加密存 config/ai_opencode.json
//! 4. **沙箱隔離**：opencode 工作區在 opencode-workspace/，proot 硬隔離
//! 5. **Session 對應**：Dashboard thread_id ↔ opencode session_id（建立者才能存取）
//! 6. **模型清單**：寫入 opencode.json 的 provider.nvidia.models；變更後要求 watchdog 重啟 sidecar
//!
//! opencode server API（https://opencode.ai/docs/server/）：
//! - 先 `POST /session` 建立 session，再 `POST /session/{id}/message`，
//!   body 為 `{ model: { providerID, modelID }, parts: [{ type: "text", text }] }`，
//!   回應為 `{ info, parts }`，文字在 parts 裡 type == "text" 的項目。

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use super::{
    session_token,
    unauthorized_response, AdminQuery, AdminWebState,
};

const OPENCODE_CONFIG_FILE: &str = "config/ai_opencode.json";
/// 對話清單（dashboard 對話 → sidecar session）。落盤、0600。
const OPENCODE_SESSIONS_FILE: &str = "config/ai_opencode_sessions.json";
/// 對話清單上限；超過時移除最久沒用的。
const OPENCODE_SESSIONS_MAX: usize = 500;
/// 公告摘要呼叫 NVIDIA 的逾時（摘要在背景 job 裡跑，可以比互動聊天久一點）。
const OPENCODE_SUMMARY_TIMEOUT_SECS: u64 = 120;
/// 摘要結果裡顯示的「由誰生成」標籤。
const OPENCODE_SUMMARY_LABEL: &str = "NVIDIA（opencode 設定）";
const OPENCODE_SERVER_PASS_FILE: &str = "config/opencode-server.txt";
const OPENCODE_RESTART_FILE: &str = "config/opencode-restart";
const OPENCODE_WORKSPACE: &str = "opencode-workspace";
const OPENCODE_DEFAULT_PORT: u16 = 4097;
const OPENCODE_HEALTH_TIMEOUT_SECS: u64 = 3;
/// 一則訊息可能包含多步工具呼叫，所以給比較長的時間。
const OPENCODE_CHAT_TIMEOUT_SECS: u64 = 600;
/// CHAT_JOB_V1：背景對話 job 的保留時間與上限，以及前端輪詢間隔（毫秒）。
const OPENCODE_JOB_TTL_SECS: u64 = 1800;
const OPENCODE_JOB_MAX: usize = 32;
const OPENCODE_CHAT_POLL_MS: u64 = 1000;
/// OPENCODE_STREAM_V1：送出訊息期間，每隔這麼久讀一次 sidecar 的訊息，更新 job.partial。
const OPENCODE_PROGRESS_POLL_MS: u64 = 1000;
const OPENCODE_API_TIMEOUT_SECS: u64 = 20;
const OPENCODE_PROVIDER: &str = "nvidia";
/// 自訂 OpenAI 相容供應商（CUSTOM_PROVIDER_V1）的 provider ID；金鑰存在 provider_keys["custom"]。
const OPENCODE_CUSTOM_PROVIDER: &str = "custom";
/// 自訂供應商金鑰檔（opencode-workspace 內，0600）。
const OPENCODE_CUSTOM_KEY_FILE: &str = "custom.key";
const OPENCODE_CUSTOM_NAME_MAX: usize = 40;
const OPENCODE_CUSTOM_URL_MAX: usize = 300;
const OPENCODE_CUSTOM_NAME_DEFAULT: &str = "自訂供應商";
const OPENCODE_MAX_MODELS: usize = 30;
const OPENCODE_MODEL_MAX_LEN: usize = 100;
const OPENCODE_MESSAGE_MAX_CHARS: usize = 20_000;
const OPENCODE_TITLE_MAX_CHARS: usize = 40;

/// opencode 設定結構（含加密後的 API key）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpencodeConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_base_url")]
    base_url: String,
    /// 預設模型（NVIDIA API 的完整 ID，例如 nvidia/nemotron-3-super-120b-a12b）
    #[serde(default = "default_model")]
    model: String,
    /// 每個服務商一把金鑰（provider → 金鑰）。同一服務商的所有模型共用這把金鑰。
    /// 磁碟上的值是 StoreSecret 加密字串；記憶體中是明文。
    #[serde(default)]
    provider_keys: BTreeMap<String, String>,
    /// 舊版單一金鑰欄位（屬於 NVIDIA）。只用來讀取時遷移，寫入時不再輸出。
    #[serde(default, rename = "api_key_encrypted", skip_serializing)]
    legacy_api_key_encrypted: String,
    #[serde(default)]
    allowed_accounts: Vec<String>,  // allowlist
    /// 模型清單（OPENCODE_AGENT_ONLY_V2）。讀取時一定包含 model。
    #[serde(default)]
    models: Vec<String>,
    /// 自訂 OpenAI 相容供應商（CUSTOM_PROVIDER_V1）。base_url 為空代表未設定。
    #[serde(default)]
    custom: CustomProviderConfig,
    #[serde(default)]
    updated_at: u64,
}

/// 自訂供應商的設定（金鑰不在這裡，見 provider_keys["custom"]）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CustomProviderConfig {
    #[serde(default)]
    name: String,
    #[serde(default)]
    base_url: String,
    /// 預設模型（必在 models 內；models 為空時為空字串）
    #[serde(default)]
    default_model: String,
    /// 這個供應商自己的模型清單（ID 格式不強制「廠商/」）
    #[serde(default)]
    models: Vec<String>,
}

fn default_base_url() -> String {
    "https://integrate.api.nvidia.com/v1".to_string()
}

fn default_model() -> String {
    "nvidia/nemotron-3-super-120b-a12b".to_string()
}

/// 舊設定可能只存了模型名（沒有廠商前綴）。NVIDIA 的模型 ID 一律是「廠商/模型名」，
/// 讀取時補上 nvidia/，與 NVIDIA 官方目錄一致。
fn normalize_stored_model(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return default_model();
    }
    if trimmed.contains('/') {
        trimmed.to_string()
    } else {
        format!("nvidia/{}", trimmed)
    }
}

/// 模型清單：正規化、去重，並保證包含預設模型。
fn effective_models(cfg: &OpencodeConfig) -> Vec<String> {
    let default = normalize_stored_model(&cfg.model);
    let mut out: Vec<String> = Vec::new();
    let candidates = cfg
        .models
        .iter()
        .map(|m| normalize_stored_model(m))
        .chain(std::iter::once(default));
    for item in candidates {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// 驗證使用者輸入的模型 ID：必須是 NVIDIA 格式「廠商/模型名」。
fn validate_model_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty() {
        return Err("模型 ID 不可為空".to_string());
    }
    if id.len() > OPENCODE_MODEL_MAX_LEN {
        return Err(format!("模型 ID 最多 {} 字元", OPENCODE_MODEL_MAX_LEN));
    }
    let charset_ok = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if !charset_ok {
        return Err("模型 ID 只能包含英數字與 - _ . /".to_string());
    }
    let parts: Vec<&str> = id.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
        return Err(
            "模型 ID 需要「廠商/模型名」格式，例如 nvidia/nemotron-3-super-120b-a12b".to_string(),
        );
    }
    Ok(id.to_string())
}

/// 自訂供應商的模型 ID：英數字與 - _ . / :（允許 model:tag 形式），不可含空白或 |。
fn validate_custom_model_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty() {
        return Err("模型 ID 不可為空".to_string());
    }
    if id.len() > OPENCODE_MODEL_MAX_LEN {
        return Err(format!("模型 ID 最多 {} 字元", OPENCODE_MODEL_MAX_LEN));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
    {
        return Err("模型 ID 只能包含英數字與 - _ . / :".to_string());
    }
    Ok(id.to_string())
}

/// 自訂供應商的 Base URL：必須以 http:// 或 https:// 開頭、不含空白，去掉結尾的 /。
fn normalize_custom_base_url(raw: &str) -> Result<String, String> {
    let url = raw.trim().trim_end_matches('/').to_string();
    if url.len() > OPENCODE_CUSTOM_URL_MAX {
        return Err(format!("Base URL 最多 {} 字元", OPENCODE_CUSTOM_URL_MAX));
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("Base URL 需以 https:// 或 http:// 開頭".to_string());
    }
    if url.chars().any(|c| c.is_whitespace()) {
        return Err("Base URL 不可含空白".to_string());
    }
    Ok(url)
}

/// 自訂供應商的有效模型清單：去重、去空白、上限 OPENCODE_MAX_MODELS，且包含預設模型。
fn effective_custom_models(cfg: &CustomProviderConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in &cfg.models {
        let id = raw.trim().to_string();
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    }
    let default = cfg.default_model.trim();
    if !default.is_empty() && !out.iter().any(|m| m == default) {
        out.insert(0, default.to_string());
    }
    out.truncate(OPENCODE_MAX_MODELS);
    out
}

/// 自訂供應商的預設模型：原本的預設若仍在清單內就沿用，否則取清單第一個。
fn custom_default_model(cfg: &CustomProviderConfig) -> String {
    let default = cfg.default_model.trim();
    if !default.is_empty() && cfg.models.iter().any(|m| m == default) {
        return default.to_string();
    }
    cfg.models.first().cloned().unwrap_or_default()
}

/// 自訂供應商在 opencode 與介面上顯示的名稱。
fn custom_display_name(cfg: &CustomProviderConfig) -> String {
    let name = cfg.name.trim();
    if name.is_empty() {
        OPENCODE_CUSTOM_NAME_DEFAULT.to_string()
    } else {
        name.to_string()
    }
}

/// opencode server 狀態回傳
#[derive(Debug, Serialize)]
struct OpencodeStateResponse {
    /// 合併健康狀態後的「實際可用」（config.enabled && healthy）。
    enabled: bool,
    /// 設定檔裡的開關原值（AI_PAGE_LAYOUT_V2 新增）：前端表單用它，
    /// 避免 sidecar 暫時離線時把「已啟用」誤存成停用。
    config_enabled: bool,
    healthy: bool,
    allowed: bool,
    can_manage: bool,
    base_url: String,
    /// 預設模型
    model: String,
    models: Vec<String>,
    key_masked: String,
    allowed_accounts: Vec<String>,
    /// 自訂供應商（CUSTOM_PROVIDER_V1）。configured = 有 Base URL 且有金鑰。
    custom: CustomProviderState,
}

/// 自訂供應商的狀態回傳（金鑰只回傳遮罩後的字串）。
#[derive(Debug, Serialize)]
struct CustomProviderState {
    configured: bool,
    name: String,
    base_url: String,
    key_masked: String,
    default_model: String,
    models: Vec<String>,
}

/// 讀取 opencode 設定
fn read_opencode_config() -> OpencodeConfig {
    let path = std::path::Path::new(OPENCODE_CONFIG_FILE);
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(mut cfg) = serde_json::from_str::<OpencodeConfig>(&text) {
            // 舊版只有單一金鑰，那把屬於 NVIDIA：遷移到 provider_keys（仍是加密字串）。
            let legacy = std::mem::take(&mut cfg.legacy_api_key_encrypted);
            if !legacy.is_empty() && !cfg.provider_keys.contains_key(OPENCODE_PROVIDER) {
                cfg.provider_keys.insert(OPENCODE_PROVIDER.to_string(), legacy);
            }
            // 解密每個服務商的金鑰；空字串視為未設定。
            for sealed in cfg.provider_keys.values_mut() {
                let unsealed = crate::secret_encryption::StoreSecret::from_stored_with_purpose(
                    sealed.as_str(),
                    "ai_opencode.api_key",
                );
                *sealed = unsealed.as_str().to_string();
            }
            cfg.provider_keys.retain(|_, key| !key.is_empty());
            cfg.model = normalize_stored_model(&cfg.model);
            cfg.models = effective_models(&cfg);
            cfg.custom.models = effective_custom_models(&cfg.custom);
            cfg.custom.default_model = custom_default_model(&cfg.custom);
            return cfg;
        }
    }
    OpencodeConfig::default()
}

/// 寫入 opencode 設定（加密 API key）
fn write_opencode_config(cfg: &OpencodeConfig) -> Result<(), String> {
    let mut on_disk = cfg.clone();
    for sealed in on_disk.provider_keys.values_mut() {
        *sealed = crate::secret_encryption::StoreSecret::new(std::mem::take(sealed))
            .store_value_with_purpose("ai_opencode.api_key");
    }
    on_disk.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let path = std::path::Path::new(OPENCODE_CONFIG_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建立目錄失敗: {e}"))?;
    }
    let text = serde_json::to_string_pretty(&on_disk)
        .map_err(|e| format!("序列化失敗: {e}"))?;
    crate::atomic_write::write_atomic_with_mode(path, text, 0o600)
        .map_err(|e| format!("寫入失敗: {e}"))
}

/// 同步寫入 opencode-workspace/nvidia.key 與 opencode.json。
///
/// 回傳 `true` 表示 opencode.json 的內容確實改變（例如舊版檔案第一次套用模型清單）。
/// 這種情況下 sidecar 必須重啟才會讀到新設定，呼叫端要自行要求重啟。
/// opencode.json 不存在時什麼都不做（由 ptero-start.sh 的 oc_ensure_config 建立）。
fn sync_opencode_workspace(config: &OpencodeConfig) -> Result<bool, String> {
    let workspace_dir = std::path::Path::new(OPENCODE_WORKSPACE);
    std::fs::create_dir_all(workspace_dir).map_err(|e| format!("建立 workspace 失敗: {e}"))?;

    // 寫入 nvidia.key：NVIDIA 服務商的共用金鑰（所有 NVIDIA 模型都參考這一個檔）。
    // 沒有金鑰時寫空檔，權限一律 0600。
    let provider_key = config
        .provider_keys
        .get(OPENCODE_PROVIDER)
        .cloned()
        .unwrap_or_default();
    let key_path = workspace_dir.join("nvidia.key");
    std::fs::write(&key_path, provider_key)
        .map_err(|e| format!("寫入 nvidia.key 失敗: {e}"))?;
    let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));

    // 自訂供應商金鑰（CUSTOM_PROVIDER_V1）：同樣寫成 0600 檔，opencode.json 用 {file:./custom.key} 參考。
    let custom_key = config
        .provider_keys
        .get(OPENCODE_CUSTOM_PROVIDER)
        .cloned()
        .unwrap_or_default();
    let custom_key_path = workspace_dir.join(OPENCODE_CUSTOM_KEY_FILE);
    std::fs::write(&custom_key_path, custom_key)
        .map_err(|e| format!("寫入 custom.key 失敗: {e}"))?;
    let _ = std::fs::set_permissions(&custom_key_path, std::fs::Permissions::from_mode(0o600));

    let opencode_json_path = workspace_dir.join("opencode.json");
    let text = match std::fs::read_to_string(&opencode_json_path) {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };
    let before: Value = serde_json::from_str(&text)
        .map_err(|e| format!("opencode.json 解析失敗: {e}"))?;
    if !before.is_object() {
        return Err("opencode.json 格式錯誤（不是物件）".to_string());
    }

    let mut oc = before.clone();
    let models_map: serde_json::Map<String, Value> = effective_models(config)
        .into_iter()
        .map(|id| {
            let entry = json!({ "name": id.clone() });
            (id, entry)
        })
        .collect();

    // 頂層預設模型：opencode 格式為 provider/model
    oc["model"] = json!(format!("{}/{}", OPENCODE_PROVIDER, config.model));

    if !oc.get("provider").map_or(false, |p| p.is_object()) {
        oc["provider"] = json!({});
    }
    if !oc["provider"].get(OPENCODE_PROVIDER).map_or(false, |p| p.is_object()) {
        oc["provider"][OPENCODE_PROVIDER] = json!({});
    }
    let nvidia: &mut Value = &mut oc["provider"][OPENCODE_PROVIDER];
    nvidia["models"] = Value::Object(models_map);
    if !nvidia.get("options").map_or(false, |o| o.is_object()) {
        nvidia["options"] = json!({});
    }
    nvidia["options"]["baseURL"] = json!(config.base_url);
    // apiKey 使用 file:./nvidia.key 參考
    nvidia["options"]["apiKey"] = json!("{file:./nvidia.key}");
    // 舊版會把 model 寫進 options（opencode 不認這個欄位），清掉
    if let Some(options) = nvidia["options"].as_object_mut() {
        options.remove("model");
    }

    // 自訂供應商：有 Base URL 才寫入 provider.custom；沒有就移除我們寫過的那一段。
    // npm 採 @ai-sdk/openai-compatible（opencode 文件建議的 OpenAI 相容寫法）。
    if config.custom.base_url.trim().is_empty() {
        if let Some(providers) = oc["provider"].as_object_mut() {
            providers.remove(OPENCODE_CUSTOM_PROVIDER);
        }
    } else {
        let custom_models: serde_json::Map<String, Value> = effective_custom_models(&config.custom)
            .into_iter()
            .map(|id| {
                let entry = json!({ "name": id.clone() });
                (id, entry)
            })
            .collect();
        oc["provider"][OPENCODE_CUSTOM_PROVIDER] = json!({
            "npm": "@ai-sdk/openai-compatible",
            "name": custom_display_name(&config.custom),
            "options": {
                "baseURL": config.custom.base_url.trim(),
                "apiKey": format!("{{file:./{}}}", OPENCODE_CUSTOM_KEY_FILE),
            },
            "models": Value::Object(custom_models),
        });
    }

    if oc == before {
        return Ok(false);
    }
    let updated = serde_json::to_string_pretty(&oc)
        .map_err(|e| format!("序列化 opencode.json 失敗: {e}"))?;
    crate::atomic_write::write_atomic_with_mode(&opencode_json_path, updated, 0o600)
        .map_err(|e| format!("寫入 opencode.json 失敗: {e}"))?;
    Ok(true)
}

/// 同步 workspace；內容有變時要求 watchdog 重啟 sidecar。回傳 true 表示已要求重啟。
fn sync_and_request_restart_if_changed(config: &OpencodeConfig) -> bool {
    match sync_opencode_workspace(config) {
        Ok(true) => {
            request_sidecar_restart();
            true
        }
        Ok(false) => false,
        Err(e) => {
            warn!("同步 opencode workspace 失敗: {}", e);
            false
        }
    }
}

/// 要求 ptero-start.sh 的 watchdog 重啟 sidecar（它會讀取並刪除這個檔案）。
fn request_sidecar_restart() {
    let path = std::path::Path::new(OPENCODE_RESTART_FILE);
    if std::fs::write(path, "").is_ok() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// 讀取 opencode server 密碼
fn read_opencode_password() -> Result<String, String> {
    std::fs::read_to_string(OPENCODE_SERVER_PASS_FILE)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("讀取密碼失敗: {e}"))
}

/// 檢查 opencode server 健康狀態
async fn check_opencode_health(password: &str) -> bool {
    let client = Client::builder()
        .timeout(Duration::from_secs(OPENCODE_HEALTH_TIMEOUT_SECS))
        .build()
        .ok();

    let Some(client) = client else { return false };

    let auth = format!("Basic {}", BASE64.encode(format!("opencode:{}", password)));
    client
        .get(format!("http://127.0.0.1:{}/global/health", OPENCODE_DEFAULT_PORT))
        .header("Authorization", auth)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// 呼叫 sidecar。回傳（HTTP 狀態碼, JSON 內容）；連線或密碼問題回傳錯誤字串。
async fn sidecar_call(
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
    timeout_secs: u64,
) -> Result<(u16, Value), String> {
    let password = read_opencode_password()?;
    let client = Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| format!("建立 HTTP client 失敗: {e}"))?;
    let auth = format!("Basic {}", BASE64.encode(format!("opencode:{}", password)));
    let url = format!("http://127.0.0.1:{}{}", OPENCODE_DEFAULT_PORT, path);
    let mut req = client.request(method, url).header("Authorization", auth);
    if let Some(payload) = body {
        req = req.json(&payload);
    }
    let resp = req.send().await.map_err(|e| {
        error!("opencode 請求失敗: {}", e);
        "無法連線到 opencode server".to_string()
    })?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    Ok((status, value))
}

/// 把 sidecar 的錯誤轉成給使用者看的文字。
fn sidecar_error_text(status: u16, value: &Value) -> String {
    let detail = value
        .pointer("/data/message")
        .or_else(|| value.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if detail.is_empty() {
        format!("opencode 回應錯誤: {}", status)
    } else {
        format!("opencode 回應錯誤: {}（{}）", status, detail)
    }
}

/// 遮蔽 API key 顯示
fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        "••••••••".to_string()
    } else {
        format!("{}••••{}", &key[..4], &key[key.len()-4..])
    }
}

fn json_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": code, "message": message.into() })),
    )
        .into_response()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// 呼叫端身分（由 session token 取得）。
struct Caller {
    account_id: String,
    is_admin: bool,
}

async fn authorize_caller(
    state: &AdminWebState,
    headers: &HeaderMap,
    body_token: &str,
) -> Result<Caller, Response> {
    let token = session_token(headers, body_token);
    let Some(session) = state.authorize_any(&token).await else {
        return Err(unauthorized_response().into_response());
    };
    Ok(Caller {
        account_id: session.account_id.clone(),
        is_admin: session.is_admin,
    })
}

/// 管理員或列在授權名單的帳號才能使用 opencode。
fn ensure_allowed(caller: &Caller, config: &OpencodeConfig) -> Result<(), Response> {
    if caller.is_admin || config.allowed_accounts.contains(&caller.account_id) {
        return Ok(());
    }
    Err(json_error(
        StatusCode::FORBIDDEN,
        "access_denied",
        "此帳號未被授權使用 opencode",
    ))
}

fn ensure_admin(caller: &Caller) -> Result<(), Response> {
    if caller.is_admin {
        Ok(())
    } else {
        Err(unauthorized_response().into_response())
    }
}

/// GET /admin/api/ai/opencode/state
pub(super) async fn state_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> impl IntoResponse {
    let caller = match authorize_caller(&state, &headers, &query.token).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    let can_manage = caller.is_admin;
    let config = read_opencode_config();

    // 管理員載入頁面時順便把舊版 opencode.json 補齊（內容有變才會要求重啟）
    if can_manage {
        let _ = sync_and_request_restart_if_changed(&config);
    }

    // 檢查帳號是否在 allowlist
    let allowed = config.allowed_accounts.contains(&caller.account_id) || caller.is_admin;

    // 檢查 server 健康狀態
    let healthy = if let Ok(pw) = read_opencode_password() {
        check_opencode_health(&pw).await
    } else {
        false
    };

    let key_masked = match config.provider_keys.get(OPENCODE_PROVIDER) {
        Some(key) if !key.is_empty() => mask_key(key),
        _ => String::new(),
    };

    let custom_key = config
        .provider_keys
        .get(OPENCODE_CUSTOM_PROVIDER)
        .cloned()
        .unwrap_or_default();
    let custom_state = CustomProviderState {
        configured: !config.custom.base_url.trim().is_empty() && !custom_key.trim().is_empty(),
        name: custom_display_name(&config.custom),
        base_url: config.custom.base_url.clone(),
        key_masked: if custom_key.is_empty() {
            String::new()
        } else {
            mask_key(&custom_key)
        },
        default_model: config.custom.default_model.clone(),
        models: config.custom.models.clone(),
    };

    (
        StatusCode::OK,
        Json(json!(OpencodeStateResponse {
            enabled: config.enabled && healthy,
            config_enabled: config.enabled,
            healthy,
            allowed,
            can_manage,
            base_url: config.base_url,
            model: config.model,
            models: config.models,
            key_masked,
            allowed_accounts: config.allowed_accounts,
            custom: custom_state,
        })),
    )
        .into_response()
}

/// POST /admin/api/ai/opencode/config（僅管理員）
#[derive(Debug, Deserialize)]
pub(super) struct OpencodeConfigRequest {
    #[serde(default)]
    token: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    api_key: String,
}

pub(super) async fn config_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<OpencodeConfigRequest>,
) -> impl IntoResponse {
    let caller = match authorize_caller(&state, &headers, &body.token).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    if let Err(resp) = ensure_admin(&caller) {
        return resp;
    }

    let mut config = read_opencode_config();
    config.enabled = body.enabled;
    if !body.base_url.trim().is_empty() {
        config.base_url = body.base_url.trim().to_string();
    }
    if !body.model.trim().is_empty() {
        // 舊版前端仍可能送 model；新版模型請用 /models 管理
        match validate_model_id(&body.model) {
            Ok(id) => {
                if !config.models.contains(&id) {
                    config.models.push(id.clone());
                }
                config.model = id;
            }
            Err(message) => return json_error(StatusCode::BAD_REQUEST, "invalid_model", message),
        }
    }
    if !body.api_key.trim().is_empty() {
        config
            .provider_keys
            .insert(OPENCODE_PROVIDER.to_string(), body.api_key.trim().to_string());
    }

    if let Err(e) = write_opencode_config(&config) {
        error!("寫入 opencode 設定失敗: {}", e);
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "config_write_failed", e);
    }

    // 同步到 workspace
    if let Err(e) = sync_opencode_workspace(&config) {
        warn!("同步 opencode workspace 失敗: {}", e);
    }

    // 觸發 sidecar 重啟（watchdog 會偵測到並重啟）
    request_sidecar_restart();

    info!("opencode 設定已更新");
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "message": "設定已儲存，sidecar 將自動重啟" })),
    )
        .into_response()
}

/// POST /admin/api/ai/opencode/custom（僅管理員）：儲存自訂 OpenAI 相容供應商。
/// base_url 留空代表停用（模型清單保留）；api_key 留空代表不更動（UI 目前不提供清除金鑰）。
#[derive(Debug, Deserialize)]
pub(super) struct OpencodeCustomRequest {
    #[serde(default)]
    token: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
}

pub(super) async fn custom_provider_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<OpencodeCustomRequest>,
) -> impl IntoResponse {
    let caller = match authorize_caller(&state, &headers, &body.token).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    if let Err(resp) = ensure_admin(&caller) {
        return resp;
    }

    let mut config = read_opencode_config();
    config.custom.name = body.name.trim().chars().take(OPENCODE_CUSTOM_NAME_MAX).collect();
    if body.base_url.trim().is_empty() {
        config.custom.base_url = String::new();
    } else {
        match normalize_custom_base_url(&body.base_url) {
            Ok(url) => config.custom.base_url = url,
            Err(message) => return json_error(StatusCode::BAD_REQUEST, "invalid_base_url", message),
        }
    }
    if !body.api_key.trim().is_empty() {
        config
            .provider_keys
            .insert(OPENCODE_CUSTOM_PROVIDER.to_string(), body.api_key.trim().to_string());
    }

    if let Err(e) = write_opencode_config(&config) {
        error!("寫入自訂供應商設定失敗: {}", e);
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "config_write_failed", e);
    }
    if let Err(e) = sync_opencode_workspace(&config) {
        warn!("同步 opencode workspace 失敗: {}", e);
    }
    request_sidecar_restart();

    info!("opencode 自訂供應商已更新");
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "message": "自訂供應商已儲存，sidecar 將自動重啟" })),
    )
        .into_response()
}

/// POST /admin/api/ai/opencode/access（僅管理員）
#[derive(Debug, Deserialize)]
pub(super) struct OpencodeAccessRequest {
    #[serde(default)]
    token: String,
    #[serde(default)]
    allowed_accounts: Vec<String>,
}

pub(super) async fn access_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<OpencodeAccessRequest>,
) -> impl IntoResponse {
    let caller = match authorize_caller(&state, &headers, &body.token).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    if let Err(resp) = ensure_admin(&caller) {
        return resp;
    }

    let mut config = read_opencode_config();
    config.allowed_accounts = body.allowed_accounts;

    if let Err(e) = write_opencode_config(&config) {
        error!("寫入 opencode access 失敗: {}", e);
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "config_write_failed", e);
    }

    info!("opencode allowlist 已更新: {:?}", config.allowed_accounts);
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "allowed_accounts": config.allowed_accounts })),
    )
        .into_response()
}

/// POST /admin/api/ai/opencode/models（僅管理員）
/// action: "add" | "remove" | "default"；provider: "nvidia"（預設）| "custom"
#[derive(Debug, Deserialize)]
pub(super) struct OpencodeModelRequest {
    #[serde(default)]
    token: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    provider: String,
}

pub(super) async fn models_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<OpencodeModelRequest>,
) -> impl IntoResponse {
    let caller = match authorize_caller(&state, &headers, &body.token).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    if let Err(resp) = ensure_admin(&caller) {
        return resp;
    }

    let provider = body.provider.trim();
    let custom = provider == OPENCODE_CUSTOM_PROVIDER;
    if !custom && !provider.is_empty() && provider != OPENCODE_PROVIDER {
        return json_error(StatusCode::BAD_REQUEST, "unknown_provider", "不支援的供應商");
    }

    let mut config = read_opencode_config();
    let model_id = body.model.trim().to_string();
    let validate: fn(&str) -> Result<String, String> = if custom {
        validate_custom_model_id
    } else {
        validate_model_id
    };
    // 依供應商取得要操作的清單與預設值（兩者都是 config 內不重疊的欄位）
    let (list, default) = if custom {
        (&mut config.custom.models, &mut config.custom.default_model)
    } else {
        (&mut config.models, &mut config.model)
    };

    match body.action.as_str() {
        "add" => {
            let id = match validate(&body.model) {
                Ok(v) => v,
                Err(message) => return json_error(StatusCode::BAD_REQUEST, "invalid_model", message),
            };
            if !list.contains(&id) {
                if list.len() >= OPENCODE_MAX_MODELS {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "too_many_models",
                        format!("模型清單最多 {} 個", OPENCODE_MAX_MODELS),
                    );
                }
                list.push(id);
            }
        }
        "remove" => {
            if model_id == *default {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "cannot_remove_default",
                    "不能刪除預設模型，請先把其他模型設為預設",
                );
            }
            let before = list.len();
            list.retain(|m| m != &model_id);
            if list.len() == before {
                return json_error(StatusCode::NOT_FOUND, "model_not_found", "清單中沒有這個模型");
            }
        }
        "default" => {
            if !list.iter().any(|m| m == &model_id) {
                return json_error(StatusCode::BAD_REQUEST, "model_not_found", "請先把模型加入清單");
            }
            *default = model_id.clone();
        }
        _ => return json_error(StatusCode::BAD_REQUEST, "unknown_action", "不支援的操作"),
    }

    if custom {
        let models = effective_custom_models(&config.custom);
        config.custom.models = models;
        config.custom.default_model = custom_default_model(&config.custom);
    } else {
        config.models = effective_models(&config);
    }
    let (models_out, default_out) = if custom {
        (config.custom.models.clone(), config.custom.default_model.clone())
    } else {
        (config.models.clone(), config.model.clone())
    };

    if let Err(e) = write_opencode_config(&config) {
        error!("寫入 opencode 模型清單失敗: {}", e);
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "config_write_failed", e);
    }
    if let Err(e) = sync_opencode_workspace(&config) {
        warn!("同步 opencode workspace 失敗: {}", e);
    }
    request_sidecar_restart();

    info!("opencode 模型清單已更新: action={} model={}", body.action, model_id);
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "provider": if custom { OPENCODE_CUSTOM_PROVIDER } else { OPENCODE_PROVIDER },
            "models": models_out,
            "model": default_out,
            "message": "模型清單已儲存，sidecar 將自動重啟（約 1 分鐘）",
        })),
    )
        .into_response()
}

/// 一筆對話對應：dashboard 的 thread_id 屬於哪個帳號、對應到 sidecar 的哪個 session。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SidecarSession {
    owner: String,
    thread_id: String,
    session_id: String,
    title: String,
    updated: u64,
}

/// 複合鍵：帳號 + thread_id。不同帳號的 thread_id 互不相通。
/// 帳號 id 不會含換行，所以用換行當分隔符不會產生歧義。
fn session_key(owner: &str, thread_id: &str) -> String {
    format!("{}\n{}", owner, thread_id)
}

/// 讀取對話清單檔。第一次存取時才執行（LazyLock）。檔案不存在或損毀時從空清單開始。
fn load_sessions() -> HashMap<String, SidecarSession> {
    let path = std::path::Path::new(OPENCODE_SESSIONS_FILE);
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, SidecarSession>>(&text) {
        Ok(map) => map,
        Err(e) => {
            warn!("opencode 對話清單檔損毀，改用空清單: {}", e);
            HashMap::new()
        }
    }
}

/// 把整份對話清單原子寫入磁碟（0600）。失敗只記 log，不影響對話本身。
fn save_sessions(map: &HashMap<String, SidecarSession>) {
    let path = std::path::Path::new(OPENCODE_SESSIONS_FILE);
    let result = (|| -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("建立目錄失敗: {e}"))?;
        }
        let text = serde_json::to_string_pretty(map).map_err(|e| format!("序列化失敗: {e}"))?;
        crate::atomic_write::write_atomic_with_mode(path, text, 0o600)
            .map_err(|e| format!("寫入失敗: {e}"))
    })();
    if let Err(e) = result {
        warn!("儲存 opencode 對話清單失敗: {}", e);
    }
}

/// 超過上限時移除 updated 最舊的項目。
fn prune_sessions(map: &mut HashMap<String, SidecarSession>) {
    while map.len() > OPENCODE_SESSIONS_MAX {
        let oldest = map
            .iter()
            .min_by_key(|(_, entry)| entry.updated)
            .map(|(key, _)| key.clone());
        match oldest {
            Some(key) => {
                map.remove(&key);
            }
            None => break,
        }
    }
}

static OPENCODE_SESSIONS: LazyLock<Mutex<HashMap<String, SidecarSession>>> =
    LazyLock::new(|| Mutex::new(load_sessions()));

/// 修改對話清單：套用變更、修剪上限、立即落盤。持鎖期間寫檔（檔案很小）。
async fn with_sessions_saved<T>(apply: impl FnOnce(&mut HashMap<String, SidecarSession>) -> T) -> T {
    let mut sessions = OPENCODE_SESSIONS.lock().await;
    let out = apply(&mut *sessions);
    prune_sessions(&mut *sessions);
    save_sessions(&*sessions);
    out
}

/// 只有建立者可以存取該對話；回傳 sidecar 的 session id。
async fn owned_session_id(thread_id: &str, account_id: &str) -> Option<String> {
    let sessions = OPENCODE_SESSIONS.lock().await;
    let key = session_key(account_id, thread_id);
    let found = sessions.get(&key).map(|entry| entry.session_id.clone());
    found
}

async fn forget_session(owner: &str, thread_id: &str) {
    let key = session_key(owner, thread_id);
    with_sessions_saved(|m| {
        m.remove(&key);
    })
    .await;
}

async fn touch_session(owner: &str, thread_id: &str) {
    let key = session_key(owner, thread_id);
    with_sessions_saved(|m| {
        if let Some(entry) = m.get_mut(&key) {
            entry.updated = now_secs();
        }
    })
    .await;
}

fn title_for(text: &str) -> String {
    let clean: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.is_empty() {
        return "新對話".to_string();
    }
    if clean.chars().count() > OPENCODE_TITLE_MAX_CHARS {
        let cut: String = clean.chars().take(OPENCODE_TITLE_MAX_CHARS).collect();
        format!("{}…", cut)
    } else {
        clean
    }
}

/// 取出一則 sidecar 訊息（{ info, parts }）裡所有文字片段。
fn extract_text(value: &Value) -> String {
    value
        .get("parts")
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// assistant 訊息若帶有 info.error，回傳錯誤文字。
fn assistant_error_text(value: &Value) -> Option<String> {
    let err = value.pointer("/info/error")?;
    if err.is_null() {
        return None;
    }
    let detail = err
        .pointer("/data/message")
        .and_then(|v| v.as_str())
        .or_else(|| err.get("name").and_then(|v| v.as_str()))
        .unwrap_or("模型回應失敗");
    Some(detail.to_string())
}

/// sidecar 的歷史訊息 → [{ role, content }]，只留有文字的項目。
fn history_from_value(value: &Value) -> Vec<Value> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let role = item
                        .pointer("/info/role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("assistant");
                    let text = extract_text(item);
                    if text.trim().is_empty() {
                        None
                    } else {
                        Some(json!({ "role": role, "content": text }))
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 必要時先在 sidecar 建立 session，並記下 thread → session 的對應。
async fn ensure_sidecar_session(
    thread_id: &str,
    account_id: &str,
    title_hint: &str,
) -> Result<String, String> {
    if let Some(existing) = owned_session_id(thread_id, account_id).await {
        return Ok(existing);
    }

    let title = title_for(title_hint);
    let (status, value) = sidecar_call(
        reqwest::Method::POST,
        "/session",
        Some(json!({ "title": title })),
        OPENCODE_API_TIMEOUT_SECS,
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(sidecar_error_text(status, &value));
    }
    let session_id = value
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "opencode 沒有回傳 session id".to_string())?
        .to_string();

    let key = session_key(account_id, thread_id);
    let owner = account_id.to_string();
    let thread = thread_id.to_string();
    let sid = session_id.clone();
    with_sessions_saved(move |m| {
        m.insert(
            key,
            SidecarSession {
                owner,
                thread_id: thread,
                session_id: sid,
                title,
                updated: now_secs(),
            },
        );
    })
    .await;
    Ok(session_id)
}

/// 聊天請求（OPENCODE_AGENT_ONLY_V2：所有訊息都走這裡）
#[derive(Debug, Deserialize)]
pub(super) struct OpencodeChatRequest {
    #[serde(default)]
    token: String,
    message: String,
    #[serde(default)]
    thread_id: String,
    /// 本則訊息使用的模型；空字串代表預設模型。必須在清單內。
    #[serde(default)]
    model: String,
    /// 供應商："nvidia"（預設）或 "custom"。
    #[serde(default)]
    provider: String,
}

/// 決定這則訊息用哪個供應商與模型，回傳 (opencode providerID, modelID)。
fn resolve_chat_model(
    config: &OpencodeConfig,
    provider: &str,
    requested: &str,
) -> Result<(String, String), Response> {
    let provider = provider.trim();
    let requested = requested.trim();
    if provider.is_empty() || provider == OPENCODE_PROVIDER {
        let model = if requested.is_empty() {
            config.model.clone()
        } else if config.models.iter().any(|m| m == requested) {
            requested.to_string()
        } else {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "model_not_available",
                "此模型不在清單中，請重新選擇",
            ));
        };
        return Ok((OPENCODE_PROVIDER.to_string(), model));
    }
    if provider == OPENCODE_CUSTOM_PROVIDER {
        if config.custom.base_url.trim().is_empty() {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "custom_not_configured",
                "自訂供應商尚未設定 Base URL",
            ));
        }
        let has_key = config
            .provider_keys
            .get(OPENCODE_CUSTOM_PROVIDER)
            .map_or(false, |k| !k.trim().is_empty());
        if !has_key {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "custom_key_missing",
                "自訂供應商尚未設定 API Key",
            ));
        }
        let model = if requested.is_empty() {
            config.custom.default_model.clone()
        } else if config.custom.models.iter().any(|m| m == requested) {
            requested.to_string()
        } else {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "model_not_available",
                "此模型不在清單中，請重新選擇",
            ));
        };
        if model.is_empty() {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "model_not_available",
                "自訂供應商還沒有模型，請先到「模型」分頁新增",
            ));
        }
        return Ok((OPENCODE_CUSTOM_PROVIDER.to_string(), model));
    }
    Err(json_error(StatusCode::BAD_REQUEST, "unknown_provider", "不支援的供應商"))
}

/// POST /admin/api/ai/opencode/chat
pub(super) async fn chat_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<OpencodeChatRequest>,
) -> impl IntoResponse {
    let caller = match authorize_caller(&state, &headers, &body.token).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    let config = read_opencode_config();
    if let Err(resp) = ensure_allowed(&caller, &config) {
        return resp;
    }

    if !config.enabled {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "disabled", "opencode 功能未啟用");
    }

    let message = body.message.trim().to_string();
    if message.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "empty_message", "訊息不可為空");
    }
    if message.chars().count() > OPENCODE_MESSAGE_MAX_CHARS {
        return json_error(StatusCode::BAD_REQUEST, "message_too_long", "訊息過長");
    }

    let (provider_id, model) = match resolve_chat_model(&config, &body.provider, &body.model) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // 舊版設定第一次使用時會補寫 opencode.json，需要先重啟 sidecar
    if sync_and_request_restart_if_changed(&config) {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "sidecar_restarting",
            "opencode 設定已更新，sidecar 重啟中，請約 1 分鐘後再試",
        );
    }

    let thread_id = if body.thread_id.trim().is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        body.thread_id.trim().to_string()
    };

    // CHAT_JOB_V1：模型推理加工具呼叫可能要好幾分鐘，Cloudflare Tunnel 會在約 100 秒後
    // 回 524／502，所以這裡只建立背景 job 並立刻回 202，前端用 chat/status 輪詢結果。
    let job_id = uuid::Uuid::new_v4().to_string();
    opencode_job_insert(
        &job_id,
        OpencodeJob {
            owner: caller.account_id.clone(),
            thread_id: thread_id.clone(),
            model: model.clone(),
            done: false,
            ok: false,
            text: String::new(),
            partial: String::new(),
            error: String::new(),
            code: String::new(),
            updated: now_secs(),
        },
    );
    let (job_for_task, owner, thread_for_task) = (job_id.clone(), caller.account_id.clone(), thread_id.clone());
    tokio::spawn(async move {
        run_chat_job(job_for_task, owner, thread_for_task, message, provider_id, model).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": job_id,
            "thread_id": thread_id,
            "state": "running",
            "poll_path": "/admin/api/ai/opencode/chat/status",
            "poll_ms": OPENCODE_CHAT_POLL_MS,
        })),
    )
        .into_response()
}

/// 背景執行一則對話：建立（或沿用）sidecar session、送出訊息，結果寫回 job。
async fn run_chat_job(
    job_id: String,
    owner: String,
    thread_id: String,
    message: String,
    provider_id: String,
    model: String,
) {
    let outcome = chat_once(&job_id, &owner, &thread_id, &message, &provider_id, &model).await;
    opencode_job_update(&job_id, |job| match outcome {
        Ok(text) => {
            job.done = true;
            job.ok = true;
            job.text = text;
        }
        Err((code, detail)) => {
            job.done = true;
            job.ok = false;
            job.code = code.to_string();
            job.error = detail;
        }
    });
}

/// 單次對話的實際工作。錯誤回傳 (代碼, 給使用者看的文字)。
async fn chat_once(
    job_id: &str,
    owner: &str,
    thread_id: &str,
    message: &str,
    provider_id: &str,
    model: &str,
) -> Result<String, (&'static str, String)> {
    let session_id = ensure_sidecar_session(thread_id, owner, message)
        .await
        .map_err(|e| ("sidecar_unavailable", e))?;

    let payload = json!({
        "model": { "providerID": provider_id, "modelID": model },
        "parts": [ { "type": "text", "text": message } ],
    });
    let path = format!("/session/{}/message", session_id);

    // OPENCODE_STREAM_V1：這個 POST 要等整則回覆完成才會回傳。期間由 watcher 每秒讀一次
    // sidecar 的訊息列表，把進行中的回覆文字寫進 job.partial，前端就能邊生成邊顯示。
    let watcher = spawn_progress_watcher(job_id.to_string(), session_id.clone(), message.to_string());
    let posted = sidecar_call(
        reqwest::Method::POST,
        &path,
        Some(payload),
        OPENCODE_CHAT_TIMEOUT_SECS,
    )
    .await;
    // 不可以用 ? 提前返回：一定要先停掉 watcher，否則它會一直輪詢下去。
    watcher.abort();
    let (status, value) = posted.map_err(|e| ("sidecar_unavailable", e))?;

    if status == 404 {
        // sidecar 重啟後 session 可能已不存在：丟掉對應，讓下次自動重建
        forget_session(owner, thread_id).await;
        return Err(("session_gone", "對話已失效，請重新送出".to_string()));
    }
    if !(200..300).contains(&status) {
        return Err(("sidecar_error", sidecar_error_text(status, &value)));
    }
    if let Some(detail) = assistant_error_text(&value) {
        return Err(("model_error", detail));
    }

    touch_session(owner, thread_id).await;
    Ok(extract_text(&value))
}

/// 進行中回覆的增量讀取（OPENCODE_STREAM_V1）。每 OPENCODE_PROGRESS_POLL_MS 讀一次 sidecar 的
/// 訊息列表，文字有變化才寫入 job.partial。讀取失敗就略過，不影響主要的 POST；
/// 由呼叫端 abort 停止。
fn spawn_progress_watcher(job_id: String, session_id: String, message: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let path = format!("/session/{}/message", session_id);
        let mut last = String::new();
        loop {
            tokio::time::sleep(Duration::from_millis(OPENCODE_PROGRESS_POLL_MS)).await;
            let Ok((status, value)) = sidecar_call(reqwest::Method::GET, &path, None, OPENCODE_API_TIMEOUT_SECS).await else {
                continue;
            };
            if !(200..300).contains(&status) {
                continue;
            }
            let Some(text) = in_progress_reply_text(&value, &message) else {
                continue;
            };
            if text.is_empty() || text == last {
                continue;
            }
            opencode_job_update(&job_id, |job| {
                if !job.done {
                    job.partial = text.clone();
                }
            });
            last = text;
        }
    })
}

/// 這次送出的回覆目前的文字：找到文字相同的最後一則使用者訊息（就是剛送出的那則），
/// 取它之後最後一則 assistant 訊息。找不到使用者訊息時回傳 None，避免把上一輪的舊回覆當成進度。
fn in_progress_reply_text(value: &Value, message: &str) -> Option<String> {
    let items = value.as_array()?;
    let user_index = items.iter().rposition(|item| {
        item.pointer("/info/role").and_then(|r| r.as_str()) == Some("user")
            && extract_text(item) == message
    })?;
    items[user_index + 1..]
        .iter()
        .rev()
        .find(|item| item.pointer("/info/role").and_then(|r| r.as_str()) == Some("assistant"))
        .map(extract_text)
}

/// GET /admin/api/ai/opencode/chat/status?job=…：回傳背景對話的目前狀態。只能看自己的 job。
#[derive(Debug, Deserialize)]
pub(super) struct ChatStatusQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    job: String,
}

pub(super) async fn chat_status_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<ChatStatusQuery>,
) -> Response {
    let caller = match authorize_caller(&state, &headers, &query.token).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let job_id = query.job.trim();
    if job_id.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "job_missing", "缺少 job 參數");
    }
    let snapshot = OPENCODE_JOBS
        .lock()
        .ok()
        .and_then(|map| map.get(job_id).cloned());
    let job = match snapshot {
        Some(job) if job.owner == caller.account_id => job,
        _ => {
            return json_error(
                StatusCode::NOT_FOUND,
                "job_not_found",
                "找不到這個回覆（可能已過期）",
            )
        }
    };
    let state_name = if !job.done {
        "running"
    } else if job.ok {
        "done"
    } else {
        "failed"
    };
    (
        StatusCode::OK,
        Json(json!({
            "state": state_name,
            "done": job.done,
            "ok": job.ok,
            "thread_id": job.thread_id,
            "model": job.model,
            "text": job.text,
            "partial": job.partial,
            "error": job.error,
            "code": job.code,
        })),
    )
        .into_response()
}

/// 背景對話 job 的狀態表（記憶體中；重啟後會消失，前端會收到「找不到」）。
#[derive(Clone)]
struct OpencodeJob {
    owner: String,
    thread_id: String,
    model: String,
    done: bool,
    ok: bool,
    text: String,
    /// 進行中的回覆文字（增量；完成後以 text 為準）。
    partial: String,
    error: String,
    code: String,
    updated: u64,
}

static OPENCODE_JOBS: LazyLock<std::sync::Mutex<HashMap<String, OpencodeJob>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn opencode_job_insert(job_id: &str, job: OpencodeJob) {
    let Ok(mut map) = OPENCODE_JOBS.lock() else {
        return;
    };
    let now = now_secs();
    map.retain(|_, j| now.saturating_sub(j.updated) <= OPENCODE_JOB_TTL_SECS);
    while map.len() >= OPENCODE_JOB_MAX {
        let oldest = map
            .iter()
            .min_by_key(|(_, j)| j.updated)
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

fn opencode_job_update(job_id: &str, apply: impl FnOnce(&mut OpencodeJob)) {
    let Ok(mut map) = OPENCODE_JOBS.lock() else {
        return;
    };
    if let Some(job) = map.get_mut(job_id) {
        apply(job);
        job.updated = now_secs();
    }
}

/// 對話操作（中止、列出、歷史、改名、刪除）共用的請求格式
#[derive(Debug, Deserialize)]
pub(super) struct OpencodeThreadRequest {
    #[serde(default)]
    token: String,
    #[serde(default)]
    thread_id: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    title: String,
}

/// POST /admin/api/ai/opencode/abort：中止正在執行的回覆
pub(super) async fn abort_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<OpencodeThreadRequest>,
) -> impl IntoResponse {
    let caller = match authorize_caller(&state, &headers, &body.token).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let config = read_opencode_config();
    if let Err(resp) = ensure_allowed(&caller, &config) {
        return resp;
    }

    let Some(session_id) = owned_session_id(body.thread_id.trim(), &caller.account_id).await else {
        return json_error(StatusCode::NOT_FOUND, "session_not_found", "找不到這個對話");
    };
    let path = format!("/session/{}/abort", session_id);
    match sidecar_call(reqwest::Method::POST, &path, None, OPENCODE_API_TIMEOUT_SECS).await {
        Ok((status, value)) if (200..300).contains(&status) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "stopped": value.as_bool().unwrap_or(true) })),
        )
            .into_response(),
        Ok((status, value)) => json_error(StatusCode::BAD_GATEWAY, "sidecar_error", sidecar_error_text(status, &value)),
        Err(e) => json_error(StatusCode::BAD_GATEWAY, "sidecar_unavailable", e),
    }
}

/// GET /admin/api/ai/opencode/sessions：目前帳號的對話清單（新到舊）
pub(super) async fn sessions_list_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> impl IntoResponse {
    let caller = match authorize_caller(&state, &headers, &query.token).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let config = read_opencode_config();
    if let Err(resp) = ensure_allowed(&caller, &config) {
        return resp;
    }

    let sessions = OPENCODE_SESSIONS.lock().await;
    let mut list: Vec<Value> = sessions
        .iter()
        .filter(|(_, entry)| entry.owner == caller.account_id)
        .map(|(_, entry)| json!({ "id": entry.thread_id.clone(), "title": entry.title.clone(), "updated": entry.updated }))
        .collect();
    list.sort_by(|a, b| b["updated"].as_u64().cmp(&a["updated"].as_u64()));
    (StatusCode::OK, Json(json!({ "sessions": list }))).into_response()
}

/// POST /admin/api/ai/opencode/sessions：action = messages | rename | delete
pub(super) async fn sessions_action_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<OpencodeThreadRequest>,
) -> impl IntoResponse {
    let caller = match authorize_caller(&state, &headers, &body.token).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let config = read_opencode_config();
    if let Err(resp) = ensure_allowed(&caller, &config) {
        return resp;
    }

    let thread_id = body.thread_id.trim().to_string();
    let Some(session_id) = owned_session_id(&thread_id, &caller.account_id).await else {
        return json_error(StatusCode::NOT_FOUND, "session_not_found", "找不到這個對話");
    };
    let path = format!("/session/{}", session_id);

    match body.action.as_str() {
        "messages" => {
            let messages_path = format!("/session/{}/message", session_id);
            match sidecar_call(reqwest::Method::GET, &messages_path, None, OPENCODE_API_TIMEOUT_SECS).await {
                Ok((status, value)) if (200..300).contains(&status) => (
                    StatusCode::OK,
                    Json(json!({ "ok": true, "messages": history_from_value(&value) })),
                )
                    .into_response(),
                Ok((status, value)) => json_error(StatusCode::BAD_GATEWAY, "sidecar_error", sidecar_error_text(status, &value)),
                Err(e) => json_error(StatusCode::BAD_GATEWAY, "sidecar_unavailable", e),
            }
        }
        "rename" => {
            let title = body.title.trim().to_string();
            if title.is_empty() || title.chars().count() > 80 {
                return json_error(StatusCode::BAD_REQUEST, "invalid_title", "標題長度需為 1–80 字");
            }
            match sidecar_call(reqwest::Method::PATCH, &path, Some(json!({ "title": title.clone() })), OPENCODE_API_TIMEOUT_SECS).await {
                Ok((status, _)) if (200..300).contains(&status) => {
                    let key = session_key(&caller.account_id, &thread_id);
                    let new_title = title.clone();
                    with_sessions_saved(move |m| {
                        if let Some(entry) = m.get_mut(&key) {
                            entry.title = new_title;
                        }
                    })
                    .await;
                    (StatusCode::OK, Json(json!({ "ok": true, "title": title }))).into_response()
                }
                Ok((status, value)) => json_error(StatusCode::BAD_GATEWAY, "sidecar_error", sidecar_error_text(status, &value)),
                Err(e) => json_error(StatusCode::BAD_GATEWAY, "sidecar_unavailable", e),
            }
        }
        "delete" => {
            match sidecar_call(reqwest::Method::DELETE, &path, None, OPENCODE_API_TIMEOUT_SECS).await {
                // 404 代表 sidecar 已經沒有這個 session，本地對應一併移除即可
                Ok((status, _)) if (200..300).contains(&status) || status == 404 => {
                    forget_session(&caller.account_id, &thread_id).await;
                    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
                }
                Ok((status, value)) => json_error(StatusCode::BAD_GATEWAY, "sidecar_error", sidecar_error_text(status, &value)),
                Err(e) => json_error(StatusCode::BAD_GATEWAY, "sidecar_unavailable", e),
            }
        }
        _ => json_error(StatusCode::BAD_REQUEST, "unknown_action", "不支援的操作"),
    }
}

impl Default for OpencodeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_base_url(),
            model: default_model(),
            provider_keys: BTreeMap::new(),
            legacy_api_key_encrypted: String::new(),
            allowed_accounts: Vec::new(),
            models: vec![default_model()],
            custom: CustomProviderConfig::default(),
            updated_at: 0,
        }
    }
}

/// 公告摘要是否可用：opencode 設定裡有 NVIDIA 金鑰。
/// 摘要共用這把金鑰，不需要另外設定。
pub(crate) fn summary_key_configured() -> bool {
    read_opencode_config()
        .provider_keys
        .get(OPENCODE_PROVIDER)
        .map_or(false, |key| !key.trim().is_empty())
}

/// 摘要專用的 HTTP client（共用，避免每次重建 TLS）。
static SUMMARY_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .timeout(Duration::from_secs(OPENCODE_SUMMARY_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| Client::new())
});

/// 上游錯誤訊息：優先取 JSON 裡的 message，取不到就用原文，並截短。
fn summary_error_message(body: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        v.pointer("/error/message")
            .or_else(|| v.pointer("/detail"))
            .or_else(|| v.pointer("/message"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
    });
    let raw = parsed.unwrap_or_else(|| body.trim().to_string());
    let cleaned: String = raw.replace(['\n', '\r'], " ").chars().take(300).collect();
    cleaned.trim().to_string()
}

/// 公告摘要：直接呼叫 NVIDIA 的 OpenAI 相容 `/chat/completions`（不經過 sidecar），
/// 使用 opencode 設定的 base_url、NVIDIA 金鑰與預設模型。回傳 (摘要文字, 模型 id)。
pub(crate) async fn summary_complete(
    messages: Vec<Value>,
    max_tokens: u32,
) -> Result<(String, String), String> {
    let config = read_opencode_config();
    let key = config
        .provider_keys
        .get(OPENCODE_PROVIDER)
        .map(|k| k.trim().to_string())
        .unwrap_or_default();
    if key.is_empty() {
        return Err("尚未設定 NVIDIA 金鑰（請到 AI 頁面的 opencode 分頁填入）".to_string());
    }
    let base = config.base_url.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return Err("尚未設定 Base URL".to_string());
    }
    let model = config.model.clone();
    let payload = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": 0.2,
        "stream": false,
    });
    let response = SUMMARY_CLIENT
        .post(format!("{base}/chat/completions"))
        .bearer_auth(&key)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("無法連線到 NVIDIA：{e}"))?;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}：{}", summary_error_message(&body)));
    }
    let value: Value = serde_json::from_str(&body).map_err(|e| format!("回應不是 JSON：{e}"))?;
    // 撞到 max_tokens 才停筆的摘要是殘缺的，不當成正常結果。
    let finish_reason = value
        .pointer("/choices/0/finish_reason")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if finish_reason == "length" {
        return Err("摘要在完成前就被長度上限截斷（finish_reason=length）".to_string());
    }
    let text = value
        .pointer("/choices/0/message/content")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("NVIDIA 沒有回傳摘要內容".to_string());
    }
    Ok((text, model))
}

/// 摘要結果顯示用的標籤（給 ai_chat.rs 的 job 使用）。
pub(crate) fn summary_label() -> &'static str {
    OPENCODE_SUMMARY_LABEL
}
