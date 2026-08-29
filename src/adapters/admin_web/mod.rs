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
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        Html, IntoResponse, Redirect, Response,
    },
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
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::account::RawAccountConfig;
use crate::adapters::events::{
    AdapterMessenger, NumberCodeRequest, OutboundMessage, QrCodeRequest, RollcallHistoryEntry,
    SecurityAlertEvent,
};
use crate::adapters::requests::{NumberCodeInput, RequestAccountState};
use crate::adapters::scanner::QrScannerRegistry;
use crate::db::{AccountsDb, MaterialEventInput};
use crate::runtime_settings::{RuntimeRestPeriod, RuntimeSettings};
use crate::trusted_ip::ClientIp;

mod actions;
mod auth;
mod blacklist;
mod class_reminder;
mod detection_schedule;
mod files;
mod homework;
mod notifications;
mod pages;
mod passkey;
mod static_assets;
mod status;
mod timetable;
mod web_push;
pub mod materials_webdav;
pub mod oidc;
mod fb_sso;
mod health;
mod logs;
mod mock;
mod pwa;
mod utils;
use self::pages::{load_runtime_theme, ADMIN_BLOCKED_HTML};
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

/// Short-lived access token lifetime (30 min). A long-lived refresh token is
/// used to mint a new access token, so a stolen access cookie is only usable
/// briefly.
const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(30 * 60);
/// Long-lived refresh token lifetime (7 days). Each refresh rotates it.
const REFRESH_TOKEN_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_SESSIONS: usize = 500;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
const RATE_LIMIT_MAX_ATTEMPTS: u32 = 5;
const IP_RATE_LIMIT_MAX_ATTEMPTS: u32 = 20;
const MAX_RATE_LIMIT_KEYS: usize = 4096;
const MAX_SECURITY_WATCH_KEYS: usize = 4096;
const SECURITY_FAIL_ALERT_THRESHOLD: u32 = 5;
/// A source IP is automatically blocked on its sixth failed password login
/// within the existing 60-second security window ("more than five" failures).
const SECURITY_AUTO_BLOCK_THRESHOLD: u32 = SECURITY_FAIL_ALERT_THRESHOLD + 1;
const SECURITY_ALERT_COOLDOWN: Duration = Duration::from_secs(10 * 60);
const SESSION_COOKIE_MARKER: &str = "cookie";
const SECURE_SESSION_COOKIE: &str = "__Host-tronclass_admin_session";
const LOCAL_SESSION_COOKIE: &str = "tronclass_admin_session";
/// HttpOnly access-token cookie lifetime (matches ACCESS_TOKEN_TTL).
const SESSION_COOKIE_MAX_AGE: u64 = 30 * 60;
/// HttpOnly refresh-token cookie name and lifetime.
const SECURE_REFRESH_COOKIE: &str = "__Host-tronclass_admin_refresh";
const LOCAL_REFRESH_COOKIE: &str = "tronclass_admin_refresh";
const REFRESH_COOKIE_MAX_AGE: u64 = 7 * 24 * 60 * 60;

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

fn rate_limited_response() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "error": "too many attempts; please wait" })),
    )
        .into_response()
}

fn blocked_ip_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "access_denied" })),
    )
        .into_response()
}

/// Browser navigations receive an explanation page, while API callers retain
/// the compact JSON error contract expected by the dashboard frontend.
fn blacklisted_browser_response() -> Response {
    let mut response = (StatusCode::FORBIDDEN, Html(ADMIN_BLOCKED_HTML)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn request_prefers_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(',').any(|part| part.trim().starts_with("text/html")))
        .unwrap_or(false)
}

async fn ip_is_blacklisted(state: &AdminWebState, ip: &str) -> bool {
    if ip == "unknown" {
        return false;
    }
    match state.accounts_db.is_ip_blacklisted(ip).await {
        Ok(blocked) => blocked,
        Err(error) => {
            warn!(ip = %ip, error = %error, "讀取 IP 黑名單失敗，暫不拒絕請求");
            false
        }
    }
}

async fn reject_blacklisted_ip_middleware(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path();
    // Static assets and logout remain available so a blocked browser can
    // finish clearing its session. Authentication, Dashboard, Filebrowser SSO
    // checks and all management APIs are denied.
    if path.starts_with("/admin/static/")
        || path == "/admin/pwa/icon.svg"
        || path == "/healthz"
        || path == "/admin/api/logout"
    {
        return next.run(request).await;
    }
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip());
    let trusted = crate::trusted_ip::trusted_from_global_config();
    let ip = crate::trusted_ip::resolve_client_ip(peer, &headers, &trusted);
    if ip_is_blacklisted(&state, &ip).await {
        // Do not turn JSON API failures into HTML: frontend fetch handlers
        // depend on `{ "error": "access_denied" }`. Normal document
        // navigations, such as `/admin?next=...`, get the explanatory page.
        if !path.starts_with("/admin/api/") && request_prefers_html(&headers) {
            return blacklisted_browser_response();
        }
        return blocked_ip_response();
    }
    next.run(request).await
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

async fn notify_admins_security_alert(state: &AdminWebState, event: SecurityAlertEvent) {
    let Some(messenger) = state.messenger.clone() else {
        warn!("security alert skipped: no Discord/LINE messenger");
        return;
    };
    let recipients = admin_discord_recipients(state).await;
    if recipients.is_empty() {
        warn!("security alert skipped: no admin Discord user id");
        return;
    }
    let message = OutboundMessage::SecurityAlert(event);
    for to in recipients {
        if let Err(error) = messenger.push(&to, &message).await {
            warn!(to = %to, error = %error, "security alert notification 推送失敗");
        }
    }
}

