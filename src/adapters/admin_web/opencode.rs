//! opencode AI coding agent 後端模組。
//!
//! OPENCODE_INTEGRATION_V1
//!
//! 設計重點：
//! 1. **Sidecar 代理**：面板透過 Rust 後端代理 opencode server (127.0.0.1:4097)
//! 2. **權限控管**：僅管理員可設定、僅 allowlist 帳號可使用
//! 3. **密鑰加密**：NVIDIA API Key 用 StoreSecret 加密存 config/ai_opencode.json
//! 4. **沙箱隔離**：opencode 工作區在 opencode-workspace/，proot 硬隔離
//! 5. **Session 對應**：Dashboard thread_id ↔ opencode session_id

use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    Json,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use super::{
    admin_login_redirect, must_change_password_redirect, session_token,
    unauthorized_response, AdminQuery, AdminWebState,
};

const OPENCODE_CONFIG_FILE: &str = "config/ai_opencode.json";
const OPENCODE_SERVER_PASS_FILE: &str = "config/opencode-server.txt";
const OPENCODE_WORKSPACE: &str = "opencode-workspace";
const OPENCODE_DEFAULT_PORT: u16 = 4097;
const OPENCODE_HEALTH_TIMEOUT_SECS: u64 = 3;

/// opencode 設定結構（含加密後的 API key）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpencodeConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_base_url")]
    base_url: String,
    #[serde(default = "default_model")]
    model: String,
    #[serde(default)]
    api_key_encrypted: String,  // StoreSecret 加密
    #[serde(default)]
    allowed_accounts: Vec<String>,  // allowlist
    #[serde(default)]
    updated_at: u64,
}

fn default_base_url() -> String {
    "https://integrate.api.nvidia.com/v1".to_string()
}

fn default_model() -> String {
    "nvidia/nemotron-3-super-120b-a12b".to_string()
}

/// opencode server 狀態回傳
#[derive(Debug, Serialize)]
struct OpencodeStateResponse {
    enabled: bool,
    healthy: bool,
    allowed: bool,
    can_manage: bool,
    base_url: String,
    model: String,
    key_masked: String,
    allowed_accounts: Vec<String>,
}

/// 讀取 opencode 設定
fn read_opencode_config() -> OpencodeConfig {
    let path = std::path::Path::new(OPENCODE_CONFIG_FILE);
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(mut cfg) = serde_json::from_str::<OpencodeConfig>(&text) {
            // 解密 API key
            if !cfg.api_key_encrypted.is_empty() {
                let unsealed = crate::secret_encryption::StoreSecret::from_stored_with_purpose(
                    &cfg.api_key_encrypted,
                    "ai_opencode.api_key",
                );
                cfg.api_key_encrypted = unsealed.as_str().to_string();
            }
            return cfg;
        }
    }
    OpencodeConfig::default()
}

/// 寫入 opencode 設定（加密 API key）
fn write_opencode_config(cfg: &OpencodeConfig) -> Result<(), String> {
    let mut on_disk = cfg.clone();
    if !on_disk.api_key_encrypted.is_empty() {
        on_disk.api_key_encrypted = crate::secret_encryption::StoreSecret::new(
            std::mem::take(&mut on_disk.api_key_encrypted)
        )
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

/// 同步寫入 opencode-workspace/nvidia.key 與 opencode.json
fn sync_opencode_workspace(config: &OpencodeConfig) -> Result<(), String> {
    let workspace_dir = std::path::Path::new(OPENCODE_WORKSPACE);
    std::fs::create_dir_all(workspace_dir).map_err(|e| format!("建立 workspace 失敗: {e}"))?;

    // 寫入 nvidia.key
    let key_path = workspace_dir.join("nvidia.key");
    std::fs::write(&key_path, &config.api_key_encrypted)
        .map_err(|e| format!("寫入 nvidia.key 失敗: {e}"))?;
    let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));

    // 更新 opencode.json
    let opencode_json_path = workspace_dir.join("opencode.json");
    if let Ok(text) = std::fs::read_to_string(&opencode_json_path) {
        if let Ok(mut oc) = serde_json::from_str::<Value>(&text) {
            if let Some(provider) = oc.get_mut("provider") {
                if let Some(nvidia) = provider.get_mut("nvidia") {
                    if let Some(options) = nvidia.get_mut("options") {
                        options["baseURL"] = json!(config.base_url);
                        options["model"] = json!(config.model);
                        // apiKey 使用 file:./nvidia.key 參考
                    }
                }
            }
            let updated = serde_json::to_string_pretty(&oc)
                .map_err(|e| format!("序列化 opencode.json 失敗: {e}"))?;
            std::fs::write(&opencode_json_path, updated)
                .map_err(|e| format!("寫入 opencode.json 失敗: {e}"))?;
        }
    }
    Ok(())
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

    let auth = format!("Basic {}", base64::encode(format!("opencode:{}", password)));
    client
        .get(format!("http://127.0.0.1:{}/global/health", OPENCODE_DEFAULT_PORT))
        .header("Authorization", auth)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// 遮蔽 API key 顯示
fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        "••••••••".to_string()
    } else {
        format!("{}••••{}", &key[..4], &key[key.len()-4..])
    }
}

