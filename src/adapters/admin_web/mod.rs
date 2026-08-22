//! Web panel with per-account Dashboard credentials.
//!
//! Dashboard passwords are deliberately separate from upstream Tronclass
//! credentials. Regular users can see and control only their own account;
//! accounts marked `is_admin` can see and control every account. The legacy
//! `access_token` configuration value is no longer accepted as a browser-login
//! bypass and is retained only for configuration compatibility.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    extract::{DefaultBodyLimit, Multipart, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use passkey_auth::{
    Attachment, AuthenticationResponse, AuthenticationState, CredentialId, RegistrationResponse,
    RegistrationState, Webauthn,
};
use qrcode::{render::svg, QrCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{info, warn};
use uuid::Uuid;

use crate::account::RawAccountConfig;
use crate::adapters::events::{
    AdapterMessenger, NumberCodeRequest, OutboundMessage, QrCodeRequest, RollcallHistoryEntry,
};
use crate::adapters::requests::{NumberCodeInput, RequestAccountState};
use crate::adapters::scanner::QrScannerRegistry;
use crate::db::{AccountsDb, MaterialEventInput};
use crate::runtime_settings::{RuntimeRestPeriod, RuntimeSettings};

mod class_reminder;
mod pages;
mod timetable;
use self::pages::{
    inject_theme, load_runtime_theme, ADMIN_ACTIVITY_HTML, ADMIN_DASHBOARD_HTML, ADMIN_FILES_HTML,
    ADMIN_HEALTH_HTML, ADMIN_PASSKEY_HTML, ADMIN_PASSWORD_HTML,
};
mod actions;
mod files;
pub mod materials_webdav;
pub mod oidc;
mod fb_sso;
mod health;
mod logs;
mod mock;
mod pwa;
mod utils;
use self::utils::*;

const PW_SALT: &[u8] = b"tronclass-rollcall-admin-web-password-hash-v2";

// Body limits are assigned to individual POST routes below. Only multipart
// material uploads need the 51 MiB allowance; ordinary JSON must stay small
// enough that extraction before handler-local authorization cannot spike RSS.
const ADMIN_TINY_BODY_LIMIT: usize = 8 * 1024;
const ADMIN_LOGIN_BODY_LIMIT: usize = 16 * 1024;
const ADMIN_MOCK_BODY_LIMIT: usize = 64 * 1024;
const ADMIN_JSON_BODY_LIMIT: usize = 256 * 1024;
const ADMIN_UPLOAD_BODY_LIMIT: usize = MATERIALS_MAX_FILE_SIZE + 1024 * 1024;

pub(super) const ADMIN_MAX_USERNAME_BYTES: usize = 128;
pub(super) const ADMIN_MAX_PASSWORD_BYTES: usize = 1024;
pub(super) const ADMIN_MAX_IDENTIFIER_BYTES: usize = 256;
pub(super) const ADMIN_MAX_ACTION_BYTES: usize = 64;
pub(super) const ADMIN_MAX_PATH_BYTES: usize = 1024;
pub(super) const ADMIN_MAX_DETAILS_BYTES: usize = 4 * 1024;
pub(super) const ADMIN_MAX_REST_PERIODS: usize = crate::runtime_settings::MAX_RUNTIME_REST_PERIODS;
pub(super) const ADMIN_MAX_NOTIFICATION_PREFERENCES: usize = 32;

const SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);
const MAX_SESSIONS: usize = 500;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
const RATE_LIMIT_MAX_ATTEMPTS: u32 = 5;
const IP_RATE_LIMIT_MAX_ATTEMPTS: u32 = 20;
const MAX_RATE_LIMIT_KEYS: usize = 4096;
const MAX_SECURITY_WATCH_KEYS: usize = 4096;
const SECURITY_FAIL_ALERT_THRESHOLD: u32 = 5;
const SECURITY_ALERT_COOLDOWN: Duration = Duration::from_secs(10 * 60);
const SESSION_COOKIE_MARKER: &str = "cookie";
const SECURE_SESSION_COOKIE: &str = "__Host-tronclass_admin_session";
const LOCAL_SESSION_COOKIE: &str = "tronclass_admin_session";
const SESSION_COOKIE_MAX_AGE: u64 = 8 * 60 * 60;

pub(super) fn validate_admin_text(
    value: &str,
    max_bytes: usize,
    field: &'static str,
) -> Result<(), Response> {
    if value.len() <= max_bytes {
        return Ok(());
    }
    Err((
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": format!("{field} is too long; max {max_bytes} bytes")
        })),
    )
        .into_response())
}

pub(super) fn validate_optional_admin_text(
    value: Option<&str>,
    max_bytes: usize,
    field: &'static str,
) -> Result<(), Response> {
    if let Some(value) = value {
        validate_admin_text(value, max_bytes, field)?;
    }
    Ok(())
}

#[derive(Clone, Default)]
struct RateLimiter {
    attempts: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn check(&self, key: &str) -> bool {
        self.check_limit(key, RATE_LIMIT_MAX_ATTEMPTS).await
    }

    async fn check_limit(&self, key: &str, max_attempts: u32) -> bool {
        let mut map = self.attempts.lock().await;
        let now = Instant::now();
        let window_start = now - RATE_LIMIT_WINDOW;
        if map.len() >= MAX_RATE_LIMIT_KEYS && !map.contains_key(key) {
            map.retain(|_, attempts| {
                attempts.retain(|at| *at > window_start);
                !attempts.is_empty()
            });
            if map.len() >= MAX_RATE_LIMIT_KEYS {
                return false;
            }
        }
        let entry = map.entry(key.to_string()).or_default();
        entry.retain(|t| *t > window_start);
        if entry.len() >= max_attempts as usize {
            return false;
        }
        entry.push(now);
        if entry.len() > 2 * RATE_LIMIT_MAX_ATTEMPTS as usize {
            map.retain(|_, v| {
                v.retain(|t| *t > window_start);
                !v.is_empty()
            });
        }
        true
    }

    async fn clear(&self, key: &str) {
        self.attempts.lock().await.remove(key);
    }
}

fn normalized_ip(value: &str) -> Option<String> {
    let value = value.trim();
    // A textual IPv6 address is at most 45 bytes. Keep a little room for
    // canonical variants, but never retain an arbitrary forwarded-header value.
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    value.parse::<IpAddr>().ok().map(|ip| ip.to_string())
}

fn client_ip_from_headers(headers: &HeaderMap) -> String {
    for name in ["cf-connecting-ip", "true-client-ip", "x-real-ip"] {
        if let Some(value) = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(normalized_ip)
        {
            return value;
        }
    }
    if let Some(value) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|value| value.split(',').next())
        .and_then(normalized_ip)
    {
        return value;
    }
    "unknown".to_string()
}

fn rate_limited_response() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "error": "too many attempts; please wait" })),
    )
        .into_response()
}

struct SecurityWatch {
    fails: HashMap<String, Vec<Instant>>,
    last_alert: HashMap<String, Instant>,
}

static SECURITY_WATCH: once_cell::sync::Lazy<Mutex<SecurityWatch>> =
    once_cell::sync::Lazy::new(|| {
        Mutex::new(SecurityWatch {
            fails: HashMap::new(),
            last_alert: HashMap::new(),
        })
    });

fn security_now_label() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

async fn security_should_alert(key: &str) -> bool {
    let mut watch = SECURITY_WATCH.lock().await;
    let now = Instant::now();
    watch
        .last_alert
        .retain(|_, at| now.saturating_duration_since(*at) < SECURITY_ALERT_COOLDOWN * 6);
    if watch
        .last_alert
        .get(key)
        .map(|at| now.saturating_duration_since(*at) < SECURITY_ALERT_COOLDOWN)
        .unwrap_or(false)
    {
        return false;
    }
    if watch.last_alert.len() >= MAX_SECURITY_WATCH_KEYS && !watch.last_alert.contains_key(key) {
        if let Some(oldest) = watch
            .last_alert
            .iter()
            .min_by_key(|(_, at)| **at)
            .map(|(key, _)| key.clone())
        {
            watch.last_alert.remove(&oldest);
        }
    }
    watch.last_alert.insert(key.to_string(), now);
    true
}

async fn record_auth_failure(username: &str, ip: &str) -> (u32, u32) {
    let mut watch = SECURITY_WATCH.lock().await;
    let now = Instant::now();
    let window_start = now - RATE_LIMIT_WINDOW;
    let mut counts = [0u32, 0u32];
    for (idx, key) in [
        format!("user:{}", username.trim().to_lowercase()),
        format!("ip:{}", ip.trim()),
    ]
    .into_iter()
    .enumerate()
    {
        if watch.fails.len() >= MAX_SECURITY_WATCH_KEYS && !watch.fails.contains_key(&key) {
            watch.fails.retain(|_, attempts| {
                attempts.retain(|at| *at > window_start);
                !attempts.is_empty()
            });
            if watch.fails.len() >= MAX_SECURITY_WATCH_KEYS {
                if let Some(old_key) = watch.fails.keys().next().cloned() {
                    watch.fails.remove(&old_key);
                }
            }
        }
        let entry = watch.fails.entry(key).or_default();
        entry.retain(|t| *t > window_start);
        entry.push(now);
        counts[idx] = entry.len() as u32;
    }
    if watch.fails.len() > MAX_SESSIONS {
        watch.fails.retain(|_, v| {
            v.retain(|t| *t > window_start);
            !v.is_empty()
        });
    }
    (counts[0], counts[1])
}

/// 登入成功後重置該帳號與 IP 的失敗計數，讓告警只在「連續」失敗時觸發。
async fn clear_auth_failures(username: &str, ip: &str) {
    let mut watch = SECURITY_WATCH.lock().await;
    watch
        .fails
        .remove(&format!("user:{}", username.trim().to_lowercase()));
    watch.fails.remove(&format!("ip:{}", ip.trim()));
}