async fn alert_password_failures(state: &AdminWebState, username: &str, ip: &str) {
    let (user_fails, ip_fails) = record_auth_failure(username, ip).await;
    // 解析被嘗試登入的帳號持有人綁定（可能為空：隨機 username 或未綁定）。
    let (target_discord, target_line) = resolve_login_target_contacts(state, username).await;

    // The blacklist is IP-based, so only the source-IP counter can trigger an
    // automatic block. The counter is scoped to RATE_LIMIT_WINDOW (60 seconds)
    // by record_auth_failure; a sixth failure is therefore "超過五次".
    if ip_fails >= SECURITY_AUTO_BLOCK_THRESHOLD {
        let normalized_ip = normalize_admin_ip(ip);
        if let Some(ref blocked_ip) = normalized_ip {
            match state
                .accounts_db
                .block_ip_with_failure_count(
                    blocked_ip,
                    "system:auto-login-failure",
                    "Automatic block: more than five failed Dashboard logins within 60 seconds",
                    ip_fails,
                )
                .await
            {
                Ok(true) => {
                    let owner_account_id = state.accounts_db.get(username).await.ok().flatten()
                        .map(|account| account.id)
                        .unwrap_or_else(|| username.trim().to_string());
                    if let Err(error) = state.accounts_db.set_auto_block_owner(blocked_ip, &owner_account_id).await {
                        warn!(ip = %blocked_ip, error = %error, "automatic blacklist owner record failed");
                    }
                    warn!(ip = %blocked_ip, ip_fails, "IP automatically blacklisted after repeated Dashboard login failures");
                    crate::logs::record_audit(
                        "web",
                        "system:auto-login-failure",
                        "ip_blacklist_auto_block",
                        None,
                        Some(blocked_ip),
                        "Automatic block after more than five failed Dashboard logins within 60 seconds",
                    );
                    let _ = state
                        .accounts_db
                        .record_activity_event(crate::db::ActivityEventInput {
                            kind: "security".to_string(),
                            actor: "system:auto-login-failure".to_string(),
                            action: "ip_blacklist_auto_block".to_string(),
                            account_id: username.trim().to_string(),
                            path: blocked_ip.clone(),
                            result: "success".to_string(),
                            details: format!("Auto-blocked after {ip_fails} failed Dashboard logins within 60 seconds"),
                            ip: blocked_ip.clone(),
                        })
                        .await;
                    // This notification deliberately has no alert cooldown:
                    // block_ip returning true makes it a one-time state change.
                    let event = SecurityAlertEvent {
                        username: username.trim().to_string(),
                        ip: blocked_ip.clone(),
                        user_failures: user_fails,
                        ip_failures: ip_fails,
                        timestamp: security_now_label(),
                        account_id: owner_account_id,
                        ip_is_blacklisted: true,
                        allow_owner_unblock: true,
                    };
                    notify_admins_security_alert(state, event.clone()).await;
                    // The account holder receives the same Discord component,
                    // but the handler only permits removal of this account's
                    // own automatic block.
                    if let (Some(messenger), Some(discord_id)) =
                        (state.messenger.as_ref(), target_discord.as_deref())
                    {
                        let _ = messenger
                            .push(discord_id, &OutboundMessage::SecurityAlert(event))
                            .await;
                    }
                }
                Ok(false) => {}
                Err(error) => warn!(ip = %blocked_ip, error = %error, "automatic IP blacklist write failed"),
            }
        }
    }

    if user_fails >= SECURITY_FAIL_ALERT_THRESHOLD {
        let key = format!("fail-user:{}", username.trim().to_lowercase());
        if security_should_alert(&key).await {
            notify_admins_security_alert(
                state,
                SecurityAlertEvent {
                    username: username.trim().to_string(),
                    ip: ip.trim().to_string(),
                    user_failures: user_fails,
                    ip_failures: ip_fails,
                    timestamp: security_now_label(),
                    ip_is_blacklisted: false,
                    account_id: username.trim().to_string(),
                    allow_owner_unblock: false,
                },
            )
            .await;

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
            notify_admins_security_alert(
                state,
                SecurityAlertEvent {
                    username: username.trim().to_string(),
                    ip: ip.trim().to_string(),
                    user_failures: user_fails,
                    ip_failures: ip_fails,
                    timestamp: security_now_label(),
                    ip_is_blacklisted: false,
                    account_id: username.trim().to_string(),
                    allow_owner_unblock: false,
                },
            )
            .await;

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

/// A freshly issued token pair. The access token authorizes API calls
/// (short-lived); the refresh token rotates to mint new access tokens.
#[derive(Debug, Clone)]
struct SessionTokens {
    access: String,
    refresh: String,
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

        // Background session cleanup every 5 minutes & initial database session hydration
        let sessions_clone = Arc::clone(&sessions);
        let accounts_db_clone = accounts_db.clone();
        tokio::spawn(async move {
            let now_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let _ = accounts_db_clone.delete_expired_dashboard_sessions(now_unix).await;
            if let Ok(existing) = accounts_db_clone.list_valid_dashboard_sessions(now_unix).await {
                let mut guard = sessions_clone.write().await;
                for row in existing {
                    let remaining_secs = (row.expires_at - now_unix).max(0) as u64;
                    if remaining_secs > 0 {
                        guard.insert(
                            row.token,
                            AdminSession {
                                account_id: row.account_id,
                                is_admin: row.is_admin,
                                must_change_password: row.must_change_password,
                                expires_at: Instant::now() + Duration::from_secs(remaining_secs),
                            },
                        );
                    }
                }
            }

            loop {
                tokio::time::sleep(Duration::from_secs(300)).await;
                sessions_clone
                    .write()
                    .await
                    .retain(|_, s| s.expires_at > Instant::now());
                let now_unix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                let _ = accounts_db_clone.delete_expired_dashboard_sessions(now_unix).await;
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

    /// 從監控器記憶體中的 `MonitorStatus` 取出已驗證的 user_name。
    ///
    /// 監控器啟動時已完成登入並存下顯示名稱，因此不需要為了顯示名字
    /// 而多打一次 `/api/profile`。
    async fn lookup_monitor_user_name(&self, account_id: &str) -> Option<String> {
        let account = self.account_by_id(account_id)?;
        let user_name = account.monitor_status.lock().await.user_name.clone();
        if user_name.trim().is_empty() {
            None
        } else {
            Some(user_name)
        }
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
            sessions.get(token).cloned()
        };

        if let Some(session) = session {
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
                let db = self.accounts_db.clone();
                let tok = token.to_string();
                tokio::spawn(async move {
                    let _ = db.delete_dashboard_session(&tok).await;
                });
            }
            return None;
        }

        // 快取未命中（例如重啟後非同步載入尚未完成或已被淘汰）：查詢 SQLite 資料庫
        if let Ok(Some(row)) = self.accounts_db.get_dashboard_session(token).await {
            let now_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if row.expires_at > now_unix {
                let remaining_secs = (row.expires_at - now_unix) as u64;
                let session = AdminSession {
                    account_id: row.account_id,
                    is_admin: row.is_admin,
                    must_change_password: row.must_change_password,
                    expires_at: Instant::now() + Duration::from_secs(remaining_secs),
                };
                let mut sessions = self.sessions.write().await;
                sessions.insert(token.to_string(), session.clone());
                return Some(session);
            } else {
                let db = self.accounts_db.clone();
                let tok = token.to_string();
                tokio::spawn(async move {
                    let _ = db.delete_dashboard_session(&tok).await;
                });
            }
        }

        None
    }

    async fn revoke_sessions_for_account(&self, account_id: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, session| session.account_id != account_id);
        if let Err(e) = self
            .accounts_db
            .delete_dashboard_sessions_by_account(account_id)
            .await
        {
            warn!(error = %e, "從資料庫刪除帳號 Dashboard sessions 失敗");
        }
    }

    async fn create_session(
        &self,
        account_id: String,
        is_admin: bool,
        must_change_password: bool,
    ) -> SessionTokens {
        let access = Uuid::new_v4().simple().to_string();
        let refresh = Uuid::new_v4().simple().to_string();
        let session = AdminSession {
            account_id: account_id.clone(),
            is_admin,
            must_change_password,
            expires_at: Instant::now() + ACCESS_TOKEN_TTL,
        };

        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let access_expires_at = now_unix + ACCESS_TOKEN_TTL.as_secs() as i64;
        let refresh_expires_at = now_unix + REFRESH_TOKEN_TTL.as_secs() as i64;

        if let Err(e) = self
            .accounts_db
            .insert_dashboard_session(
                &access,
                &account_id,
                is_admin,
                must_change_password,
                access_expires_at,
                &refresh,
                refresh_expires_at,
            )
            .await
        {
            warn!(error = %e, "無法將 Dashboard session 寫入資料庫");
        }

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
        sessions.insert(access.clone(), session);
        SessionTokens { access, refresh }
    }

    /// Rotate a refresh token into a fresh access+refresh pair.
    ///
    /// Validates the refresh token (still present in DB and not expired),
    /// then issues a new access token and a **new** refresh token, revoking the
    /// old one so a reused refresh token cannot be replayed after rotation.
    async fn rotate_session(&self, refresh_token: &str) -> Option<SessionTokens> {
        let refresh_token = refresh_token.trim();
        // REFRESH_TOKEN_STRICT_FORMAT_V1 (M-2): reject malformed values here as
        // well as at the cookie reader. `admin_session_refresh_handler` also
        // accepts a `?token=` fallback for cookie-less clients, so the cookie
        // check alone does not cover every way a value reaches this function.
        //
        // This also stops a legacy row whose `refresh_token` column is the
        // schema default `''` from ever being matched (the empty-string guard
        // this replaces existed for exactly that reason, and is subsumed here).
        if !is_valid_session_token_format(refresh_token) {
            return None;
        }
        let row = self
            .accounts_db
            .get_dashboard_session_by_refresh(refresh_token)
            .await
            .ok()??;
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if row.refresh_expires_at <= now_unix {
            // Expired refresh token: revoke it.
            let _ = self
                .accounts_db
                .delete_dashboard_session_by_refresh(refresh_token)
                .await;
            return None;
        }

        // Rotate: revoke the old access token's in-memory session too.
        self.sessions.write().await.remove(&row.token);

        let tokens = self
            .create_session(row.account_id.clone(), row.is_admin, row.must_change_password)
            .await;

        // Revoke the old row (identified by its refresh token) so the old
        // refresh token cannot be used again.
        let _ = self
            .accounts_db
            .delete_dashboard_session_by_refresh(refresh_token)
            .await;

        Some(tokens)
    }

    async fn revoke_session(&self, token: &str) {
        let token_trimmed = token.trim();
        self.sessions.write().await.remove(token_trimmed);
        if let Err(e) = self.accounts_db.delete_dashboard_session(token_trimmed).await {
            warn!(error = %e, "從資料庫刪除 Dashboard session 失敗");
        }
    }

    /// Password changes invalidate every Dashboard session for the account.
    async fn revoke_account_sessions(&self, account_id: &str) {
        self.revoke_sessions_for_account(account_id).await;
    }
}

pub fn build_admin_router(state: AdminWebState) -> Router {
    let password_guard_state = state.clone();
    let blacklist_guard_state = state.clone();
    Router::new()
        .route("/", get(pages::admin_root_redirect_handler))
        .route("/healthz", get(health::healthz_handler))
        .route("/manifest.webmanifest", get(pwa::manifest_handler))
        .route("/service-worker.js", get(pwa::service_worker_handler))
        .route("/admin/pwa/icon.svg", get(pwa::icon_handler))
        .route("/admin/static/theme.css", get(static_assets::admin_theme_css_handler))
        .route(
            "/admin/static/session-security.js",
            get(static_assets::admin_session_security_js_handler),
        )
        .route("/admin/static/core-api.js", get(static_assets::admin_core_api_js_handler))
        .route("/admin/static/web-push.js", get(static_assets::admin_web_push_js_handler))
        .route(
            "/admin/static/js/dompurify.min.js",
            get(static_assets::admin_dompurify_js_handler),
        )
        .route(
            "/admin/static/js/trusted-types-init.js",
            get(static_assets::admin_trusted_types_init_js_handler),
        )
        .route(
            "/admin/static/accounts.js",
            get(static_assets::admin_accounts_module_js_handler),
        )
        .route(
            "/admin/static/mock-rollcall.js",
            get(static_assets::admin_mock_rollcall_module_js_handler),
        )
        .route(
            "/admin/static/notifications.js",
            get(static_assets::admin_notifications_module_js_handler),
        )
        .route(
            "/admin/static/timetable.js",
            get(static_assets::admin_timetable_ui_module_js_handler),
        )
        .route(
            "/admin/static/maintenance-implementation.js",
            get(static_assets::admin_maintenance_implementation_js_handler),
        )
        .route(
            "/admin/static/maintenance.js",
            get(static_assets::admin_maintenance_module_js_handler),
        )
        .route(
            "/admin/static/maintenance-components.css",
            get(static_assets::admin_maintenance_components_css_handler),
        )
        .route(
            "/admin/static/account-components.css",
            get(static_assets::admin_account_components_css_handler),
        )
        .route(
            "/admin/static/mobile-timetable.css",
            get(static_assets::admin_mobile_timetable_css_handler),
        )
        .route(
            "/admin/static/mobile-navigation.css",
            get(static_assets::admin_mobile_navigation_css_handler),
        )
        // DASHBOARD_MODULE_LOADER_PHASE1
        .route(
            "/admin/static/dashboard.css",
            get(static_assets::admin_dashboard_css_handler),
        )
        .route(
            "/admin/static/dashboard-late-fixes.css",
            get(static_assets::admin_dashboard_late_fixes_css_handler),
        )
        .route(
            "/admin/static/dashboard-mobile.css",
            get(static_assets::admin_dashboard_mobile_css_handler),
        )
        .route(
            "/admin/static/dashboard-components.css",
            get(static_assets::admin_dashboard_components_css_handler),
        )
        .route(
            "/admin/static/dashboard-access.css",
            get(static_assets::admin_dashboard_access_css_handler),
        )
        .route(
            "/admin/static/dashboard-runtime.js",
            get(static_assets::admin_dashboard_runtime_handler),
        )
        .route(
            "/admin/static/dashboard-modules.js",
            get(static_assets::admin_dashboard_modules_handler),
        )
        .route("/admin", get(pages::admin_page_handler))
        .route("/admin/activity", get(pages::admin_activity_page_handler))
        .route("/admin/blacklist", get(blacklist::admin_blacklist_page_handler))
        .route("/admin/homework", get(pages::admin_homework_page_handler))
        .route("/homework", get(pages::admin_homework_page_handler))
        .route("/admin/dashboard", get(pages::admin_dashboard_page_handler))
        // "我的頁面" is linked from the header of three pages and from the PWA
        // manifest shortcut, but had no route and returned 404. The dashboard
        // already narrows itself to the session's own account for non-admins,
        // so it is the correct destination.
        .route("/admin/me", get(pages::admin_dashboard_page_handler))
        .route(
            "/admin/detection-schedule",
            get(detection_schedule::page_handler),
        )
        .route("/admin/push-status", get(web_push::page_handler))
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
        .route("/admin/password", get(pages::admin_password_page_handler))
        .route("/admin/passkey", get(pages::admin_passkey_page_handler))
        .route("/admin/logs", get(logs::admin_logs_page_handler))
        .route("/admin/api/public_status", get(status::admin_public_status_handler))
        .route("/admin/api/blacklist", get(blacklist::admin_blacklist_list_handler))
        .route(
            "/admin/api/blacklist/block",
            post(blacklist::admin_blacklist_block_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/blacklist/unblock",
            post(blacklist::admin_blacklist_unblock_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route("/admin/api/health", get(health::admin_health_api_handler))
        .route(
            "/admin/api/health/check",
            get(health::admin_health_check_handler),
        )
        // Caddy uses this authenticated GET route only for Filebrowser's
        // initial HTML shell, allowing the Rust Dashboard to add a return link.
        .route(
            "/admin/api/filebrowser-shell",
            get(files::admin_filebrowser_shell_handler),
        )
        .route(
            "/admin/api/filebrowser/ready",
            get(files::admin_filebrowser_ready_handler),
        )
        .route(
            "/share/{*path}",
            get(files::admin_filebrowser_share_shell_handler),
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
            "/admin/api/files/rmdir",
            post(files::admin_files_rmdir_handler)
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
            post(auth::admin_login_handler).layer(DefaultBodyLimit::max(ADMIN_LOGIN_BODY_LIMIT)),
        )
        .route(
            "/admin/api/logout",
            post(auth::admin_logout_handler).layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/password",
            post(auth::admin_password_change_handler)
                .layer(DefaultBodyLimit::max(ADMIN_LOGIN_BODY_LIMIT)),
        )
        .route(
            "/admin/api/passkey/register/start",
            post(passkey::admin_passkey_register_start_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/passkey/register/finish",
            post(passkey::admin_passkey_register_finish_handler)
                .layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .route(
            "/admin/api/passkey/login/start",
            post(passkey::admin_passkey_login_start_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/passkey/login/finish",
            post(passkey::admin_passkey_login_finish_handler)
                .layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .route(
            "/admin/api/activity/login",
            get(auth::admin_login_activity_handler),
        )
        .route("/admin/api/push/config", get(web_push::config_handler))
        .route(
            "/admin/api/push/subscribe",
            post(web_push::subscribe_handler).layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .route(
            "/admin/api/push/unsubscribe",
            post(web_push::unsubscribe_handler).layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route("/admin/api/push/test", post(web_push::test_handler))
        .route(
            "/admin/api/homework/test-notification",
            post(homework::admin_homework_test_notification_handler),
        )
        .route("/admin/api/status", get(status::admin_status_handler))
        .route(
            "/admin/api/rollcall/events",
            get(status::admin_rollcall_events_handler),
        )
        .route(
            "/admin/api/class_reminder",
            get(class_reminder::admin_class_reminder_get_handler)
                .post(class_reminder::admin_class_reminder_update_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/detection-schedule",
            get(detection_schedule::get_handler)
                .post(detection_schedule::update_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/notifications/preferences",
            get(notifications::admin_notification_preferences_get_handler)
                .post(notifications::admin_notification_preferences_update_handler)
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
        // TIMETABLE_AUTO_SYNC_V1
        // 讀取狀態走 GET；會產生對外連線或改變狀態的操作走 POST，
        // 因此自動受既有的 same-origin CSRF 檢查保護。
        .route(
            "/admin/api/timetable/sync",
            get(timetable::admin_timetable_sync_status_handler),
        )
        .route(
            "/admin/api/timetable/sync/toggle",
            post(timetable::admin_timetable_sync_toggle_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/timetable/sync/run",
            post(timetable::admin_timetable_sync_run_handler)
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
            "/admin/api/homework",
            get(homework::admin_homework_list_handler),
        )
        .route(
            "/admin/api/homework/complete",
            post(homework::admin_homework_complete_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/session/refresh",
            post(auth::admin_session_refresh_handler).layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/timetable",
            axum::routing::get(pages::admin_clean_timetable_page_handler),
        )
        .layer(axum::middleware::from_fn(admin_security_headers_middleware))
        .layer(axum::middleware::from_fn_with_state(
            blacklist_guard_state,
            reject_blacklisted_ip_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            password_guard_state,
            require_dashboard_password_change_middleware,
        ))
        .layer(axum::middleware::from_fn(admin_same_origin_middleware))
        .layer(axum::middleware::from_fn(
            session_cookie_to_query_middleware,
        ))
        .layer(axum::middleware::from_fn(https_redirect_middleware))
        // HTTPS_TRUSTED_PROXY_ONLY_V1 (H-4): outermost admin layer, so every
        // middleware and handler below sees forwarded headers only when they
        // came from a trusted proxy hop.
        .layer(axum::middleware::from_fn(
            sanitize_forwarded_headers_middleware,
        ))
        .layer(
            // The dashboard HTML is ~260 KB of inline script/CSS; it compresses
            // to roughly a tenth of that. Brotli beats gzip on this kind of
            // text payload, so offer it first and keep gzip as the fallback.
            tower_http::compression::CompressionLayer::new()
                .br(true)
                .gzip(true)
                .deflate(true),
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
        // HTTPS_TRUSTED_PROXY_ONLY_V1 (H-4): `sanitize_forwarded_headers_middleware`
        // has already dropped `x-forwarded-proto` for untrusted peers, so any
        // value seen here came from a trusted hop.
        //
        // An absent header therefore means "a trusted hop did not declare a
        // scheme", which in practice is direct loopback access during local
        // development. Redirecting that to https:// would break dev (nothing
        // serves TLS on that port) without protecting anyone: an untrusted
        // client that reached us in plaintext has already sent its request, and
        // the cookie hardening in `request_uses_https` is what actually keeps a
        // session off a plaintext channel. So absence still means "do not
        // redirect" — but it is now a deliberate, narrow case rather than a
        // header an attacker can influence.
        //
        // The comparison is first-hop, trimmed and case-insensitive; the
        // previous exact `== "http"` silently failed to redirect for values
        // such as `HTTP` or `http, https`.
        let is_plain_http = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("http"));

        if is_plain_http {
            // Build the redirect target from the forwarded/Host value, but only
            // when it is a plausible authority. The Location header is a 301
            // (cacheable), so a value containing a path, scheme or credentials
            // must never be interpolated verbatim.
            let host = headers
                .get("x-forwarded-host")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .or_else(|| headers.get(header::HOST).and_then(|v| v.to_str().ok()))
                .map(str::trim)
                .filter(|value| is_safe_redirect_authority(value));

            let Some(host) = host else {
                // No usable host: fall through rather than emit a redirect to
                // an attacker-shaped authority.
                return next.run(request).await;
            };

            let https_uri = format!(
                "https://{host}{}",
                uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("")
            );
            if let Ok(location) = HeaderValue::from_str(&https_uri) {
                return (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, location)])
                    .into_response();
            }
            return next.run(request).await;
        }
    }
    next.run(request).await
}

/// Accept only `host[:port]` shapes for use in a redirect `Location`.
/// Rejects anything carrying a scheme, path, query, fragment, userinfo or
/// control characters, which could otherwise redirect elsewhere entirely.
fn is_safe_redirect_authority(value: &str) -> bool {
    if value.is_empty() || value.len() > 255 {
        return false;
    }
    value.bytes().all(|b| {
        b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b':' || b == b'_'
    }) && !value.starts_with(':')
}

/// Decide whether the *original* client connection used HTTPS.
///
/// HTTPS_TRUSTED_PROXY_ONLY_V1 (H-4): this reads only forwarded scheme headers,
/// which is safe **because `sanitize_forwarded_headers_middleware` has already
/// removed them when the request did not arrive from a trusted proxy hop**.
/// Keeping the sanitisation in one middleware, rather than threading a peer
/// address through every handler, means a new call site cannot accidentally
/// reintroduce the vulnerability.
///
/// Removed signal and why:
/// * "the Cookie header mentions the `__Host-` name" proved only that a client
///   *sent* that name. Anyone can send `Cookie: __Host-anything=1`, so it was
///   never evidence about the transport, trusted hop or not. It is gone.
///
/// `CF-Visitor` is retained as a fallback (Cloudflare Tunnel does not always
/// set `X-Forwarded-Proto`) but is now only ever seen from a trusted hop.
///
/// Deployment note: `cloudflared` runs inside the same container and connects
/// over loopback, and `TrustedProxies` always includes loopback, so genuine
/// Tunnel traffic keeps its `__Host-` cookies and no login loop is introduced.
fn request_uses_https(headers: &HeaderMap) -> bool {
    if headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"))
    {
        return true;
    }

    headers
        .get("cf-visitor")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("https"))
}

/// Headers that describe the *original* client connection. They are only
/// meaningful when set by our own reverse proxy; from any other peer they are
/// attacker-controlled input and must not survive into handler logic.
const UNTRUSTED_FORWARDED_HEADERS: &[&str] = &[
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-forwarded-for",
    "cf-visitor",
    "cf-connecting-ip",
    "true-client-ip",
    "x-real-ip",
];

/// HTTPS_TRUSTED_PROXY_ONLY_V1 (H-4): strip proxy-provenance headers unless the
/// TCP peer is a configured trusted proxy (loopback is always trusted, which
/// covers the in-container `cloudflared`).
///
/// This runs before every other admin middleware, so downstream code —
/// `request_uses_https`, `same_origin_request`, `https_redirect_middleware` —
/// can read these headers without repeating the trust check.
async fn sanitize_forwarded_headers_middleware(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());

    let trusted = crate::trusted_ip::trusted_from_global_config();
    if !trusted.is_trusted_peer(peer) {
        let headers = request.headers_mut();
        for name in UNTRUSTED_FORWARDED_HEADERS {
            headers.remove(*name);
        }
    }

    next.run(request).await
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
        // REFRESH_TOKEN_STRICT_FORMAT_V1 (M-2): shares one definition with the
        // refresh-cookie reader so the two cannot drift apart.
        if is_valid_session_token_format(value) {
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
        "{name}={token}; Path=/; Max-Age={SESSION_COOKIE_MAX_AGE}; HttpOnly; SameSite=Lax{secure}"
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

/// Set the HttpOnly refresh-token cookie (long-lived, rotates on each refresh).
fn append_refresh_cookie(
    mut response: Response,
    refresh_token: &str,
    headers: &HeaderMap,
) -> Response {
    let secure = request_uses_https(headers);
    let (name, secure_attr) = if secure {
        (SECURE_REFRESH_COOKIE, "; Secure")
    } else {
        (LOCAL_REFRESH_COOKIE, "")
    };
    let cookie = format!(
        "{name}={refresh_token}; Path=/; Max-Age={REFRESH_COOKIE_MAX_AGE}; HttpOnly; SameSite=Lax{secure_attr}"
    );
    if let Ok(value) = cookie.parse::<HeaderValue>() {
        response.headers_mut().append(header::SET_COOKIE, value);
    } else {
        tracing::error!("refresh cookie 無法建立");
    }
    response
}

/// Read the refresh token from the request cookies.
///
/// REFRESH_TOKEN_STRICT_FORMAT_V1 (M-2): refresh tokens are minted by
/// `create_session` as `Uuid::new_v4().simple()` — exactly 32 hex characters,
/// byte-for-byte the same shape as the access token. The previous
/// `len() >= 16` test accepted anything longer than that, including multi-
/// kilobyte values and arbitrary non-hex bytes, and passed them on to the DB
/// lookup in `rotate_session` and to any log line that records the token.
/// Validating the exact format here keeps that entire input surface closed.
fn refresh_cookie_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookies.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name != SECURE_REFRESH_COOKIE && name != LOCAL_REFRESH_COOKIE {
            continue;
        }
        let value = value.trim();
        if is_valid_session_token_format(value) {
            return Some(value.to_string());
        }
    }
    None
}

fn append_expired_session_cookies(mut response: Response) -> Response {
    for (name, secure) in [
        (SECURE_SESSION_COOKIE, "; Secure"),
        (LOCAL_SESSION_COOKIE, ""),
        (SECURE_REFRESH_COOKIE, "; Secure"),
        (LOCAL_REFRESH_COOKIE, ""),
    ] {
        let cookie = format!("{name}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}");
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

/// Promote the HttpOnly session cookie into the query extractor for internal
/// handler consumption, and **always** strip any externally-supplied `token`
/// query parameter. This ensures that session tokens never arrive via URL
/// (which would leak into server logs, CDN logs, and browser history) and
/// that handlers only see `token` values injected from the HttpOnly cookie
/// by this middleware.
///
/// SECURITY-F03: The frontend has been migrated to cookie-only authentication.
/// Any `token=` in the URL is either a stale legacy marker or an attacker
/// attempting to inject a session token via query string. Both are stripped.
async fn session_cookie_to_query_middleware(
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // TOKEN_QUERY_STRIP_ALL_ADMIN_V1 (H-2): this scrubbing must cover *every*
    // authenticated surface, not just `/admin/api/`. Page handlers call
    // `session_token(&headers, &query.token)`, which prefers the query value,
    // so limiting the strip to the API prefix left URLs such as
    // `/admin/push-status?token=<victim token>` fully functional. That places a
    // live session token in browser history, `Referer` headers and every
    // reverse-proxy access log, and lets an attacker seed a session by sending
    // a link.
    //
    // Scrubbing here (rather than in each handler) keeps the guarantee in one
    // place: after this middleware, `token` in the query string is *only* ever
    // the value read from the HttpOnly cookie.
    let path = request.uri().path();
    if !(path == "/admin" || path.starts_with("/admin/") || path == "/homework") {
        return next.run(request).await;
    }

    let session_token = session_cookie_from_headers(&headers);
    let has_token_param = request
        .uri()
        .query()
        .is_some_and(|q| q.contains("token="));

    // Fast path: no cookie and no token= in URL — pass through unchanged.
    if session_token.is_none() && !has_token_param {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    let rewritten = {
        let path = parts.uri.path().to_string();
        let existing_query = parts.uri.query().unwrap_or("");
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        // Always strip externally-supplied `token` parameters. Only the
        // HttpOnly cookie value (if present) is re-injected below.
        for (name, value) in url::form_urlencoded::parse(existing_query.as_bytes()) {
            if name != "token" {
                serializer.append_pair(&name, &value);
            }
        }
        if let Some(ref token) = session_token {
            serializer.append_pair("token", token);
        }
        let query_string = serializer.finish();
        if query_string.is_empty() {
            path
        } else {
            format!("{path}?{query_string}")
        }
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
            | "/admin/api/session/refresh"
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
    let path = request.uri().path();
    if !path.starts_with("/admin/api/") || bootstrap_allowed_api_path(path) {
        return next.run(request).await;
    }

    // PERFORMANCE_PASSWORD_CHECK_AUTH_V1:
    // The session_cookie_to_query_middleware already ran and injected the
    // cookie into the URI query string. Read from URI only — avoids a second
    // pass through the Cookie header on every authenticated request.
    //
    // BOOTSTRAP_GATE_AUTHORIZE_ANY_V1 (M-1): this must call `authorize_any`,
    // not `authorize`.
    //
    // `authorize` is defined as "a session that is fully usable", and it
    // implements that by returning `None` for exactly the sessions this
    // middleware exists to catch (`must_change_password == true`, see
    // `AdminWebState::authorize`). Written against `authorize`, the
    // `if session.must_change_password` branch below was unreachable — the
    // gate was dead code and the 403 could never be emitted.
    //
    // Enforcement happened to still occur, but only as a side effect: each
    // handler separately called `authorize`, got `None`, and returned 401.
    // That left the guarantee spread across every handler instead of in this
    // one layer, so any handler using `authorize_any` (a legitimate need —
    // page handlers and `/admin/api/status` must be able to *see* a bootstrap
    // session in order to redirect it to the password form) silently sat
    // outside the gate.
    //
    // `authorize_any` returns the session regardless of password state, so the
    // check below now actually runs and this middleware becomes the single
    // authoritative chokepoint. Endpoints the bootstrap session legitimately
    // needs (`bootstrap_allowed_api_path`, checked above) are exempted before
    // we get here.
    let token = session_token_from_uri(request.uri());
    if let Some(token) = token {
        if let Some(session) = state.authorize_any(&token).await {
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

/// Decide whether a state-changing request demonstrably originates from this
/// site.
///
/// CSRF_FAIL_CLOSED_V1 (H-1): this check is fail-closed. A mutating request is
/// only accepted when it *proves* same-origin provenance, either through
/// `Sec-Fetch-Site: same-origin` or through an `Origin` header that exactly
/// matches this deployment's scheme+host. Anything ambiguous — no `Origin` and
/// no `Sec-Fetch-Site`, an opaque `Origin: null`, or `Sec-Fetch-Site: none` —
/// is rejected.
///
/// Rationale for the removed allowances:
/// * `.unwrap_or(true)` previously accepted every request that carried neither
///   header, so any client able to suppress them bypassed the only CSRF control
///   guarding `/admin/api/`.
/// * `Sec-Fetch-Site: none` means a user-initiated navigation (address bar,
///   bookmark), not a same-origin fetch, and must not authorize a mutation.
/// * `Sec-Fetch-Site: same-site` allows sibling subdomains. The Filebrowser SSO
///   cookie is deliberately scoped to the parent domain, so a compromised
///   sibling host must not be able to drive Dashboard state changes.
///
/// This is safe for the shipped frontend: every mutation is issued via
/// `fetch()`, which always attaches a real `Origin` on POST/PUT/PATCH/DELETE.
/// There are no native `<form method=post>` submissions (which, under this
/// deployment's `Referrer-Policy: no-referrer`, would send `Origin: null`).
fn same_origin_request(headers: &HeaderMap) -> bool {
    // Signal 1: the browser's own provenance metadata. Page JavaScript cannot
    // forge this, so `same-origin` is by itself sufficient proof.
    //
    // Any other value is an explicit rejection: `same-site` would admit sibling
    // subdomains (the Filebrowser SSO cookie is scoped to the parent domain, so
    // a compromised sibling host must not drive Dashboard state changes),
    // `cross-site` is hostile, and `none` is a top-level user navigation
    // (address bar / bookmark) rather than a same-origin fetch.
    let sec_fetch_site = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    match sec_fetch_site {
        Some(site) if site.eq_ignore_ascii_case("same-origin") => return true,
        Some(_) => return false,
        None => {}
    }

    // Signal 2: no Sec-Fetch-Site (older browsers, non-browser clients). Fall
    // back to an exact Origin match. A missing Origin is no longer treated as
    // proof of anything.
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    else {
        return false;
    };

    // `Origin: null` is opaque (sandboxed iframe, cross-origin redirect chain,
    // or a form post under `Referrer-Policy: no-referrer`) and proves nothing.
    if origin.is_empty() || origin.eq_ignore_ascii_case("null") {
        return false;
    }

    // This deployment is HTTPS-only behind Cloudflare/Caddy, so an absent
    // forwarded scheme defaults to https rather than weakening the comparison.
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .unwrap_or("https")
        .trim();
    // Take the first hop of X-Forwarded-Host; a comma-joined list would never
    // match a browser-sent Origin and must not be compared verbatim.
    let host = headers
        .get("x-forwarded-host")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .or_else(|| headers.get(header::HOST).and_then(|v| v.to_str().ok()))
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        return false;
    }
    let expected = format!("{proto}://{host}");
    origin.trim_end_matches('/') == expected.trim_end_matches('/')
}

/// Reject browser cross-site state changes before they reach authenticated
/// handlers. SameSite=Lax cookies remain the primary CSRF control.
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

fn generate_csp_nonce() -> String {
    Uuid::new_v4().simple().to_string()
}

fn csp_header_value(nonce: Option<&str>) -> String {
    let script_nonce = nonce
        .map(|value| format!(" 'nonce-{value}'"))
        .unwrap_or_default();
    let style_nonce = nonce
        .map(|value| format!(" 'nonce-{value}'"))
        .unwrap_or_default();
    format!(
        "default-src 'self'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'; object-src 'none'; script-src 'self'{script_nonce} https://static.cloudflareinsights.com; script-src-attr 'none'; style-src 'self'{style_nonce}; style-src-attr 'unsafe-inline'; require-trusted-types-for 'script'; trusted-types default dompurify; img-src 'self' data: blob:; connect-src 'self' https://cloudflareinsights.com; media-src 'self' blob:; frame-src 'self'; worker-src 'self' blob:; upgrade-insecure-requests"
    )
}

/// Add a per-response nonce to every inline `<script>` and `<style>` element.
/// Static JavaScript/CSS files remain allowed by `'self'`; inline event handlers
/// remain blocked. Legacy inline style attributes are allowed because the
/// timetable grid and several runtime controls position themselves dynamically.
fn add_csp_nonce_to_inline_tags(html: &str, nonce: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() + 512);
    let mut cursor = 0usize;

    while cursor < html.len() {
        let script_at = lower[cursor..].find("<script").map(|at| cursor + at);
        let style_at = lower[cursor..].find("<style").map(|at| cursor + at);
        let Some(tag_at) = (match (script_at, style_at) {
            (Some(script), Some(style)) => Some(script.min(style)),
            (Some(script), None) => Some(script),
            (None, Some(style)) => Some(style),
            (None, None) => None,
        }) else {
            out.push_str(&html[cursor..]);
            break;
        };

        let tag_name_len = if lower[tag_at..].starts_with("<script") {
            "<script".len()
        } else {
            "<style".len()
        };
        let next_byte = html.as_bytes().get(tag_at + tag_name_len).copied();
        if !next_byte
            .map(|byte| byte.is_ascii_whitespace() || byte == b'>' || byte == b'/')
            .unwrap_or(false)
        {
            // Ignore string literals such as `"<script..."` inside an inline
            // script; they are not HTML elements and must not be rewritten.
            let end = (tag_at + tag_name_len).min(html.len());
            out.push_str(&html[cursor..end]);
            cursor = end;
            continue;
        }
        let Some(relative_end) = lower[tag_at..].find('>') else {
            out.push_str(&html[cursor..]);
            break;
        };
        let tag_end = tag_at + relative_end;
        let tag = &html[tag_at..=tag_end];
        out.push_str(&html[cursor..tag_at]);
        if tag.to_ascii_lowercase().contains(" nonce=") {
            out.push_str(tag);
        } else {
            let without_close = tag.strip_suffix('>').unwrap_or(tag);
            out.push_str(without_close);
            out.push_str(" nonce=\"");
            out.push_str(nonce);
            out.push_str("\">");
        }
        cursor = tag_end + 1;
    }

    out
}

/// Security headers for the externally reachable Admin Web. Inline elements
/// receive a fresh nonce in `html_page_response`; inline event/style
/// attributes are blocked instead of enabling the global inline bypass.
async fn admin_security_headers_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // FAST_PAGE_LOAD_V1: remember path before moving the request so versioned
    // /admin/static assets can keep their Cache-Control instead of no-store.
    // PERFORMANCE_PATH_CHECK_V1: use &str directly, no allocation needed.
    let path = request.uri().path();
    let is_admin_path = path.starts_with("/admin")
        || path == "/manifest.webmanifest"
        || path == "/service-worker.js";
    let cache_static = path.starts_with("/admin/static/") || path == "/admin/pwa/icon.svg";
    let mut response = next.run(request).await;

    // This middleware covers the whole Admin Web router, including `/` and
    // other redirect/error responses. Security headers must not depend on the
    // final status code being 200 or the path starting with `/admin`.
    //
    // Compute this before taking the mutable header borrow: reading `response`
    // afterwards would conflict with the `headers` borrow held below.
    let html_document = is_html_document(&response);

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
    // Page handlers install a nonce-bearing CSP. Use the strict no-inline
    // fallback only for JSON/static responses that do not contain inline code.
    if !headers.contains_key(header::CONTENT_SECURITY_POLICY) {
        if let Ok(value) = csp_header_value(None).parse::<HeaderValue>() {
            headers.insert(header::CONTENT_SECURITY_POLICY, value);
        }
    }
    if is_admin_path && !cache_static {
        // Admin HTML must always be revalidated (it is behind auth and may
        // change on redeploy), but `no-store` also forbade conditional
        // requests, so every navigation re-downloaded the full ~260 KB
        // dashboard. `no-cache` still revalidates on each load while allowing
        // the ETag below to answer with an empty 304.
        if html_document {
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-cache, must-revalidate"),
            );
        } else {
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
    }
    response
}

fn is_html_document(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"))
}

/// Serve a static in-binary HTML page with a content-derived ETag so repeat
/// navigations get a 304 instead of the full payload.
fn html_page_response(headers: &HeaderMap, html: String) -> Response {
    let nonce = generate_csp_nonce();
    let html = add_csp_nonce_to_inline_tags(&html, &nonce);

    // Hash the nonce-bearing body directly. Each response gets a fresh nonce,
    // so a previously cached ETag cannot be reused for a different response.
    let mut hasher = Sha256::new();
    hasher.update(html.as_bytes());
    let etag = format!("\"{}\"", hex::encode(&hasher.finalize()[..16]));

    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|c| c.trim() == etag));

    if matches {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        if let Ok(value) = etag.parse::<HeaderValue>() {
            response.headers_mut().insert(header::ETAG, value);
        }
        if let Ok(value) = csp_header_value(Some(&nonce)).parse::<HeaderValue>() {
            response
                .headers_mut()
                .insert(header::CONTENT_SECURITY_POLICY, value);
        }
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, max-age=0"),
        );
        return response;
    }

    let mut response = Html(html).into_response();
    if let Ok(value) = etag.parse::<HeaderValue>() {
        response.headers_mut().insert(header::ETAG, value);
    }
    if let Ok(value) = csp_header_value(Some(&nonce)).parse::<HeaderValue>() {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    // Admin HTML contains account/session-specific state. Never let Safari,
    // the installed PWA, or an intermediary reuse it after a server restart.
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
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

fn normalize_admin_ip(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    value.parse::<IpAddr>().ok().map(|ip| ip.to_string())
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

fn session_login_response(
    request_headers: &HeaderMap,
    tokens: &SessionTokens,
    account_id: String,
    is_admin: bool,
    must_change_password: bool,
) -> Response {
    let response = append_session_cookie(
        (
            StatusCode::OK,
            Json(json!(auth::AdminLoginResponse {
                token: SESSION_COOKIE_MARKER.to_string(),
                account_id: account_id.clone(),
                is_admin,
                must_change_password,
            })),
        )
            .into_response(),
        &tokens.access,
        request_headers,
    );
    // Refresh cookie is always set alongside the access cookie (bootstrap too,
    // so the mandatory password-change flow can keep refreshing).
    let response = append_refresh_cookie(response, &tokens.refresh, request_headers);

    // A bootstrap login authenticates with the upstream Tronclass password and
    // is restricted to changing the Dashboard password. It must not also hand
    // out a Filebrowser SSO ticket: that cookie is checked only for a valid
    // signature, so it would grant full file access (and, for an admin, the
    // whole cloud/ tree) without the account ever setting a Dashboard password.
    if must_change_password {
        return response;
    }

    fb_sso::append_fb_sso_cookie(
        response,
        &account_id,
        is_admin,
        &tokens.access,
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

fn admin_login_redirect(next_path: &str) -> Response {
    let encoded_next: String = url::form_urlencoded::byte_serialize(next_path.as_bytes()).collect();
    let location = format!("/admin?next={encoded_next}");
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

fn must_change_password_redirect() -> Response {
    (
        StatusCode::TEMPORARY_REDIRECT,
        [(header::LOCATION, "/admin/password?first=1")],
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

fn session_token(headers: &HeaderMap, query_token: &str) -> String {
    // PERFORMANCE_SESSION_TOKEN_V1:
    // session_cookie_to_query_middleware already injected the HttpOnly cookie
    // into query.token for every /admin path, so preferring query.token avoids
    // re-parsing the Cookie header on each request.
    //
    // TOKEN_QUERY_STRIP_ALL_ADMIN_V1 (H-2): this is only safe because that
    // middleware unconditionally removes any caller-supplied `token` before
    // re-adding the cookie value. The query parameter is therefore server-
    // controlled, never attacker-controlled. Defence in depth: reject anything
    // that is not a well-formed access token (32 lowercase hex chars, matching
    // `Uuid::simple`) so a future routing change cannot silently reintroduce
    // query-string session injection.
    let trimmed = query_token.trim();
    if is_valid_access_token_format(trimmed) {
        return trimmed.to_string();
    }
    session_cookie_from_headers(headers).unwrap_or_default()
}

/// Both session token kinds are `Uuid::new_v4().simple()` — exactly 32 hex
/// characters (see `AdminWebState::create_session`, which mints the access and
/// refresh tokens the same way). Used to reject malformed/injected values
/// before they reach a DB lookup or a log line.
///
/// REFRESH_TOKEN_STRICT_FORMAT_V1 (M-2): this is the single definition shared
/// by `session_cookie_from_headers`, `refresh_cookie_from_headers` and
/// `session_token`, so the access and refresh paths cannot drift apart again.
fn is_valid_session_token_format(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Historical name kept as a thin alias so existing call sites and tests read
/// naturally where an *access* token specifically is meant.
fn is_valid_access_token_format(value: &str) -> bool {
    is_valid_session_token_format(value)
}

/// Refresh endpoint: exchange a valid refresh token for a fresh access token
/// (and a rotated refresh token). Called by the frontend when an API call
/// returns 401 because the short-lived access token expired.
fn unauthorized_response() -> Response {
    tracing::debug!("admin web unauthorized request");
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
        let tokens = state
            .create_session("cookie-user".to_string(), false, false)
            .await;
        let session_token = tokens.access;
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

        let response = auth::admin_login_handler(
            State(state.clone()),
            headers.clone(),
            ClientIp("127.0.0.1".to_string()),
            serde_json::to_vec(&auth::AdminLoginRequest {
                username: "student-a".to_string(),
                password: dashboard_password.to_string(),
            })
            .unwrap()
            .into(),
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
        assert!(set_cookie.contains("SameSite=Lax"));

        let rejected = auth::admin_login_handler(
            State(state),
            headers,
            ClientIp("127.0.0.1".to_string()),
            serde_json::to_vec(&auth::AdminLoginRequest {
                username: "student-a".to_string(),
                // The upstream provider password must no longer authenticate
                // the public Dashboard.
                password: provider_password.to_string(),
            })
            .unwrap()
            .into(),
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn session_persists_across_state_recreation() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        // 建立初始狀態並產生 Session
        let state1 = AdminWebState::new("", vec![], db.clone(), None, None, 10);
        let tokens = state1
            .create_session("student-persisted".to_string(), true, false)
            .await;
        let session_token = tokens.access;

        let auth1 = state1.authorize(&session_token).await.unwrap();
        assert_eq!(auth1.account_id, "student-persisted");
        assert!(auth1.is_admin);

        // 模擬伺服器重啟：建立全新的 AdminWebState，快取初時為空或從 SQLite 讀取
        let state2 = AdminWebState::new("", vec![], db.clone(), None, None, 10);

        // 透過 authorize 測試從 SQLite 讀回 Session
        let auth2 = state2.authorize(&session_token).await.unwrap();
        assert_eq!(auth2.account_id, "student-persisted");
        assert!(auth2.is_admin);
        assert!(!auth2.must_change_password);

        // 測試登出後從 SQLite 刪除
        state2.revoke_session(&session_token).await;
        assert!(state2.authorize(&session_token).await.is_none());

        // 重啟後的 state3 也應無法使用已登出的 session
        let state3 = AdminWebState::new("", vec![], db, None, None, 10);
        assert!(state3.authorize(&session_token).await.is_none());
    }

    // ── CSRF_FAIL_CLOSED_V1 (H-1) ────────────────────────────────────────────
    // same_origin_request() must be fail-closed: a mutating request is only
    // accepted when it proves same-origin provenance.

    fn origin_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn csrf_bare_request_without_any_provenance_header_is_rejected() {
        // Regression for H-1: this previously returned true via unwrap_or(true),
        // leaving /admin/api/ mutations with no CSRF protection at all.
        assert!(!same_origin_request(&origin_headers(&[(
            "host",
            "dashboard.jasoncloudserver.me"
        )])));
        assert!(!same_origin_request(&origin_headers(&[])));
    }

    #[test]
    fn csrf_sec_fetch_site_none_is_rejected() {
        // `none` is a top-level navigation, not a same-origin fetch.
        assert!(!same_origin_request(&origin_headers(&[
            ("sec-fetch-site", "none"),
            ("host", "dashboard.jasoncloudserver.me"),
        ])));
    }

    #[test]
    fn csrf_sec_fetch_site_cross_and_same_site_are_rejected() {
        for site in ["cross-site", "same-site"] {
            assert!(
                !same_origin_request(&origin_headers(&[
                    ("sec-fetch-site", site),
                    ("host", "dashboard.jasoncloudserver.me"),
                ])),
                "sec-fetch-site: {site} must be rejected"
            );
        }
    }

    #[test]
    fn csrf_sec_fetch_site_same_origin_is_accepted() {
        assert!(same_origin_request(&origin_headers(&[
            ("sec-fetch-site", "same-origin"),
            ("host", "dashboard.jasoncloudserver.me"),
        ])));
        // Case-insensitive per RFC header token comparison.
        assert!(same_origin_request(&origin_headers(&[(
            "sec-fetch-site",
            "Same-Origin"
        )])));
    }

    #[test]
    fn csrf_matching_origin_is_accepted_without_sec_fetch_site() {
        assert!(same_origin_request(&origin_headers(&[
            ("origin", "https://dashboard.jasoncloudserver.me"),
            ("host", "dashboard.jasoncloudserver.me"),
            ("x-forwarded-proto", "https"),
        ])));
        // A trailing slash on Origin is tolerated.
        assert!(same_origin_request(&origin_headers(&[
            ("origin", "https://dashboard.jasoncloudserver.me/"),
            ("host", "dashboard.jasoncloudserver.me"),
        ])));
    }

    #[test]
    fn csrf_foreign_origin_is_rejected() {
        assert!(!same_origin_request(&origin_headers(&[
            ("origin", "https://evil.example"),
            ("host", "dashboard.jasoncloudserver.me"),
            ("x-forwarded-proto", "https"),
        ])));
    }

    #[test]
    fn csrf_null_origin_is_rejected() {
        // Sandboxed iframe / no-referrer form post.
        assert!(!same_origin_request(&origin_headers(&[
            ("origin", "null"),
            ("host", "dashboard.jasoncloudserver.me"),
        ])));
    }

    #[test]
    fn csrf_origin_scheme_must_match() {
        // http:// Origin against an https deployment must not pass.
        assert!(!same_origin_request(&origin_headers(&[
            ("origin", "http://dashboard.jasoncloudserver.me"),
            ("host", "dashboard.jasoncloudserver.me"),
            ("x-forwarded-proto", "https"),
        ])));
    }

    #[test]
    fn csrf_sibling_subdomain_origin_is_rejected() {
        // The fb-sso cookie is scoped to .jasoncloudserver.me, so a sibling
        // host must not be able to drive Dashboard mutations.
        assert!(!same_origin_request(&origin_headers(&[
            ("origin", "https://files.jasoncloudserver.me"),
            ("host", "dashboard.jasoncloudserver.me"),
        ])));
    }

    #[test]
    fn csrf_missing_host_is_rejected() {
        assert!(!same_origin_request(&origin_headers(&[(
            "origin",
            "https://dashboard.jasoncloudserver.me"
        )])));
    }

    #[test]
    fn csrf_forwarded_host_list_uses_first_hop() {
        // A comma-joined X-Forwarded-Host must not be compared verbatim.
        assert!(same_origin_request(&origin_headers(&[
            ("origin", "https://dashboard.jasoncloudserver.me"),
            (
                "x-forwarded-host",
                "dashboard.jasoncloudserver.me, internal.local",
            ),
            ("x-forwarded-proto", "https, http"),
        ])));
    }

    #[tokio::test]
    async fn csrf_mutating_api_request_without_provenance_is_forbidden() {
        // End-to-end: the middleware must reject before the handler runs.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("csrf-user".to_string(), true, false)
            .await;
        let app = build_admin_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/admin/api/action")
                    .header(
                        header::COOKIE,
                        format!("{SECURE_SESSION_COOKIE}={}", tokens.access),
                    )
                    .header("x-forwarded-proto", "https")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"action":"stop"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn csrf_same_origin_mutating_request_passes_middleware() {
        // The shipped frontend uses fetch(), which always sends Origin on POST.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("csrf-user".to_string(), true, false)
            .await;
        let app = build_admin_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/admin/api/action")
                    .header(
                        header::COOKIE,
                        format!("{SECURE_SESSION_COOKIE}={}", tokens.access),
                    )
                    .header("origin", "https://dashboard.jasoncloudserver.me")
                    .header("host", "dashboard.jasoncloudserver.me")
                    .header("x-forwarded-proto", "https")
                    .header("sec-fetch-site", "same-origin")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"action":"stop"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Must NOT be the CSRF rejection; the request reaches the handler.
        assert_ne!(response.status(), StatusCode::FORBIDDEN);
    }

    // ── TOKEN_QUERY_STRIP_ALL_ADMIN_V1 (H-2) ─────────────────────────────────
    // A session token must never be accepted from, or echoed into, the URL on
    // any /admin surface.

    #[test]
    fn access_token_format_matches_cookie_validation() {
        assert!(is_valid_access_token_format(
            "0123456789abcdef0123456789abcdef"
        ));
        assert!(is_valid_access_token_format(
            "0123456789ABCDEF0123456789ABCDEF"
        ));
        assert!(!is_valid_access_token_format("cookie"));
        assert!(!is_valid_access_token_format(""));
        // 31 and 33 chars
        assert!(!is_valid_access_token_format(
            "0123456789abcdef0123456789abcde"
        ));
        assert!(!is_valid_access_token_format(
            "0123456789abcdef0123456789abcdef0"
        ));
        // right length, non-hex
        assert!(!is_valid_access_token_format(
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
        ));
    }

    #[tokio::test]
    async fn page_route_ignores_attacker_supplied_query_token() {
        // Regression for H-2: /admin/push-status?token=<victim> used to
        // authenticate, because the strip middleware only covered /admin/api/.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let victim = state
            .create_session("victim".to_string(), false, false)
            .await;
        let app = build_admin_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/admin/push-status?token={}", victim.access))
                    .header("x-forwarded-proto", "https")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // No cookie was sent, so the page must bounce to login instead of
        // honouring the token in the URL.
        assert!(
            response.status().is_redirection(),
            "expected redirect to login, got {}",
            response.status()
        );
    }

    #[tokio::test]
    async fn page_route_still_authenticates_via_cookie() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("cookie-user".to_string(), false, false)
            .await;
        let app = build_admin_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/push-status")
                    .header(
                        header::COOKIE,
                        format!("{SECURE_SESSION_COOKIE}={}", tokens.access),
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
    async fn root_redirect_drops_token_but_keeps_other_params() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let app = build_admin_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/?token=0123456789abcdef0123456789abcdef&logout=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            !location.contains("token="),
            "token must not survive the redirect: {location}"
        );
        assert!(
            location.contains("logout=1"),
            "unrelated params must be preserved: {location}"
        );
    }

    // ── HTTPS_TRUSTED_PROXY_ONLY_V1 (H-4) ────────────────────────────────────
    // A client-settable header must never be able to convince the server that
    // a cleartext connection was HTTPS (which would send the session cookie
    // without `Secure`), nor to shape a 301 Location.

    /// Drive the full router with an explicit TCP peer, the way a real
    /// connection arrives via `into_make_service_with_connect_info`.
    async fn router_response_from_peer(
        app: Router,
        peer: &str,
        request: Request<Body>,
    ) -> Response {
        let peer: std::net::SocketAddr = peer.parse().unwrap();
        app.layer(axum::Extension(axum::extract::ConnectInfo(peer)))
            .oneshot(request)
            .await
            .unwrap()
    }

    #[test]
    fn https_detection_ignores_client_settable_cookie_name() {
        // Regression: `request_uses_https` used to return true merely because
        // the Cookie header mentioned a `__Host-` name. Anyone can send that.
        assert!(!request_uses_https(&origin_headers(&[(
            "cookie",
            "__Host-tronclass_admin_session=0123456789abcdef0123456789abcdef",
        )])));
        assert!(!request_uses_https(&origin_headers(&[(
            "cookie",
            "__Host-anything=1",
        )])));
    }

    #[test]
    fn https_detection_reads_forwarded_scheme_first_hop() {
        assert!(request_uses_https(&origin_headers(&[(
            "x-forwarded-proto",
            "https",
        )])));
        // Case-insensitive and whitespace tolerant.
        assert!(request_uses_https(&origin_headers(&[(
            "x-forwarded-proto",
            " HTTPS ",
        )])));
        // Only the first hop counts: the edge-most proxy states the client
        // scheme, so `http, https` is a plaintext client connection.
        assert!(!request_uses_https(&origin_headers(&[(
            "x-forwarded-proto",
            "http, https",
        )])));
        assert!(request_uses_https(&origin_headers(&[(
            "x-forwarded-proto",
            "https, http",
        )])));
        // No signal at all means "assume cleartext".
        assert!(!request_uses_https(&HeaderMap::new()));
        assert!(request_uses_https(&origin_headers(&[(
            "cf-visitor",
            "{\"scheme\":\"https\"}",
        )])));
    }

    #[tokio::test]
    async fn untrusted_peer_cannot_forge_https_and_gets_insecure_cookie_name() {
        // The attack: connect in cleartext from an untrusted address and claim
        // X-Forwarded-Proto: https. If believed, the session cookie is issued
        // as `__Host-...; Secure` over plaintext.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("victim".to_string(), false, false)
            .await;
        let app = build_admin_router(state);

        let response = router_response_from_peer(
            app,
            "203.0.113.9:51234",
            Request::builder()
                .uri("/admin/api/session/refresh")
                .method(Method::POST)
                .header("x-forwarded-proto", "https")
                .header("cf-visitor", "{\"scheme\":\"https\"}")
                .header("origin", "https://dashboard.jasoncloudserver.me")
                .header("host", "dashboard.jasoncloudserver.me")
                .header("sec-fetch-site", "same-origin")
                .header(
                    header::COOKIE,
                    format!("{SECURE_REFRESH_COOKIE}={}", tokens.refresh),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        // No *live* cookie may be minted as Secure/__Host- for a peer that only
        // claimed to be behind TLS. Cookie-clearing headers (Max-Age=0) are
        // emitted for both variants by design and are not part of this check.
        let minted: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter(|cookie| !cookie.contains("Max-Age=0"))
            .map(str::to_string)
            .collect();
        assert!(
            !minted.is_empty(),
            "expected the refresh to mint cookies so the assertion is meaningful"
        );
        for cookie in minted {
            assert!(
                !cookie.contains("; Secure"),
                "forged X-Forwarded-Proto produced a Secure cookie: {cookie}"
            );
            assert!(
                !cookie.starts_with("__Host-"),
                "forged X-Forwarded-Proto produced a __Host- cookie: {cookie}"
            );
        }
    }

    #[tokio::test]
    async fn trusted_loopback_peer_keeps_forwarded_scheme() {
        // The in-container `cloudflared` connects over loopback, which
        // `TrustedProxies` always trusts, so genuine Tunnel traffic must keep
        // its headers and its hardened cookie. This is what stops the fix from
        // causing the documented endless-401 login loop.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("cookie-user".to_string(), false, false)
            .await;
        let app = build_admin_router(state);

        let response = router_response_from_peer(
            app,
            "127.0.0.1:41000",
            Request::builder()
                .uri("/admin/api/status")
                .header(
                    header::COOKIE,
                    format!("{SECURE_SESSION_COOKIE}={}", tokens.access),
                )
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn untrusted_peer_forged_forwarded_host_cannot_pass_csrf_check() {
        // Sanitising X-Forwarded-Host also hardens H-1: the CSRF origin
        // comparison must not accept an attacker-declared host.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("csrf-user".to_string(), true, false)
            .await;
        let app = build_admin_router(state);

        let response = router_response_from_peer(
            app,
            "203.0.113.9:51234",
            Request::builder()
                .method(Method::POST)
                .uri("/admin/api/action")
                .header(
                    header::COOKIE,
                    format!("{SECURE_SESSION_COOKIE}={}", tokens.access),
                )
                .header("origin", "https://evil.example")
                // Without sanitisation this forged hop would make the expected
                // origin `https://evil.example`, matching Origin exactly and
                // letting the CSRF check pass.
                .header("x-forwarded-host", "evil.example")
                .header("x-forwarded-proto", "https")
                .header("host", "dashboard.jasoncloudserver.me")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"action":"stop"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn untrusted_peer_cannot_shape_https_redirect_location() {
        // With X-Forwarded-Proto stripped, an untrusted peer can no longer
        // trigger the 301 at all, so it cannot plant a cacheable redirect to a
        // host of its choosing.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let app = build_admin_router(state);

        let response = router_response_from_peer(
            app,
            "203.0.113.9:51234",
            Request::builder()
                .uri("/admin/push-status")
                .header("x-forwarded-proto", "http")
                .header("host", "evil.example/@attacker")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            !location.starts_with("https://evil.example"),
            "untrusted peer shaped the redirect target: {location}"
        );
    }

    #[test]
    fn redirect_authority_rejects_non_authority_values() {
        assert!(is_safe_redirect_authority("dashboard.jasoncloudserver.me"));
        assert!(is_safe_redirect_authority("127.0.0.1:26000"));
        // Anything that could redirect elsewhere or split the URL.
        assert!(!is_safe_redirect_authority(""));
        assert!(!is_safe_redirect_authority("evil.example/@victim.example"));
        assert!(!is_safe_redirect_authority("user@evil.example"));
        assert!(!is_safe_redirect_authority("evil.example?next=/"));
        assert!(!is_safe_redirect_authority("evil.example#frag"));
        assert!(!is_safe_redirect_authority("host with space"));
        assert!(!is_safe_redirect_authority(":26000"));
    }

    #[test]
    fn trusted_proxy_peer_gate_matches_client_ip_logic() {
        let trusted = crate::trusted_ip::TrustedProxies::from_config(&[]);
        // Loopback is always trusted (in-container cloudflared).
        assert!(trusted.is_trusted_peer(Some("127.0.0.1".parse().unwrap())));
        assert!(trusted.is_trusted_peer(Some("::1".parse().unwrap())));
        // Public addresses are not.
        assert!(!trusted.is_trusted_peer(Some("203.0.113.9".parse().unwrap())));
        // An absent peer must never be trusted.
        assert!(!trusted.is_trusted_peer(None));
    }

    // ── BOOTSTRAP_GATE_AUTHORIZE_ANY_V1 (M-1) ────────────────────────────────
    // A session still carrying `must_change_password` must be confined to the
    // bootstrap endpoints by the middleware itself, not incidentally by each
    // handler's own `authorize()` call.

    #[tokio::test]
    async fn authorize_rejects_bootstrap_session_but_authorize_any_returns_it() {
        // The root cause of M-1: `authorize` filters out exactly the sessions
        // the gate needs to inspect, so a gate written against it is dead code.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("bootstrap-user".to_string(), false, true)
            .await;

        assert!(
            state.authorize(&tokens.access).await.is_none(),
            "authorize() must hide a must_change_password session"
        );
        let session = state
            .authorize_any(&tokens.access)
            .await
            .expect("authorize_any() must still return it");
        assert!(
            session.must_change_password,
            "the gate depends on this flag being observable"
        );
    }

    #[tokio::test]
    async fn bootstrap_session_is_403_on_a_normal_api_route() {
        // Regression: this 403 was unreachable before the fix.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("bootstrap-user".to_string(), false, true)
            .await;
        let app = build_admin_router(state);

        let response = router_response_from_peer(
            app,
            "127.0.0.1:41000",
            Request::builder()
                .uri("/admin/api/timetable?account_id=bootstrap-user")
                .header(
                    header::COOKIE,
                    format!("{SECURE_SESSION_COOKIE}={}", tokens.access),
                )
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "the password-change gate must reject before the handler runs"
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "password_change_required");
        assert_eq!(json["must_change_password"], true);
    }

    #[tokio::test]
    async fn bootstrap_session_reaches_authorize_any_handlers_that_used_to_bypass() {
        // `/admin/api/status` uses `authorize_any`, so before the fix a
        // bootstrap session sailed straight past the gate. It is deliberately
        // on the allowlist (the password form polls it), so it must STILL be
        // reachable — and must report the flag the frontend redirects on.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("bootstrap-user".to_string(), false, true)
            .await;
        let app = build_admin_router(state);

        let response = router_response_from_peer(
            app,
            "127.0.0.1:41000",
            Request::builder()
                .uri("/admin/api/status")
                .header(
                    header::COOKIE,
                    format!("{SECURE_SESSION_COOKIE}={}", tokens.access),
                )
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the password form polls this endpoint; gating it would deadlock login"
        );
    }

    #[tokio::test]
    async fn bootstrap_allowlist_covers_every_endpoint_the_password_form_needs() {
        // password.html calls /admin/api/status and /admin/api/password;
        // session-security.js additionally calls /admin/api/logout. If any of
        // these were gated, a first-time user could never escape the modal.
        for path in [
            "/admin/api/status",
            "/admin/api/password",
            "/admin/api/logout",
            "/admin/api/session/refresh",
        ] {
            assert!(
                bootstrap_allowed_api_path(path),
                "{path} must stay exempt or first-login deadlocks"
            );
        }
        // Ordinary endpoints must NOT be exempt.
        for path in [
            "/admin/api/timetable",
            "/admin/api/action",
            "/admin/api/health",
        ] {
            assert!(
                !bootstrap_allowed_api_path(path),
                "{path} must be gated"
            );
        }
    }

    #[tokio::test]
    async fn normal_session_is_unaffected_by_the_gate() {
        // must_change_password == false must pass through untouched.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("normal-user".to_string(), false, false)
            .await;
        let app = build_admin_router(state);

        let response = router_response_from_peer(
            app,
            "127.0.0.1:41000",
            Request::builder()
                .uri("/admin/api/timetable?account_id=normal-user")
                .header(
                    header::COOKIE,
                    format!("{SECURE_SESSION_COOKIE}={}", tokens.access),
                )
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_ne!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a normal session must not be caught by the password-change gate"
        );
    }

    // ── REFRESH_TOKEN_STRICT_FORMAT_V1 (M-2) ─────────────────────────────────
    // The refresh cookie must be validated to the same 32-hex shape as the
    // access cookie, instead of the old "any value of length >= 16".

    fn cookie_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let joined = pairs
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        origin_headers(&[("cookie", joined.as_str())])
    }

    #[test]
    fn refresh_cookie_accepts_only_uuid_simple_shape() {
        let valid = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            refresh_cookie_from_headers(&cookie_headers(&[(SECURE_REFRESH_COOKIE, valid)])),
            Some(valid.to_string())
        );
        assert_eq!(
            refresh_cookie_from_headers(&cookie_headers(&[(LOCAL_REFRESH_COOKIE, valid)])),
            Some(valid.to_string())
        );
        // Uppercase hex is still a valid Uuid rendering.
        let upper = "0123456789ABCDEF0123456789ABCDEF";
        assert_eq!(
            refresh_cookie_from_headers(&cookie_headers(&[(SECURE_REFRESH_COOKIE, upper)])),
            Some(upper.to_string())
        );
    }

    #[test]
    fn refresh_cookie_rejects_values_the_old_length_check_allowed() {
        // Every one of these passed the previous `len() >= 16` test.
        let rejected = [
            // 16-31 chars: long enough for the old check, wrong length.
            "0123456789abcdef",
            "0123456789abcdef0123456789abcde",
            // 33 chars.
            "0123456789abcdef0123456789abcdef0",
            // Right length, non-hex — could carry delimiters or control bytes
            // into the DB lookup and the logs.
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            "0123456789abcdef0123456789abcde'",
            "../../../../etc/passwd0123456789",
            // Oversized payload.
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ];
        for value in rejected {
            assert_eq!(
                refresh_cookie_from_headers(&cookie_headers(&[(SECURE_REFRESH_COOKIE, value)])),
                None,
                "refresh cookie must reject {value:?}"
            );
        }
    }

    #[test]
    fn refresh_cookie_skips_malformed_entry_and_keeps_scanning() {
        // A hostile or stale cookie earlier in the header must not mask the
        // real one later in the list.
        let valid = "0123456789abcdef0123456789abcdef";
        let headers = cookie_headers(&[
            ("_ga", "GA1.1.123"),
            (SECURE_REFRESH_COOKIE, "short"),
            (LOCAL_REFRESH_COOKIE, valid),
        ]);
        assert_eq!(
            refresh_cookie_from_headers(&headers),
            Some(valid.to_string())
        );
    }

    #[test]
    fn access_and_refresh_share_one_format_definition() {
        // Guards against the two paths drifting apart again.
        for value in [
            "0123456789abcdef0123456789abcdef",
            "0123456789ABCDEF0123456789ABCDEF",
        ] {
            assert!(is_valid_session_token_format(value));
            assert!(is_valid_access_token_format(value));
        }
        for value in ["", "cookie", "0123456789abcdef", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"] {
            assert!(!is_valid_session_token_format(value));
            assert!(!is_valid_access_token_format(value));
        }
    }

    #[tokio::test]
    async fn rotate_session_rejects_malformed_token_without_touching_the_db() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);

        for value in [
            "",
            "   ",
            // The schema default for legacy rows: must never match.
            "''",
            "short",
            "0123456789abcdef",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        ] {
            assert!(
                state.rotate_session(value).await.is_none(),
                "rotate_session must reject {value:?}"
            );
        }
    }

    #[tokio::test]
    async fn rotate_session_still_accepts_a_freshly_minted_refresh_token() {
        // The tightened format must not break the real rotation flow.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("rotate-user".to_string(), false, false)
            .await;

        assert!(
            is_valid_session_token_format(&tokens.refresh),
            "minted refresh tokens must satisfy the validator"
        );
        assert!(
            is_valid_session_token_format(&tokens.access),
            "minted access tokens must satisfy the validator"
        );

        let rotated = state
            .rotate_session(&tokens.refresh)
            .await
            .expect("a freshly minted refresh token must still rotate");
        assert!(is_valid_session_token_format(&rotated.access));
        assert!(is_valid_session_token_format(&rotated.refresh));
        assert_ne!(
            rotated.refresh, tokens.refresh,
            "rotation must issue a new refresh token"
        );
    }

    #[tokio::test]
    async fn refresh_endpoint_rejects_oversized_cookie_end_to_end() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let app = build_admin_router(state);

        let junk = "a".repeat(4096);
        let response = router_response_from_peer(
            app,
            "127.0.0.1:41000",
            Request::builder()
                .method(Method::POST)
                .uri("/admin/api/session/refresh")
                .header(
                    header::COOKIE,
                    format!("{SECURE_REFRESH_COOKIE}={junk}"),
                )
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // ── FB_SHELL_SERVER_SIDE_ROLE_V1 (M-5) ───────────────────────────────────
    // The Filebrowser shell bridge must derive `is_admin` from evidence this
    // process can verify, never from an `X-Fb-Admin` request header.

    /// Drive `/admin/api/filebrowser-shell` with caller-controlled headers.
    async fn filebrowser_shell_response(
        state: AdminWebState,
        extra_headers: &[(&str, &str)],
    ) -> Response {
        let app = build_admin_router(state);
        let mut builder = Request::builder()
            .uri("/admin/api/filebrowser-shell")
            .header("x-forwarded-proto", "https");
        for (name, value) in extra_headers {
            builder = builder.header(*name, *value);
        }
        router_response_from_peer(
            app,
            "127.0.0.1:41000",
            builder.body(Body::empty()).unwrap(),
        )
        .await
    }

    #[tokio::test]
    async fn shell_secret_alone_cannot_self_assert_admin() {
        // The M-5 attack: reach the Dashboard port directly (ptero-start.sh
        // binds it to 0.0.0.0), present the shell secret, and claim admin.
        // Without a verifiable identity the role must resolve to user scope,
        // so the admin backend is refused.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);

        // `shell_request_secret_valid` returns false when the secret env var is
        // unset, so an unauthenticated caller is rejected outright here. Either
        // outcome is acceptable; what must never happen is a 200 serving the
        // admin backend.
        let response = filebrowser_shell_response(
            state,
            &[
                ("x-fb-shell-secret", "guessed-or-leaked-secret"),
                ("x-fb-admin", "yes"),
                ("x-fb-backend", "admin"),
            ],
        )
        .await;

        assert!(
            matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ),
            "a self-asserted X-Fb-Admin must never reach the admin backend, got {}",
            response.status()
        );
        assert_ne!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn non_admin_session_cannot_reach_admin_backend_via_header() {
        // A genuine, *non-admin* Dashboard session that adds the headers by
        // hand must still be confined to the user backend.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("regular-user".to_string(), false, false)
            .await;

        let response = filebrowser_shell_response(
            state,
            &[
                (
                    "cookie",
                    &format!("{SECURE_SESSION_COOKIE}={}", tokens.access),
                ),
                ("x-fb-admin", "yes"),
                ("x-fb-backend", "admin"),
            ],
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a non-admin session must be refused the admin backend regardless of headers"
        );
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["error"],
            "admin Filebrowser backend requires an admin Dashboard session"
        );
    }

    #[tokio::test]
    async fn shell_bridge_requires_some_verifiable_caller() {
        // No session, no ticket, no secret: refused before any upstream call.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);

        let response = filebrowser_shell_response(state, &[("x-fb-admin", "yes")]).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn fb_sso_ticket_role_requires_a_live_matching_session() {
        // The ticket is the only header-borne role source still trusted, and
        // only because it is HMAC-signed and re-bound to a live session here.
        // A syntactically plausible but unsigned cookie must not authenticate.
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let tokens = state
            .create_session("ticket-user".to_string(), true, false)
            .await;

        // Forged ticket claiming the admin role ("a") for a real live session.
        let forged = format!(
            "v3.ticket-user.{}.9999999999.a.{}",
            tokens.access,
            "0".repeat(64)
        );
        let headers = origin_headers(&[("cookie", &format!("tronclass_fb_sso={forged}"))]);
        assert!(
            fb_sso::verified_ticket_role(&state, &headers).await.is_none(),
            "an unsigned/forged ticket must not yield a role"
        );

        // No cookie at all.
        assert!(
            fb_sso::verified_ticket_role(&state, &HeaderMap::new())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn root_redirect_without_params_is_clean() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let state = AdminWebState::new("", vec![], db, None, None, 10);
        let app = build_admin_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/?token=0123456789abcdef0123456789abcdef")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(location, "/admin");
    }
}