/// GET /admin/api/ai/opencode/state
pub(super) async fn state_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> impl IntoResponse {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response().into_response();
    };

    let can_manage = session.is_admin;
    let config = read_opencode_config();

    // 檢查帳號是否在 allowlist
    let allowed = config.allowed_accounts.contains(&session.account_id) || session.is_admin;

    // 檢查 server 健康狀態
    let healthy = if let Ok(pw) = read_opencode_password() {
        check_opencode_health(&pw).await
    } else {
        false
    };

    let key_masked = if config.api_key_encrypted.is_empty() {
        String::new()
    } else {
        mask_key(&config.api_key_encrypted)
    };

    (
        StatusCode::OK,
        Json(json!(OpencodeStateResponse {
            enabled: config.enabled && healthy,
            healthy,
            allowed,
            can_manage,
            base_url: config.base_url,
            model: config.model,
            key_masked,
            allowed_accounts: config.allowed_accounts,
        })),
    )
        .into_response()
}

/// POST /admin/api/ai/opencode/config（僅管理員）
#[derive(Debug, Deserialize)]
struct OpencodeConfigRequest {
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
    Json(body): Json<OpencodeConfigRequest>,
) -> impl IntoResponse {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response().into_response();
    };
    if !session.is_admin {
        return unauthorized_response().into_response();
    }

    let mut config = read_opencode_config();
    config.enabled = body.enabled;
    if !body.base_url.trim().is_empty() {
        config.base_url = body.base_url.trim().to_string();
    }
    if !body.model.trim().is_empty() {
        config.model = body.model.trim().to_string();
    }
    if !body.api_key.trim().is_empty() {
        config.api_key_encrypted = body.api_key.trim().to_string();
    }

    if let Err(e) = write_opencode_config(&config) {
        error!("寫入 opencode 設定失敗: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "config_write_failed", "message": e })),
        )
            .into_response();
    }

    // 同步到 workspace
    if let Err(e) = sync_opencode_workspace(&config) {
        warn!("同步 opencode workspace 失敗: {}", e);
    }

    // 觸發 sidecar 重啟（watchdog 會偵測到並重啟）
    let _ = std::fs::write("config/opencode-restart", "");
    let _ = std::fs::set_permissions("config/opencode-restart", std::fs::Permissions::from_mode(0o600));

    info!("opencode 設定已更新");
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "message": "設定已儲存，sidecar 將自動重啟" })),
    )
        .into_response()
}

/// POST /admin/api/ai/opencode/access（僅管理員）
#[derive(Debug, Deserialize)]
struct OpencodeAccessRequest {
    #[serde(default)]
    token: String,
    #[serde(default)]
    allowed_accounts: Vec<String>,
}