async fn admin_discord_recipients(state: &AdminWebState) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(messenger) = &state.messenger {
        let admin = messenger.admin_user_id().trim();
        if !admin.is_empty() {
            ids.push(admin.to_string());
        }
    }
    if let Ok(accounts) = state.accounts_db.list().await {
        for account in accounts {
            if !account.is_admin {
                continue;
            }
            let id = account.discord_user_id.trim();
            if id.is_empty() {
                continue;
            }
            if !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

async fn notify_admins_discord(state: &AdminWebState, text: String) {
    let Some(messenger) = state.messenger.clone() else {
        warn!("security alert skipped: no Discord/LINE messenger");
        return;
    };
    let recipients = admin_discord_recipients(state).await;
    if recipients.is_empty() {
        warn!("security alert skipped: no admin Discord user id");
        return;
    }
    let message = OutboundMessage::Text(text);
    for to in recipients {
        if let Err(e) = messenger.push(&to, &message).await {
            warn!(to = %to, error = %e, "security alert Discord 推送失敗");
        }
    }
}

async fn alert_password_failures(state: &AdminWebState, username: &str, ip: &str) {
    let (user_fails, ip_fails) = record_auth_failure(username, ip).await;
    // 解析被嘗試登入的帳號持有人綁定（可能為空：隨機 username 或未綁定）。
    let (target_discord, target_line) = resolve_login_target_contacts(state, username).await;

    if user_fails >= SECURITY_FAIL_ALERT_THRESHOLD {
        let key = format!("fail-user:{}", username.trim().to_lowercase());
        if security_should_alert(&key).await {
            let admin_text = format!(
                "⚠️ Dashboard 安全警示\n類型：連續密碼錯誤\n帳號：{}\nIP：{}\n60 秒內失敗：{} 次\n時間：{}\n請到 /admin/activity 查看登入審計。",
                username.trim(),
                ip.trim(),
                user_fails,
                security_now_label()
            );
            notify_admins_discord(state, admin_text).await;

            let user_text = format!(
                "⚠️ 安全提醒\n有人連續嘗試登入你的 Dashboard 帳號（帳號：{}），60 秒內已失敗 {} 次。\n若非你本人操作，請立即變更 Dashboard 密碼。",
                username.trim(),
                user_fails
            );
            notify_account_holder(
                state,
                target_discord.as_deref(),
                target_line.as_deref(),
                &user_text,
            )
            .await;
        }
    } else if ip_fails >= SECURITY_FAIL_ALERT_THRESHOLD {
        let key = format!("fail-ip:{}", ip.trim());
        if security_should_alert(&key).await {
            let admin_text = format!(
                "⚠️ Dashboard 安全警示\n類型：同一 IP 多次登入失敗\n最近帳號：{}\nIP：{}\n60 秒內失敗：{} 次\n時間：{}\n請到 /admin/activity 查看登入審計。",
                username.trim(),
                ip.trim(),
                ip_fails,
                security_now_label()
            );
            notify_admins_discord(state, admin_text).await;

            let user_text = format!(
                "⚠️ 安全提醒\n有人持續嘗試登入你的 Dashboard 帳號（帳號：{}）。\n若非你本人操作，請立即變更 Dashboard 密碼。",
                username.trim()
            );
            notify_account_holder(
                state,
                target_discord.as_deref(),
                target_line.as_deref(),
                &user_text,
            )
            .await;
        }
    }
}

/// 解析 `username` 對應帳號的綁定聯絡方式（先以帳號 id 查詢，再以 Tronclass username 查詢）。
/// 回傳 `(discord_user_id, line_user_id)`，未綁定時為 `None`。
async fn resolve_login_target_contacts(
    state: &AdminWebState,
    username: &str,
) -> (Option<String>, Option<String>) {
    let username = username.trim();
    if username.is_empty() {
        return (None, None);
    }
    let mut account = None;
    if let Ok(Some(acct)) = state.accounts_db.get(username).await {
        account = Some(acct);
    }
    if account.is_none() {
        if let Ok(accounts) = state.accounts_db.find_by_username(username, None).await {
            account = accounts.into_iter().next();
        }
    }
    match account {
        Some(acct) => {
            let discord = acct.discord_user_id.trim().to_string();
            let line = acct.line_user_id.trim().to_string();
            (
                if discord.is_empty() {
                    None
                } else {
                    Some(discord)
                },
                if line.is_empty() { None } else { Some(line) },
            )
        }
        None => (None, None),
    }
}

/// 推送安全警示給被嘗試登入的帳號持有人（Discord / LINE）。
async fn notify_account_holder(
    state: &AdminWebState,
    discord_user_id: Option<&str>,
    line_user_id: Option<&str>,
    text: &str,
) {
    let Some(messenger) = state.messenger.clone() else {
        return;
    };
    let message = OutboundMessage::Text(text.to_string());
    if let Some(to) = discord_user_id.filter(|v| !v.trim().is_empty()) {
        if let Err(e) = messenger.push(to, &message).await {
            warn!(to = %to, error = %e, "security alert 使用者 Discord 推送失敗");
        }
    }
    if let Some(to) = line_user_id.filter(|v| !v.trim().is_empty()) {
        if let Err(e) = messenger.push(to, &message).await {
            warn!(to = %to, error = %e, "security alert 使用者 LINE 推送失敗");
        }
    }
}

async fn alert_abnormal_traffic(state: &AdminWebState, source: &str, username: &str, ip: &str) {
    let key = format!("traffic:{source}:{}", ip.trim());
    if !security_should_alert(&key).await {
        return;
    }
    let who = if username.trim().is_empty() {
        "-".to_string()
    } else {
        username.trim().to_string()
    };
    let text = format!(
        "⚠️ Dashboard 安全警示\n類型：異常流量\n來源：{source}\n帳號：{who}\nIP：{}\n60 秒內嘗試次數超過上限，已暫時封鎖。\n時間：{}\n請到 /admin/activity 查看登入審計。",
        ip.trim(),
        security_now_label()
    );
    notify_admins_discord(state, text).await;
}

#[derive(Clone)]
pub struct AdminWebState {
    accounts: Arc<Vec<RequestAccountState>>,
    accounts_by_id: Arc<HashMap<String, usize>>,
    accounts_db: AccountsDb,
    scanner: Option<Arc<QrScannerRegistry>>,
    messenger: Option<Arc<dyn AdapterMessenger>>,
    sessions: Arc<RwLock<HashMap<String, AdminSession>>>,
    rate_limiter: RateLimiter,
    default_poll_interval_secs: u64,
}

#[derive(Debug, Clone)]
struct AdminSession {
    account_id: String,
    is_admin: bool,
    /// A one-time first-login session authenticated with the upstream
    /// Tronclass password. It may only change the Dashboard password or log
    /// out; no account data or control API is exposed before that change.
    must_change_password: bool,
    expires_at: Instant,
}

impl AdminWebState {
    pub fn new(
        legacy_access_token: impl Into<String>,
        accounts: Vec<RequestAccountState>,
        accounts_db: AccountsDb,
        scanner: Option<Arc<QrScannerRegistry>>,
        messenger: Option<Arc<dyn AdapterMessenger>>,
        default_poll_interval_secs: u64,
    ) -> Self {
        // Keep accepting the old constructor argument so deployments can roll
        // forward without a simultaneous config migration. It is intentionally
        // discarded: a static numeric token must not bypass web authentication.
        let _legacy_access_token = legacy_access_token.into();
        let sessions: Arc<RwLock<HashMap<String, AdminSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        // PERFORMANCE_ACCOUNT_LOOKUP_V1:
        // Keep a stable account_id -> Vec index map so frequent per-account
        // Admin API routing does not linearly scan all runtime accounts.
        let accounts_by_id = Arc::new(
            accounts
                .iter()
                .enumerate()
                .map(|(index, account)| (account.account_id.clone(), index))
                .collect::<HashMap<_, _>>(),
        );

        // Background session cleanup every 5 minutes
        let sessions_clone = Arc::clone(&sessions);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await;
                sessions_clone
                    .write()
                    .await
                    .retain(|_, s| s.expires_at > Instant::now());
            }
        });

        let state = Self {
            accounts: Arc::new(accounts),
            accounts_by_id,
            accounts_db,
            scanner,
            messenger,
            sessions,
            rate_limiter: RateLimiter::new(),
            default_poll_interval_secs: default_poll_interval_secs.max(1),
        };
        load_runtime_theme();
        state
    }

    fn account_by_id(&self, account_id: &str) -> Option<&RequestAccountState> {
        self.accounts_by_id
            .get(account_id)
            .and_then(|index| self.accounts.get(*index))
    }

    async fn authorize(&self, token: &str) -> Option<AdminSession> {
        let session = self.authorize_any(token).await?;
        if session.must_change_password {
            return None;
        }
        Some(session)
    }

    async fn authorize_any(&self, token: &str) -> Option<AdminSession> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }

        // A legacy numeric access token is intentionally not accepted here.
        // Public deployments must authenticate as a real Dashboard account.

        // PERFORMANCE_SESSION_RWLOCK_V2:
        // Most Admin API requests only read session state. Use a read lock for
        // the common valid-session path and only upgrade to a write lock when the
        // current token is expired and needs removal.
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(token).cloned()?
        };
        if session.expires_at > Instant::now() {
            return Some(session);
        }

        let mut sessions = self.sessions.write().await;
        if sessions
            .get(token)
            .map(|session| session.expires_at <= Instant::now())
            .unwrap_or(false)
        {
            sessions.remove(token);
        }
        None
    }

    async fn revoke_sessions_for_account(&self, account_id: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, session| session.account_id != account_id);
    }

    async fn create_session(
        &self,
        account_id: String,
        is_admin: bool,
        must_change_password: bool,
    ) -> String {
        let token = Uuid::new_v4().simple().to_string();
        let session = AdminSession {
            account_id,
            is_admin,
            must_change_password,
            expires_at: Instant::now() + SESSION_TTL,
        };
        let mut sessions = self.sessions.write().await;
        // Enforce max sessions: drop oldest if over limit
        if sessions.len() >= MAX_SESSIONS {
            let oldest_key = sessions
                .iter()
                .min_by_key(|(_, s)| s.expires_at)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest_key {
                sessions.remove(&key);
            }
        }
        sessions.insert(token.clone(), session);
        token
    }

    async fn revoke_session(&self, token: &str) {
        self.sessions.write().await.remove(token.trim());
    }

    /// Password changes invalidate every Dashboard session for the account.
    async fn revoke_account_sessions(&self, account_id: &str) {
        self.sessions
            .write()
            .await
            .retain(|_, session| session.account_id != account_id);
    }
}

pub fn build_admin_router(state: AdminWebState) -> Router {
    let password_guard_state = state.clone();
    Router::new()
        .route("/", get(admin_root_redirect_handler))
        .route("/manifest.webmanifest", get(pwa::manifest_handler))
        .route("/service-worker.js", get(pwa::service_worker_handler))
        .route("/admin/pwa/icon.svg", get(pwa::icon_handler))
        .route("/admin/static/theme.css", get(admin_theme_css_handler))
        .route(
            "/admin/static/session-security.js",
            get(admin_session_security_js_handler),
        )
        .route(
            "/admin/static/timetable-ux.js",
            get(admin_timetable_ux_js_handler),
        )
        .route("/admin/static/core-api.js", get(admin_core_api_js_handler))
        .route(
            "/admin/static/accounts.js",
            get(admin_accounts_module_js_handler),
        )
        .route(
            "/admin/static/mock-rollcall.js",
            get(admin_mock_rollcall_module_js_handler),
        )
        .route(
            "/admin/static/notifications.js",
            get(admin_notifications_module_js_handler),
        )
        .route(
            "/admin/static/timetable.js",
            get(admin_timetable_ui_module_js_handler),
        )
        .route(
            "/admin/static/maintenance-implementation.js",
            get(admin_maintenance_implementation_js_handler),
        )
        .route(
            "/admin/static/maintenance.js",
            get(admin_maintenance_module_js_handler),
        )
        .route(
            "/admin/static/maintenance-components.css",
            get(admin_maintenance_components_css_handler),
        )
        .route(
            "/admin/static/account-components.css",
            get(admin_account_components_css_handler),
        )
        .route(
            "/admin/static/mobile-timetable.css",
            get(admin_mobile_timetable_css_handler),
        )
        .route(
            "/admin/static/mobile-navigation.css",
            get(admin_mobile_navigation_css_handler),
        )
        // DASHBOARD_MODULE_LOADER_PHASE1
        .route(
            "/admin/static/dashboard.css",
            get(admin_dashboard_css_handler),
        )
        .route(
            "/admin/static/dashboard-runtime.js",
            get(admin_dashboard_runtime_handler),
        )
        .route(
            "/admin/static/dashboard-modules.js",
            get(admin_dashboard_modules_handler),
        )
        .route("/admin", get(admin_page_handler))
        .route("/admin/activity", get(admin_activity_page_handler))
        .route("/admin/dashboard", get(admin_dashboard_page_handler))
        .route("/admin/health", get(health::admin_health_page_handler))
        .route("/admin/mock", get(mock::admin_mock_page_handler))
        .route("/admin/files", get(files::admin_files_page_handler))
        .route(
            "/.well-known/openid-configuration",
            get(oidc::discovery_handler),
        )
        .route(
            "/admin/oidc/.well-known/openid-configuration",
            get(oidc::discovery_handler),
        )
        .route("/admin/oidc/authorize", get(oidc::authorize_handler))
        .route("/admin/oidc/token", post(oidc::token_handler))
        .route("/admin/oidc/userinfo", get(oidc::userinfo_handler))
        .route("/admin/oidc/jwks", get(oidc::jwks_handler))
        .route("/admin/password", get(admin_password_page_handler))
        .route("/admin/passkey", get(admin_passkey_page_handler))
        .route("/admin/logs", get(logs::admin_logs_page_handler))
        .route("/admin/api/public_status", get(admin_public_status_handler))
        .route("/admin/api/health", get(health::admin_health_api_handler))
        .route(
            "/admin/api/health/check",
            get(health::admin_health_check_handler),
        )
        .route(
            "/admin/api/files/list",
            get(files::admin_files_list_handler),
        )
        .route(
            "/admin/api/files/history",
            get(files::admin_files_history_handler),
        )
        .route(
            "/admin/api/files/download",
            get(files::admin_files_download_handler),
        )
        .route(
            "/admin/api/files/preview",
            get(files::admin_files_preview_handler),
        )
        .route(
            "/admin/api/files/upload",
            post(files::admin_files_upload_handler)
                .layer(DefaultBodyLimit::max(ADMIN_UPLOAD_BODY_LIMIT)),
        )
        .route(
            "/admin/api/files/mkdir",
            post(files::admin_files_mkdir_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/files/delete",
            post(files::admin_files_delete_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route("/admin/api/logs", get(logs::admin_logs_api_handler))
        .route("/admin/api/fb-sso-check", get(fb_sso::fb_sso_check_handler))
        .route(
            "/admin/api/login",
            post(admin_login_handler).layer(DefaultBodyLimit::max(ADMIN_LOGIN_BODY_LIMIT)),
        )
        .route(
            "/admin/api/logout",
            post(admin_logout_handler).layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/password",
            post(admin_password_change_handler)
                .layer(DefaultBodyLimit::max(ADMIN_LOGIN_BODY_LIMIT)),
        )
        .route(
            "/admin/api/passkey/register/start",
            post(admin_passkey_register_start_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/passkey/register/finish",
            post(admin_passkey_register_finish_handler)
                .layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .route(
            "/admin/api/passkey/login/start",
            post(admin_passkey_login_start_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/passkey/login/finish",
            post(admin_passkey_login_finish_handler)
                .layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .route(
            "/admin/api/activity/login",
            get(admin_login_activity_handler),
        )
        .route("/admin/api/status", get(admin_status_handler))
        .route(
            "/admin/api/class_reminder",
            get(class_reminder::admin_class_reminder_get_handler)
                .post(class_reminder::admin_class_reminder_update_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/notifications/preferences",
            get(admin_notification_preferences_get_handler)
                .post(admin_notification_preferences_update_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/timetable",
            get(timetable::admin_timetable_list_handler),
        )
        .route(
            "/admin/api/timetable/import",
            post(timetable::admin_timetable_import_handler)
                .layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .route(
            "/admin/api/timetable/clear",
            post(timetable::admin_timetable_clear_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/timetable/templates",
            get(timetable::admin_timetable_templates_list_handler),
        )
        .route(
            "/admin/api/timetable/templates/save",
            post(timetable::admin_timetable_template_save_handler)
                .layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .route(
            "/admin/api/timetable/templates/apply",
            post(timetable::admin_timetable_template_apply_handler)
                .layer(DefaultBodyLimit::max(ADMIN_LOGIN_BODY_LIMIT)),
        )
        .route(
            "/admin/api/action",
            post(actions::admin_action_handler).layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .route(
            "/admin/api/mock/start",
            post(mock::admin_mock_start_handler)
                .layer(DefaultBodyLimit::max(ADMIN_MOCK_BODY_LIMIT)),
        )
        .route(
            "/admin/api/mock/qr.svg",
            get(mock::admin_mock_qr_svg_handler),
        )
        .route(
            "/admin/timetable",
            axum::routing::get(admin_clean_timetable_page_handler),
        )
        .layer(axum::middleware::from_fn(admin_security_headers_middleware))
        .layer(axum::middleware::from_fn_with_state(
            password_guard_state,
            require_dashboard_password_change_middleware,
        ))
        .layer(axum::middleware::from_fn(admin_same_origin_middleware))
        .layer(axum::middleware::from_fn(
            session_cookie_to_query_middleware,
        ))
        .layer(axum::middleware::from_fn(https_redirect_middleware))
        .layer(
            tower_http::compression::CompressionLayer::new()
                .gzip(true),
        )
        .with_state(state)
}

async fn https_redirect_middleware(
    headers: HeaderMap,
    uri: axum::http::Uri,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = uri.path();
    if path.starts_with("/admin") {
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("https");
        if proto == "http" {
            let host = headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("localhost");
            let https_uri = format!(
                "https://{host}{}",
                uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("")
            );
            return (
                StatusCode::MOVED_PERMANENTLY,
                [(header::LOCATION, https_uri)],
            )
                .into_response();
        }
    }
    next.run(request).await
}

fn request_uses_https(headers: &HeaderMap) -> bool {
    if headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"))
    {
        return true;
    }
    // Cloudflare Tunnel does not always set X-Forwarded-Proto. `CF-Visitor`
    // carries the original scheme, and any `__Host-` cookie the browser already
    // accepted proves the origin is HTTPS. Misdetecting HTTPS as HTTP makes the
    // server emit a `__Host-` cookie without `Secure`, which browsers silently
    // discard, producing an endless 401 login loop.
    if headers
        .get("cf-visitor")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("https"))
    {
        return true;
    }
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains(SECURE_SESSION_COOKIE))
}

fn session_cookie_name(headers: &HeaderMap) -> &'static str {
    if request_uses_https(headers) {
        SECURE_SESSION_COOKIE
    } else {
        // Local HTTP is only a development convenience. A Cloudflare Tunnel
        // request carries X-Forwarded-Proto=https and therefore always uses the
        // hardened __Host- cookie variant.
        LOCAL_SESSION_COOKIE
    }
}

fn session_cookie_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookies.split(';') {
        // NOTE: `?` here used to abort the whole scan as soon as any cookie in
        // the header had no `=` (Cloudflare and analytics cookies routinely do),
        // so the session cookie later in the list was never inspected.
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name != SECURE_SESSION_COOKIE && name != LOCAL_SESSION_COOKIE {
            continue;
        }
        let value = value.trim();
        if value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Some(value.to_string());
        }
    }
    None
}

fn append_session_cookie(mut response: Response, token: &str, headers: &HeaderMap) -> Response {
    let name = session_cookie_name(headers);
    let secure = if name == SECURE_SESSION_COOKIE {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{name}={token}; Path=/; Max-Age={SESSION_COOKIE_MAX_AGE}; HttpOnly; SameSite=Strict{secure}"
    );
    // The session token is generated hex so this always parses, but never panic
    // on a header in the login path.
    match cookie.parse::<HeaderValue>() {
        Ok(value) => {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
        Err(e) => {
            tracing::error!(error = %e, "session cookie 無法建立");
        }
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn append_expired_session_cookies(mut response: Response) -> Response {
    for (name, secure) in [
        (SECURE_SESSION_COOKIE, "; Secure"),
        (LOCAL_SESSION_COOKIE, ""),
    ] {
        let cookie = format!("{name}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{secure}");
        let value = cookie
            .parse::<HeaderValue>()
            .expect("generated expired cookie must be a valid HTTP header");
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Promote the HttpOnly session cookie into the existing query extractor only
/// inside the server. Browser-visible URLs retain at most the harmless
/// `token=cookie` compatibility marker; the real random session token is never
/// placed in localStorage or a URL.
async fn session_cookie_to_query_middleware(
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !request.uri().path().starts_with("/admin/api/") {
        return next.run(request).await;
    }
    let Some(session_token) = session_cookie_from_headers(&headers) else {
        return next.run(request).await;
    };

    let (mut parts, body) = request.into_parts();
    // Keep all borrows into `parts.uri` inside this block. Axum middleware
    // futures must be Send while awaiting the next service.
    let rewritten = {
        let path = parts.uri.path().to_string();
        let existing_query = parts.uri.query().unwrap_or("");
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in url::form_urlencoded::parse(existing_query.as_bytes()) {
            if name != "token" {
                serializer.append_pair(&name, &value);
            }
        }
        serializer.append_pair("token", &session_token);
        format!("{path}?{}", serializer.finish())
    };
    if let Ok(uri) = rewritten.parse() {
        parts.uri = uri;
    }

    next.run(axum::extract::Request::from_parts(parts, body))
        .await
}

fn session_token_from_uri(uri: &axum::http::Uri) -> Option<String> {
    let query = uri.query()?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == "token")
        .map(|(_, value)| value.into_owned())
}

fn bootstrap_allowed_api_path(path: &str) -> bool {
    matches!(
        path,
        "/admin/api/login"
            | "/admin/api/logout"
            | "/admin/api/fb-sso-check"
            | "/admin/api/password"
            | "/admin/api/status"
            | "/admin/api/public_status"
            | "/admin/api/passkey/login/start"
            | "/admin/api/passkey/login/finish"
    )
}

/// A first login may authenticate with the existing upstream Tronclass
/// credential only when that account has no Dashboard password yet. Restrict
/// that short-lived bootstrap session to password change/logout/status so it
/// cannot be used as an ordinary Dashboard session.
async fn require_dashboard_password_change_middleware(
    State(state): State<AdminWebState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path().to_string();
    if !path.starts_with("/admin/api/") || bootstrap_allowed_api_path(&path) {
        return next.run(request).await;
    }

    let token = session_cookie_from_headers(request.headers())
        .or_else(|| session_token_from_uri(request.uri()));
    if let Some(token) = token {
        if let Some(session) = state.authorize(&token).await {
            if session.must_change_password {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": "password_change_required",
                        "must_change_password": true,
                    })),
                )
                    .into_response();
            }
        }
    }

    next.run(request).await
}

fn same_origin_request(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        // Browsers send Sec-Fetch-Site for cross-site form submissions. Permit
        // command-line maintenance clients that do not send browser metadata.
        return headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            .map(|value| matches!(value, "same-origin" | "same-site" | "none"))
            .unwrap_or(true);
    };

    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .unwrap_or("https")
        .trim();
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        return false;
    }
    let expected = format!("{proto}://{host}");
    origin.trim_end_matches('/') == expected.trim_end_matches('/')
}

/// Reject browser cross-site state changes before they reach authenticated
/// handlers. SameSite=Strict cookies remain the primary CSRF control.
async fn admin_same_origin_middleware(
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = request.method();
    let mutating = method == Method::POST
        || method == Method::PUT
        || method == Method::PATCH
        || method == Method::DELETE;
    if mutating && request.uri().path().starts_with("/admin/api/") && !same_origin_request(&headers)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "cross-origin request rejected" })),
        )
            .into_response();
    }
    next.run(request).await
}

/// Security headers for the externally reachable Admin Web. Inline scripts are
/// still permitted because the legacy dashboard is currently static-inline; a
/// future frontend migration can remove 'unsafe-inline' and adopt CSP nonces.
async fn admin_security_headers_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // FAST_PAGE_LOAD_V1: remember path before moving the request so versioned
    // /admin/static assets can keep their Cache-Control instead of no-store.
    let path = request.uri().path().to_string();
    let is_admin_path = path.starts_with("/admin")
        || path == "/manifest.webmanifest"
        || path == "/service-worker.js";
    let cache_static = path.starts_with("/admin/static/") || path == "/admin/pwa/icon.svg";
    let mut response = next.run(request).await;
    if !is_admin_path {
        return response;
    }

    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(self), geolocation=(), microphone=()"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("strict-transport-security"),
        HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'; object-src 'none'; script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net https://static.cloudflareinsights.com; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self' https://cloudflareinsights.com; media-src 'self' blob:; frame-src 'self'; worker-src 'self' blob:",
        ),
    );
    if !cache_static {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

#[derive(Debug, Deserialize)]
struct AdminQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    include_history: Option<bool>,
    #[serde(default)]
    history_limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct AdminLogsQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct AdminActivityQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct AdminPasskeyLoginStartRequest {
    username: String,
}

#[derive(Debug, Deserialize)]
struct AdminPasskeyRegisterFinishRequest {
    ceremony_id: String,
    response: RegistrationResponse,
}

#[derive(Debug, Deserialize)]
struct AdminPasskeyLoginFinishRequest {
    ceremony_id: String,
    response: AuthenticationResponse,
}

#[derive(Debug, Deserialize)]
struct AdminNotificationPreferencesQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdminNotificationPreferenceInput {
    key: String,
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct AdminNotificationPreferencesUpdateRequest {
    #[serde(default)]
    account_id: Option<String>,
    preferences: Vec<AdminNotificationPreferenceInput>,
}

#[derive(Debug, Deserialize)]
struct AdminLoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct AdminPasswordChangeRequest {
    #[serde(default)]
    current_password: String,
    #[serde(default)]
    new_password: String,
}

#[derive(Debug, Serialize)]
struct AdminLoginResponse {
    /// Compatibility marker for legacy static pages. The actual random token is
    /// in a HttpOnly cookie and is never returned to browser JavaScript.
    token: String,
    account_id: String,
    is_admin: bool,
    /// True only for a first login that used the upstream Tronclass password.
    /// The browser must immediately show the mandatory password-change flow.
    must_change_password: bool,
}

fn session_login_response(
    request_headers: &HeaderMap,
    session_token: &str,
    account_id: String,
    is_admin: bool,
    must_change_password: bool,
) -> Response {
    fb_sso::append_fb_sso_cookie(
        append_session_cookie(
            (
                StatusCode::OK,
                Json(json!(AdminLoginResponse {
                    token: SESSION_COOKIE_MARKER.to_string(),
                    account_id: account_id.clone(),
                    is_admin,
                    must_change_password,
                })),
            )
                .into_response(),
            session_token,
            request_headers,
        ),
        &account_id,
        is_admin,
        request_headers,
    )
}

#[derive(Debug, Deserialize)]
struct AdminActionRequest {
    action: String,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    line_user_id: Option<String>,
    #[serde(default)]
    discord_user_id: Option<String>,
    #[serde(default)]
    is_admin: Option<bool>,
    #[serde(default)]
    mock_kind: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    poll_interval_secs: Option<Option<u64>>,
    #[serde(default)]
    group_submit_interval_ms: Option<u64>,
    #[serde(default)]
    auto_stop_time: Option<Option<String>>,
    #[serde(default)]
    rest_periods: Option<Vec<RuntimeRestPeriod>>,
}

#[derive(Debug, Serialize)]
struct AdminPublicStatusResponse {
    total_accounts: usize,
    running_accounts: usize,
    detected_accounts: usize,
}

#[derive(Debug, Serialize)]
struct AdminStatusResponse {
    current_account_id: String,
    is_admin: bool,
    /// A bootstrap session has authenticated once with the upstream password
    /// and may only change its Dashboard password.
    must_change_password: bool,
    default_poll_interval_secs: u64,
    settings: RuntimeSettings,
    accounts: Vec<AdminAccountStatus>,
}

#[derive(Debug, Serialize)]
struct AdminAccountStatus {
    account_id: String,
    username: String,
    line_user_id: String,
    discord_user_id: String,
    account_admin: bool,
    is_running: bool,
    user_name: String,
    last_poll_timestamp: Option<i64>,
    last_success_course: Option<String>,
    last_success_rollcall_id: Option<u64>,
    last_detected_timestamp: Option<i64>,
    last_detected_course: Option<String>,
    last_detected_rollcall_id: Option<u64>,
    last_detected_url: Option<String>,
    rollcall_history: Vec<RollcallHistoryEntry>,
    consecutive_failures: u32,
    started_at: i64,
}

async fn admin_dashboard_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    if session_must_change_password(&state, &headers, &query.token).await {
        return must_change_password_redirect();
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(inject_theme(ADMIN_DASHBOARD_HTML)),
    )
        .into_response()
}

async fn admin_activity_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(inject_theme(ADMIN_ACTIVITY_HTML)),
    )
        .into_response()
}