pub(super) async fn access_handler(
    State(state): State<AdminWebState>,
    Json(body): Json<OpencodeAccessRequest>,
) -> impl IntoResponse {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response().into_response();
    };
    if !session.is_admin {
        return unauthorized_response().into_response();
    }

    let mut config = read_opencode_config();
    config.allowed_accounts = body.allowed_accounts;

    if let Err(e) = write_opencode_config(&config) {
        error!("寫入 opencode access 失敗: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "config_write_failed", "message": e })),
        )
            .into_response();
    }

    info!("opencode allowlist 已更新: {:?}", config.allowed_accounts);
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "allowed_accounts": config.allowed_accounts })),
    )
        .into_response()
}

/// 聊天請求
#[derive(Debug, Deserialize)]
struct OpencodeChatRequest {
    #[serde(default)]
    token: String,
    message: String,
    #[serde(default)]
    thread_id: String,
}

/// 聊天回應
#[derive(Debug, Serialize)]
struct OpencodeChatResponse {
    thread_id: String,
    ok: bool,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// 對應：Dashboard thread_id -> opencode session_id
static OPENCODE_SESSIONS: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());

/// POST /admin/api/ai/opencode/chat
pub(super) async fn chat_handler(
    State(state): State<AdminWebState>,
    Json(body): Json<OpencodeChatRequest>,
) -> impl IntoResponse {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response().into_response();
    };

    // 檢查 allowlist
    let config = read_opencode_config();
    if !config.allowed_accounts.contains(&session.account_id) && !session.is_admin {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "access_denied", "message": "此帳號未被授權使用 opencode" })),
        )
            .into_response();
    }

    if !config.enabled {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "disabled", "message": "opencode 功能未啟用" })),
        )
            .into_response();
    }

    if body.message.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "empty_message", "message": "訊息不可為空" })),
        )
            .into_response();
    }

    let thread_id = if body.thread_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        body.thread_id
    };

    // 取得或建立 opencode session
    let session_id = {
        let mut sessions = OPENCODE_SESSIONS.lock().await;
        sessions
            .entry(thread_id.clone())
            .or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone()
    };

    // 密碼
    let password = match read_opencode_password() {
        Ok(pw) => pw,
        Err(e) => {
            error!("讀取 opencode 密碼失敗: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "server_error", "message": "opencode server 未就緒" })),
            )
                .into_response();
        }
    };

    // 發送訊息到 opencode
    let client = match Client::builder().timeout(Duration::from_secs(120)).build() {
        Ok(c) => c,
        Err(e) => {
            error!("建立 HTTP client 失敗: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "server_error", "message": "內部錯誤" })),
            )
                .into_response();
        }
    };

    let auth = format!("Basic {}", base64::encode(format!("opencode:{}", password)));

    // 發送 message
    let msg_url = format!(
        "http://127.0.0.1:{}/session/{}/message",
        OPENCODE_DEFAULT_PORT, session_id
    );
    let msg_body = json!({
        "role": "user",
        "content": body.message,
    });

    let resp = client
        .post(&msg_url)
        .header("Authorization", &auth)
        .header("Content-Type", "application/json")
        .json(&msg_body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let resp_json: Value = r.json().await.unwrap_or_default();
            let text = resp_json.get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            (
                StatusCode::OK,
                Json(OpencodeChatResponse {
                    thread_id,
                    ok: true,
                    text,
                    error: None,
                }),
            )
                .into_response()
        }
        Ok(r) => {
            let status = r.status();
            let err_text = r.text().await.unwrap_or_default();
            error!("opencode message 失敗: {} - {}", status, err_text);
            (
                StatusCode::BAD_GATEWAY,
                Json(OpencodeChatResponse {
                    thread_id,
                    ok: false,
                    text: String::new(),
                    error: Some(format!("opencode 回應錯誤: {}", status)),
                }),
            )
                .into_response()
        }
        Err(e) => {
            error!("opencode 請求失敗: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(OpencodeChatResponse {
                    thread_id,
                    ok: false,
                    text: String::new(),
                    error: Some("無法連線到 opencode server".to_string()),
                }),
            )
                .into_response()
        }
    }
}

impl Default for OpencodeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_base_url(),
            model: default_model(),
            api_key_encrypted: String::new(),
            allowed_accounts: Vec::new(),
            updated_at: 0,
        }
    }
}