// Cache admin flags for /admin/api/status to avoid querying accounts.db on every dashboard refresh.
static ADMIN_STATUS_ADMIN_FLAG_CACHE: once_cell::sync::Lazy<
    tokio::sync::Mutex<Option<(std::time::Instant, std::collections::HashMap<String, bool>)>>,
> = once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(None));

const ADMIN_STATUS_SHORT_CACHE_TTL: Duration = Duration::from_secs(2);
const ADMIN_STATUS_HISTORY_LIMIT: usize = 30;

// Cache status responses briefly to absorb burst dashboard refreshes.
// The key includes the session role/account because the payload contains
// `current_account_id` and regular users only see their own account.
static ADMIN_STATUS_SHORT_CACHE: once_cell::sync::Lazy<
    tokio::sync::Mutex<HashMap<String, (Instant, serde_json::Value)>>,
> = once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Clear status caches after an account permission change so the Dashboard
/// immediately renders the new administrator badge/button state.
pub(super) async fn invalidate_admin_status_cache() {
    *ADMIN_STATUS_ADMIN_FLAG_CACHE.lock().await = None;
    ADMIN_STATUS_SHORT_CACHE.lock().await.clear();
}

fn admin_status_short_cache_key(
    session: &AdminSession,
    include_history: bool,
    history_limit: usize,
) -> String {
    let role = if session.is_admin { "admin" } else { "user" };
    format!(
        "{role}:{}:must_change={}:history={}:limit={}",
        session.account_id, session.must_change_password, include_history, history_limit
    )
}

async fn admin_status_short_cache_get(key: &str) -> Option<serde_json::Value> {
    let mut cache = ADMIN_STATUS_SHORT_CACHE.lock().await;
    let now = Instant::now();
    if cache.len() > MAX_SESSIONS {
        cache.retain(|_, (updated_at, _)| {
            now.saturating_duration_since(*updated_at) <= ADMIN_STATUS_SHORT_CACHE_TTL
        });
    }
    cache.get(key).and_then(|(updated_at, value)| {
        (now.saturating_duration_since(*updated_at) <= ADMIN_STATUS_SHORT_CACHE_TTL)
            .then(|| value.clone())
    })
}

async fn admin_status_short_cache_put(key: String, value: serde_json::Value) {
    let mut cache = ADMIN_STATUS_SHORT_CACHE.lock().await;
    cache.insert(key, (Instant::now(), value));
    if cache.len() > MAX_SESSIONS {
        let now = Instant::now();
        cache.retain(|_, (updated_at, _)| {
            now.saturating_duration_since(*updated_at) <= ADMIN_STATUS_SHORT_CACHE_TTL
        });
    }
}

async fn admin_status_admin_flags_cached(
    state: &AdminWebState,
) -> std::result::Result<std::collections::HashMap<String, bool>, Response> {
    {
        let cache = ADMIN_STATUS_ADMIN_FLAG_CACHE.lock().await;
        if let Some((updated_at, flags)) = cache.as_ref() {
            if updated_at.elapsed() <= std::time::Duration::from_secs(30) {
                return Ok(flags.clone());
            }
        }
    }

    let raw_accounts = match state.accounts_db.list().await {
        Ok(accounts) => accounts,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {e}") })),
            )
                .into_response());
        }
    };
    let flags = raw_accounts
        .into_iter()
        .map(|account| (account.id, account.is_admin))
        .collect::<std::collections::HashMap<_, _>>();

    *ADMIN_STATUS_ADMIN_FLAG_CACHE.lock().await = Some((std::time::Instant::now(), flags.clone()));
    Ok(flags)
}

async fn admin_status_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // Use the shared cookie parser. A bespoke copy here previously matched only
    // the plain cookie names and therefore never saw the hardened
    // `__Host-tronclass_admin_session` cookie that HTTPS / Cloudflare Tunnel
    // deployments actually receive, making every status request return 401.
    let token = session_cookie_from_headers(&headers)
        .unwrap_or_else(|| query.token.trim().to_string());
    let Some(session) = state.authorize_any(&token).await else {
        // Ptero only exposes logs, so make this failure self-diagnosing.
        let raw_cookie = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>");
        let names: Vec<&str> = raw_cookie
            .split(';')
            .filter_map(|p| p.trim().split_once('=').map(|(k, _)| k.trim()))
            .collect();
        warn!(
            cookie_names = ?names,
            parsed_token_len = token.len(),
            xfp = ?headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()),
            uses_https = request_uses_https(&headers),
            "STATUS 401: session lookup failed"
        );
        return unauthorized_response();
    };

    let include_history = query.include_history.unwrap_or(true);
    let history_limit = if include_history {
        query
            .history_limit
            .unwrap_or(ADMIN_STATUS_HISTORY_LIMIT)
            .min(ADMIN_STATUS_HISTORY_LIMIT)
    } else {
        0
    };

    let cache_key = admin_status_short_cache_key(&session, include_history, history_limit);
    if let Some(cached) = admin_status_short_cache_get(&cache_key).await {
        return (StatusCode::OK, Json(cached)).into_response();
    }

    // A first-login session may use this endpoint only to learn that it must
    // change its Dashboard password. Do not expose account status, schedules,
    // or management data until the password is changed.
    if session.must_change_password {
        let response_json = json!(AdminStatusResponse {
            current_account_id: session.account_id.clone(),
            is_admin: session.is_admin,
            must_change_password: true,
            default_poll_interval_secs: state.default_poll_interval_secs,
            settings: crate::runtime_settings::snapshot().await,
            accounts: vec![],
        });
        admin_status_short_cache_put(cache_key, response_json.clone()).await;
        return (StatusCode::OK, Json(response_json)).into_response();
    }

    let admin_by_id = if session.is_admin {
        match admin_status_admin_flags_cached(&state).await {
            Ok(flags) => flags,
            Err(resp) => return resp,
        }
    } else {
        std::collections::HashMap::new()
    };

    let visible_accounts = resolve_visible_accounts(&state, &session);
    // PERFORMANCE_STATUS_PAYLOAD_V1:
    // Build the response directly while each account status is locked instead
    // of cloning the full MonitorStatus.  The dashboard only renders the first
    // 30 history entries, so cap history in the hot /admin/api/status payload.
    let status_futures: Vec<_> = visible_accounts
        .iter()
        .map(|account| {
            let account_admin = admin_by_id
                .get(&account.account_id)
                .copied()
                .unwrap_or(false);
            async move {
                let status = account.monitor_status.lock().await;
                let rollcall_history = status
                    .rollcall_history
                    .iter()
                    .take(history_limit)
                    .cloned()
                    .collect::<Vec<_>>();
                let user_name = status.user_name.clone();
                let last_success_course = status.last_success_course.clone();
                let last_detected_course = status.last_detected_course.clone();
                let last_detected_url = status.last_detected_url.clone();
                let last_poll_timestamp = status.last_poll_timestamp;
                let last_success_rollcall_id = status.last_success_rollcall_id;
                let last_detected_timestamp = status.last_detected_timestamp;
                let last_detected_rollcall_id = status.last_detected_rollcall_id;
                let consecutive_failures = status.consecutive_failures;
                let started_at = status.started_at;
                drop(status);

                let target = account.target.lock().await.clone();
                let running = *account.is_running.lock().await;

                AdminAccountStatus {
                    account_id: account.account_id.clone(),
                    username: account.username.clone(),
                    line_user_id: target.line_user_id,
                    discord_user_id: target.discord_user_id,
                    account_admin,
                    is_running: running,
                    user_name,
                    last_poll_timestamp,
                    last_success_course,
                    last_success_rollcall_id,
                    last_detected_timestamp,
                    last_detected_course,
                    last_detected_rollcall_id,
                    last_detected_url,
                    rollcall_history,
                    consecutive_failures,
                    started_at,
                }
            }
        })
        .collect();
    let mut accounts = futures::future::join_all(status_futures).await;
    accounts.sort_by(|a, b| a.account_id.cmp(&b.account_id));

    let settings = crate::runtime_settings::snapshot().await;
    let response_json = json!(AdminStatusResponse {
        current_account_id: session.account_id.clone(),
        is_admin: session.is_admin,
        must_change_password: false,
        default_poll_interval_secs: state.default_poll_interval_secs,
        settings,
        accounts,
    });

    admin_status_short_cache_put(cache_key, response_json.clone()).await;

    (StatusCode::OK, Json(response_json)).into_response()
}

const DISCORD_NOTIFICATION_KEYS: &[(&str, &str, &str)] = &[
    ("system_started", "系統啟動", "服務啟動或帳號監控開始時通知"),
    ("rollcall_detected", "偵測到點名", "偵測到新點名時通知"),
    ("number_code", "數字點名", "需要輸入數字點名碼時通知"),
    ("qr_code", "QR Code 點名", "需要掃描或送出 QR Code 時通知"),
    (
        "rollcall_finished",
        "點名完成結果",
        "點名成功或失敗結果通知",
    ),
    ("status", "狀態回覆", "主動推送狀態摘要通知"),
    (
        "control",
        "控制操作",
        "暫停、恢復、Force、Reauth 等控制通知",
    ),
    ("other", "其他通知", "其他 Discord 通知"),
];

fn resolve_notification_target(
    session: &AdminSession,
    requested: Option<String>,
) -> Result<String, Response> {
    if session.is_admin {
        let target = requested
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| session.account_id.clone());
        if target == "<token-admin>" {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "請指定 account_id" })),
            )
                .into_response());
        }
        Ok(target)
    } else {
        if let Some(requested) = requested
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            if requested != session.account_id {
                return Err(unauthorized_response());
            }
        }
        Ok(session.account_id.clone())
    }
}

async fn admin_notification_preferences_get_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminNotificationPreferencesQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_optional_admin_text(
        query.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_notification_target(&session, query.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    let rows = match state
        .accounts_db
        .list_notification_preferences(&target, "discord")
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("notification database error: {e}") })),
            )
                .into_response();
        }
    };
    let mut saved = std::collections::HashMap::new();
    for row in rows {
        saved.insert(row.notification_key, row.enabled);
    }
    let preferences = DISCORD_NOTIFICATION_KEYS
        .iter()
        .map(|(key, label, description)| {
            json!({
                "key": key,
                "label": label,
                "description": description,
                "enabled": saved.get(*key).copied().unwrap_or(true),
            })
        })
        .collect::<Vec<_>>();
    (
        StatusCode::OK,
        Json(json!({
            "account_id": target,
            "channel": "discord",
            "is_admin": session.is_admin,
            "preferences": preferences,
        })),
    )
        .into_response()
}

async fn admin_notification_preferences_update_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(req): Json<AdminNotificationPreferencesUpdateRequest>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if req.preferences.len() > ADMIN_MAX_NOTIFICATION_PREFERENCES {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "too many notification preferences; max {}",
                    ADMIN_MAX_NOTIFICATION_PREFERENCES
                )
            })),
        )
            .into_response();
    }
    if let Err(response) = validate_optional_admin_text(
        req.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_notification_target(&session, req.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    let allowed = DISCORD_NOTIFICATION_KEYS
        .iter()
        .map(|(key, _, _)| *key)
        .collect::<std::collections::HashSet<_>>();
    let mut preferences = Vec::with_capacity(req.preferences.len());
    let mut seen = std::collections::HashSet::with_capacity(req.preferences.len());
    for pref in req.preferences {
        if let Err(response) =
            validate_admin_text(&pref.key, ADMIN_MAX_ACTION_BYTES, "notification key")
        {
            return response;
        }
        let key = pref.key.trim().to_string();
        if !seen.insert(key.clone()) {
            continue;
        }
        if !allowed.contains(key.as_str()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unknown notification key: {key}") })),
            )
                .into_response();
        }
        preferences.push((key, pref.enabled));
    }
    if let Err(e) = state
        .accounts_db
        .set_notification_preferences(&target, "discord", &preferences)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("save notification preference failed: {e}") })),
        )
            .into_response();
    }
    crate::logs::record_audit(
        "web",
        &session.account_id,
        "notification_preferences",
        Some(&target),
        Some("discord"),
        "更新 Discord 通知設定",
    );
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "account_id": target })),
    )
        .into_response()
}

const ADMIN_CLEAN_TIMETABLE_HTML: &str = include_str!("static/timetable.html");

async fn admin_clean_timetable_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(inject_theme(ADMIN_CLEAN_TIMETABLE_HTML)),
    )
        .into_response()
}

const ADMIN_STATIC_CACHE_CONTROL: &str = "public, max-age=604800, immutable, stale-while-revalidate=86400";

fn admin_static_asset(content_type: &'static str, body: impl IntoResponse) -> Response {
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(ADMIN_STATIC_CACHE_CONTROL),
    );
    response
}

async fn admin_theme_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::theme_css())
}

async fn admin_session_security_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::SESSION_SECURITY_JS,
    )
}

async fn admin_timetable_ux_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::TIMETABLE_UX_JS,
    )
}

async fn admin_dashboard_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::DASHBOARD_MODULE_CSS)
}

async fn admin_dashboard_runtime_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::DASHBOARD_RUNTIME_JS,
    )
}

async fn admin_dashboard_modules_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::DASHBOARD_FEATURES_JS,
    )
}

async fn admin_mobile_navigation_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::MOBILE_NAVIGATION_CSS)
}

async fn admin_mobile_timetable_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::MOBILE_TIMETABLE_CSS)
}

async fn admin_account_components_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::ACCOUNT_COMPONENTS_CSS)
}

async fn admin_maintenance_components_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::MAINTENANCE_COMPONENTS_CSS)
}

async fn admin_maintenance_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::MAINTENANCE_MODULE_JS,
    )
}

async fn admin_maintenance_implementation_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::MAINTENANCE_IMPLEMENTATION_JS,
    )
}

async fn admin_timetable_ui_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::TIMETABLE_UI_MODULE_JS,
    )
}

async fn admin_notifications_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::NOTIFICATIONS_MODULE_JS,
    )
}

async fn admin_mock_rollcall_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::MOCK_ROLLCALL_MODULE_JS,
    )
}

async fn admin_accounts_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::ACCOUNTS_MODULE_JS,
    )
}

async fn admin_core_api_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::CORE_API_JS)
}

async fn admin_root_redirect_handler(uri: axum::http::Uri) -> Response {
    let loc = match uri.query() {
        Some(q) if !q.is_empty() => format!("/admin?{q}"),
        _ => "/admin".to_string(),
    };
    (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, loc)]).into_response()
}

async fn admin_password_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(inject_theme(ADMIN_PASSWORD_HTML)),
    )
        .into_response()
}

async fn admin_passkey_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(inject_theme(ADMIN_PASSKEY_HTML)),
    )
        .into_response()
}

fn must_change_password_redirect() -> Response {
    (
        StatusCode::TEMPORARY_REDIRECT,
        [(header::LOCATION, "/admin/password?first=1")],
    )
        .into_response()
}

async fn session_must_change_password(
    state: &AdminWebState,
    headers: &HeaderMap,
    query_token: &str,
) -> bool {
    state
        .authorize_any(&session_token(headers, query_token))
        .await
        .map(|session| session.must_change_password)
        .unwrap_or(false)
}

async fn admin_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    if session_must_change_password(&state, &headers, &query.token).await {
        return must_change_password_redirect();
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(inject_theme(ADMIN_DASHBOARD_HTML)),
    )
        .into_response()
}

async fn admin_public_status_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // Shared cookie parser: also matches the hardened `__Host-` cookie used on
    // HTTPS / Cloudflare Tunnel deployments.
    let token = session_cookie_from_headers(&headers)
        .unwrap_or_else(|| query.token.trim().to_string());
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    let visible = resolve_visible_accounts(&state, &session);
    let total_accounts = visible.len();
    let statuses: Vec<_> = visible
        .into_iter()
        .map(|account| async move {
            let running = *account.is_running.lock().await;
            let detected = account
                .monitor_status
                .lock()
                .await
                .last_detected_timestamp
                .is_some();
            (running, detected)
        })
        .collect();
    let all = futures::future::join_all(statuses).await;
    let running_accounts = all.iter().filter(|(r, _)| *r).count();
    let detected_accounts = all.iter().filter(|(_, d)| *d).count();

    (
        StatusCode::OK,
        Json(json!(AdminPublicStatusResponse {
            total_accounts,
            running_accounts,
            detected_accounts,
        })),
    )
        .into_response()
}

async fn record_persistent_login_event(
    state: &AdminWebState,
    actor: &str,
    account_id: Option<&str>,
    action: &str,
    result: &str,
    details: &str,
    ip: &str,
) {
    let _ = state
        .accounts_db
        .record_activity_event(crate::db::ActivityEventInput {
            kind: "login".to_string(),
            actor: actor.to_string(),
            action: action.to_string(),
            account_id: account_id.unwrap_or_default().to_string(),
            path: String::new(),
            result: result.to_string(),
            details: details.to_string(),
            ip: ip.trim().to_string(),
        })
        .await;
}

async fn admin_login_activity_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminActivityQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }

    let limit = query.limit.unwrap_or(200).clamp(1, 1000) as i64;
    match state
        .accounts_db
        .list_activity_events(Some("login"), limit)
        .await
    {
        Ok(events) => (StatusCode::OK, Json(json!({ "events": events }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("activity database error: {e}") })),
        )
            .into_response(),
    }
}

struct PasskeyRegistrationCeremony {
    account_id: String,
    state: RegistrationState,
    expires_at: Instant,
}

struct PasskeyAuthenticationCeremony {
    account_id: String,
    state: AuthenticationState,
    expires_at: Instant,
}

static PASSKEY_REG_STATES: once_cell::sync::Lazy<
    Mutex<HashMap<String, PasskeyRegistrationCeremony>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
static PASSKEY_AUTH_STATES: once_cell::sync::Lazy<
    Mutex<HashMap<String, PasskeyAuthenticationCeremony>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
const MAX_PENDING_PASSKEY_STATES: usize = 128;
const MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT: usize = 32;

fn passkey_user_handle(account_id: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"tronclass-rollcall-passkey-user-handle-v1");
    hasher.update(account_id.as_bytes());
    hasher.finalize().to_vec()
}

fn passkey_webauthn_from_headers(headers: &HeaderMap) -> Result<Webauthn, Response> {
    for (name, max_bytes) in [
        (header::ORIGIN.as_str(), 512usize),
        (header::HOST.as_str(), 255usize),
        ("x-forwarded-proto", 16usize),
    ] {
        if headers
            .get(name)
            .map(|value| value.as_bytes().len() > max_bytes)
            .unwrap_or(false)
        {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("{name} header is too long") })),
            )
                .into_response());
        }
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            let host = headers.get(header::HOST)?.to_str().ok()?;
            let proto = headers
                .get("x-forwarded-proto")
                .and_then(|value| value.to_str().ok())
                .unwrap_or(
                    if host.starts_with("127.0.0.1") || host.starts_with("localhost") {
                        "http"
                    } else {
                        "https"
                    },
                );
            Some(format!("{proto}://{host}"))
        })
        .unwrap_or_else(|| "https://dashboard.jasoncloudserver.me".to_string());
    let parsed = match url::Url::parse(&origin) {
        Ok(parsed) => parsed,
        Err(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid Origin for passkey" })),
            )
                .into_response());
        }
    };
    let Some(rp_id) = parsed.host_str() else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing host for passkey rp_id" })),
        )
            .into_response());
    };
    Ok(Webauthn::new(rp_id, "Tronclass Rollcall", &origin)
        .authenticator_attachment(Attachment::Any)
        .require_user_verification(false))
}

async fn cleanup_passkey_states() {
    let now = Instant::now();
    PASSKEY_REG_STATES
        .lock()
        .await
        .retain(|_, state| state.expires_at > now);
    PASSKEY_AUTH_STATES
        .lock()
        .await
        .retain(|_, state| state.expires_at > now);
}

async fn admin_passkey_register_start_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if session.account_id == "<token-admin>" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "emergency token cannot register passkey; login as a real account first" })),
        )
            .into_response();
    }
    let wa = match passkey_webauthn_from_headers(&headers) {
        Ok(wa) => wa,
        Err(resp) => return resp,
    };

    let account = match state.accounts_db.get(&session.account_id).await {
        Ok(Some(account)) if account.enabled => account,
        Ok(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "account not found or disabled" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {e}") })),
            )
                .into_response();
        }
    };
    match state
        .accounts_db
        .count_passkey_credentials(&account.id)
        .await
    {
        Ok(count) if count >= MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT as i64 => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "too many passkeys for this account; max {}",
                        MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT
                    )
                })),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {e}") })),
            )
                .into_response();
        }
    }
    let existing_credentials = state
        .accounts_db
        .list_passkey_credentials(&account.id)
        .await
        .unwrap_or_default();
    let existing: Vec<CredentialId> = existing_credentials
        .iter()
        .map(|credential| credential.id.clone())
        .collect();
    let display_name = if account.id == account.username || account.username.trim().is_empty() {
        account.id.clone()
    } else {
        format!("{} ({})", account.id, account.username)
    };
    let (challenge, reg_state) = wa.start_registration(
        &passkey_user_handle(&account.id),
        &account.id,
        &display_name,
        &existing,
    );
    let ceremony_id = Uuid::new_v4().simple().to_string();
    cleanup_passkey_states().await;
    let mut pending = PASSKEY_REG_STATES.lock().await;
    if pending.len() >= MAX_PENDING_PASSKEY_STATES {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "too many pending passkey registrations; retry shortly" })),
        )
            .into_response();
    }
    pending.insert(
        ceremony_id.clone(),
        PasskeyRegistrationCeremony {
            account_id: account.id.clone(),
            state: reg_state,
            expires_at: Instant::now() + Duration::from_secs(300),
        },
    );
    drop(pending);
    (
        StatusCode::OK,
        Json(json!({ "ceremony_id": ceremony_id, "publicKey": challenge })),
    )
        .into_response()
}

async fn admin_passkey_register_finish_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(req): Json<AdminPasskeyRegisterFinishRequest>,
) -> Response {
    if let Err(response) =
        validate_admin_text(&req.ceremony_id, ADMIN_MAX_IDENTIFIER_BYTES, "ceremony_id")
    {
        return response;
    }
    let wa = match passkey_webauthn_from_headers(&headers) {
        Ok(wa) => wa,
        Err(resp) => return resp,
    };
    cleanup_passkey_states().await;
    let ceremony = match PASSKEY_REG_STATES.lock().await.remove(&req.ceremony_id) {
        Some(ceremony) if ceremony.expires_at > Instant::now() => ceremony,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "passkey registration expired; please try again" })),
            )
                .into_response();
        }
    };
    let credential = match wa.finish_registration(&ceremony.state, &req.response) {
        Ok(credential) => credential,
        Err(e) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": format!("passkey registration verify failed: {e}") })),
            )
                .into_response();
        }
    };
    let credential_id = credential.id.to_b64url();
    match state
        .accounts_db
        .upsert_passkey_credential(&ceremony.account_id, &credential)
        .await
    {
        Ok(()) => {
            crate::logs::record_audit(
                "web",
                &ceremony.account_id,
                "passkey_register",
                Some(&ceremony.account_id),
                Some(&credential_id),
                "Web 管理頁新增通行密鑰",
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "credential_id": credential_id })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("save passkey failed: {e}") })),
        )
            .into_response(),
    }
}

async fn resolve_passkey_login_account(
    state: &AdminWebState,
    username: &str,
) -> Result<crate::account::RawAccountConfig, Response> {
    let username = username.trim();
    if username.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "username is required" })),
        )
            .into_response());
    }
    let mut candidates = Vec::new();
    if let Ok(Some(account)) = state.accounts_db.get(username).await {
        candidates.push(account);
    }
    if candidates.is_empty() {
        if let Ok(accounts) = state.accounts_db.find_by_username(username, None).await {
            candidates = accounts;
        }
    }
    let enabled = candidates
        .into_iter()
        .filter(|account| account.enabled)
        .collect::<Vec<_>>();
    match enabled.as_slice() {
        [account] => Ok(account.clone()),
        [] => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "account not found or disabled" })),
        )
            .into_response()),
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "error": "multiple accounts match this username; please use account ID" }),
            ),
        )
            .into_response()),
    }
}

async fn admin_passkey_login_start_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(req): Json<AdminPasskeyLoginStartRequest>,
) -> Response {
    if let Err(response) = validate_admin_text(&req.username, ADMIN_MAX_USERNAME_BYTES, "username")
    {
        return response;
    }
    let ip = client_ip_from_headers(&headers);
    if !state
        .rate_limiter
        .check_limit(&format!("passkey-ip:{ip}"), IP_RATE_LIMIT_MAX_ATTEMPTS)
        .await
    {
        warn!(ip = %ip, "passkey login rate limited by ip");
        alert_abnormal_traffic(&state, "通行密鑰登入", &req.username, &ip).await;
        return rate_limited_response();
    }
    let account = match resolve_passkey_login_account(&state, &req.username).await {
        Ok(account) => account,
        Err(resp) => return resp,
    };
    let credentials = match state
        .accounts_db
        .list_passkey_credentials(&account.id)
        .await
    {
        Ok(credentials) => credentials,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {e}") })),
            )
                .into_response();
        }
    };
    if credentials.len() > MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "this account has too many stored passkeys; remove legacy entries before login" })),
        )
            .into_response();
    }
    if credentials.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "this account has no passkey; login with password then register one" })),
        )
            .into_response();
    }
    let wa = match passkey_webauthn_from_headers(&headers) {
        Ok(wa) => wa,
        Err(resp) => return resp,
    };
    let (challenge, auth_state) = wa
        .start_authentication_with_creds_for_user(&passkey_user_handle(&account.id), &credentials);
    let ceremony_id = Uuid::new_v4().simple().to_string();
    cleanup_passkey_states().await;
    let mut pending = PASSKEY_AUTH_STATES.lock().await;
    if pending.len() >= MAX_PENDING_PASSKEY_STATES {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "too many pending passkey logins; retry shortly" })),
        )
            .into_response();
    }
    pending.insert(
        ceremony_id.clone(),
        PasskeyAuthenticationCeremony {
            account_id: account.id,
            state: auth_state,
            expires_at: Instant::now() + Duration::from_secs(300),
        },
    );
    drop(pending);
    (
        StatusCode::OK,
        Json(json!({ "ceremony_id": ceremony_id, "publicKey": challenge })),
    )
        .into_response()
}

async fn admin_passkey_login_finish_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(req): Json<AdminPasskeyLoginFinishRequest>,
) -> Response {
    if let Err(response) =
        validate_admin_text(&req.ceremony_id, ADMIN_MAX_IDENTIFIER_BYTES, "ceremony_id")
    {
        return response;
    }
    if let Err(response) = validate_admin_text(&req.response.id, 4096, "credential id") {
        return response;
    }
    let ip = client_ip_from_headers(&headers);
    let wa = match passkey_webauthn_from_headers(&headers) {
        Ok(wa) => wa,
        Err(resp) => return resp,
    };
    cleanup_passkey_states().await;
    let ceremony = match PASSKEY_AUTH_STATES.lock().await.remove(&req.ceremony_id) {
        Some(ceremony) if ceremony.expires_at > Instant::now() => ceremony,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "passkey authentication expired; please try again" })),
            )
                .into_response();
        }
    };
    let mut credentials = match state
        .accounts_db
        .list_passkey_credentials(&ceremony.account_id)
        .await
    {
        Ok(credentials) => credentials,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {e}") })),
            )
                .into_response();
        }
    };
    if credentials.len() > MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "this account has too many stored passkeys; remove legacy entries before login" })),
        )
            .into_response();
    }
    let asserted_id = match CredentialId::from_b64url(&req.response.id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "bad passkey credential id" })),
            )
                .into_response();
        }
    };
    let Some(stored) = credentials
        .iter_mut()
        .find(|credential| credential.id == asserted_id)
    else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unknown passkey credential" })),
        )
            .into_response();
    };
    let outcome = match wa.finish_authentication(&ceremony.state, &req.response, stored) {
        Ok(outcome) => outcome,
        Err(e) => {
            record_persistent_login_event(
                &state,
                &ceremony.account_id,
                Some(&ceremony.account_id),
                "login_failed",
                "fail",
                "Web 管理頁通行密鑰驗證失敗",
                &ip,
            )
            .await;
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": format!("passkey verify failed: {e}") })),
            )
                .into_response();
        }
    };
    stored.counter = outcome.new_counter;
    let _ = state
        .accounts_db
        .upsert_passkey_credential(&ceremony.account_id, stored)
        .await;

    let account = match state.accounts_db.get(&ceremony.account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "account not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {e}") })),
            )
                .into_response();
        }
    };
    let account_id = account.id.clone();
    let is_admin = account.is_admin;
    let token = state
        .create_session(account_id.clone(), is_admin, false)
        .await;
    crate::logs::record_audit(
        "web",
        &account_id,
        "passkey_login",
        Some(&account_id),
        None,
        "Web 管理頁通行密鑰登入成功",
    );
    session_login_response(&headers, &token, account_id, is_admin, false)
}

fn login_client_identifier(headers: &HeaderMap) -> String {
    client_ip_from_headers(headers)
}

async fn admin_login_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(login): Json<AdminLoginRequest>,
) -> Response {
    if let Err(response) =
        validate_admin_text(&login.username, ADMIN_MAX_USERNAME_BYTES, "username")
    {
        return response;
    }
    if let Err(response) =
        validate_admin_text(&login.password, ADMIN_MAX_PASSWORD_BYTES, "password")
    {
        return response;
    }
    let ip = client_ip_from_headers(&headers);
    if !state
        .rate_limiter
        .check_limit(&format!("login-ip:{ip}"), IP_RATE_LIMIT_MAX_ATTEMPTS)
        .await
    {
        warn!(ip = %ip, "login rate limited by ip");
        alert_abnormal_traffic(&state, "密碼登入", "", &ip).await;
        return rate_limited_response();
    }

    let username = login.username.trim();
    // Passwords are opaque strings: do not trim user-entered whitespace.
    let password = login.password.as_str();
    if username.is_empty() || password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "username and password are required" })),
        )
            .into_response();
    }

    let client = login_client_identifier(&headers);
    let rate_key = format!("login:{}:{client}", username.to_lowercase());
    if !state.rate_limiter.check(&rate_key).await {
        warn!(username = %username, client = %client, "admin web login rate limited");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "too many attempts; please wait" })),
        )
            .into_response();
    }

    let mut candidates = Vec::new();
    // Try exact account-id match first, then fall back to username search.
    if let Ok(Some(account)) = state.accounts_db.get(username).await {
        candidates.push(account);
    }
    if candidates.is_empty() {
        if let Ok(accounts) = state.accounts_db.find_by_username(username, None).await {
            candidates = accounts;
        }
    }

    // Normal logins use the separate Dashboard verifier. If no Dashboard
    // password exists yet, allow the stored Tronclass password once and create
    // a restricted bootstrap session that can only set a new Dashboard password.
    let mut matched = None;
    for account in candidates {
        if !account.enabled {
            continue;
        }
        if !account.dashboard_password_hash.is_empty()
            && crate::dashboard_auth::verify_dashboard_password(
                password,
                &account.dashboard_password_hash,
            )
        {
            matched = Some((account, false));
            break;
        }
        if account.dashboard_password_hash.is_empty()
            && crate::dashboard_auth::verify_first_login_password(password, &account.password)
        {
            matched = Some((account, true));
            break;
        }
    }

    let Some((account, must_change_password)) = matched else {
        warn!(username = %username, client = %client, "admin web login failed");
        crate::logs::record_audit(
            "web",
            username,
            "login_failed",
            None,
            None,
            "Web 管理頁登入失敗",
        );
        record_persistent_login_event(
            &state,
            username,
            None,
            "login",
            "failed",
            "Web 管理頁登入失敗",
            &ip,
        )
        .await;
        // 連續密碼錯誤達門檻時，通知管理員與被嘗試登入的帳號持有人。
        alert_password_failures(&state, username, &ip).await;
        return unauthorized_response();
    };

    state.rate_limiter.clear(&rate_key).await;
    // 登入成功即重置失敗計數，確保只有「連續」失敗才會觸發告警。
    clear_auth_failures(username, &ip).await;
    let account_id = account.id.clone();
    let is_admin = account.is_admin;
    let session_token = state
        .create_session(account_id.clone(), is_admin, must_change_password)
        .await;
    let login_action = if must_change_password {
        "login_bootstrap"
    } else {
        "login"
    };
    let login_message = if must_change_password {
        "首次使用 Tronclass 密碼登入；必須立即設定新的 Dashboard 密碼"
    } else if is_admin {
        "Web 管理頁管理員登入成功"
    } else {
        "Web 管理頁登入成功"
    };
    info!(account = %account_id, is_admin = is_admin, bootstrap = must_change_password, client = %client, "admin web login success");
    crate::logs::record_audit(
        "web",
        &account_id,
        login_action,
        Some(&account_id),
        None,
        login_message,
    );
    record_persistent_login_event(
        &state,
        &account_id,
        Some(&account_id),
        login_action,
        "success",
        login_message,
        &ip,
    )
    .await;
    session_login_response(
        &headers,
        &session_token,
        account_id,
        is_admin,
        must_change_password,
    )
}

fn session_token(headers: &HeaderMap, query_token: &str) -> String {
    // Must go through the shared parser so the hardened `__Host-` cookie used on
    // HTTPS deployments is recognised too.
    session_cookie_from_headers(headers).unwrap_or_else(|| query_token.trim().to_string())
}

async fn admin_logout_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
) -> Response {
    if let Some(session) = state.authorize(&query.token).await {
        if session.account_id != "<token-admin>" {
            state.revoke_session(&query.token).await;
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "logout",
                Some(&session.account_id),
                None,
                "Web 管理頁登出",
            );
        }
    }

    fb_sso::append_expired_fb_sso_cookie(append_expired_session_cookies(
        (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
    ))
}

fn password_json_pick(value: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(|v| v.as_str()) {
            return text.to_string();
        }
    }
    String::new()
}

fn password_change_from_body(body: &[u8]) -> AdminPasswordChangeRequest {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).unwrap_or(serde_json::json!({}));
    let new_password =
        password_json_pick(&parsed, &["new_password", "password", "newPassword", "new"]);
    let current_password = password_json_pick(
        &parsed,
        &[
            "current_password",
            "old_password",
            "currentPassword",
            "oldPassword",
        ],
    );
    AdminPasswordChangeRequest {
        current_password,
        new_password,
    }
}

async fn admin_password_change_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let request = password_change_from_body(&body);
    if let Err(response) = validate_admin_text(
        &request.current_password,
        ADMIN_MAX_PASSWORD_BYTES,
        "current password",
    ) {
        return response;
    }
    if let Err(response) = validate_admin_text(
        &request.new_password,
        ADMIN_MAX_PASSWORD_BYTES,
        "new password",
    ) {
        return response;
    }
    let Some(session) = state.authorize_any(&query.token).await else {
        return unauthorized_response();
    };
    if session.account_id == "<token-admin>" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "emergency token cannot change a Dashboard password" })),
        )
            .into_response();
    }

    let account = match state.accounts_db.get(&session.account_id).await {
        Ok(Some(account)) if account.enabled => account,
        Ok(_) => return unauthorized_response(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {error}") })),
            )
                .into_response();
        }
    };
    let first_setup =
        session.must_change_password || account.dashboard_password_hash.trim().is_empty();

    // The first-login session has already verified the upstream Tronclass
    // password. It may set a Dashboard password without asking the user to
    // submit that upstream password a second time. Every normal password change
    // still requires the current Dashboard password.
    if !first_setup
        && (!session.must_change_password
            && (account.dashboard_password_hash.is_empty()
                || !crate::dashboard_auth::verify_dashboard_password(
                    request.current_password.as_str(),
                    &account.dashboard_password_hash,
                )))
    {
        crate::logs::record_audit(
            "web",
            &account.id,
            "dashboard_password_change_failed",
            Some(&account.id),
            None,
            "Dashboard 密碼變更驗證失敗",
        );
        return unauthorized_response();
    }

    let hash = match crate::dashboard_auth::hash_dashboard_password(
        &request.new_password,
        &account.password,
    ) {
        Ok(hash) => hash,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };
    match state
        .accounts_db
        .set_dashboard_password_hash(&account.id, &hash)
        .await
    {
        Ok(true) => {}
        Ok(false) => return unauthorized_response(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("save password failed: {error}") })),
            )
                .into_response();
        }
    }

    // Revoke all prior sessions, including this one, then issue a fresh cookie.
    state.revoke_account_sessions(&account.id).await;
    let session_token = state
        .create_session(account.id.clone(), account.is_admin, false)
        .await;
    crate::logs::record_audit(
        "web",
        &account.id,
        "dashboard_password_change",
        Some(&account.id),
        None,
        "Dashboard 密碼已變更，先前登入工作階段已失效",
    );
    session_login_response(
        &headers,
        &session_token,
        account.id,
        account.is_admin,
        false,
    )
}

fn resolve_visible_accounts<'a>(
    state: &'a AdminWebState,
    session: &AdminSession,
) -> Vec<&'a RequestAccountState> {
    if session.is_admin {
        state.accounts.iter().collect()
    } else {
        state
            .account_by_id(&session.account_id)
            .into_iter()
            .collect()
    }
}

fn unauthorized_response() -> Response {
    warn!("admin web unauthorized request");
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tempfile::NamedTempFile;
    use tower::ServiceExt;

    #[tokio::test]
    async fn legacy_access_token_is_not_a_web_login_bypass() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("1234567890", vec![], db, None, None, 10);
        assert!(state.authorize("1234567890").await.is_none());
    }

    #[tokio::test]
    async fn http_only_cookie_authenticates_legacy_marker_requests() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let session_token = state
            .create_session("cookie-user".to_string(), false, false)
            .await;
        let app = build_admin_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/status?token=cookie")
                    .header(
                        header::COOKIE,
                        format!("{SECURE_SESSION_COOKIE}={session_token}"),
                    )
                    .header("x-forwarded-proto", "https")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn password_login_requires_dashboard_hash_and_sets_secure_cookie() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let dashboard_password = "separate dashboard password 2026";
        let provider_password = "upstream-tronclass-password";
        let dashboard_password_hash =
            crate::dashboard_auth::hash_dashboard_password(dashboard_password, provider_password)
                .unwrap();
        db.insert(&RawAccountConfig {
            id: "student-a".to_string(),
            provider: "fju".to_string(),
            username: "student-a".to_string(),
            password: provider_password.to_string(),
            dashboard_password_hash,
            enabled: true,
            line_user_id: String::new(),
            discord_user_id: String::new(),
            session_cookie: String::new(),
            is_admin: false,
        })
        .await
        .unwrap();
        let state = AdminWebState::new("legacy-token", vec![], db, None, None, 10);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));

        let response = admin_login_handler(
            State(state.clone()),
            headers.clone(),
            Json(AdminLoginRequest {
                username: "student-a".to_string(),
                password: dashboard_password.to_string(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let set_cookie = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .find_map(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(set_cookie.starts_with("__Host-tronclass_admin_session="));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("Secure"));
        assert!(set_cookie.contains("SameSite=Strict"));

        let rejected = admin_login_handler(
            State(state),
            headers,
            Json(AdminLoginRequest {
                username: "student-a".to_string(),
                // The upstream provider password must no longer authenticate
                // the public Dashboard.
                password: provider_password.to_string(),
            }),
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    }
}
