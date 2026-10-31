//! Web panel with per-account Dashboard credentials.
//!
//! Dashboard passwords are deliberately separate from upstream Tronclass
//! credentials. Regular users can see and control only their own account;
//! accounts marked `is_admin` can see and control every account. The legacy
//! `access_token` configuration value is no longer accepted as a browser-login
//! bypass and is retained only for configuration compatibility.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tower_http::compression::predicate::Predicate as _;
use axum::{
    extract::{DefaultBodyLimit, Multipart, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        Html, IntoResponse, Response,
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
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{error, info, warn};
// fix128 曾誤移下行：files.rs／passkey.rs／indexnow.rs 等子模組靠 `use super::*`
// 的 glob 從這裡拿到 Uuid——刪掉會讓那三檔編不過。此 import 屬模組間契約，勿刪。
use uuid::Uuid;

use crate::account::RawAccountConfig;
use crate::adapters::events::{
    AdapterMessenger, NumberCodeRequest, OutboundMessage, QrCodeRequest, RollcallHistoryEntry,
    SecurityAlertEvent,
};
use crate::adapters::requests::{AccountRegistry, NumberCodeInput, RequestAccountState};
use crate::adapters::scanner::QrScannerRegistry;
use crate::db::{AccountsDb, MaterialEventInput};
use crate::runtime_settings::{RuntimeRestPeriod, RuntimeSettings};
use crate::trusted_ip::ClientIp;

mod actions;
mod ai_chat;
mod asset_versions;
mod auth;
mod blacklist;
mod csrf;
mod class_reminder;
mod bulletins;
mod detection_schedule;
// REST_DAY_PAGE_V1：休息日與寒暑假頁面。
mod rest_days;
// LOGIN_DEVICES_V1
mod devices;
// UPDATE_TIMES_PAGE_V1：資料更新時間頁。
mod update_times;
mod digest_pref;

// BULLETIN_PUSH_V1: 供 main.rs 啟動課程公告 Web Push 背景任務。
// BULLETIN_NOTIFY_SUMMARY_V1: 同時把 adapter messenger（Discord 等）往下傳，
// 讓公告通知帶 AI 重點摘要後同步推送到 Discord。
pub(crate) fn start_bulletin_notifier(
    state: AdminWebState,
    messenger: Option<Arc<dyn AdapterMessenger>>,
) {
    bulletins::start_bulletin_notifier(state, messenger);
}

/// MORNING_DIGEST_V1：晨間摘要排程器（每日定時推送課表/作業/公告摘要）。
pub(crate) fn start_morning_digest(state: AdminWebState) {
    morning_digest::start(state);
}

/// INDEXNOW_V1（fix105）：啟動後主動把公開首頁提交給 Bing（同一個網址 24 小時內只送一次）。
pub(crate) fn start_indexnow_auto_submit() {
    indexnow::start_auto_submit();
}

/// STOCKGENIE_BOOT_V1：面板 process 起來就接上（或補啟動）智選 sidecar。
/// ptero-start 才是主機級監工；這裡只做 health 探測＋舊腳本 fallback，不擋點名。
pub(crate) fn start_stockgenie_embed() {
    tokio::spawn(async {
        match stock::ensure_stock_sidecar().await {
            Ok(()) => tracing::info!("stockgenie sidecar ready"),
            Err(e) => tracing::warn!(error = %e, "stockgenie sidecar start failed"),
        }
    });
}
mod files;
mod materials_import;
mod homework;
mod morning_digest;
mod tc_proxy;
mod notifications;
mod pages;
mod passkey;
// RADAR_PANEL_V1（fix70）：雷達點名頁（僅管理員；設定存 radar_preferences 表）。
mod radar;
// RADAR_INPUT_PAGE_V1（fix71）：雷達點名輸入頁（管理員手動輸入座標）。
mod radar_input;
// SEO_LANDING_V1（fix60）：公開介紹頁 ＋ robots.txt／sitemap.xml。
mod seo;
// WAF_FIREWALL_V1：Web 應用防火牆與全方位資訊安全防護引擎。
mod waf;
// INDEXNOW_V1（fix105）：主動把公開首頁提交給 Bing（Bing 官方協定；Google 不參與）。
mod indexnow;
mod static_assets;
mod status;
// ROLLCALL_ATTENDANCE_V1（fix66）：點名出席統計與送出者查詢。
mod rollcall_stats;
mod credits;
// STOCKGENIE_EMBED_V1：智選股 sidecar（/admin/stock + /admin/stock-api → 127.0.0.1:18765）。
mod stock;
// ACCOUNT_SUSPEND_V1：帳號暫時停權（管理頁＋API）。
mod suspensions;
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
use self::pages::{load_runtime_theme, ADMIN_BLOCKED_HTML, ADMIN_GEO_BLOCKED_HTML, ADMIN_HOSTING_BLOCKED_HTML};
use self::utils::*;

const PW_SALT: &[u8] = b"tronclass-rollcall-admin-web-password-hash-v2";

// Body limits are assigned to individual POST routes below. Multipart material
// uploads no longer have a per-file cap (UPLOAD_NO_FILE_CAP_V1, fix54), so the
// route-level allowance has to cover a single file that fills the whole quota;
// ordinary JSON must stay small enough that extraction before handler-local
// authorization cannot spike RSS.
const ADMIN_TINY_BODY_LIMIT: usize = 8 * 1024;
const ADMIN_LOGIN_BODY_LIMIT: usize = 16 * 1024;
// AI_CHAT_V7：AI 聊天可以附圖片（base64 會膨脹約 4/3），預設的 2 MB 不夠用。
const ADMIN_AI_CHAT_BODY_LIMIT: usize = 20 * 1024 * 1024;
const ADMIN_MOCK_BODY_LIMIT: usize = 64 * 1024;
const ADMIN_JSON_BODY_LIMIT: usize = 256 * 1024;
// UPLOAD_NO_FILE_CAP_V1（fix54）：教材單檔已無 50 MiB 上限，body limit 必須容得下
// 「單一檔案吃滿整個配額」的情形（總容量 10 GiB ＋ multipart 邊界與欄位標頭的少量開銷）。
// DefaultBodyLimit 只是門檻、不會預先配置記憶體；真正的配額仍由 handler 在收檔迴圈裡
// 逐片段檢查（超過就 413 並刪除暫存檔）。
const ADMIN_UPLOAD_BODY_LIMIT: usize = MATERIALS_MAX_TOTAL_SIZE + 16 * 1024 * 1024;

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
// DASHBOARD_ACCESS_TTL_V1: 原 30 分鐘太短，使用者工作常態超過 30 分鐘後第一次
// 請求會先收到 401 才觸發 refresh（前端顯示「Unauth」，再點一次才成功）。
// 延長到 12 小時：白天工作期間不易過期，仍保留 7 天的 refresh token 作長效保障。
// 純延長時間，不設定 cookie 資料、不改安全性質（仍 HttpOnly / SameSite=Lax）。
/// TC_NAV_TOKEN_V1（fix34）：轉址 URL 一次性 token 的效期。
const TC_NAV_TOKEN_TTL_SECS: u64 = 90;
/// TC_NAV_TOKEN_V1：tc_proxy_ticket cookie（子資源短效票券）的效期。
const TC_NAV_TICKET_TTL_SECS: u64 = 120;
/// TC_REFERER_GRANT_V1（fix35）：URL 消費後的「頁面授權」效期——滑動式
///（頁面持續發請求就不過期）。
///
/// TC_URL_GRANT_REPLAY_V1（fix36）：15 → 30 分鐘，且 URL 本身也放行重放。
/// 舊版把「URL 重放」一律當成攻擊面，首次消費後就拒絕 `?tc_token` 再被使用，
/// 結果使用者在 iClass 分頁按 F5／上一頁時鑑權全滅，被導到面板登入頁——
/// 回報就是「按了按鈕之後重整就被登出」。URL 內的 token 與頁面 Referer 帶的
/// 是同一個祕密，而 Referer 授權本來就能在效期內重複使用，因此 URL 一併放行
/// （每次使用滑動延長）；閒置 30 分鐘後失效，需回面板重按按鈕。
const TC_PAGE_GRANT_TTL_SECS: u64 = 30 * 60;

const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Long-lived refresh token lifetime (7 days). Each refresh rotates it.
const REFRESH_TOKEN_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// TKU_SHORT_SESSION_V1：淡江大學校園網段（163.13.0.0/16，TANet 配賦給淡江大學，
/// 已以 WHOIS 核實）登入時，access／refresh 一律縮短為 1 小時，逾時即需重新登入
/// ——不會被背景 refresh 悄悄延長。此網段是校園共用網路（實驗室／公用電腦等風險
/// 較高的環境常見於此類網段），縮短曝險窗口；其餘來源 IP 不受影響，仍是原本的
/// 12 小時／7 天。套用點只有 `create_session_with` 一處：無論是全新登入或是
/// refresh 輪換（輪換時沿用的是原始登入 IP，見 `SessionOrigin::or_fill`），
/// 只要這個 session 的來源 IP 落在此網段，每次重新核發 token 都會套用這個短效期。
const TKU_SHORT_SESSION_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_SESSIONS: usize = 500;
/// ERROR_DETAIL_HIDDEN_V1：回給瀏覽器的錯誤只保留操作名稱；內部細節（資料庫、路徑、上游回應）
/// 寫進伺服器日誌，不外洩給前端。
pub(crate) fn public_error(context: &str, detail: &dyn std::fmt::Display) -> String {
    tracing::warn!(context = %context, error = %detail, "API 錯誤（細節未回傳給瀏覽器）");
    let label = context.trim_end_matches(|c: char| c == '：' || c == ':' || c == ' ');
    format!("{label}，請查看伺服器日誌")
}

const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
// PEN_TEST_V1：測試模式放寬登入嘗試上限，讓暴力破解／撞庫類測試能跑完。
// ⚠️ 風險提醒：這兩個值同時也是「真實使用者帳號」的暴力破解防護。
//    若在對外服務上以 pen_test 建置，任何人都能對任何帳號無限次嘗試密碼。
//    測完請務必用不帶 feature 的建置重新部署。
const RATE_LIMIT_MAX_ATTEMPTS: u32 = 5;
const IP_RATE_LIMIT_MAX_ATTEMPTS: u32 = 20;
const MAX_RATE_LIMIT_KEYS: usize = 4096;
/// TEST_NOTIFICATION_RATE_LIMIT_V1：各「發送測試」端點每帳號每分鐘上限
/// （digest_test／push/test／bulletins/test／homework/test-notification 共用同一套邏輯，
/// 但各自獨立計數）。防止登入者把測試端點當推播轟炸工具，也避免 Web Push 額度被耗盡。
const TEST_NOTIFICATION_MAX_PER_WINDOW: u32 = 3;
const MAX_SECURITY_WATCH_KEYS: usize = 4096;
const SECURITY_FAIL_ALERT_THRESHOLD: u32 = 5;
/// LOGIN_TIERED_LOCKOUT_V1：安全計數（帳號／IP 連續失敗）的觀察窗口。
/// 與 60 秒的速率限制窗口分開：速率限制只擋洪水，這裡要抓「慢速但持續」的猜密碼，
/// 所以拉長到 15 分鐘；登入成功會立即清零（見 `clear_auth_failures`）。
const SECURITY_FAIL_WINDOW: Duration = Duration::from_secs(15 * 60);
/// LOGIN_TIERED_LOCKOUT_V1：來源 IP 在觀察窗口內失敗「超過十次」（第 11 次）自動加入黑名單。
/// 第一層（5 次）只鎖該帳號的密碼登入並通知持有人；第二層才封 IP。
const SECURITY_AUTO_BLOCK_THRESHOLD: u32 = 11;
/// LOGIN_TIERED_LOCKOUT_V1：第一層鎖定的時間（只鎖密碼登入；通行密鑰不受影響）。
/// 帳號持有人可從 Discord 通知按「取消這次鎖定」立即解除。
const ACCOUNT_LOGIN_LOCK_TTL: Duration = Duration::from_secs(10 * 60);
const SECURITY_ALERT_COOLDOWN: Duration = Duration::from_secs(10 * 60);
/// LOGIN_GEO_ALERT_V1：成功登入前 15 分鐘窗口內，同帳號或同 IP 的密碼失敗次數達此值，
/// 即使新 IP 在本國也視為「可疑（疑似撞對密碼）」而告警。
const LOGIN_GEO_SUSPICIOUS_FAILURES: u32 = 3;
const SESSION_COOKIE_MARKER: &str = "cookie";
const SECURE_SESSION_COOKIE: &str = "__Host-tronclass_admin_session";
const LOCAL_SESSION_COOKIE: &str = "tronclass_admin_session";
/// HttpOnly refresh-token cookie name.
const SECURE_REFRESH_COOKIE: &str = "__Host-tronclass_admin_refresh";
const LOCAL_REFRESH_COOKIE: &str = "tronclass_admin_refresh";
// TKU_SHORT_SESSION_V1：access/refresh cookie 的實際 Max-Age 不再是固定常數——
// 一律由 `create_session_with` 依來源 IP 算好、經 `SessionTokens.access_max_age`／
// `refresh_max_age` 交給 `append_session_cookie`／`append_refresh_cookie`。一般情況
// 下數值仍等於 `ACCESS_TOKEN_TTL`／`REFRESH_TOKEN_TTL`；原本這裡重複定義的
// `SESSION_COOKIE_MAX_AGE`／`REFRESH_COOKIE_MAX_AGE`（與上面兩個常數數值完全相同）
// 已移除，避免兩份數字各自維護、容易改一個漏改另一個。

/// HSTS_PRELOAD_ALIGN_V1：HSTS 只在這一處定義，面板／智選／掃碼頁共用。
///
/// 線上 Cloudflare 曾蓋成 `max-age=15552000; includeSubDomains; preload`
///（180 天），源站原本是一年且無 preload，瀏覽器看到的與程式不一致。
/// 正式政策改為一年 + `includeSubDomains` + `preload`：preload 在 CF 已經開啟，
/// max-age 拉回 31536000。部署後仍要在 CF「SSL/TLS → Edge Certificates → HSTS」
/// 設成同一組（或關閉 CF HSTS 讓源站標頭出去），否則邊緣會繼續蓋掉。
pub(crate) const HSTS_HEADER_VALUE: &str = "max-age=31536000; includeSubDomains; preload";

/// REFRESH_ROTATE_GRACE_V1（fix38）：refresh 輪換的寬限視窗（秒）。
///
/// 舊制度是「一次性」輪換：舊 refresh 立刻刪除、舊 access 立刻撤銷。於是
/// 多分頁（面板＋子頁面）、同一分頁的並行重試、或「背景分頁的 refresh 與使用者
/// 剛完成的新登入」只要撞在一起，晚到的那個一定失敗 → 前端把 cookie 當失效、
/// 導回登入頁。使用者看到的就是「一直被自動登出」與「顯示登入成功後又跳回登入
/// 畫面」。
///
/// 現在：視窗內的舊 refresh 重播時直接拿回同一組新 token（不再輪換一次），舊
/// access 也保留到視窗結束才移除；視窗一過，行為與舊版完全相同。
const SESSION_ROTATE_GRACE_SECS: u64 = 180;

/// PROXY_CSRF_BRIDGE_V1（fix40）：iClass `csrftoken` 在記憶體中的保存時間。
/// Django 於登入時輪換它；保存值只用來讓代理請求與注入頁面帶「同一個」token，
/// 過期就重新從上游回應學回來。
const PROXIED_CSRF_TTL_SECS: u64 = 6 * 60 * 60;

pub(super) fn validate_admin_text(
    value: &str,
    max_bytes: usize,
    field: &'static str,
) -> Result<(), Response> {
    if value.len() > max_bytes {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("{field} is too long; max {max_bytes} bytes")
            })),
        )
            .into_response());
    }
    // LOG_INJECTION_GUARD_V1（fix118）：拒絕所有 Unicode 控制字元
    // （C0/C1/DEL）。這些欄位（username／password／file path／multipart 欄位名…）
    // 會原樣寫進伺服器日誌、稽核紀錄、限流鍵與 Discord／Web Push 告警；允許
    // \n／\r 就能偽造日誌行甚至注入假告警內容。正常 Web 輸入皆不含控制字元。
    if value.chars().any(|c| c.is_control()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("{field} contains disallowed control characters")
            })),
        )
            .into_response());
    }
    Ok(())
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
    /// LOGIN_THROTTLE_SQLITE_V1：有 DB 時以 SQLite 為主（重啟不歸零）；DB 出錯退回記憶體。
    db: Option<AccountsDb>,
}

/// LOGIN_THROTTLE_SQLITE_V1：`record_auth_failure` / `lock_account_login` 等自由函式
/// 也要能碰 DB（Discord 的 `cancel_login_lock` 沒有 state 可傳），所以在
/// `AdminWebState::new` 時登記一份 handle。
static THROTTLE_DB: once_cell::sync::OnceCell<AccountsDb> = once_cell::sync::OnceCell::new();

fn throttle_db() -> Option<&'static AccountsDb> {
    THROTTLE_DB.get()
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            attempts: Arc::new(Mutex::new(HashMap::new())),
            db: None,
        }
    }

    /// LOGIN_THROTTLE_SQLITE_V1：帶 SQLite 後端的速率限制器。
    fn with_db(db: AccountsDb) -> Self {
        Self {
            attempts: Arc::new(Mutex::new(HashMap::new())),
            db: Some(db),
        }
    }

    async fn check(&self, key: &str) -> bool {
        self.check_limit(key, RATE_LIMIT_MAX_ATTEMPTS).await
    }

    async fn check_limit(&self, key: &str, max_attempts: u32) -> bool {
        // LOGIN_THROTTLE_SQLITE_V1：先走 SQLite；語意同記憶體版——被拒絕的嘗試不記入窗口。
        if let Some(db) = self.db.as_ref() {
            let now = unix_now_secs();
            let since = now - RATE_LIMIT_WINDOW.as_secs() as i64;
            match db.throttle_count("rate", key, since).await {
                Ok(count) if count >= max_attempts => return false,
                Ok(_) => match db.throttle_record("rate", key, now, RATE_LIMIT_WINDOW.as_secs() as i64).await {
                    Ok(_) => return true,
                    Err(error) => warn!(error = %error, "節流寫入 SQLite 失敗，退回記憶體計數"),
                },
                Err(error) => warn!(error = %error, "節流讀取 SQLite 失敗，退回記憶體計數"),
            }
        }
        self.check_limit_memory(key, max_attempts).await
    }

    async fn check_limit_memory(&self, key: &str, max_attempts: u32) -> bool {
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
        if let Some(db) = self.db.as_ref() {
            if let Err(error) = db.throttle_clear("rate", key).await {
                warn!(error = %error, "清除 SQLite 節流計數失敗");
            }
        }
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

/// TEST_NOTIFICATION_RATE_LIMIT_V1：測試通知端點的每帳號節流。
/// `kind` 區分端點（digest / push / bulletin / homework），同一帳號各端點獨立計數；
/// 超過上限回 429 並附中文訊息，前端 runButton 會原樣顯示。
async fn check_test_notification_rate(
    state: &AdminWebState,
    kind: &str,
    account_id: &str,
) -> Result<(), Response> {
    let key = format!("test-notify:{kind}:{}", account_id.trim());
    if state
        .rate_limiter
        .check_limit(&key, TEST_NOTIFICATION_MAX_PER_WINDOW)
        .await
    {
        Ok(())
    } else {
        warn!(kind, account = %account_id, "測試通知端點觸發節流");
        Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": format!(
                    "測試通知太頻繁：每分鐘最多 {TEST_NOTIFICATION_MAX_PER_WINDOW} 次，請稍後再試"
                )
            })),
        )
            .into_response())
    }
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
        || path.starts_with("/admin/pwa/")
        || path.starts_with("/apple-touch-icon")
        || path.starts_with("/favicon.")
        || path == "/healthz"
        || path == "/admin/api/logout"
        // IP_BLACKLIST_AUTO_REDIRECT_V1：被擋頁面（`blocked.html`）輪詢用，
        // 只回傳「這個 IP 現在還在不在黑名單」的布林值，不會讓任何人多拿到
        // 存取權限，跟上面幾個豁免路徑是同一種考量。
        || path == "/admin/api/blacklist/status"
        || path == "/admin/logout"
        // BLACKLIST_CONTACT_ADMIN_GAP_FIX_V1：這支中介層跑在 `reject_hosting_ip_
        // middleware`／`reject_foreign_admin_ip_middleware` 之後（見本檔案組裝
        // router 的 `.layer()` 疊加順序），三層共用同一個 `/admin/api/*` 範圍。
        // 那兩層都已經豁免「聯絡管理員」與輪詢這兩支端點（道理見它們各自的
        // 註解），但這層當初沒有一起加——於是出現一個組合漏洞：某個訪客被
        // `hosting_blocked.html`／`geo_blocked.html` 擋下，**同時**他的來源 IP
        // 又剛好也在這份黑名單裡（例如 iCloud 私密轉寄的出口 IP 是很多不同
        // 使用者共用的一小撮 Akamai／Cloudflare IP，容易因為別人的行為被列入
        // 黑名單），他按下「聯絡管理員」送出的請求會被這層攔下來回
        // `access_denied`，前端顯示「傳送失敗」——事實上請求根本沒送到，連
        // 管理員的待處理清單都看不到這筆。這兩支端點本身不授予任何存取權限
        // （只是新增一筆待處理申請／查詢是否已放行），與上面兩層排除它們的
        // 理由完全相同，這裡補上同樣的豁免。
        || path == "/admin/api/geo-restrict/request-access"
        || path == "/admin/api/geo-restrict/access-status"
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

// ── ADMIN_GEO_RESTRICT_V1：僅限台灣（或設定檔 `login_home_countries` 列出的國家）
//    IP 存取 /admin 管理後台 ───────────────────────────────────────────────
//
// 需求原文：「國外 IP 一律被系統擋下來」。範圍與使用者確認過的刻意排除：
// * `/pub/*`（公開分享連結）——本來就是要分享給外部人看的，可能人在國外。
// * `/admin/oidc/*`（OIDC 端點）——可能被架在國外的第三方伺服器呼叫（server-to-
//   server token 交換），不是使用者本人的瀏覽器連線。
// * `/`、`robots.txt`、`sitemap.xml`、PWA 圖示／manifest、`/healthz`——本來就
//   不在 `/admin` 底下，且是公開資訊或搜尋引擎／探活用途，與既有
//   `reject_blacklisted_ip_middleware` 的豁免範圍一致（不要讓 Google 的美國
//   爬蟲因為這個新功能連首頁都爬不到）。
// * `/homework`——雖然不是 `/admin` 開頭，但只是 `/admin/homework` 的別名、同一
//   支 handler，視同管理後台的一部分一併納管。

/// SECURITY_SETTING_CACHE_V1：「僅限台灣 IP」與「封鎖雲端／VPS機房 IP」這兩個
/// 開關，分別掛在全 /admin 與全站的中介層上——等於每一個進來的請求都會各自
/// 查一次 `security_settings` 表。這兩個開關平常只有管理員在 `/admin/blacklist`
/// 頁手動切換時才會變，頻率極低，卻因為掛在熱路徑上被查得最頻繁，尤其是這個
/// 中介層本來就是為了擋「大量自動化掃描流量」而存在，流量越大、這類可以快取
/// 掉的 DB 往返反而越可觀，本末倒置。
///
/// 做法：短 TTL（10 秒）記憶體快取，key 是設定鍵名（兩者都是 `&'static str`
/// 常數，不會動態產生，用它們本身當 key 不需要額外配置一份字串）。更重要的是
/// 「寫入」那一刻（`admin_geo_restrict_set_handler` / `admin_hosting_restrict_
/// set_handler`）會主動呼叫 [`invalidate_security_toggle_cache`] 清掉對應的
/// 快取項目——管理員按下開關，下一個請求就看得到新值，不必被動等 10 秒 TTL
/// 過期，避免「我明明已經關掉了，怎麼還被擋」這種困惑。
const SECURITY_SETTING_CACHE_TTL: Duration = Duration::from_secs(10);

static SECURITY_SETTING_CACHE: once_cell::sync::Lazy<Mutex<HashMap<&'static str, (Instant, bool)>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

/// 讀快取；命中且還沒過 TTL 才回傳 `Some`，否則 `None`（呼叫端自己查 DB，
/// 查完後再呼叫 [`store_security_toggle_cache`] 寫回去——這裡刻意不負責
/// 查詢本身，兩個呼叫端的「查不到／查詢失敗時要怎麼辦」政策並不相同，
/// 硬塞進同一支共用函式只會讓邏輯變難讀，不值得為了省幾行重複程式碼）。
async fn cached_security_toggle(key: &str) -> Option<bool> {
    let cache = SECURITY_SETTING_CACHE.lock().await;
    cache.get(key).and_then(|(at, value)| {
        if Instant::now().saturating_duration_since(*at) < SECURITY_SETTING_CACHE_TTL {
            Some(*value)
        } else {
            None
        }
    })
}

async fn store_security_toggle_cache(key: &'static str, value: bool) {
    let mut cache = SECURITY_SETTING_CACHE.lock().await;
    cache.insert(key, (Instant::now(), value));
}

/// 寫入開關後立刻呼叫，清掉快取——見上方模組註解。沒有這個 key 的快取項目時
/// `remove` 本來就是沒事發生，呼叫端不需要先判斷快取是否存在。
async fn invalidate_security_toggle_cache(key: &str) {
    let mut cache = SECURITY_SETTING_CACHE.lock().await;
    cache.remove(key);
}

/// 讀取「僅限台灣 IP 存取 /admin」開關的生效值。
///
/// 兩種「不確定」情況，刻意採不同的保守方向（都已與使用者確認過）：
/// * 資料庫裡**從未設定過**這個鍵（帳號資料庫是舊的、這個功能剛上線）→ 預設
///   **開啟**（使用者要求的預設行為）。
/// * 讀取這個鍵本身就**失敗**（資料庫一時故障等）→ 視為**關閉**（不擋），
///   與既有 `ip_is_blacklisted` 讀取失敗時「暫不拒絕請求」同一套保守原則：
///   安全性開關本身壞掉，不該連帶把整個台灣的使用者一起鎖在外面。
async fn geo_restrict_admin_enabled(state: &AdminWebState) -> bool {
    if let Some(cached) = cached_security_toggle(crate::db::GEO_RESTRICT_ADMIN_KEY).await {
        return cached;
    }
    let value = match state
        .accounts_db
        .get_security_setting(crate::db::GEO_RESTRICT_ADMIN_KEY)
        .await
    {
        Ok(Some(value)) => value.trim() == "1",
        Ok(None) => true,
        Err(error) => {
            warn!(error = %error, "讀取「僅限台灣 IP 存取」開關失敗，暫不擋國外 IP");
            false
        }
    };
    store_security_toggle_cache(crate::db::GEO_RESTRICT_ADMIN_KEY, value).await;
    value
}

/// 設定檔 `adapters.admin_web.login_home_countries` 當「本國」清單用；
/// 與 LOGIN_GEO_ALERT_V1 共用同一份清單（見 `config.rs` 欄位文件註解）。
/// 空陣列對這個功能沒有意義（等同全擋），固定退回 `["TW"]`。
fn geo_restrict_home_countries() -> Vec<String> {
    let configured: Vec<String> = crate::config::try_get_config()
        .map(|cfg| {
            cfg.adapters
                .admin_web
                .login_home_countries
                .iter()
                .map(|c| c.trim().to_ascii_uppercase())
                .filter(|c| !c.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if configured.is_empty() {
        vec!["TW".to_string()]
    } else {
        configured
    }
}

/// 最小的 HTML 跳脫：geo.label() 可能含外部服務回傳、未必可信的文字
/// （城市／ISP 名稱），嵌回靜態頁面前一定要跳脫，否則是反射型 XSS 風險。
fn geo_escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn geo_blocked_browser_response(geo: &crate::geoip::GeoInfo) -> Response {
    let page = ADMIN_GEO_BLOCKED_HTML
        .replace("__GEO_LOCATION__", &geo_escape_html(&geo.label()));
    let mut response = (StatusCode::FORBIDDEN, Html(page)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn geo_blocked_json_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "access_denied_geo" })),
    )
        .into_response()
}

/// 這個路徑是不是「管理後台的一部分」（含 `/homework` 別名），決定要不要套用
/// 國外 IP 限制。刻意排除 `/admin/oidc/*`（見本節開頭說明）。
fn is_admin_guarded_path(path: &str) -> bool {
    if path == "/homework" {
        return true;
    }
    if !path.starts_with("/admin") {
        return false;
    }
    if path.starts_with("/admin/oidc/") {
        return false;
    }
    true
}

async fn reject_foreign_admin_ip_middleware(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path();
    if !is_admin_guarded_path(path)
        || path.starts_with("/admin/static/")
        || path.starts_with("/admin/pwa/")
        || path == "/admin/api/logout"
        || path == "/admin/logout"
        // ADMIN_GEO_RESTRICT_V1：被擋下的人要能按「聯絡管理員」——這支端點只會
        // 寫一筆「待處理」申請並通知管理員，不會讓任何人多拿到存取權限（真正
        // 放行要管理員本人在 Discord 或 /admin/blacklist 按過才算數），跟
        // `/admin/api/logout` 一樣是刻意公開、本質上不構成繞過限制的例外。
        || path == "/admin/api/geo-restrict/request-access"
        // GEO_ACCESS_AUTO_REDIRECT_V1：被擋頁面輪詢用——只回傳「這個 IP 現在
        // 是否已經被核准暫時放行」的布林值，本身不會讓任何人多拿到存取權限
        // （跟上面那支一樣，查詢結果是 true 也只是代表「已經有管理員核准過」，
        // 不是這支端點自己授權的），所以可以公開、不算繞過限制。
        || path == "/admin/api/geo-restrict/access-status"
        // IP_BLACKLIST_AUTO_REDIRECT_V1：黑名單擋下頁面也會輪詢這支（道理同
        // 上一支），讓「國外 IP 同時也在黑名單」這種組合狀況下，黑名單那邊
        // 的輪詢一樣連得到，不會被這層攔住。
        || path == "/admin/api/blacklist/status"
    {
        return next.run(request).await;
    }
    if !geo_restrict_admin_enabled(&state).await {
        return next.run(request).await;
    }
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip());
    let trusted = crate::trusted_ip::trusted_from_global_config();
    let ip = crate::trusted_ip::resolve_client_ip(peer, &headers, &trusted);
    // IP 都解析不出來（"unknown"）：不是這層該處理的問題，交給其他防線，這裡放行。
    // 區網／私有位址（內部健康檢查、同網段存取）本來就查不出國家，直接放行。
    if ip == "unknown" || blacklist::is_private_or_local_ip(&ip) {
        return next.run(request).await;
    }
    // ADMIN_GEO_RESTRICT_V1：管理員已經核准過這個 IP 的「暫時放行」申請——
    // 時限內一律放行，不查地區。讀取失敗視為「沒有放行」（fail-closed在這裡才
    // 對：查不到就當作沒放行，退回正常的國別判斷，不會因為這一步出錯就放行
    // 不該放行的連線）。
    match state.accounts_db.geo_access_is_ip_allowed(&ip).await {
        Ok(true) => return next.run(request).await,
        Ok(false) => {}
        Err(error) => warn!(error = %error, "查詢國外存取暫時放行狀態失敗，退回正常地區判斷"),
    }
    let cf_ipcountry = cf_ipcountry_header(&headers);
    let geo = crate::geoip::resolve(&ip, cf_ipcountry.as_deref()).await;
    // 查不到國家（逾時／外部服務掛掉／Tor 的 CF 標頭 `T1` 沒有國碼）：使用者已
    // 明確選擇 fail-open——查詢服務萬一掛掉，寧可偶爾放過一個真正的國外連線，
    // 也不要因為查不到就把全台灣的人一起擋在外面。
    if geo.country_code.is_empty() {
        return next.run(request).await;
    }
    let home_countries = geo_restrict_home_countries();
    if home_countries.iter().any(|c| c == &geo.country_code) {
        return next.run(request).await;
    }
    if !path.starts_with("/admin/api/") && request_prefers_html(&headers) {
        return geo_blocked_browser_response(&geo);
    }
    geo_blocked_json_response()
}

// ── HOSTING_IP_BLOCK_V1：全站封鎖雲端／VPS機房出口 IP ─────────────────────
//
// 需求原文：「國外 IP 擋得到，那像 AWS／Google Cloud 這種雲端／VPS 機房的 IP
// 能不能也自動擋掉，不要讓它進到我的伺服器」。與使用者確認過的三項範圍：
// 1. 套用範圍＝**全站**，比照 `waf_firewall_middleware`（WAF 本身沒有任何路徑
//    豁免，連 `/pub/*` 都會被檢查）——不是只保護 `/admin`。因此這裡刻意
//    **不**比照 ADMIN_GEO_RESTRICT_V1 把 `/pub/*` 排除在外；若之後發現有
//    正常使用情境被誤擋（例如有人用公司 VPN／雲端主機看分享連結），再回頭
//    加例外即可。
// 2. 涵蓋商家＝「所有 VPS／機房代管商」，不只三大雲——改用 ip-api.com 的
//    `hosting` 欄位（見 `crate::hosting_ip` 模組文件），不是只列 AWS/GCP/Azure
//    官方 IP 段。
// 3. 誤擋的 bypass 機制＝沿用既有「聯絡管理員」流程，不另開一套：共用
//    ADMIN_GEO_RESTRICT_V1 的 `geo_access_requests` 表與
//    `/admin/api/geo-restrict/request-access` + `/admin/api/geo-restrict/
//    access-status` 兩支端點（這兩支端點本來就只認 IP，不管「被擋的原因是
//    地區還是機房」），管理員在 Discord／`/admin/blacklist` 核准一次，兩邊
//    （地區限制與機房限制）都會對這個 IP 放行 4 小時，不需要學兩套操作。
//
// `/admin/oidc/*` 仍然排除：很多機構的 OIDC 身分提供者本身就架在雲端
// （Azure AD、Google 等），擋掉等於直接打斷所有人的第三方登入，這點比
// ADMIN_GEO_RESTRICT_V1 排除它的理由（server-to-server 呼叫可能來自國外）
// 更直接——OIDC 的伺服器對伺服器呼叫幾乎必然來自雲端 IP。

fn hosting_blocked_browser_response(info: &crate::hosting_ip::HostingInfo) -> Response {
    let page = ADMIN_HOSTING_BLOCKED_HTML.replace("__HOSTING_LOCATION__", &geo_escape_html(&info.label()));
    let mut response = (StatusCode::FORBIDDEN, Html(page)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn hosting_blocked_json_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "access_denied_hosting" })),
    )
        .into_response()
}

/// 讀取「封鎖雲端／VPS機房 IP」開關的生效值；預設值／失敗時的保守方向與
/// `geo_restrict_admin_enabled` 完全一致（見其文件註解），不重複贅述。
async fn hosting_ip_block_enabled(state: &AdminWebState) -> bool {
    // SECURITY_SETTING_CACHE_V1：這支掛在全站中介層上，短 TTL 快取見
    // `cached_security_toggle` 模組註解。
    if let Some(cached) = cached_security_toggle(crate::db::HOSTING_IP_BLOCK_KEY).await {
        return cached;
    }
    let value = match state
        .accounts_db
        .get_security_setting(crate::db::HOSTING_IP_BLOCK_KEY)
        .await
    {
        Ok(Some(value)) => value.trim() == "1",
        Ok(None) => true,
        Err(error) => {
            warn!(error = %error, "讀取「封鎖雲端／VPS機房 IP」開關失敗，暫不擋機房 IP");
            false
        }
    };
    store_security_toggle_cache(crate::db::HOSTING_IP_BLOCK_KEY, value).await;
    value
}

async fn reject_hosting_ip_middleware(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path();
    if path.starts_with("/admin/static/")
        || path.starts_with("/admin/pwa/")
        || path.starts_with("/apple-touch-icon")
        || path.starts_with("/favicon.")
        || path == "/healthz"
        || path == "/admin/api/logout"
        || path == "/admin/logout"
        || path.starts_with("/admin/oidc/")
        // HOSTING_IP_BLOCK_V1：沿用 ADMIN_GEO_RESTRICT_V1 既有的「聯絡管理員」
        // 與輪詢端點（見本節開頭說明）——被擋的人才能回報／查詢自己是否已
        // 被放行，否則連申請放行的入口都被這層自己擋住。
        || path == "/admin/api/geo-restrict/request-access"
        || path == "/admin/api/geo-restrict/access-status"
    {
        return next.run(request).await;
    }
    if !hosting_ip_block_enabled(&state).await {
        return next.run(request).await;
    }
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip());
    let trusted = crate::trusted_ip::trusted_from_global_config();
    let ip = crate::trusted_ip::resolve_client_ip(peer, &headers, &trusted);
    // IP 解析不出來、或區網／私有位址：不是這層該處理的問題，直接放行
    // （道理同 `reject_foreign_admin_ip_middleware`）。
    if ip == "unknown" || blacklist::is_private_or_local_ip(&ip) {
        return next.run(request).await;
    }
    // GOOGLE_CRAWLER_ALLOWLIST_V1：Google 官方爬蟲／抓取器的 IP 一律放行（ip-api 會把 Googlebot
    // 標成機房 IP，不放行的話整站對搜尋引擎回 403）。只比對 Google 官方公布的範圍，不靠 UA。
    // 黑名單與其他層的檢查不受影響（這裡只豁免「機房 IP」這一層）。
    if crate::hosting_ip::is_verified_google_crawler(&ip) {
        return next.run(request).await;
    }
    // 共用既有「國外存取」暫時放行表：不管被擋原因是地區還是機房 IP，
    // 管理員核准一次、時限內兩邊都放行，見本節開頭第 3 點。
    match state.accounts_db.geo_access_is_ip_allowed(&ip).await {
        Ok(true) => return next.run(request).await,
        Ok(false) => {}
        Err(error) => warn!(error = %error, "查詢暫時放行狀態失敗，退回正常機房 IP 判斷"),
    }
    let info = match crate::hosting_ip::lookup(&ip).await {
        Some(info) => info,
        // 查詢失敗／逾時：fail-open，不擋（與 `crate::hosting_ip` 模組文件中
        // 說明的政策一致）。
        None => return next.run(request).await,
    };
    if !info.is_hosting {
        return next.run(request).await;
    }
    // HOSTING_IP_RECENT_BLOCKS_V1：只在真的要擋下來的這一刻記一筆，放行的
    // 請求（上面已經 return 的那些分支）不記，見 `hosting_ip` 模組文件。
    crate::hosting_ip::record_block(&ip, path, &info.label());
    if !path.starts_with("/admin/api/") && request_prefers_html(&headers) {
        return hosting_blocked_browser_response(&info);
    }
    hosting_blocked_json_response()
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

pub(super) fn security_now_label() -> String {
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
    // LOGIN_THROTTLE_SQLITE_V1：失敗計數先寫 SQLite（重啟不歸零）；任一步失敗退回記憶體。
    if let Some(db) = throttle_db() {
        let now = unix_now_secs();
        let window = SECURITY_FAIL_WINDOW.as_secs() as i64;
        let user_key = format!("user:{}", username.trim().to_lowercase());
        // IP_BLACKLIST_V6_GROUP_V1：失敗計數以「群組鍵」累計——IPv6 位址輪換
        // 不能歸零計數、閃過自動封鎖門檻。
        let ip_key = format!("ip:{}", crate::trusted_ip::blacklist_group_key(ip));
        let user = db.throttle_record("fail", &user_key, now, window).await;
        let ip_count = db.throttle_record("fail", &ip_key, now, window).await;
        match (user, ip_count) {
            (Ok(u), Ok(i)) => return (u, i),
            (user, ip_count) => {
                if let Err(error) = user.and(ip_count) {
                    warn!(error = %error, "失敗計數寫入 SQLite 失敗，退回記憶體計數");
                }
            }
        }
    }
    let mut watch = SECURITY_WATCH.lock().await;
    let now = Instant::now();
    // LOGIN_TIERED_LOCKOUT_V1：改用 15 分鐘觀察窗口（原本沿用 60 秒速率限制窗口）。
    let window_start = now - SECURITY_FAIL_WINDOW;
    let mut counts = [0u32, 0u32];
    for (idx, key) in [
        format!("user:{}", username.trim().to_lowercase()),
        format!("ip:{}", crate::trusted_ip::blacklist_group_key(ip)),
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

/// LOGIN_GEO_ALERT_V1：讀出（不清除）15 分鐘窗口內該帳號與該 IP 的密碼失敗次數，取較大者。
/// 必須在 `clear_auth_failures` 之前呼叫。
async fn peek_recent_auth_failures(username: &str, ip: &str) -> u32 {
    if let Some(db) = throttle_db() {
        let since = unix_now_secs() - SECURITY_FAIL_WINDOW.as_secs() as i64;
        let user = db
            .throttle_count("fail", &format!("user:{}", username.trim().to_lowercase()), since)
            .await;
        let ip_count = db
            .throttle_count(
                "fail",
                &format!("ip:{}", crate::trusted_ip::blacklist_group_key(ip)),
                since,
            )
            .await;
        if let (Ok(u), Ok(i)) = (&user, &ip_count) {
            return (*u).max(*i);
        }
        if let Err(error) = user.and(ip_count) {
            warn!(error = %error, "讀取 SQLite 失敗計數失敗，退回記憶體計數");
        }
    }
    let watch = SECURITY_WATCH.lock().await;
    let window_start = Instant::now() - SECURITY_FAIL_WINDOW;
    let count = |key: String| -> u32 {
        watch
            .fails
            .get(&key)
            .map(|attempts| attempts.iter().filter(|at| **at > window_start).count() as u32)
            .unwrap_or(0)
    };
    count(format!("user:{}", username.trim().to_lowercase())).max(count(format!(
        "ip:{}",
        crate::trusted_ip::blacklist_group_key(ip)
    )))
}

/// 登入成功後重置該帳號與 IP 的失敗計數，讓告警只在「連續」失敗時觸發。
async fn clear_auth_failures(username: &str, ip: &str) {
    if let Some(db) = throttle_db() {
        let user_key = format!("user:{}", username.trim().to_lowercase());
        let ip_key = format!("ip:{}", crate::trusted_ip::blacklist_group_key(ip));
        if let Err(error) = db.throttle_clear("fail", &user_key).await {
            warn!(error = %error, "清除 SQLite 失敗計數失敗");
        }
        if let Err(error) = db.throttle_clear("fail", &ip_key).await {
            warn!(error = %error, "清除 SQLite 失敗計數失敗");
        }
    }
    let mut watch = SECURITY_WATCH.lock().await;
    watch
        .fails
        .remove(&format!("user:{}", username.trim().to_lowercase()));
    watch.fails.remove(&format!(
        "ip:{}",
        crate::trusted_ip::blacklist_group_key(ip)
    ));
}

// ── LOGIN_TIERED_LOCKOUT_V1：第一層「帳號密碼登入鎖定」 ─────────────────────
//
// 觸發：同一帳號在 SECURITY_FAIL_WINDOW 內密碼失敗達 SECURITY_FAIL_ALERT_THRESHOLD 次。
// 效果：該帳號的「密碼登入」在 ACCOUNT_LOGIN_LOCK_TTL 內一律回 423（通行密鑰不受影響，
//       因為它不是被猜的那組憑證）。鎖定期間的每次嘗試仍計入失敗，所以攻擊者持續打
//       仍會走到第二層（IP 自動黑名單）。
// 解除：到期自動解除；或帳號持有人在 Discord 通知按「取消這次鎖定」
//       （`cancel_login_lock`，由 discord adapter 在驗證 Discord 綁定後呼叫）。
// 鍵：輸入的名稱 + 解析到的帳號 id / username（小寫），三者同時上鎖、同時解鎖，
//     不論攻擊者用 id 還是學號打，持有人取消時都能一次清掉。
struct LoginLock {
    until: Instant,
}

static LOGIN_LOCKS: once_cell::sync::Lazy<Mutex<HashMap<String, LoginLock>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

fn login_lock_key(value: &str) -> String {
    value.trim().to_lowercase()
}

/// 對一組鍵上鎖；回傳 `true` 表示這次「新建立」了鎖（之前沒有生效中的鎖）。
async fn lock_account_login(keys: &[&str]) -> bool {
    // LOGIN_THROTTLE_SQLITE_V1：鎖定先寫 SQLite（重啟後仍鎖著）；失敗退回記憶體。
    if let Some(db) = throttle_db() {
        let now = unix_now_secs();
        let owned: Vec<String> = keys.iter().map(|k| login_lock_key(k)).collect();
        match db
            .login_lock_insert(&owned, now + ACCOUNT_LOGIN_LOCK_TTL.as_secs() as i64, now)
            .await
        {
            Ok(created) => return created,
            Err(error) => warn!(error = %error, "登入鎖定寫入 SQLite 失敗，退回記憶體鎖定"),
        }
    }
    let mut locks = LOGIN_LOCKS.lock().await;
    let now = Instant::now();
    locks.retain(|_, lock| lock.until > now);
    if locks.len() >= MAX_SECURITY_WATCH_KEYS {
        // 極端情況（大量帳號同時被打）：整表清空重來，只是少鎖幾個帳號，不會 panic 或無限成長。
        locks.clear();
    }
    let mut created = false;
    for key in keys {
        let key = login_lock_key(key);
        if key.is_empty() {
            continue;
        }
        // 鎖定是「從第一次觸發起固定 10 分鐘」：已在鎖定中的鍵不延長，
        // 否則攻擊者只要持續打就能無限期把持有人擋在密碼登入之外。
        if locks.contains_key(&key) {
            continue;
        }
        created = true;
        locks.insert(
            key,
            LoginLock {
                until: now + ACCOUNT_LOGIN_LOCK_TTL,
            },
        );
    }
    created
}

/// 任一鍵仍在鎖定中 → 回傳剩餘秒數（取最大值）。
async fn login_lock_remaining(keys: &[String]) -> Option<u64> {
    if let Some(db) = throttle_db() {
        match db.login_lock_remaining(keys, unix_now_secs()).await {
            Ok(remaining) => {
                // DB 是主要來源；記憶體只在 DB 失敗時才有東西，但仍看一下以免退路期間的鎖被略過。
                let memory = login_lock_remaining_memory(keys).await;
                return match (remaining, memory) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            }
            Err(error) => warn!(error = %error, "讀取 SQLite 登入鎖定失敗，退回記憶體鎖定"),
        }
    }
    login_lock_remaining_memory(keys).await
}

async fn login_lock_remaining_memory(keys: &[String]) -> Option<u64> {
    let mut locks = LOGIN_LOCKS.lock().await;
    let now = Instant::now();
    locks.retain(|_, lock| lock.until > now);
    keys.iter()
        .filter_map(|key| locks.get(&login_lock_key(key)))
        .map(|lock| lock.until.saturating_duration_since(now).as_secs().max(1))
        .max()
}

/// 帳號持有人（或管理員）取消第一層鎖定：移除鎖並清掉該帳號的失敗計數，
/// 讓持有人下一次輸入正確密碼就能進來，不會因為殘留計數又立刻被鎖。
/// 回傳 `true` 表示真的有鎖被解除。
pub async fn cancel_login_lock(keys: &[&str]) -> bool {
    let mut removed = false;
    {
        let mut locks = LOGIN_LOCKS.lock().await;
        for key in keys {
            if locks.remove(&login_lock_key(key)).is_some() {
                removed = true;
            }
        }
    }
    // LOGIN_THROTTLE_SQLITE_V1：同時解除 SQLite 中的鎖與該帳號的失敗計數。
    if let Some(db) = throttle_db() {
        let owned: Vec<String> = keys.iter().map(|k| login_lock_key(k)).collect();
        match db.login_lock_remove(&owned).await {
            Ok(db_removed) => removed = removed || db_removed,
            Err(error) => warn!(error = %error, "移除 SQLite 登入鎖定失敗"),
        }
        for key in &owned {
            if let Err(error) = db.throttle_clear("fail", &format!("user:{key}")).await {
                warn!(error = %error, "清除 SQLite 失敗計數失敗");
            }
        }
    }
    let mut watch = SECURITY_WATCH.lock().await;
    for key in keys {
        watch.fails.remove(&format!("user:{}", login_lock_key(key)));
        watch
            .last_alert
            .remove(&format!("fail-user:{}", login_lock_key(key)));
    }
    removed
}

async fn admin_discord_recipients(state: &AdminWebState) -> Vec<(String, Option<String>)> {
    // (Discord user id, 對應帳號 id)：DC_CHANNEL_PREFS_V1 逐位管理員檢查通知偏好；
    // messenger admin 無對應帳號時為 None（不檢查、維持送達）。
    let mut ids: Vec<(String, Option<String>)> = Vec::new();
    if let Some(messenger) = &state.messenger {
        let admin = messenger.admin_user_id().trim();
        if !admin.is_empty() {
            ids.push((admin.to_string(), None));
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
            if !ids.iter().any(|(existing, _)| existing == id) {
                ids.push((id.to_string(), Some(account.id.clone())));
            }
        }
    }
    ids
}

async fn notify_admins_discord(state: &AdminWebState, text: String, pref_key: &str) {
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
    for (to, pref_account) in recipients {
        // DC_CHANNEL_PREFS_V1：管理員可在通知設定頁關閉對應類型的 Discord 通知。
        if let Some(account_id) = &pref_account {
            match state
                .accounts_db
                .notification_preference_enabled(account_id, "discord", pref_key)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(
                        account = %account_id,
                        key = pref_key,
                        "安全類 Discord 通知已由使用者關閉"
                    );
                    continue;
                }
                Err(error) => {
                    warn!(
                        account = %account_id,
                        error = %error,
                        "讀取安全類 Discord 通知設定失敗，仍發送"
                    );
                }
            }
        }
        if let Err(e) = messenger.push(&to, &message).await {
            warn!(to = %to, error = %e, "security alert Discord 推送失敗");
        }
    }
}

pub(super) async fn notify_admins_security_alert(state: &AdminWebState, event: SecurityAlertEvent) {
    // SECURITY_ALERT_PUSH_V1：先取出推播需要的欄位（event 稍後會 move 進 Discord 訊息）。
    let push_username = event.username.clone();
    let push_ip = event.ip.clone();
    let push_user_failures = event.user_failures;
    let push_ip_failures = event.ip_failures;
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
    for (to, pref_account) in recipients {
        // DC_CHANNEL_PREFS_V1：管理員可在通知設定頁關閉安全警示的 Discord 通知。
        if let Some(account_id) = &pref_account {
            match state
                .accounts_db
                .notification_preference_enabled(account_id, "discord", "security_alert")
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(
                        account = %account_id,
                        "安全警示 Discord 通知已由使用者關閉"
                    );
                    continue;
                }
                Err(error) => {
                    warn!(
                        account = %account_id,
                        error = %error,
                        "讀取安全警示 Discord 通知設定失敗，仍發送"
                    );
                }
            }
        }
        if let Err(error) = messenger.push(&to, &message).await {
            warn!(to = %to, error = %error, "security alert notification 推送失敗");
        }
    }
    // SECURITY_ALERT_PUSH_V1：安全警示改雙渠道——管理員的手機 Web Push 同步發送
    //（notify_security_alert 內會依偏好鍵 security_alert 逐帳號檢查）。
    let mut admin_account_ids = Vec::new();
    if let Ok(accounts) = state.accounts_db.list().await {
        for account in accounts {
            if account.is_admin {
                admin_account_ids.push(account.id);
            }
        }
    }
    if !admin_account_ids.is_empty() {
        let title = "[安全] Dashboard 安全警示：連續密碼錯誤".to_string();
        let body = format!(
            "帳號：{push_username}\n來源 IP：{push_ip}\n帳號失敗次數：{push_user_failures}\nIP 失敗次數：{push_ip_failures}"
        );
        crate::web_push_notifications::notify_security_alert(
            admin_account_ids,
            title,
            body,
            format!("tronclass-security-alert-{push_ip}"),
        );
    }
}

/// ADMIN_GEO_RESTRICT_V1：通知管理員「有國外 IP 申請暫時放行」——與
/// `notify_admins_security_alert` 同一種雙渠道寫法（Discord 按鈕＋Web Push），
/// 只有送達對象、文案、偏好鍵（`geo_access_request`）、落地頁不同。只在
/// `create_or_reuse_geo_access_request` 回報「這是新申請」時呼叫一次，同一個
/// IP 在時間窗內重複按「聯絡管理員」不會重複通知。
pub(super) async fn notify_admins_geo_access_request(
    state: &AdminWebState,
    event: crate::adapters::events::GeoAccessRequestEvent,
) {
    let push_ip = event.ip.clone();
    let push_location = event.location.clone();
    let push_path = event.path.clone();
    let Some(messenger) = state.messenger.clone() else {
        warn!("geo access request skipped: no Discord/LINE messenger");
        return;
    };
    let recipients = admin_discord_recipients(state).await;
    if recipients.is_empty() {
        warn!("geo access request skipped: no admin Discord user id");
        return;
    }
    let message = OutboundMessage::GeoAccessRequest(event);
    for (to, pref_account) in recipients {
        if let Some(account_id) = &pref_account {
            match state
                .accounts_db
                .notification_preference_enabled(account_id, "discord", "geo_access_request")
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(
                        account = %account_id,
                        "國外存取申請 Discord 通知已由使用者關閉"
                    );
                    continue;
                }
                Err(error) => {
                    warn!(
                        account = %account_id,
                        error = %error,
                        "讀取國外存取申請 Discord 通知設定失敗，仍發送"
                    );
                }
            }
        }
        if let Err(error) = messenger.push(&to, &message).await {
            warn!(to = %to, error = %error, "geo access request notification 推送失敗");
        }
    }
    let mut admin_account_ids = Vec::new();
    if let Ok(accounts) = state.accounts_db.list().await {
        for account in accounts {
            if account.is_admin {
                admin_account_ids.push(account.id);
            }
        }
    }
    if !admin_account_ids.is_empty() {
        let title = "[安全] 國外 IP 嘗試存取管理後台".to_string();
        let body = format!(
            "來源 IP：{push_ip}\n位置：{push_location}\n想前往：{}\n\n請到「IP 黑名單」頁查看並決定是否暫時放行。",
            if push_path.is_empty() { "/admin" } else { &push_path }
        );
        crate::web_push_notifications::notify_geo_access_request(
            admin_account_ids,
            title,
            body,
            format!("tronclass-geo-access-request-{push_ip}"),
        );
    }
}

/// GEO_ACCESS_BURST_MUTE_V1：短時間內「不同 IP」的國外存取申請暴增時，改發
/// 這一則一次性的彙總警示，取代原本逐筆的 [`notify_admins_geo_access_request`]
/// （呼叫端在觸發靜音旗標的那一刻呼叫一次，之後新申請一律靜音到管理員手動
/// 在 `/admin/blacklist` 按「清空並恢復通知」為止）。純文字訊息、沒有按鈕
/// ——引導管理員去網頁處理，不在 Discord 上對大量申請逐一核准／忽略。
pub(super) async fn notify_admins_geo_access_burst(state: &AdminWebState, distinct_ip_count: i64) {
    let text = format!(
        "[安全] 國外存取申請短時間內大量出現（過去 10 分鐘內已有 {distinct_ip_count} 個不同來源 IP 申請），為避免洗版已暫停逐筆通知。請到「IP 黑名單」頁查看，按「清空並恢復通知」後才會再逐筆通知。"
    );
    if let Some(messenger) = state.messenger.clone() {
        let recipients = admin_discord_recipients(state).await;
        for (to, pref_account) in recipients {
            if let Some(account_id) = &pref_account {
                match state
                    .accounts_db
                    .notification_preference_enabled(account_id, "discord", "geo_access_request")
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        warn!(
                            account = %account_id,
                            error = %error,
                            "讀取國外存取申請 Discord 通知設定失敗，仍發送"
                        );
                    }
                }
            }
            if let Err(error) = messenger
                .push(&to, &OutboundMessage::Text(text.clone()))
                .await
            {
                warn!(to = %to, error = %error, "geo access burst notification 推送失敗");
            }
        }
    }
    let mut admin_account_ids = Vec::new();
    if let Ok(accounts) = state.accounts_db.list().await {
        for account in accounts {
            if account.is_admin {
                admin_account_ids.push(account.id);
            }
        }
    }
    if !admin_account_ids.is_empty() {
        crate::web_push_notifications::notify_geo_access_request(
            admin_account_ids,
            "[安全] 國外存取申請短時間暴增".to_string(),
            text,
            "tronclass-geo-access-burst".to_string(),
        );
    }
}

async fn alert_password_failures(state: &AdminWebState, username: &str, ip: &str) {
    let (user_fails, ip_fails) = record_auth_failure(username, ip).await;
    // 解析被嘗試登入的帳號（可能為 None：隨機 username）；持有人綁定由此取得。
    let target_account = resolve_login_target_account(state, username).await;
    let (target_discord, target_line) = contacts_of(target_account.as_ref());
    let owner_account_id = target_account
        .as_ref()
        .map(|account| account.id.clone())
        .unwrap_or_else(|| username.trim().to_string());

    // LOGIN_TIERED_LOCKOUT_V1 第二層：來源 IP 在觀察窗口內失敗超過十次 → 自動黑名單。
    // 黑名單是以 IP 為單位，所以只有來源 IP 計數能觸發；record_auth_failure 已把計數
    // 限制在 SECURITY_FAIL_WINDOW 內。
    if ip_fails >= SECURITY_AUTO_BLOCK_THRESHOLD {
        let normalized_ip = normalize_admin_ip(ip);
        if let Some(ref blocked_ip) = normalized_ip {
            match state
                .accounts_db
                .block_ip_with_failure_count(
                    blocked_ip,
                    "system:auto-login-failure",
                    "Automatic block: more than ten failed Dashboard logins within 15 minutes",
                    ip_fails,
                )
                .await
            {
                Ok(true) => {
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
                        "Automatic block after more than ten failed Dashboard logins within 15 minutes",
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
                            details: format!("Auto-blocked after {ip_fails} failed Dashboard logins within 15 minutes"),
                            ip: blocked_ip.clone(),
                            user_agent: String::new(),
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
                        account_id: owner_account_id.clone(),
                        ip_is_blacklisted: true,
                        allow_owner_unblock: true,
                        login_locked: false,
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
        // LOGIN_TIERED_LOCKOUT_V1 第一層：鎖定該帳號的密碼登入，先通知持有人。
        // 鍵同時涵蓋「輸入的名稱」與「解析到的帳號 id / 學號」，持有人取消時一次清掉。
        let mut keys: Vec<String> = vec![username.trim().to_string()];
        if let Some(account) = target_account.as_ref() {
            keys.push(account.id.clone());
            keys.push(account.username.clone());
        }
        let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let created = lock_account_login(&key_refs).await;
        if created {
            warn!(username = %username, ip = %ip, user_fails, "Dashboard 帳號密碼登入已暫時鎖定（第一層）");
            crate::logs::record_audit(
                "web",
                "system:login-lock",
                "login_lock",
                Some(&owner_account_id),
                Some(ip),
                format!("帳號 {} 連續密碼錯誤 {} 次，暫時鎖定密碼登入 {} 分鐘", username.trim(), user_fails, ACCOUNT_LOGIN_LOCK_TTL.as_secs() / 60),
            );
            let _ = state
                .accounts_db
                .record_activity_event(crate::db::ActivityEventInput {
                    kind: "security".to_string(),
                    actor: "system:login-lock".to_string(),
                    action: "login_lock".to_string(),
                    account_id: owner_account_id.clone(),
                    path: String::new(),
                    result: "locked".to_string(),
                    details: format!("{} consecutive failed password logins; password login locked for {} minutes", user_fails, ACCOUNT_LOGIN_LOCK_TTL.as_secs() / 60),
                    ip: ip.trim().to_string(),
                    user_agent: String::new(),
                })
                .await;
            let event = SecurityAlertEvent {
                username: username.trim().to_string(),
                ip: normalize_admin_ip(ip).unwrap_or_else(|| ip.trim().to_string()),
                user_failures: user_fails,
                ip_failures: ip_fails,
                timestamp: security_now_label(),
                ip_is_blacklisted: false,
                account_id: owner_account_id.clone(),
                allow_owner_unblock: false,
                login_locked: true,
            };
            // 持有人先收到（Discord 有「取消這次鎖定」／「封鎖此 IP」按鈕；LINE 收純文字）。
            if let Some(messenger) = state.messenger.as_ref() {
                if let Some(discord_id) = target_discord.as_deref() {
                    if let Err(e) = messenger
                        .push(discord_id, &OutboundMessage::SecurityAlert(event.clone()))
                        .await
                    {
                        warn!(to = %discord_id, error = %e, "login lock 使用者 Discord 推送失敗");
                    }
                }
                if let Some(line_id) = target_line.as_deref() {
                    let text = format!(
                        "安全提醒\n有人連續嘗試登入你的 Dashboard 帳號（帳號：{}），已失敗 {} 次。\n系統已暫時鎖定此帳號的密碼登入 {} 分鐘（通行密鑰不受影響）。\n若是你本人操作，請稍候再試或改用通行密鑰；若不是你，請立即變更 Dashboard 密碼。",
                        username.trim(),
                        user_fails,
                        ACCOUNT_LOGIN_LOCK_TTL.as_secs() / 60
                    );
                    if let Err(e) = messenger.push(line_id, &OutboundMessage::Text(text)).await {
                        warn!(to = %line_id, error = %e, "login lock 使用者 LINE 推送失敗");
                    }
                }
            }
            notify_admins_security_alert(state, event).await;
        } else {
            // 鎖定期間仍持續失敗：對管理員維持既有的冷卻告警，不再重複打擾持有人。
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
                        account_id: owner_account_id.clone(),
                        allow_owner_unblock: false,
                        login_locked: true,
                    },
                )
                .await;
            }
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
                    account_id: owner_account_id.clone(),
                    allow_owner_unblock: false,
                    login_locked: false,
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

/// LOGIN_TIERED_LOCKOUT_V1：解析 `username` 對應的帳號（先以帳號 id 查詢，再以 Tronclass username 查詢）。
async fn resolve_login_target_account(
    state: &AdminWebState,
    username: &str,
) -> Option<RawAccountConfig> {
    let username = username.trim();
    if username.is_empty() {
        return None;
    }
    if let Ok(Some(acct)) = state.accounts_db.get(username).await {
        return Some(acct);
    }
    if let Ok(accounts) = state.accounts_db.find_by_username(username, None).await {
        return accounts.into_iter().next();
    }
    None
}

fn contacts_of(account: Option<&RawAccountConfig>) -> (Option<String>, Option<String>) {
    match account {
        Some(acct) => {
            let discord = acct.discord_user_id.trim().to_string();
            let line = acct.line_user_id.trim().to_string();
            (
                if discord.is_empty() { None } else { Some(discord) },
                if line.is_empty() { None } else { Some(line) },
            )
        }
        None => (None, None),
    }
}

/// 解析 `username` 對應帳號的綁定聯絡方式（先以帳號 id 查詢，再以 Tronclass username 查詢）。
/// 回傳 `(discord_user_id, line_user_id)`，未綁定時為 `None`。
async fn resolve_login_target_contacts(
    state: &AdminWebState,
    username: &str,
) -> (Option<String>, Option<String>) {
    contacts_of(resolve_login_target_account(state, username).await.as_ref())
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
    let body = format!(
        "來源：{source}\n帳號：{who}\nIP：{}\n時間：{}\n\n60 秒內嘗試次數超過上限，已暫時封鎖。請到 /admin/activity 查看登入審計。",
        ip.trim(),
        security_now_label()
    );
    let text = format!(
        // DISCORD_NOTIFY_UNIFY_V1：統一的「[安全] 標題」格式。
        "[安全] Dashboard 安全警示：異常流量\n\n{body}"
    );
    // TRAFFIC_ALERT_PUSH_V1：異常流量改雙渠道（Discord＋Web Push），偏好鍵 traffic_alert。
    notify_admins_discord(state, text, "traffic_alert").await;
    let mut admin_account_ids = Vec::new();
    if let Ok(accounts) = state.accounts_db.list().await {
        for account in accounts {
            if account.is_admin {
                admin_account_ids.push(account.id);
            }
        }
    }
    if !admin_account_ids.is_empty() {
        crate::web_push_notifications::notify_traffic_alert(
            admin_account_ids,
            "[安全] Dashboard 安全警示：異常流量".to_string(),
            body,
        );
    }
}

/// LOGIN_GEO_ALERT_V1：純判斷——新 IP 是否需要告警。回 `Some(原因)` 表示要告警。
/// 順序：Tor → 登入前多次密碼失敗 → 查不到國家 → 不在本國；都不成立（本國且乾淨）回 None。
/// `home_countries` 為空 = 沿用舊行為（任何新 IP 都告警）。
fn login_alert_reason(
    geo: &crate::geoip::GeoInfo,
    home_countries: &[String],
    recent_failures: u32,
) -> Option<String> {
    if geo.is_tor {
        return Some("來源為 Tor 出口節點".to_string());
    }
    if recent_failures >= LOGIN_GEO_SUSPICIOUS_FAILURES {
        return Some(format!(
            "登入前 15 分鐘內有 {recent_failures} 次密碼錯誤，之後才成功"
        ));
    }
    if home_countries.is_empty() {
        return Some("此帳號過去從未從這個 IP 登入".to_string());
    }
    if geo.country_code.is_empty() {
        return Some("無法判定來源國家".to_string());
    }
    if home_countries.iter().any(|c| c == &geo.country_code) {
        return None;
    }
    Some(format!(
        "來源不在本國（{}）：{}",
        home_countries.join("/"),
        geo.country_code
    ))
}

/// LOGIN_LOCATION_ALERT_V1 + LOGIN_GEO_ALERT_V1：登入成功後判斷是否需要「新位置登入」告警。
///
/// 使用者要求：不要「只要有人登入就通知」，只有 IP 不在台灣或其他真正異常才通知。
///
/// 規則（依序）：
/// * 該帳號從未有任何已知 IP（第一次登入）→ 只記錄，不告警。
/// * IP 為空／loopback／私有網段 → 只記錄。
/// * 已知 IP（這個帳號之前成功登入過）→ 更新 last_seen／次數，不告警。
/// * 新 IP → 查國家（Cloudflare `CF-IPCountry` 優先，否則 ipwho.is）：
///   - 屬於 `adapters.admin_web.login_home_countries`（預設 TW）且不是 Tor → **只記錄，不告警**。
///   - 境外、Tor 出口節點、查不到國家（保守視為異常）→ 告警。
/// * 另一種「真正異常」：這次成功登入前 15 分鐘內，同一帳號或同一 IP 累積 ≥3 次密碼失敗
///   （撞對密碼的跡象）→ 即使是本國 IP 也告警（`SECURITY_WATCH` 的失敗計數在
///   `clear_auth_failures` 之前先讀出來，由呼叫端傳入）。
/// * 同一帳號＋同一新 IP 在 SECURITY_ALERT_COOLDOWN（10 分鐘）內只告警一次。
///
/// 告警對象（帳號持有人＋管理員）：
/// * 持有人：Discord／LINE 文字訊息 + Web Push（點開直達 /admin/devices）。
/// * 管理員：Discord 文字（沿用 `notify_admins_discord`）+ Web Push。
/// 通知內容無 emoji。
async fn check_unusual_login_location(
    state: &AdminWebState,
    account_id: &str,
    ip: &str,
    user_agent: &str,
    method_label: &str,
    cf_ipcountry: Option<&str>,
    recent_failures: u32,
) {
    let account_id = account_id.trim();
    let ip = ip.trim();
    if account_id.is_empty() || account_id == "<token-admin>" {
        return;
    }
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let ip_is_private = ip.is_empty()
        || ip
            .parse::<std::net::IpAddr>()
            .map(|addr| match addr {
                std::net::IpAddr::V4(v4) => {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
                }
                std::net::IpAddr::V6(v6) => {
                    v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00
                }
            })
            .unwrap_or(true);

    let known_count = state
        .accounts_db
        .count_known_login_ips(account_id)
        .await
        .unwrap_or(0);
    let is_known = if known_count == 0 {
        false
    } else {
        state
            .accounts_db
            .is_known_login_ip(account_id, ip)
            .await
            .unwrap_or(true)
    };

    // 先記憶，再決定是否告警（不論是否告警都要記住這個 IP）。
    if let Err(error) = state.accounts_db.remember_login_ip(account_id, ip, now_unix).await {
        warn!(account = %account_id, error = %error, "寫入登入 IP 記憶失敗");
    }

    if known_count == 0 || is_known || ip_is_private {
        return;
    }

    // LOGIN_GEO_ALERT_V1：新 IP → 查國家，本國且無其他異常就安靜記錄。
    let geo = crate::geoip::resolve(ip, cf_ipcountry).await;
    let home_countries: Vec<String> = crate::config::try_get_config()
        .map(|cfg| {
            cfg.adapters
                .admin_web
                .login_home_countries
                .iter()
                .map(|c| c.trim().to_ascii_uppercase())
                .filter(|c| !c.is_empty())
                .collect()
        })
        .unwrap_or_else(|| vec!["TW".to_string()]);
    let location = geo.label();
    let alert_reason = login_alert_reason(&geo, &home_countries, recent_failures);

    // 編譯修正：`ua` 原本宣告在「確定要告警」之後，但下面兩個分支（本國不通知的 info、
    // 告警的 alert）都要寫進事件，因此把正規化（空＝"-"、截 120 字元）提到分支之前共用。
    let ua = if user_agent.trim().is_empty() {
        "-".to_string()
    } else {
        crate::text_utils::truncate_chars(user_agent.trim(), 120).to_string()
    };

    // 不告警的情況也留一筆 activity（result=info），事後在活動頁仍看得到新 IP 從哪裡來。
    let Some(reason) = alert_reason else {
        info!(account = %account_id, ip = %ip, location = %location, "新 IP 登入（本國，不通知）");
        let _ = state
            .accounts_db
            .record_activity_event(crate::db::ActivityEventInput {
                kind: "security".to_string(),
                actor: "system:login-location".to_string(),
                action: "login_new_ip_home".to_string(),
                account_id: account_id.to_string(),
                path: String::new(),
                result: "info".to_string(),
                details: format!("新 IP 登入（{method_label}）；位置：{location}；本國，未通知；UA: {ua}"),
                ip: ip.to_string(),
                user_agent: ua.to_string(),
            })
            .await;
        return;
    };

    let key = format!("login-location:{account_id}:{ip}");
    if !security_should_alert(&key).await {
        return;
    }

    info!(account = %account_id, ip = %ip, location = %location, %reason, "偵測到異常位置登入 Dashboard");
    let when = security_now_label();
    crate::logs::record_audit(
        "web",
        "system:login-location",
        "login_new_location",
        Some(account_id),
        Some(ip),
        "Dashboard 帳號從異常位置登入成功",
    );
    let _ = state
        .accounts_db
        .record_activity_event(crate::db::ActivityEventInput {
            kind: "security".to_string(),
            actor: "system:login-location".to_string(),
            action: "login_new_location".to_string(),
            account_id: account_id.to_string(),
            path: String::new(),
            result: "alert".to_string(),
            details: format!("異常位置登入（{method_label}）；位置：{location}；原因：{reason}；UA: {ua}"),
            ip: ip.to_string(),
            user_agent: ua.to_string(),
        })
        .await;

    // 帳號持有人
    let (owner_discord, owner_line) = resolve_login_target_contacts(state, account_id).await;
    let owner_text = format!(
        "安全提醒：你的 Dashboard 帳號（{account_id}）剛剛從一個異常位置登入。\n登入方式：{method_label}\n位置：{location}\nIP：{ip}\n原因：{reason}\n裝置：{ua}\n時間：{when}\n若不是你本人，請立即到「設定 > 登入裝置管理」登出其他裝置，並變更 Dashboard 密碼。"
    );
    notify_account_holder(
        state,
        owner_discord.as_deref(),
        owner_line.as_deref(),
        &owner_text,
    )
    .await;
    crate::web_push_notifications::notify_security_alert(
        vec![account_id.to_string()],
        "Dashboard 異常位置登入".to_string(),
        format!("帳號 {account_id} 從 {location}（{ip}）以{method_label}登入。若不是你，請立即登出其他裝置。"),
        format!("tronclass-login-location-{account_id}"),
    );

    // 管理員（Discord 文字 + Web Push）
    let admin_text = format!(
        "Dashboard 安全警示\n類型：異常位置登入\n帳號：{account_id}\n登入方式：{method_label}\n位置：{location}\nIP：{ip}\n原因：{reason}\n裝置：{ua}\n時間：{when}\n此帳號過去從未從這個 IP 成功登入。若可疑，可到 /admin/activity 查看登入審計或到黑名單頁封鎖此 IP。"
    );
    notify_admins_discord(state, admin_text, "security_alert").await;
    let mut admin_account_ids = Vec::new();
    if let Ok(accounts) = state.accounts_db.list().await {
        for account in accounts {
            if account.is_admin && account.id != account_id {
                admin_account_ids.push(account.id);
            }
        }
    }
    if !admin_account_ids.is_empty() {
        crate::web_push_notifications::notify_security_alert(
            admin_account_ids,
            "Dashboard 異常位置登入".to_string(),
            format!("帳號 {account_id} 從 {location}（{ip}）以{method_label}登入；{reason}。"),
            format!("tronclass-login-location-admin-{account_id}"),
        );
    }
}

#[derive(Clone)]
pub struct AdminWebState {
    // ACCOUNT_HOT_ADD_V1：改用可執行期追加的共享登錄簿（與 LINE／Discord 同一份）。
    accounts: AccountRegistry,
    // ACCOUNT_HOT_ADD_V1：新增帳號後直接熱啟動監控循環；None 代表本次啟動沒有
    // 監控 runtime（例如測試），此時退回「請重啟服務」的舊行為。
    monitor_spawner: Option<crate::monitor::MonitorSpawner>,
    accounts_db: AccountsDb,
    scanner: Option<Arc<QrScannerRegistry>>,
    messenger: Option<Arc<dyn AdapterMessenger>>,
    sessions: Arc<RwLock<HashMap<String, AdminSession>>>,
    /// TC_NAV_TOKEN_V1（fix34）：/admin/tc-go 簽發的一次性／短效導航 token。
    /// 背景：實測（2026-09-14 log）使用者的瀏覽器對 /tc/ 頂層導航不帶任何
    /// cookie（has_access_cookie=false has_refresh_cookie=false），但同源
    /// XHR 正常——/tc/ 導航改用 URL token 鑑權，不再依賴 cookie。
    tc_nav_tokens: Arc<std::sync::Mutex<HashMap<String, TcNavTokenEntry>>>,
    /// REFRESH_ROTATE_GRACE_V1（fix38）：舊 refresh → 該次輪換發出的新 token。
    /// 讓並行／重播的 refresh 拿回同一組結果，而不是被判失敗。
    refresh_grace: Arc<std::sync::Mutex<HashMap<String, GraceEntry>>>,
    /// PROXY_CSRF_BRIDGE_V1（fix40）：account_id → 最近一次從 iClass 收到的
    /// `csrftoken`。上游 cookie 不再落地，CSRF 需求靠這裡＋注入頁面的 script。
    proxied_csrf: Arc<std::sync::Mutex<HashMap<String, ProxiedCsrfEntry>>>,
    rate_limiter: RateLimiter,
    default_poll_interval_secs: u64,
}

/// TC_NAV_TOKEN_V1：token 內容。single_use=true 給轉址 URL；false 給
/// tc_proxy_ticket cookie（短效子資源票券）。兩者用途不同，見 `url_grant`。
struct TcNavTokenEntry {
    account_id: String,
    is_admin: bool,
    expires_at: Instant,
    single_use: bool,
    /// TC_REFERER_GRANT_V1（fix35）：URL 消費後轉為「頁面授權」——同一頁面的
    /// 子資源／XHR／站內導航以 Referer（=頁面完整 URL，含 tc_token）鑑權；
    /// 效期滑動。
    referer_grant: bool,
    /// TC_URL_GRANT_REPLAY_V1（fix36）：此條目可否「以 URL query 身分重複使用」。
    /// 由 `mint_tc_nav_token` 依 single_use 決定：轉址 URL 用的 token 可以
    ///（F5／上一頁／站內導航都靠它），cookie 票券的不行（避免票券被塞進網址）。
    url_grant: bool,
}

/// REFRESH_ROTATE_GRACE_V1（fix38）：輪換寬限視窗內保留的輪換結果。
struct GraceEntry {
    access: String,
    refresh: String,
    expires_at: Instant,
    /// TKU_SHORT_SESSION_V1：延續原本那次核發時算好的 cookie Max-Age（秒），
    /// 讓寬限視窗內的重播回應也帶正確的壽命（而不是重播時又套用預設值）。
    access_max_age: u64,
    refresh_max_age: u64,
}

/// PROXY_CSRF_BRIDGE_V1（fix40）：iClass 的 `csrftoken`（Django CSRF cookie）。
///
/// 代理路徑**不把上游 cookie 種到面板網域**（fix22 以來的硬規則，fix38 破壞它
/// 造成「代理頁轉圈＋面板被登出」）；但 iClass 的 SPA 需要 CSRF token 才能 POST。
/// 折衷：值只留在這裡（伺服器記憶體），由代理注入的 script 交給頁面——
/// cookie 與標頭都用同一個值，與瀏覽器 cookie jar 完全隔離。
struct ProxiedCsrfEntry {
    token: String,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
struct AdminSession {
    account_id: String,
    is_admin: bool,
    /// VIEWER_ROLE_V1：檢視者（唯讀）。看得到管理員視角的所有帳號狀態（敏感欄位
    /// 遮罩），但所有 POST /admin/api/* 寫入一律 403（`require_viewer_readonly_middleware`）。
    is_viewer: bool,
    /// A one-time first-login session authenticated with the upstream
    /// Tronclass password. It may only change the Dashboard password or log
    /// out; no account data or control API is exposed before that change.
    must_change_password: bool,
    expires_at: Instant,
}

/// LOGIN_GEO_ALERT_V1：讀取 Cloudflare `CF-IPCountry`。`sanitize_forwarded_headers_middleware`
/// 已把非可信代理送來的這個標頭移除，所以這裡拿到的值只可能來自我們自己的代理。
fn cf_ipcountry_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("cf-ipcountry")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// LOGIN_DEVICES_V1：登入時的來源資訊，寫進 dashboard_sessions 供「登入裝置管理」頁。
#[derive(Debug, Clone, Default)]
struct SessionOrigin {
    ip: String,
    user_agent: String,
    /// 第一次登入的 Unix 秒；0 = 現在。refresh 輪換時沿用原值，裝置頁的
    /// 「登入時間」才不會每 12 小時被重設一次。
    first_login_at: i64,
}

impl SessionOrigin {
    fn from_request(headers: &HeaderMap, ip: &str) -> Self {
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(|v| crate::text_utils::truncate_chars(v, 256).to_string())
            .unwrap_or_default();
        // `ClientIp` 在缺少 ConnectInfo 時會給 "unknown"；那不是位址，不要寫進 DB
        // （寫了會佔住 ip 欄位，之後就無法回填）。
        let ip = ip.trim();
        let ip = if ip.is_empty() || ip.eq_ignore_ascii_case("unknown") {
            String::new()
        } else {
            ip.to_string()
        };
        Self {
            ip,
            user_agent,
            first_login_at: 0,
        }
    }

    /// 以 `self` 為主，欄位為空時用 `fallback` 補上（refresh 輪換／回填用）。
    fn or_fill(mut self, fallback: &SessionOrigin) -> Self {
        if self.ip.is_empty() {
            self.ip = fallback.ip.clone();
        }
        if self.user_agent.is_empty() {
            self.user_agent = fallback.user_agent.clone();
        }
        self
    }
}

/// TKU_SHORT_SESSION_V1：判斷來源 IP 是否落在淡江大學網段（163.13.0.0/16，
/// 經 WHOIS 核實為該校配賦範圍）。空字串／無法解析一律回傳 false（fail-open：
/// 查不出來源 IP 時維持原本的 12 小時／7 天效期，不會誤傷其他使用者）。
fn is_tku_campus_ip(ip: &str) -> bool {
    let ip = ip.trim();
    if ip.is_empty() {
        return false;
    }
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            let o = v4.octets();
            o[0] == 163 && o[1] == 13
        }
        Ok(std::net::IpAddr::V6(v6)) => v6
            .to_ipv4_mapped()
            .map(|v4| {
                let o = v4.octets();
                o[0] == 163 && o[1] == 13
            })
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// VIEWER_ROLE_V1 / LOGIN_DEVICES_V1：`create_session` 的完整參數。
#[derive(Debug, Clone, Default)]
struct NewSession {
    account_id: String,
    is_admin: bool,
    is_viewer: bool,
    must_change_password: bool,
    origin: SessionOrigin,
}

/// A freshly issued token pair. The access token authorizes API calls
/// (short-lived); the refresh token rotates to mint new access tokens.
#[derive(Debug, Clone)]
struct SessionTokens {
    access: String,
    refresh: String,
    /// TKU_SHORT_SESSION_V1：這次核發的 cookie 該用多少 Max-Age（秒）——一般
    /// 情況下等於 `ACCESS_TOKEN_TTL`／`REFRESH_TOKEN_TTL`，但來源 IP
    /// 落在淡江大學網段時會是 `TKU_SHORT_SESSION_TTL`。由 `create_session_with`
    /// 算好後經由這兩個欄位交給 `append_session_cookie`／`append_refresh_cookie`，
    /// 這樣瀏覽器裡 cookie 本身宣告的壽命才會跟伺服器端真正認定的到期時間一致
    /// （不然伺服器端 1 小時後就會拒絕，但瀏覽器 DevTools 看到的 Max-Age 卻還是
    /// 舊的 12 小時／7 天，造成「怎麼看起來沒生效」的困惑）。
    access_max_age: u64,
    refresh_max_age: u64,
}

impl AdminWebState {
    pub fn new(
        legacy_access_token: impl Into<String>,
        accounts: impl Into<AccountRegistry>,
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
        // TC_NAV_TOKEN_V1（fix34）：一次性導航 token 儲存（並在每次簽發時順手清理過期項）。
        let tc_nav_tokens: Arc<std::sync::Mutex<HashMap<String, TcNavTokenEntry>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        // PERFORMANCE_ACCOUNT_LOOKUP_V1 / ACCOUNT_HOT_ADD_V1:
        // account_id -> index 查表現在由 AccountRegistry 內部維護。
        let accounts: AccountRegistry = accounts.into();

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
                    // REVOKE_ORDER_V1：撤銷是先刪 DB 再取鎖移除記憶體；這裡在鎖內確認列仍在，
                    // 否則「讀列表 → 撤銷 → 寫回記憶體」會復活已登出的 session。
                    let still_valid = accounts_db_clone
                        .dashboard_session_key_exists(&row.token)
                        .await
                        .unwrap_or(false);
                    if remaining_secs > 0 && still_valid {
                        guard.insert(
                            row.token,
                            AdminSession {
                                account_id: row.account_id,
                                is_admin: row.is_admin,
                                is_viewer: row.is_viewer,
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
                // LOGIN_THROTTLE_SQLITE_V1：清掉超過最長觀察窗（15 分鐘）的節流事件與過期鎖。
                let _ = accounts_db_clone
                    .throttle_prune(now_unix - SECURITY_FAIL_WINDOW.as_secs() as i64, now_unix)
                    .await;
            }
        });

        // LOGIN_THROTTLE_SQLITE_V1：登記給 record_auth_failure / lock_account_login 等自由函式用。
        // 測試會建多個 state（不同暫存 DB）；OnceCell 只收第一個，之後的 set 失敗無妨——
        // 節流函式退回記憶體路徑，測試語意不變。
        let _ = THROTTLE_DB.set(accounts_db.clone());

        let state = Self {
            accounts,
            monitor_spawner: None,
            rate_limiter: RateLimiter::with_db(accounts_db.clone()),
            accounts_db,
            scanner,
            messenger,
            sessions,
            tc_nav_tokens,
            refresh_grace: Arc::new(std::sync::Mutex::new(HashMap::new())),
            proxied_csrf: Arc::new(std::sync::Mutex::new(HashMap::new())),
            default_poll_interval_secs: default_poll_interval_secs.max(1),
        };
        load_runtime_theme();
        state
    }

    /// ACCOUNT_HOT_ADD_V1：掛上監控 spawner，讓 Web 新增帳號後不必重啟即可開始監控。
    pub fn with_monitor_spawner(mut self, spawner: crate::monitor::MonitorSpawner) -> Self {
        self.monitor_spawner = Some(spawner);
        self
    }

    fn account_by_id(&self, account_id: &str) -> Option<RequestAccountState> {
        self.accounts.find(account_id)
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

    /// TC_NAV_TOKEN_V1（fix34）：簽發 /tc/ 導航 token。single_use=true 供轉址
    /// URL；false 供 tc_proxy_ticket cookie（短效可重複）。每次簽發順手清理
    /// 過期條目，map 體積有界。
    ///
    /// TC_URL_GRANT_REPLAY_V1（fix36）：`url_grant` 直接沿用 single_use——
    /// 「放進轉址 URL 的 token」就是允許以 URL 身分重複使用的那一顆。
    fn mint_tc_nav_token(&self, account_id: &str, is_admin: bool, single_use: bool) -> String {
        // SESSION_TOKEN_256_V1（fix128）：與 session 同級的 256-bit 亂數。
        let token = mint_secure_token();
        let mut guard = self
            .tc_nav_tokens
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard.retain(|_, entry| entry.expires_at > Instant::now());
        guard.insert(
            token.clone(),
            TcNavTokenEntry {
                account_id: account_id.to_string(),
                is_admin,
                expires_at: Instant::now()
                    + Duration::from_secs(if single_use {
                        TC_NAV_TOKEN_TTL_SECS
                    } else {
                        TC_NAV_TICKET_TTL_SECS
                    }),
                single_use,
                referer_grant: false,
                url_grant: single_use,
            },
        );
        token
    }

    /// TC_NAV_TOKEN_V1：以「cookie 票券」身分消費 token（tc_proxy_ticket）。
    /// single_use 條目即取即刪；ticket 條目在效期內可重複消費。
    /// TC_PAGE_GRANT_SLIDE_V1（fix36）：每次消費滑動延長效期——票券與 Referer
    /// 授權同一把祕密，既然 Referer 可滑動，票券也一致（閒置才過期）。
    fn consume_tc_nav_token(&self, token: &str) -> Option<(String, bool)> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        let mut guard = self
            .tc_nav_tokens
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let entry = guard.get_mut(token)?;
        if entry.expires_at <= Instant::now() {
            guard.remove(token);
            return None;
        }
        let result = Some((entry.account_id.clone(), entry.is_admin));
        if entry.single_use {
            guard.remove(token);
        } else {
            entry.expires_at = Instant::now() + Duration::from_secs(TC_PAGE_GRANT_TTL_SECS);
        }
        result
    }

    /// TC_REFERER_GRANT_V1（fix35）：以「URL query」身分消費 token（導航請求）。
    /// 首次消費把 single_use 條目轉成頁面授權（referer_grant=true）。
    ///
    /// TC_URL_GRANT_REPLAY_V1（fix36）：`url_grant` 條目在效期內**可以**重複
    /// 以 URL 消費（每次滑動延長）——這是 iClass 分頁 F5／上一頁／站內導航的
    /// 生路。舊版一律拒絕第二次，重載就掉到登入頁。cookie 票券（url_grant=
    /// false）仍然不得當 URL 用。
    fn consume_tc_nav_token_via_url(&self, token: &str) -> Option<(String, bool)> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        let mut guard = self
            .tc_nav_tokens
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let entry = guard.get_mut(token)?;
        if entry.expires_at <= Instant::now() {
            guard.remove(token);
            return None;
        }
        if !entry.url_grant {
            return None;
        }
        let result = (entry.account_id.clone(), entry.is_admin);
        if entry.single_use {
            entry.single_use = false;
            entry.referer_grant = true;
        }
        entry.expires_at = Instant::now() + Duration::from_secs(TC_PAGE_GRANT_TTL_SECS);
        Some(result)
    }

    /// TC_REFERER_GRANT_V1（fix35）：以「Referer」身分消費 token（頁面的子資源／
    /// XHR／站內導航；Referer=頁面完整 URL，query 內含 tc_token）。任何有效
    /// 條目皆可用；每次使用滑動延長效期（頁面持續活動就不過期）。
    fn consume_tc_nav_token_via_referer(&self, token: &str) -> Option<(String, bool)> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        let mut guard = self
            .tc_nav_tokens
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let entry = guard.get_mut(token)?;
        if entry.expires_at <= Instant::now() {
            guard.remove(token);
            return None;
        }
        entry.expires_at = Instant::now() + Duration::from_secs(TC_PAGE_GRANT_TTL_SECS);
        Some((entry.account_id.clone(), entry.is_admin))
    }

    async fn authorize(&self, token: &str) -> Option<AdminSession> {
        let session = self.authorize_any(token).await?;
        if session.must_change_password {
            return None;
        }
        Some(session)
    }

    async fn authorize_any(&self, token: &str) -> Option<AdminSession> {
        let session = self.authorize_any_unsuspended(token).await?;
        // ACCOUNT_SUSPEND_V1：停權中的帳號即使持有有效 token 也一律拒絕（查詢失敗視為停權）。
        if self.account_is_suspended(&session.account_id).await {
            return None;
        }
        Some(session)
    }

    /// ACCOUNT_SUSPEND_V1：帳號是否處於生效中的暫時停權。查詢失敗一律視為停權（fail closed）。
    pub(super) async fn account_is_suspended(&self, account_id: &str) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        match self.accounts_db.account_suspension_active(account_id, now).await {
            Ok(row) => row.is_some(),
            Err(error) => {
                warn!(account = %account_id, error = %error, "讀取帳號停權失敗，暫時拒絕該帳號");
                true
            }
        }
    }

    async fn authorize_any_unsuspended(&self, token: &str) -> Option<AdminSession> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        // SESSION_TOKEN_HASH_V1（fix125）：in-memory map 與 DB 一律以
        // SHA-256(token) 為鍵——browser cookie 裡的明文在伺服器端永不長留。
        let lookup_key = crate::db::session_lookup_key(token);

        // A legacy numeric access token is intentionally not accepted here.
        // Public deployments must authenticate as a real Dashboard account.

        // PERFORMANCE_SESSION_RWLOCK_V2:
        // Most Admin API requests only read session state. Use a read lock for
        // the common valid-session path and only upgrade to a write lock when the
        // current token is expired and needs removal.
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(&lookup_key).cloned()
        };

        if let Some(session) = session {
            if session.expires_at > Instant::now() {
                return Some(session);
            }

            let mut sessions = self.sessions.write().await;
            if sessions
                .get(&lookup_key)
                .map(|session| session.expires_at <= Instant::now())
                .unwrap_or(false)
            {
                sessions.remove(&lookup_key);
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
                    is_viewer: row.is_viewer,
                    must_change_password: row.must_change_password,
                    expires_at: Instant::now() + Duration::from_secs(remaining_secs),
                };
                let mut sessions = self.sessions.write().await;
                // REVOKE_ORDER_V1：鎖內再確認列仍在。若讀取與撤銷並行，這裡不可把已撤銷的 session 寫回記憶體。
                if !matches!(self.accounts_db.get_dashboard_session(token).await, Ok(Some(_))) {
                    return None;
                }
                sessions.insert(lookup_key.clone(), session.clone());
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

    /// TC_PROXY_NO_ROTATE_V1（fix36）：以請求的 cookie 解析面板 session，
    /// **只讀取、絕不輪換**。供 iClass 代理路徑使用。
    ///
    /// 代理路徑舊版呼叫 `resolve_page_session`（access 失效→refresh 輪換），
    /// 一旦輪換發生而該請求稍後走 early return（409 沒有 iClass session、
    /// 上游 502、被導向 iClass 登入頁…），新 cookie 就永遠不會回到瀏覽器，
    /// 但舊 refresh 已被單用消耗、舊 access 也已被撤銷 → 使用者下一步就被
    /// 登出。代理的鑑權改由 /admin/tc-go 簽發的 token 承擔，因此這裡不需要
    /// 動面板 token。
    ///
    /// SESSION_DUAL_CANDIDATE_V1：兩個名字的 access cookie 都試。
    async fn authorize_any_from_cookies(&self, headers: &HeaderMap) -> Option<AdminSession> {
        for candidate in session_cookie_candidates(headers) {
            if let Some(session) = self.authorize_any(&candidate).await {
                return Some(session);
            }
        }
        None
    }

    async fn revoke_sessions_for_account(&self, account_id: &str) {
        // REVOKE_ORDER_V1：先刪 DB、再移除記憶體（理由見 revoke_session）。
        if let Err(e) = self
            .accounts_db
            .delete_dashboard_sessions_by_account(account_id)
            .await
        {
            warn!(error = %e, "從資料庫刪除帳號 Dashboard sessions 失敗");
        }
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, session| session.account_id != account_id);
    }

    /// 相容包裝：不帶來源資訊、非 viewer（測試與 refresh 輪換以外的舊呼叫點）。
    async fn create_session(
        &self,
        account_id: String,
        is_admin: bool,
        must_change_password: bool,
    ) -> SessionTokens {
        self.create_session_with(NewSession {
            account_id,
            is_admin,
            is_viewer: false,
            must_change_password,
            origin: SessionOrigin::default(),
        })
        .await
    }

    /// VIEWER_ROLE_V1 / LOGIN_DEVICES_V1：完整版建立 session。
    async fn create_session_with(&self, new: NewSession) -> SessionTokens {
        let NewSession {
            account_id,
            is_admin,
            is_viewer,
            must_change_password,
            origin,
        } = new;
        // SESSION_TOKEN_256_V1（fix128）：256-bit CSPRNG（64 hex）——既不可猜、
        // 也不可預測；搭配落盤 SHA-256 與重放偵測，枚舉空間趨近無限大。
        let access = mint_secure_token();
        let refresh = mint_secure_token();
        // TKU_SHORT_SESSION_V1：來源 IP 落在淡江大學網段時，這次核發的 access／
        // refresh 都只給 1 小時（見常數定義處的完整說明）。
        let (access_ttl, refresh_ttl) = if is_tku_campus_ip(&origin.ip) {
            (TKU_SHORT_SESSION_TTL, TKU_SHORT_SESSION_TTL)
        } else {
            (ACCESS_TOKEN_TTL, REFRESH_TOKEN_TTL)
        };
        let session = AdminSession {
            account_id: account_id.clone(),
            is_admin,
            is_viewer,
            must_change_password,
            expires_at: Instant::now() + access_ttl,
        };

        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let access_expires_at = now_unix + access_ttl.as_secs() as i64;
        let refresh_expires_at = now_unix + refresh_ttl.as_secs() as i64;

        if let Err(e) = self
            .accounts_db
            .insert_dashboard_session_full(&crate::db::DashboardSessionInsert {
                token: access.clone(),
                account_id: account_id.clone(),
                is_admin,
                is_viewer,
                must_change_password,
                expires_at: access_expires_at,
                refresh_token: refresh.clone(),
                refresh_expires_at,
                ip: origin.ip,
                user_agent: origin.user_agent,
                created_at: origin.first_login_at,
            })
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
        // SESSION_TOKEN_HASH_V1：map 鍵 = SHA-256(access)；回給呼叫端的
        // SessionTokens 仍是明文（走 cookie），伺服器端不留明文。
        sessions.insert(crate::db::session_lookup_key(&access), session);
        SessionTokens {
            access,
            refresh,
            access_max_age: access_ttl.as_secs(),
            refresh_max_age: refresh_ttl.as_secs(),
        }
    }

    /// Rotate a refresh token into a fresh access+refresh pair.
    ///
    /// Validates the refresh token (still present in DB and not expired),
    /// then issues a new access token and a **new** refresh token, revoking the
    /// old one so a reused refresh token cannot be replayed after rotation.
    ///
    /// `current`：這次 refresh 請求的來源。舊列的 ip / user_agent 為空時
    /// （功能上線前建立的 session）用它補齊；有值時一律保留原始登入來源。
    /// REFRESH_REUSE_DETECT_V1（fix126）：refresh 在 DB 查不到時呼叫。
    ///
    /// 可查不到＝已輪換／已銷毀／自然過期／純偽造。前三種裡，只有「曾被輪換」
    /// 會在石碑表留痕——合法客戶端靠的是每次輪換後寫回的最新 cookie（瀏覽器
    /// cookie 罐跨分頁共享），永遠不會主動拿舊值來換；**舊值再現＝極可能憑證
    /// 遭竊**（OAuth2 RFC 6819 refresh-token theft 的標準訊號）。處置採
    /// fail-closed：撤銷該帳號**全部** session（記憶體＋DB）＋活動日誌＋安全
    /// 推播，寧可讓本人重登一次也不能讓竊取者接續。
    ///
    /// 兩個誤判護欄：
    ///   1. 寬限窗內的並行重播由 `refresh_grace_replay`（記憶體）先攔截；這裡
    ///      再多給一段「跨重啟保護窗」——服務重啟會清掉記憶體 grace，剛輪換完
    ///      的並行請求才不會被當竊聽。
    ///   2. 登出／改密碼／自然過期等「非輪換」刪除不立碑 → 不會誤檢。
    async fn detect_refresh_reuse(&self, refresh_token: &str) {
        let tombstone = self
            .accounts_db
            .lookup_refresh_tombstone(refresh_token)
            .await
            .ok()
            .flatten();
        let Some((account_id, rotated_at)) = tombstone else {
            return;
        };
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        // 跨重啟保護窗：寬限窗（180s）加 60s 緩衝；窗內一律靜默放棄，避免把
        // 服務重啟時的並行請求誤判成竊聽。
        if rotated_at + (SESSION_ROTATE_GRACE_SECS as i64) + 60 >= now_unix {
            return;
        }

        let warned_age_secs = now_unix - rotated_at;
        warn!(
            account_id = %account_id,
            age_secs = warned_age_secs,
            "REFRESH_REUSE_DETECT_V1：已輪換的 refresh token 被重複使用，疑似憑證遭竊——執行家族撤銷"
        );

        // 家族撤銷：記憶體＋DB 全清（revoke_sessions_for_account 走 account_id 路徑，
        // 與 token 形狀無關，SESSION_TOKEN_HASH_V1 之後照樣成立）。
        self.revoke_sessions_for_account(&account_id).await;

        let _ = self
            .accounts_db
            .record_activity_event(crate::db::ActivityEventInput {
                kind: "security".to_string(),
                actor: "system:refresh-reuse-detect".to_string(),
                action: "refresh_token_reuse_detected".to_string(),
                account_id: account_id.clone(),
                path: "/admin/api/auth/refresh".to_string(),
                result: "family_revoked".to_string(),
                details: format!(
                    "偵測到已輪換的 refresh token 被重複使用（輪換後 {warned_age_secs} 秒），\
                     疑似憑證遭竊；已撤銷該帳號所有裝置的登入 session"
                ),
                ip: String::new(),
                user_agent: String::new(),
            })
            .await;

        crate::web_push_notifications::notify_security_alert(
            vec![account_id.clone()],
            "您的登入憑證疑似遭竊用".to_string(),
            format!(
                "系統偵測到帳號 {account_id} 的舊登入憑證被重複使用，可能已遭他人截取。\
                 已立即登出該帳號在所有裝置上的 session，請盡快重新登入並更改密碼。"
            ),
            "refresh-token-reuse".to_string(),
        );
    }

    async fn rotate_session(
        &self,
        refresh_token: &str,
        current: Option<SessionOrigin>,
    ) -> Option<SessionTokens> {
        let refresh_token = refresh_token.trim();
        // REFRESH_TOKEN_STRICT_FORMAT_V1 (M-2): reject malformed values here as
        // well as at the cookie reader. 自 TOKEN_QUERY_FALLBACK_REMOVE_V1
        // （fix127）起 refresh 只信 HttpOnly cookie；TOKEN_QUERY_STRIP_ALL_ADMIN_V1
        // （H-2）在進 handler 前就已剝除呼叫端送來的任何 query token，因此唯一的
        // 輸入口就是 cookie。這層嚴格格式檢查是最後一道深度防禦。
        //
        // This also stops a legacy row whose `refresh_token` column is the
        // schema default `''` from ever being matched (the empty-string guard
        // this replaces existed for exactly that reason, and is subsumed here).
        if !is_valid_session_token_format(refresh_token) {
            return None;
        }
        // REFRESH_ROTATE_GRACE_V1（fix38）：這顆 refresh 剛被輪換過（並行分頁、
        // 前端重試、或「背景分頁 refresh 撞上剛完成的新登入」）→ 直接回同一次
        // 輪換的結果。舊版在這裡一定失敗，前端就把 cookie 當失效、跳登入頁。
        if let Some(tokens) = self.refresh_grace_replay(refresh_token).await {
            return Some(tokens);
        }
        let row = match self
            .accounts_db
            .get_dashboard_session_by_refresh(refresh_token)
            .await
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                // REFRESH_REUSE_DETECT_V1（fix126）：DB 查不到＝已輪換／已銷毀／偽造。
                // 查石碑表命中＝曾被輪換的舊值再現 → 疑似遭竊 → 帳號家族撤銷＋警報。
                self.detect_refresh_reuse(refresh_token).await;
                return None;
            }
            Err(_) => return None,
        };
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

        // ACCOUNT_SUSPEND_V1：停權中的帳號不得輪換出新 session；順便撤銷其既有 session。
        if self.account_is_suspended(&row.account_id).await {
            self.revoke_sessions_for_account(&row.account_id).await;
            return None;
        }

        // ROTATE_ACCESS_GRACE_V1（fix38）：舊 access **不立即撤銷**。它可能還拿在
        // 另一個分頁／另一個行程手上（面板總覽＋子頁面＋iClass 分頁同時開著是常態），
        // 舊版直接 remove＋delete＝那些分頁下一個請求就 401 → 前端清 cookie、跳登入頁
        // ——使用者看到的「一直被自動登出」。改成只把壽命壓到寬限視窗，之後由背景
        // 任務連 DB 一起清掉。
        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(&row.token) {
                let cap = Instant::now() + Duration::from_secs(SESSION_ROTATE_GRACE_SECS);
                if session.expires_at > cap {
                    session.expires_at = cap;
                }
            }
        }
        {
            let db = self.accounts_db.clone();
            // SESSION_TOKEN_HASH_V1：row.token 是散列鍵（非明文）→ 走 by_key 刪除。
            let old_access_key = row.token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(SESSION_ROTATE_GRACE_SECS)).await;
                let _ = db.delete_dashboard_session_hashed_key(&old_access_key).await;
            });
        }

        // VIEWER_ROLE_V1 / LOGIN_DEVICES_V1：輪換後沿用 viewer 旗標與原始登入來源
        // （裝置頁看到的仍是「同一台裝置」，不會因為每 12 小時輪換而變成新裝置）。
        let tokens = self
            .create_session_with(NewSession {
                account_id: row.account_id.clone(),
                is_admin: row.is_admin,
                is_viewer: row.is_viewer,
                must_change_password: row.must_change_password,
                origin: {
                    let stored = SessionOrigin {
                        ip: row.ip.clone(),
                        user_agent: row.user_agent.clone(),
                        first_login_at: row.created_at,
                    };
                    match current.as_ref() {
                        Some(now) => stored.or_fill(now),
                        None => stored,
                    }
                },
            })
            .await;

        // Revoke the old row (identified by its refresh token) so the old
        // refresh token cannot be used again——唯一的例外是寬限視窗內的**重播**：
        // 那條路徑由 refresh_grace 直接回同一組 token，不會走到這裡。
        let _ = self
            .accounts_db
            .delete_dashboard_session_by_refresh(refresh_token)
            .await;

        // REFRESH_REUSE_DETECT_V1（fix126）：為剛廢棄的舊 refresh 立碑——日後同一把
        // 舊值再現（超出寬限窗的重播）＝疑似憑證遭竊，觸發帳號家族撤銷＋安全警報。
        let _ = self
            .accounts_db
            .record_refresh_tombstone(refresh_token, &row.account_id, now_unix)
            .await;

        // REFRESH_ROTATE_GRACE_V1（fix38）：記下「舊 refresh → 這次的新 token」，
        // 讓晚到的重複請求拿回同一組結果（等同同一顆 session，不會互相撤銷）。
        self.refresh_grace_remember(refresh_token, &tokens);

        Some(tokens)
    }

    /// REFRESH_ROTATE_GRACE_V1（fix38）：寬限視窗內的輪換結果查詢（並順手清過期項）。
    ///
    /// 只有「那次輪換發出來的 access 仍在記憶體 session 表中」才算有效——登出、
    /// 改密碼、帳號停用都會撤掉 session，此時不得讓舊 refresh 復活。
    async fn refresh_grace_replay(&self, refresh_token: &str) -> Option<SessionTokens> {
        let entry = {
            let mut map = self
                .refresh_grace
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let now = Instant::now();
            map.retain(|_, entry| entry.expires_at > now);
            map.get(refresh_token).map(|entry| {
                (
                    entry.access.clone(),
                    entry.refresh.clone(),
                    entry.access_max_age,
                    entry.refresh_max_age,
                )
            })
        };
        let (access, refresh, access_max_age, refresh_max_age) = entry?;
        // SESSION_TOKEN_HASH_V1：grace 記錄存的是明文 access，比對前先散列對齊 map 鍵。
        let alive = self
            .sessions
            .read()
            .await
            .contains_key(&crate::db::session_lookup_key(&access));
        if !alive {
            self.refresh_grace_forget(refresh_token);
            return None;
        }
        Some(SessionTokens {
            access,
            refresh,
            access_max_age,
            refresh_max_age,
        })
    }

    /// REFRESH_ROTATE_GRACE_V1（fix38）：記下舊 refresh 對應的輪換結果。
    fn refresh_grace_remember(&self, old_refresh: &str, tokens: &SessionTokens) {
        let mut map = self
            .refresh_grace
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();
        map.retain(|_, entry| entry.expires_at > now);
        map.insert(
            old_refresh.to_string(),
            GraceEntry {
                access: tokens.access.clone(),
                refresh: tokens.refresh.clone(),
                expires_at: now + Duration::from_secs(SESSION_ROTATE_GRACE_SECS),
                access_max_age: tokens.access_max_age,
                refresh_max_age: tokens.refresh_max_age,
            },
        );
    }

    /// REFRESH_ROTATE_GRACE_V1（fix38）：撤掉單一一筆寬限記錄（session 已不存在時）。
    fn refresh_grace_forget(&self, old_refresh: &str) {
        let mut map = self
            .refresh_grace
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.remove(old_refresh);
    }

    /// PROXY_CSRF_BRIDGE_V1（fix40）：記下帳號最新的 iClass `csrftoken`。
    ///
    /// 值由 `tc_proxy.rs` 從上游回應的 `Set-Cookie` 攔下（格式已在那裡驗過），
    /// 這裡只做長度與空值的第二道保險。呼叫端已保證不為空。
    fn remember_proxied_csrf(&self, account_id: &str, token: &str) {
        let account_id = account_id.trim();
        if account_id.is_empty() || token.is_empty() || token.len() > 128 {
            return;
        }
        let mut map = self
            .proxied_csrf
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();
        map.retain(|_, entry| entry.expires_at > now);
        map.insert(
            account_id.to_string(),
            ProxiedCsrfEntry {
                token: token.to_string(),
                expires_at: now + Duration::from_secs(PROXIED_CSRF_TTL_SECS),
            },
        );
    }

    /// PROXY_CSRF_BRIDGE_V1（fix40）：取回帳號目前應使用的 iClass `csrftoken`。
    fn proxied_csrf_token(&self, account_id: &str) -> Option<String> {
        let account_id = account_id.trim();
        if account_id.is_empty() {
            return None;
        }
        let mut map = self
            .proxied_csrf
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();
        map.retain(|_, entry| entry.expires_at > now);
        map.get(account_id).map(|entry| entry.token.clone())
    }

    /// SESSION_TOKEN_HASH_V1：刪「明文 token」路徑（logout／middleware 注入）。
    /// 記憶體鍵先散列對齊；DB 由 `delete_dashboard_session` 內部散列。
    async fn revoke_session(&self, token: &str) {
        let token_trimmed = token.trim();
        let key = crate::db::session_lookup_key(token_trimmed);
        // REVOKE_ORDER_V1：先刪 DB、再移除記憶體。反過來的話，寫回記憶體的一方
        // （開機水合、DB 回填）可能在兩步之間讀到還沒刪的列，把已撤銷的 session 復活。
        if let Err(e) = self.accounts_db.delete_dashboard_session(token_trimmed).await {
            warn!(error = %e, "從資料庫刪除 Dashboard session 失敗");
        }
        self.sessions.write().await.remove(&key);
    }

    /// SESSION_TOKEN_HASH_V1：刪「DB 散列鍵」路徑——給已持有 `row.token`（散列
    /// 值）的呼叫端（登入裝置管理、rotate 延遲清理），全程零明文處理。
    async fn revoke_session_by_hashed_key(&self, key: &str) {
        let key_trimmed = key.trim();
        // REVOKE_ORDER_V1：先刪 DB、再移除記憶體（理由見 revoke_session）。
        if let Err(e) = self
            .accounts_db
            .delete_dashboard_session_hashed_key(key_trimmed)
            .await
        {
            warn!(error = %e, "依散列鍵刪除 Dashboard session 失敗");
        }
        self.sessions.write().await.remove(key_trimmed);
    }

    /// Password changes invalidate every Dashboard session for the account.
    async fn revoke_account_sessions(&self, account_id: &str) {
        self.revoke_sessions_for_account(account_id).await;
    }

    /// LOGIN_DEVICES_V1：回寫 session 最近活動時間。以 token 為 key 做 60 秒節流，
    /// 節流表最多 MAX_SESSIONS 筆，超過就整個清空重來（極端情況下只是多寫一次）。
    ///
    /// `origin`：目前請求的來源；只用來回填 DB 裡仍為空的 ip / user_agent
    /// （見 `AccountsDb::touch_dashboard_session`）。
    async fn touch_session_last_seen(&self, token: &str, origin: SessionOrigin) {
        static LAST_TOUCH: once_cell::sync::Lazy<tokio::sync::Mutex<HashMap<String, Instant>>> =
            once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(HashMap::new()));
        const TOUCH_INTERVAL: Duration = Duration::from_secs(60);
        let token = token.trim();
        if token.is_empty() {
            return;
        }
        let should_write = {
            let mut map = LAST_TOUCH.lock().await;
            let now = Instant::now();
            let due = map
                .get(token)
                .map(|at| now.saturating_duration_since(*at) >= TOUCH_INTERVAL)
                .unwrap_or(true);
            if due {
                if map.len() >= MAX_SESSIONS && !map.contains_key(token) {
                    map.clear();
                }
                map.insert(token.to_string(), now);
            }
            due
        };
        if !should_write {
            return;
        }
        let db = self.accounts_db.clone();
        let tok = token.to_string();
        tokio::spawn(async move {
            let now_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let _ = db
                .touch_dashboard_session(&tok, now_unix, &origin.ip, &origin.user_agent)
                .await;
        });
    }

    /// LOGIN_DEVICES_V1：撤銷「某帳號的其他所有 session」（保留目前這一個）。
    async fn revoke_other_sessions(&self, account_id: &str, keep_token: &str) -> usize {
        // SESSION_TOKEN_HASH_V1：row.token 是 SHA-256 散列鍵，保留鍵先雜湊對齊。
        let keep_key = crate::db::session_lookup_key(keep_token);
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let rows = self
            .accounts_db
            .list_dashboard_sessions_by_account(account_id, now_unix)
            .await
            .unwrap_or_default();
        let mut revoked = 0usize;
        for row in rows {
            if row.token == keep_key {
                continue;
            }
            self.revoke_session_by_hashed_key(&row.token).await;
            revoked += 1;
        }
        revoked
    }
}

pub fn build_admin_router(state: AdminWebState) -> Router {
    let password_guard_state = state.clone();
    let blacklist_guard_state = state.clone();
    let geo_guard_state = state.clone();
    let waf_guard_state = state.clone();
    let hosting_guard_state = state.clone();
    let same_origin_guard_state = state.clone();
    Router::new()
        // SEO_LANDING_V1（fix60）：`/` 依身分分流——已登入照舊 302 到 `/admin`，
        // 未登入回公開介紹頁（Google 唯一收得到的內容）。
        .route("/", get(pages::admin_root_page_handler))
        // 搜尋引擎用：只允許首頁被檢索（見 seo.rs 的 robots_txt／sitemap_xml）。
        .route("/robots.txt", get(seo::robots_txt_handler))
        .route("/sitemap.xml", get(seo::sitemap_xml_handler))
        // INDEXNOW_V1（fix105）：Bing 的所有權驗證檔（`/{key}.txt`）。
        // 金鑰只含 `[A-Za-z0-9-]`，所以這是一條確切的字面路徑（不是 `{參數}`），
        // 只會攔到這個檔名 —— 其餘根目錄請求照舊走原本的路由／404。
        .route(indexnow::key_path(), get(indexnow::key_file_handler))
        .route("/healthz", get(health::healthz_handler))
        // TRONCLASS_PROXY_JUMP_V1：iClass 反向代理。SPA 資源是根絕對路徑
        //（/static/...），因此頂層 /static、/api、/course、/uploads 也掛代理；
        // 面板自己不用這些頂層路徑（面板資源在 /admin/static、API 在 /admin/api）。
        // 代理需要大 body 上限（axum 預設 2MB 會擋作業附件上傳）。
        .route(
            "/tc/{*path}",
            axum::routing::any(tc_proxy::tc_proxy_handler)
                .layer(DefaultBodyLimit::max(tc_proxy::TC_PROXY_BODY_LIMIT)),
        )
        .route(
            "/static/{*path}",
            axum::routing::any(tc_proxy::tc_proxy_handler)
                .layer(DefaultBodyLimit::max(tc_proxy::TC_PROXY_BODY_LIMIT)),
        )
        .route(
            "/api/{*path}",
            axum::routing::any(tc_proxy::tc_proxy_handler)
                .layer(DefaultBodyLimit::max(tc_proxy::TC_PROXY_BODY_LIMIT)),
        )
        .route(
            "/course/{*path}",
            axum::routing::any(tc_proxy::tc_proxy_handler)
                .layer(DefaultBodyLimit::max(tc_proxy::TC_PROXY_BODY_LIMIT)),
        )
        .route(
            "/uploads/{*path}",
            axum::routing::any(tc_proxy::tc_proxy_handler)
                .layer(DefaultBodyLimit::max(tc_proxy::TC_PROXY_BODY_LIMIT)),
        )
        // TC_BULLETIN_LIST_ROUTE_V1（fix36）：iClass 的「課程公告」頁。使用者從面板
        // 公告卡片按「↗ iClass」時，伺服器是 302 到 `/tc/bulletin-list/?type=course`
        // （/tc 前綴會被剝掉轉給上游）；但 iClass SPA 的站內連結與資產都是「根絕對
        // 路徑」寫法，站內導覽或 F5 之後瀏覽器會直接要 `/bulletin-list/...`——
        // 面板沒有這個路由就 404（使用者看到的「按了公告按鈕進不去 iClass」）。
        // 這裡把該前綴也掛上代理；面板本身不使用 /bulletin-list。
        .route(
            "/bulletin-list",
            axum::routing::any(tc_proxy::tc_proxy_handler)
                .layer(DefaultBodyLimit::max(tc_proxy::TC_PROXY_BODY_LIMIT)),
        )
        .route(
            "/bulletin-list/{*path}",
            axum::routing::any(tc_proxy::tc_proxy_handler)
                .layer(DefaultBodyLimit::max(tc_proxy::TC_PROXY_BODY_LIMIT)),
        )
        .route("/manifest.webmanifest", get(pwa::manifest_handler))
        .route("/service-worker.js", get(pwa::service_worker_handler))
        .route("/admin/pwa/icon.svg", get(pwa::icon_handler))
        .route("/admin/pwa/icon-192.png", get(pwa::icon_192_handler))
        .route("/admin/pwa/icon-512.png", get(pwa::icon_512_handler))
        .route("/admin/pwa/apple-touch-icon.png", get(pwa::apple_touch_icon_handler))
        // PWA_ICON_FULL_BLEED_V1：iOS 在找不到／忽略 <link> 時會直接向網站根目錄要這幾個檔名；
        // 沒有的話就退回網頁截圖或「小圖墊白底」。全部指向同一張 180 滿版 PNG。
        .route("/apple-touch-icon.png", get(pwa::apple_touch_icon_handler))
        .route("/apple-touch-icon-precomposed.png", get(pwa::apple_touch_icon_handler))
        .route("/apple-touch-icon-180x180.png", get(pwa::apple_touch_icon_handler))
        .route("/favicon.ico", get(pwa::icon_192_handler))
        .route("/favicon.png", get(pwa::icon_192_handler))
        .route("/og-image.png", get(pwa::og_image_handler))
        .route("/admin/static/theme.css", get(static_assets::admin_theme_css_handler))
        .route(
            "/admin/static/session-security.js",
            get(static_assets::admin_session_security_js_handler),
        )
        .route(
            "/admin/static/app-settings.js",
            get(static_assets::admin_app_settings_js_handler),
        )
        .route(
            "/admin/static/notifications-page.js",
            get(static_assets::admin_notifications_page_js_handler),
        )
        .route("/admin/static/core-api.js", get(static_assets::admin_core_api_js_handler))
        .route("/admin/static/web-push.js", get(static_assets::admin_web_push_js_handler))
        // SUBPAGE_FETCH_SHARED_V1 / CLIENT_ERROR_REPORT_V1
        .route("/admin/static/subpage-fetch.js", get(static_assets::admin_subpage_fetch_js_handler))
        .route("/admin/static/client-log.js", get(static_assets::admin_client_log_js_handler))
        .route(
            "/admin/api/client-log",
            post(health::client_log_handler).layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // DASHBOARD_LIVE_EVENTS_V1
        .route("/admin/static/live-events.js", get(static_assets::admin_live_events_js_handler))
        // URL_SAFETY_SHARED_V1
        .route(
            "/admin/static/url-safety.js",
            get(static_assets::admin_url_safety_js_handler),
        )
        // ROLLCALL_DETECT_SHARED_V1：要排在 rollcall-pending-fab.js 的路由「之前」
        // 沒有順序要求（Axum router 比對的是獨立路徑，跟宣告順序無關），但程式碼放在這裡
        // 跟 inject_theme() 裡 <script> 標籤的順序對應，方便日後維護時一眼看出依賴關係。
        .route(
            "/admin/static/rollcall-detect.js",
            get(static_assets::admin_rollcall_detect_js_handler),
        )
        // ROLLCALL_PENDING_FAB_V1
        .route(
            "/admin/static/rollcall-pending-fab.js",
            get(static_assets::admin_rollcall_pending_fab_js_handler),
        )
        .route(
            "/admin/static/js/dompurify.min.js",
            get(static_assets::admin_dompurify_js_handler),
        )
        .route(
            "/admin/static/js/trusted-types-init.js",
            get(static_assets::admin_trusted_types_init_js_handler),
        )
        .route(
            "/admin/static/js/jszip.min.js",
            get(static_assets::admin_jszip_js_handler),
        )
        .route(
            "/admin/static/js/office-view.js",
            get(static_assets::admin_office_view_js_handler),
        )
        // QR_SHARE_V1
        .route(
            "/admin/static/js/qrcode.min.js",
            get(static_assets::admin_qrcode_js_handler),
        )
        // SHARE_ADMIN_V1
        .route(
            "/admin/static/js/shares-admin.js",
            get(static_assets::admin_shares_admin_js_handler),
        )
        // ACCOUNT_SUSPEND_V1
        .route(
            "/admin/static/js/suspensions-admin.js",
            get(static_assets::admin_suspensions_admin_js_handler),
        )
        // DASH_ASSET_V1
        .route(
            "/admin/static/js/dashboard-particles.js",
            get(static_assets::admin_dashboard_particles_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-login-card.js",
            get(static_assets::admin_dashboard_login_card_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-live-alerts.js",
            get(static_assets::admin_dashboard_live_alerts_js_handler),
        )
        // DASH_ASSET_V2
        .route(
            "/admin/static/js/dashboard-permissions.js",
            get(static_assets::admin_dashboard_permissions_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-cookie-badge.js",
            get(static_assets::admin_dashboard_cookie_badge_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-modal.js",
            get(static_assets::admin_dashboard_modal_js_handler),
        )
        // DASH_ASSET_V3
        .route(
            "/admin/static/js/dashboard-header-more.js",
            get(static_assets::admin_dashboard_header_more_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-more-buttons.js",
            get(static_assets::admin_dashboard_more_buttons_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-more-links.js",
            get(static_assets::admin_dashboard_more_links_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-more-pages.js",
            get(static_assets::admin_dashboard_more_pages_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-notify-purge.js",
            get(static_assets::admin_dashboard_notify_purge_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-desktop-size.js",
            get(static_assets::admin_dashboard_desktop_size_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-header-spacious.js",
            get(static_assets::admin_dashboard_header_spacious_js_handler),
        )
        .route(
            "/admin/static/js/dashboard-more-actions.js",
            get(static_assets::admin_dashboard_more_actions_js_handler),
        )
        // SHARED_MOBILE_NAV_V1
        .route(
            "/admin/static/js/mobile-nav.js",
            get(static_assets::shared_mobile_nav_js_handler),
        )
        // ONBOARDING_GUIDE_V1
        .route(
            "/admin/static/js/dashboard-onboarding.js",
            get(static_assets::admin_dashboard_onboarding_js_handler),
        )
        // PDFJS_PREVIEW_V1
        .route(
            "/admin/static/js/pdf.min.js",
            get(static_assets::admin_pdf_js_handler),
        )
        .route(
            "/admin/static/js/pdf.worker.min.js",
            get(static_assets::admin_pdf_worker_js_handler),
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
            "/admin/static/timetable.js",
            get(static_assets::admin_timetable_ui_module_js_handler),
        )
        .route(
            "/admin/static/credits.js",
            get(static_assets::admin_credits_page_js_handler),
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
        // DASHBOARD_UI_BUNDLE_V1
        .route(
            "/admin/static/js/dashboard-ui.js",
            get(static_assets::admin_dashboard_ui_js_handler),
        )
        .route(
            "/admin/static/dashboard-extras.css",
            get(static_assets::admin_dashboard_extras_css_handler),
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
        // LOGOUT_SERVER_SIDE_V1：導覽式登出（POST 表單 / fetch → 302 /admin）。
        .route(
            "/admin/logout",
            post(auth::admin_logout_redirect_handler).layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
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
        // RADAR_PANEL_V1（fix70）：雷達點名設定與狀態（僅管理員）。
        .route("/admin/radar", get(radar::page_handler))
        // RADAR_INPUT_PAGE_V1（fix71）：輸入座標的頁面（數字點名有 /number，雷達用這一頁）。
        .route("/admin/radar/input", get(radar_input::page_handler))
        .route(
            "/admin/api/radar",
            get(radar::get_handler)
                .post(radar::post_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/radar/input",
            get(radar_input::get_handler)
                .post(radar_input::post_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // REST_DAY_PAGE_V1：休息日與寒暑假（唯讀）。
        .route("/admin/rest-days", get(rest_days::page_handler))
        // LOGIN_DEVICES_V1：登入裝置管理（任何已登入帳號、只管自己的 session）
        .route("/admin/devices", get(devices::page_handler))
        .route("/admin/api/devices", get(devices::list_handler))
        // UPDATE_TIMES_PAGE_V1：資料更新時間（任何已登入帳號；可見範圍同 /admin/api/status）
        .route("/admin/update-times", get(update_times::page_handler))
        .route("/admin/api/update-times", get(update_times::api_handler))
        .route(
            "/admin/api/devices/revoke",
            post(devices::revoke_handler).layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/rest-days",
            get(rest_days::get_handler)
                .post(rest_days::update_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // WAF_TELEMETRY_API_V1：WAF 即時遙測統計，供管理後台查看防禦指標。
        .route(
            "/admin/api/waf/stats",
            get(waf::waf_stats_handler).layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // BULLETINS_V1: 課程公告頁＋API。
        .route("/admin/bulletins", get(bulletins::page_handler))
        .route("/admin/api/bulletins", get(bulletins::list_handler))
        // BULLETIN_ATTACHMENT_V1（fix55）：公告附件代理（伺服器端用保存的
        // iClass session 取簽章 URL 再串流；支援 Range／206）。
        .route(
            "/admin/api/bulletins/attachment",
            get(bulletins::admin_bulletin_attachment_handler),
        )
        .route(
            "/admin/api/bulletins/read",
            post(bulletins::read_handler),
        )
        .route(
            "/admin/api/bulletins/test",
            post(bulletins::test_handler),
        )
        .route(
            "/admin/api/bulletins/admin",
            post(bulletins::admin_create_handler),
        )
        .route(
            "/admin/api/bulletins/admin/delete",
            post(bulletins::admin_delete_handler),
        )
        // BULLETIN_TRANSLATE_V8：翻譯改為背景 job（立即回 202），
        // 避免占住 HTTP request 直到 proxy 逾時 → 502。狀態走 GET 輪詢。
        .route(
            "/admin/api/bulletins/translate",
            post(bulletins::translate_handler),
        )
        .route(
            "/admin/api/bulletins/translate/status",
            get(bulletins::translate_status_handler),
        )
        // BULLETIN_TRANSLATE_V7：自我檢測（僅管理員）。用來分辨
        // 「502 是反向代理／程序中斷」還是「本服務連不到 Google」。
        .route(
            "/admin/api/bulletins/translate/selftest",
            get(bulletins::translate_selftest_handler),
        )
        // AI_CHAT_V1：ChatGPT 風格對話頁。密鑰存後端並自動輪替，
        // 生成走「背景 job + 輪詢」避免反向代理逾時回 502。
        .route("/admin/ai", get(ai_chat::page_handler))
        .route("/admin/api/ai/state", get(ai_chat::state_handler))
        .route("/admin/api/ai/keys", post(ai_chat::keys_handler))
        .route("/admin/api/ai/keys/test", post(ai_chat::key_test_handler))
        .route("/admin/api/ai/settings", post(ai_chat::settings_handler))
        .route("/admin/api/ai/models", post(ai_chat::models_handler))
        // BULLETIN_AI_SUMMARY_V1：公告 AI 摘要（與聊天共用密鑰輪替與 status 輪詢）。
        .route(
            "/admin/api/ai/summarize",
            post(ai_chat::summarize_handler).layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .route(
            "/admin/api/ai/chat",
            post(ai_chat::chat_handler).layer(DefaultBodyLimit::max(ADMIN_AI_CHAT_BODY_LIMIT)),
        )
        .route("/admin/api/ai/chat/status", get(ai_chat::chat_status_handler))
        .route("/admin/api/ai/chat/stream", get(ai_chat::chat_stream_handler))
        .route(
            "/admin/api/ai/workspace/file",
            get(ai_chat::workspace_file_handler),
        )
        .route("/admin/api/ai/chat/stop", post(ai_chat::chat_stop_handler))
        .route("/admin/api/ai/threads", post(ai_chat::threads_handler))
        .route("/admin/push-status", get(web_push::page_handler))
        .route("/admin/health", get(health::admin_health_page_handler))
        .route("/admin/mock", get(mock::admin_mock_page_handler))
        // FILES_NATIVE_V1: 廢除 FileBrowser，/admin/files 改服務同源教材雲端硬碟頁。
        .route("/admin/files", get(pages::admin_files_page_handler))
        // SHARE_ADMIN_V1
        .route("/admin/shares", get(pages::admin_shares_page_handler))
        .route("/admin/api/shares", get(files::admin_shares_list_handler))
        .route("/admin/api/shares/revoke", post(files::admin_shares_revoke_handler))
        // ACCOUNT_SUSPEND_V1：帳號暫時停權管理（僅管理員可設定）。
        .route("/admin/suspensions", get(pages::admin_suspensions_page_handler))
        .route("/admin/api/suspensions", get(suspensions::admin_suspensions_list_handler))
        .route("/admin/api/suspensions/set", post(suspensions::admin_suspensions_set_handler))
        .route("/admin/api/suspensions/clear", post(suspensions::admin_suspensions_clear_handler))
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
        .route("/admin/settings", get(pages::admin_settings_page_handler))
        .route("/admin/notifications", get(pages::admin_notifications_page_handler))
        .route("/admin/passkey", get(pages::admin_passkey_page_handler))
        .route("/admin/logs", get(logs::admin_logs_page_handler))
        .route("/admin/api/public_status", get(status::admin_public_status_handler))
        .route("/admin/api/blacklist", get(blacklist::admin_blacklist_list_handler))
        // BLACKLIST_UX_V1（fix58）：黑名單頁的「最近登入失敗」改用這個端點——它會
        // 附上「位置」（geoip，24h 快取）與「裝置」（User-Agent 摘要），並標記該 IP
        // 是否已經在黑名單裡（前端就不用自己比對，也不會再出現重複的「加入黑名單」）。
        .route(
            "/admin/api/blacklist/recent-logins",
            get(blacklist::admin_blacklist_recent_logins_handler),
        )
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
        // IP_BLACKLIST_AUTO_REDIRECT_V1：`blocked.html` 輪詢用，不需要 session
        // （道理同 `/admin/api/geo-restrict/access-status`）。
        .route(
            "/admin/api/blacklist/status",
            get(blacklist::admin_blacklist_status_handler),
        )
        // ADMIN_GEO_RESTRICT_V1：「僅限台灣 IP 存取 /admin」開關，放在黑名單頁一起管理
        // ——同屬 IP 層級的存取控制，不另開一個管理頁面。GET／POST 分開兩條路徑，
        // 與既有 `/admin/api/blacklist`（讀）+ `/admin/api/blacklist/block`（寫）
        // 同一種風格，不用同一路徑疊加方法 handler。
        .route(
            "/admin/api/geo-restrict",
            get(blacklist::admin_geo_restrict_get_handler),
        )
        .route(
            "/admin/api/geo-restrict/set",
            post(blacklist::admin_geo_restrict_set_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // HOSTING_IP_BLOCK_V1：「封鎖雲端／VPS機房 IP」開關，與僅限台灣 IP 開關
        // 同一種風格（GET 讀、POST 寫），放在黑名單頁一起管理。被擋訪客的
        // 「聯絡管理員」與輪詢共用上面 `/admin/api/geo-restrict/*` 既有端點，
        // 不另外開一組，所以這裡只需要開關本身的兩支路由。
        .route(
            "/admin/api/hosting-restrict",
            get(blacklist::admin_hosting_restrict_get_handler),
        )
        .route(
            "/admin/api/hosting-restrict/set",
            post(blacklist::admin_hosting_restrict_set_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // HOSTING_IP_RECENT_BLOCKS_V1：「最近被雲端／VPS機房 IP 擋下」清單，
        // 讓管理員能實際看到這個功能有沒有在擋東西（見 `hosting_ip` 模組文件）。
        .route(
            "/admin/api/hosting-restrict/recent-blocks",
            get(blacklist::admin_hosting_recent_blocks_handler),
        )
        // ADMIN_GEO_RESTRICT_V1：被擋下的訪客按「聯絡管理員」——這條刻意不需要
        // session（見 handler 文件註解），已在 `reject_foreign_admin_ip_
        // middleware` 的豁免清單排除，否則被擋的人連這支都連不到。
        .route(
            "/admin/api/geo-restrict/request-access",
            post(blacklist::admin_geo_access_request_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // GEO_ACCESS_AUTO_REDIRECT_V1：被擋頁面用 JS 輪詢，管理員核准後不用
        // 使用者手動重新整理——一樣不需要 session（道理同上面那支）。
        .route(
            "/admin/api/geo-restrict/access-status",
            get(blacklist::admin_geo_access_status_handler),
        )
        // 待處理申請清單＋核准／忽略：Discord 按鈕的網頁版備援，一樣要登入。
        .route(
            "/admin/api/geo-restrict/requests",
            get(blacklist::admin_geo_access_pending_handler),
        )
        .route(
            "/admin/api/geo-restrict/requests/approve",
            post(blacklist::admin_geo_access_approve_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/geo-restrict/requests/dismiss",
            post(blacklist::admin_geo_access_dismiss_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // GEO_ACCESS_BURST_MUTE_V1：洗版時「清空並恢復通知」按鈕——把目前所有
        // 待處理申請標成忽略（維持擋下）並解除靜音旗標，恢復逐筆通知。
        .route(
            "/admin/api/geo-restrict/requests/clear-all",
            post(blacklist::admin_geo_access_clear_all_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route("/admin/api/health", get(health::admin_health_api_handler))
        // METRICS_HISTORY_V1
        .route(
            "/admin/api/health/history",
            get(health::admin_health_history_handler),
        )
        .route(
            "/admin/api/health/check",
            get(health::admin_health_check_handler),
        )
        // Caddy uses this authenticated GET route only for Filebrowser's
        // initial HTML shell, allowing the Rust Dashboard to add a return link.
        // FILES_NATIVE_V1: FileBrowser shell / share 代理路由已廢除。
        // 教材改走同源 /admin/api/files/* 端點；如需重新啟用 FileBrowser，
        // 可在此恢復下方三條路由（並保留 fb_sso / materials_webdav 模組）。
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
        // OFFICE_ONLINE_V1: 核發限時簽章公開連結（已登入）。
        .route(
            "/admin/api/files/office-embed",
            get(files::admin_files_office_embed_handler),
        )
        // SHARE_V1: 核發免登入可預覽分享連結
        .route(
            "/admin/api/files/share-link",
            get(files::admin_files_share_link_handler),
        )
        // PREVIEW_LOG_V1: 前端預覽報錯回報（後台日誌可見）。
        .route(
            "/admin/api/files/preview-log",
            post(files::admin_files_preview_log_handler),
        )
        // OFFICE_ONLINE_V1: 公開、限時、簽章驗證的檔案串流（供微軟檢視器抓取）。
        .route("/pub/office", get(files::public_office_file_handler))
        // SHARE_V1: 免登入分享預覽頁＋其所需的公開 viewer 靜態檔。
        .route("/pub/share", get(files::public_share_preview_handler))
        // SHARE_V2: 分享密碼表單解鎖（HTML 表單 POST，導回分享頁）。
        .route(
            "/pub/share/unlock",
            post(files::public_share_unlock_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route("/pub/preview-log", post(files::public_share_log_handler))
        .route(
            "/pub/static/js/jszip.min.js",
            get(static_assets::admin_jszip_js_handler),
        )
        .route(
            "/pub/static/js/office-view.js",
            get(static_assets::admin_office_view_js_handler),
        )
        .route(
            "/pub/static/js/share-preview.js",
            get(static_assets::admin_share_preview_js_handler),
        )
        .route(
            "/pub/static/css/share.css",
            get(static_assets::admin_share_css_handler),
        )
        .route(
            "/pub/static/js/pdf.min.js",
            get(static_assets::admin_pdf_js_handler),
        )
        .route(
            "/pub/static/js/pdf.worker.min.js",
            get(static_assets::admin_pdf_worker_js_handler),
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
        // MATERIAL_RENAME_V1 / MATERIAL_MOVE_V1: 改名、拖曳移動（同一套路徑檢查，不覆蓋既有項目）。
        .route(
            "/admin/api/files/rename",
            post(files::admin_files_rename_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/files/move",
            post(files::admin_files_move_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // SHARE_V2: 自訂有效期／分享密碼／檢視次數上限的分享連結。
        .route(
            "/admin/api/files/share-link2",
            post(files::admin_files_share_link2_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // MATERIALS_IMPORT_V1: 自動從 iClass 匯入教材到教材雲端硬碟。
        // MATERIALS_IMPORT_SHARED_V1: 匯入核心改為背景 job（立即回 202，
        // 避免佔住 HTTP request 直到 proxy 逾時 → 502）；另提供進度查詢。
        .route(
            "/admin/api/files/import",
            get(materials_import::admin_materials_import_handler),
        )
        .route(
            "/admin/api/files/import/status",
            get(materials_import::admin_materials_import_status_handler),
        )
        .route("/admin/api/logs", get(logs::admin_logs_api_handler))
        // FILES_NATIVE_V1: FileBrowser 的 forward_auth 檢查點已廢除。
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
        .route("/admin/api/push/overview", get(web_push::overview_handler))
        .route(
            "/admin/api/push/subscribe",
            post(web_push::subscribe_handler).layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .route(
            "/admin/api/push/unsubscribe",
            post(web_push::unsubscribe_handler).layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // PUSH_GONE_VISIBLE_V1：service worker 的 pushsubscriptionchange 端點輪替。
        .route(
            "/admin/api/push/rotate",
            post(web_push::rotate_handler).layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
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
        // DASHBOARD_LIVE_EVENTS_V1：狀態變更／版本更新事件流（前端自動刷新與自動重載）
        .route("/admin/api/events", get(status::admin_dashboard_events_handler))
        .route(
            "/admin/api/class_reminder",
            get(class_reminder::admin_class_reminder_get_handler)
                .post(class_reminder::admin_class_reminder_update_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // DIGEST_PER_ACCOUNT_V1：晨間摘要每帳號開關
        .route(
            "/admin/api/digest/preference",
            get(digest_pref::get_handler)
                .post(digest_pref::update_handler)
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
            "/admin/api/homework/ladder",
            get(notifications::admin_homework_ladder_get_handler)
                .post(notifications::admin_homework_ladder_update_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/api/timetable",
            get(timetable::admin_timetable_list_handler),
        )
        // ROLLCALL_ATTENDANCE_V1（fix66）：點名頁的出席統計與「由誰送出」。
        // 兩者都是唯讀（GET），資料來源＝rollcall_events 資料表。
        .route(
            "/admin/api/rollcalls/stats",
            get(rollcall_stats::admin_rollcall_stats_handler),
        )
        .route(
            "/admin/api/rollcalls/records",
            get(rollcall_stats::admin_rollcall_records_handler),
        )
        .route(
            "/admin/api/credits",
            get(credits::admin_credits_report_handler),
        )
        .route(
            "/admin/api/credits/sync/run",
            post(credits::admin_credits_sync_run_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
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
        // TRONCLASS_PROXY_JUMP_V1：免登入直達 iClass 的入口（設代理帳號 cookie 後 302）。
        .route(
            "/admin/tc-go",
            get(tc_proxy::tc_go_handler),
        )
        // HOMEWORK_DETAIL_V1（fix41）：面板內看作業內容（說明＋附件），
        // 不需登入 iClass、不經過 /tc/ 代理。
        .route(
            "/admin/api/homework/detail",
            get(homework::admin_homework_detail_handler),
        )
        .route(
            "/admin/api/homework/attachment",
            get(homework::admin_homework_attachment_handler),
        )
        // HOMEWORK_FILE_V1（fix43）：作業附件／自己繳交的檔案，可預覽（inline）與下載
        // （attachment）。與 attachment 同一條實作，差別只在 Content-Disposition 與
        // 是否轉送 Range（影音／PDF 分段讀取都要靠它）。
        .route(
            "/admin/api/homework/file",
            get(homework::admin_homework_file_handler),
        )
        .route(
            "/admin/api/homework/complete",
            post(homework::admin_homework_complete_handler)
                .layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        // HOMEWORK_SUBMIT_V1（fix42）：面板內直接繳交作業。
        // 檔案走 multipart（欄位名 `file`，可多個），因此 body 上限放寬到
        // 100 MiB＋餘裕；真正的把關在 handler 內（單檔 50 MiB、總量 100 MiB、10 檔）。
        .route(
            "/admin/api/homework/submit",
            post(homework::admin_homework_submit_handler)
                .layer(DefaultBodyLimit::max(homework::HOMEWORK_SUBMIT_BODY_LIMIT)),
        )
        // HOMEWORK_SUBMIT_V1（fix42）：目前繳交狀態（彈窗顯示用）。
        .route(
            "/admin/api/homework/submission",
            get(homework::admin_homework_submission_handler),
        )
        .route(
            "/admin/api/session/refresh",
            post(auth::admin_session_refresh_handler).layer(DefaultBodyLimit::max(ADMIN_TINY_BODY_LIMIT)),
        )
        .route(
            "/admin/timetable",
            axum::routing::get(pages::admin_clean_timetable_page_handler),
        )
        .route(
            "/admin/credits",
            axum::routing::get(pages::admin_credits_page_handler),
        )
        // STOCKGENIE_EMBED_V1：智選頁與 JSON 代理。不掛頂層 /api（那是 iClass）。
        .route(
            "/admin/stock",
            axum::routing::get(stock::admin_stock_page_handler),
        )
        .route(
            "/admin/stock-api/{*path}",
            axum::routing::any(stock::admin_stock_api_proxy)
                .layer(DefaultBodyLimit::max(ADMIN_JSON_BODY_LIMIT)),
        )
        .layer(axum::middleware::from_fn(admin_security_headers_middleware))
        // ADMIN_GEO_RESTRICT_V1：刻意放在 IP 黑名單檢查「之內」（layer 疊加順序下，
        // 後加的 layer 較外層、較早執行）——已被列入黑名單的 IP 一律先看到黑名單的
        // 說明頁，不論是不是國外，黑名單的訊息更明確、也是管理員手動／既有規則判定的
        // 結果，優先序理當高於這個新功能。
        .layer(axum::middleware::from_fn_with_state(
            geo_guard_state,
            reject_foreign_admin_ip_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            blacklist_guard_state,
            reject_blacklisted_ip_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            password_guard_state,
            require_dashboard_password_change_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            same_origin_guard_state,
            admin_same_origin_middleware,
        ))
        .layer(axum::middleware::from_fn(
            session_cookie_to_query_middleware,
        ))
        .layer(axum::middleware::from_fn(https_redirect_middleware))
        // WAF_FIREWALL_V1: 系統防火牆最外層防禦，攔截惡意探針、攻擊載荷與 L7 洪水掃描。
        .layer(axum::middleware::from_fn_with_state(
            waf_guard_state,
            waf::waf_firewall_middleware,
        ))
        // HOSTING_IP_BLOCK_V1：刻意放在 WAF「之外」（layer 疊加順序下，後加的
        // layer 較外層、較早執行）——已知是雲端／VPS機房出口的 IP 直接擋下，
        // 連 WAF 的特徵比對都不必跑，語意上與 WAF 同屬「site-wide 第一線防禦」，
        // 緊鄰擺放。
        .layer(axum::middleware::from_fn_with_state(
            hosting_guard_state,
            reject_hosting_ip_middleware,
        ))
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
                .deflate(true)
                // AI_CHAT_V8：壓縮器會把 SSE 累在緩衝區裡等資料，等於把串流變成
                // 「生成完才一次吐出來」。明確排除，不倚賴 tower-http 的預設行為。
                .compress_when(
                    tower_http::compression::predicate::DefaultPredicate::new()
                        .and(tower_http::compression::predicate::NotForContentType::SSE),
                ),
        )
        // STREAM_NO_COMPRESSION_V1（fix43）：上面的壓縮層要在**更外層**先看到請求，
        // 才能把串流路徑的 `accept-encoding` 拿掉；axum 的 `.layer()` 是後加的在外層。
        .layer(axum::middleware::from_fn(
            no_compression_for_file_streams_middleware,
        ))
        // SEO_LANDING_V1（fix60）：私有路徑一律補 `X-Robots-Tag: noindex`。
        .layer(axum::middleware::from_fn(
            noindex_private_paths_middleware,
        ))
        .with_state(state)
}

/// SEO_LANDING_V1（fix60）：這些前綴的路徑不該出現在搜尋結果。
///
/// 為什麼 robots.txt 之外還需要這個標頭：robots.txt 只擋「爬取」，Google 仍可能因為
/// 外部連結而以「只顯示網址、不顯示摘要」的形式收錄（那對隱私沒幫助、對品牌也難看）；
/// `X-Robots-Tag: noindex` 才是要求「不要收錄」。兩者併用最乾淨。
///
/// `/tc`、`/api`、`/course`、`/uploads` 是 iClass 反向代理：上游內容已經有自己的網域，
/// 讓它掛在本站下面只會產生重複內容（duplicate content），一併排除。
fn path_is_private_for_search(path: &str) -> bool {
    // ROBOTS_NO_BLANKET_DISALLOW_V1（fix105）：robots.txt 不再有一條大範圍的
    // `Disallow: /`，所以「不要被收錄」這件事改由這裡（X-Robots-Tag: noindex）與
    // robots.txt 的逐條清單共同保證 —— 兩份清單必須一致，
    // `checks/seo_check.py` 會逐條比對。
    //
    // 公開管理頁面（radar、bulletins、settings、blacklist）豁免 noindex，
    // 允許搜尋引擎收錄。
    const PUBLIC_ADMIN_PATHS: [&str; 4] = [
        "/admin/radar",
        "/admin/bulletins",
        "/admin/settings",
        "/admin/blacklist",
    ];
    if PUBLIC_ADMIN_PATHS.iter().any(|p| path == *p || path.starts_with(&format!("{p}/"))) {
        return false;
    }

    const PRIVATE_PREFIXES: [&str; 16] = [
        "/admin",
        "/pub",
        "/tc/",
        "/api/",
        "/static/",
        "/course/",
        "/uploads/",
        "/bulletin-list",
        "/homework",
        "/scanner",
        "/number",
        "/radar",
        "/healthz",
        "/manifest.webmanifest",
        "/service-worker.js",
        "/.well-known/",
    ];
    PRIVATE_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
}

async fn noindex_private_paths_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    if path_is_private_for_search(&path) {
        // 不覆蓋既有的值（管線內其他層若已標註，以它為準）。
        response.headers_mut().entry(HeaderName::from_static("x-robots-tag")).or_insert_with(|| {
            HeaderValue::from_static("noindex, nofollow")
        });
    }
    response
}

/// STREAM_NO_COMPRESSION_V1（fix43）：這些路徑回的是「檔案位元組」，不做即時壓縮。
///
/// 為什麼要排除：`CompressionLayer` 的預設判定只看「大小 > 32 bytes」與
/// gRPC／SSE／圖片，**不看內容是否已經是壓縮格式**。教材裡的 PDF、PPTX、ZIP、
/// 影片、音檔本來就壓過了，再過一次 brotli/gzip 幾乎不會變小，卻讓 CPU 成為下載
/// 速度的天花板；串流回應還會在每個 chunk 觸發 flush，成本更高。症狀就是
/// 「明明是區網等級的頻寬，教材卻下載得很慢」。
///
/// 只影響下列檔案串流路徑；HTML／JSON／JS／CSS 的壓縮完全不變。
fn is_no_compression_stream_path(path: &str) -> bool {
    matches!(
        path,
        "/admin/api/files/download"
            | "/admin/api/files/preview"
            | "/admin/api/bulletins/attachment"
            | "/admin/api/homework/file"
            | "/admin/api/homework/attachment"
            | "/pub/office"
            | "/pub/share"
    )
}

async fn no_compression_for_file_streams_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !is_no_compression_stream_path(request.uri().path()) {
        return next.run(request).await;
    }
    // 拿掉 accept-encoding：壓縮層協商不到編碼，就會原樣傳輸位元組。
    let (mut parts, body) = request.into_parts();
    parts.headers.remove(axum::http::header::ACCEPT_ENCODING);
    next.run(axum::extract::Request::from_parts(parts, body)).await
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
    // LOGIN_GEO_ALERT_V1：國家判定也只信任代理送來的值。
    "cf-ipcountry",
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
    // SESSION_COOKIE_PRIORITY_V1（fix27）：兩輪掃描，先 __Host-（https）版再 local 版。
    // 舊版「先掃到誰算誰」的問題：兩顆並存（歷史 local cookie＋新的 __Host-）時，
    // 瀏覽器送 Cookie 的順序不保證新的在前——掃到過期的舊 token 就 401，
    // 造成「登入成功→立刻又被導回登入頁」的無限循環（部署重啟清空記憶體
    // sessions 後觸發，2026-09-14 使用者實測回報）。
    for name in [SECURE_SESSION_COOKIE, LOCAL_SESSION_COOKIE] {
        if let Some(value) = session_cookie_value_named(headers, name) {
            return Some(value);
        }
    }
    None
}

/// SESSION_DUAL_CANDIDATE_V1（fix36）：兩個名字的 session cookie **候選清單**
///（`__Host-` 優先，同值只留一份）。
///
/// 為什麼需要「清單」而不只是「第一個」：fix27 讓讀取端優先讀 `__Host-`，
/// 但如果瀏覽器手上是「過期的 __Host-＋剛種下的 local」（見
/// SESSION_COOKIE_SYNC_V1 的說明），優先序反而讓舊 token 每次都勝出——單一值
/// 的讀取端永遠救不回來。resolve_page_session／refresh handler／代理會依序
/// 試完所有候選，任何一顆有效就成功，等於「舊名殘留」再也不會把使用者鎖在
/// 登入頁外。
fn session_cookie_candidates(headers: &HeaderMap) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in [SECURE_SESSION_COOKIE, LOCAL_SESSION_COOKIE] {
        if let Some(value) = session_cookie_value_named(headers, name) {
            if !out.contains(&value) {
                out.push(value);
            }
        }
    }
    out
}

/// 掃 Cookie header 中「指定名稱」的 session cookie；格式檢查與
/// REFRESH_TOKEN_STRICT_FORMAT_V1 (M-2) 共用同一定義。
fn session_cookie_value_named(headers: &HeaderMap, want: &str) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookies.split(';') {
        // NOTE: `?` here used to abort the whole scan as soon as any cookie in
        // the header had no `=` (Cloudflare and analytics cookies routinely do),
        // so the session cookie later in the list was never inspected.
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name != want {
            continue;
        }
        let value = value.trim();
        if is_valid_session_token_format(value) {
            return Some(value.to_string());
        }
    }
    None
}

/// `max_age_secs`：TKU_SHORT_SESSION_V1 起，呼叫端傳入這次核發時實際算好的壽命
/// （一般情況下就是 `ACCESS_TOKEN_TTL`；來源 IP 在淡江大學網段時會更短）
/// ——瀏覽器裡 cookie 本身宣告的到期時間，才會跟伺服器端真正認定的到期時間一致。
fn append_session_cookie(
    mut response: Response,
    token: &str,
    headers: &HeaderMap,
    max_age_secs: u64,
) -> Response {
    let name = session_cookie_name(headers);
    let secure = if name == SECURE_SESSION_COOKIE {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{name}={token}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{secure}"
    );
    // The session token is generated hex so this always parses, but never panic
    // on a header in the login path.
    match cookie.parse::<HeaderValue>() {
        Ok(value) => {
            response.headers_mut().append(header::SET_COOKIE, value);
            // SESSION_COOKIE_PRIORITY_V1：種 __Host- 版時，順手把 local 名的
            // 舊 cookie 過期清掉，避免瀏覽器兩顆並存、下次請求又送舊 token。
            //
            // SESSION_COOKIE_SYNC_V1（fix36）：反向也要清。舊版只清了「種
            // __Host- → 清 local」這一個方向；在「這台伺服器判定為 http（例如
            // 反向代理沒帶 X-Forwarded-Proto/CF-Visitor）」但瀏覽器其實持有舊
            // __Host- cookie 的情況下，新 token 一律以 local 名落地，而讀取端
            // 又優先讀 __Host-（fix27 的 SESSION_COOKIE_PRIORITY_V1）→ 每次請求
            // 都拿著過期的 __Host- token ＝「登入成功又被導回登入頁」。兩個方向
            // 都清之後，瀏覽器永遠只會留著最新那一顆。清除 __Host- 的標頭一定
            // 帶 Secure；在真正的 http 連線上瀏覽器會直接忽略（無害）。
            let other = if name == SECURE_SESSION_COOKIE {
                format!("{LOCAL_SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax")
            } else {
                format!(
                    "{SECURE_SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
                )
            };
            if let Ok(expire) = HeaderValue::from_str(&other) {
                response.headers_mut().append(header::SET_COOKIE, expire);
            }
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
///
/// SESSION_COOKIE_SYNC_V1（fix36）：與 access cookie 相同的「兩個名字互清」
/// 規則——舊版連單向清理都沒有，只要歷史上出現過不同名字的 refresh cookie
///（http 期 vs https 期），讀取端永遠優先讀到舊的那一顆，輪換就會以「已消耗
/// 的 refresh token」失敗，前端接著把使用者登出。
/// `max_age_secs`：TKU_SHORT_SESSION_V1 起，呼叫端傳入這次核發時實際算好的壽命
/// （一般情況下就是 `REFRESH_TOKEN_TTL`；來源 IP 在淡江大學網段時會更短）。
fn append_refresh_cookie(
    mut response: Response,
    refresh_token: &str,
    headers: &HeaderMap,
    max_age_secs: u64,
) -> Response {
    let secure = request_uses_https(headers);
    let (name, secure_attr) = if secure {
        (SECURE_REFRESH_COOKIE, "; Secure")
    } else {
        (LOCAL_REFRESH_COOKIE, "")
    };
    let cookie = format!(
        "{name}={refresh_token}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{secure_attr}"
    );
    if let Ok(value) = cookie.parse::<HeaderValue>() {
        response.headers_mut().append(header::SET_COOKIE, value);
        let other = if secure {
            format!("{LOCAL_REFRESH_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax")
        } else {
            format!("{SECURE_REFRESH_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure")
        };
        if let Ok(expire) = HeaderValue::from_str(&other) {
            response.headers_mut().append(header::SET_COOKIE, expire);
        }
    } else {
        tracing::error!("refresh cookie 無法建立");
    }
    response
}

/// Read the refresh token from the request cookies.
///
/// REFRESH_TOKEN_STRICT_FORMAT_V1 (M-2): refresh tokens are minted by
/// `create_session` 自 SESSION_TOKEN_256_V1 起 = 64 hex（256-bit）；舊 UUID 時代 32 hex,
/// byte-for-byte the same shape as the access token. The previous
/// `len() >= 16` test accepted anything longer than that, including multi-
/// kilobyte values and arbitrary non-hex bytes, and passed them on to the DB
/// lookup in `rotate_session` and to any log line that records the token.
/// Validating the exact format here keeps that entire input surface closed.
fn refresh_cookie_from_headers(headers: &HeaderMap) -> Option<String> {
    // SESSION_COOKIE_PRIORITY_V1（fix27）：與 session cookie 相同的優先序問題，
    // 兩輪掃描先 __Host- 版，避免並存的舊 local cookie 搶先被採用。
    for name in [SECURE_REFRESH_COOKIE, LOCAL_REFRESH_COOKIE] {
        if let Some(value) = session_cookie_value_named(headers, name) {
            return Some(value);
        }
    }
    None
}

/// SESSION_DUAL_CANDIDATE_V1（fix36）：兩個名字的 refresh cookie 候選清單。
/// 輪換（refresh token 單用）必須依序試完——只試第一個的話，「過期 __Host-＋
/// 有效 local」會讓輪換必定失敗，前端接著把使用者登出（使用者回報的
/// 「登入成功後又跳回登入畫面」）。
fn refresh_cookie_candidates(headers: &HeaderMap) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in [SECURE_REFRESH_COOKIE, LOCAL_REFRESH_COOKIE] {
        if let Some(value) = session_cookie_value_named(headers, name) {
            if !out.contains(&value) {
                out.push(value);
            }
        }
    }
    out
}

fn append_expired_session_cookies(mut response: Response) -> Response {
    for (name, secure) in [
        (SECURE_SESSION_COOKIE, "; Secure"),
        (LOCAL_SESSION_COOKIE, ""),
        (SECURE_REFRESH_COOKIE, "; Secure"),
        (LOCAL_REFRESH_COOKIE, ""),
    ] {
        let cookie = format!("{name}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}");
        // UNWRAP_SWEEP_V1：字串是固定格式必定合法；仍以 match 收斂，不在 handler 內 panic。
        match cookie.parse::<HeaderValue>() {
            Ok(value) => {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            Err(error) => {
                warn!(cookie = %name, %error, "清除 cookie 的 Set-Cookie 標頭建構失敗，已略過");
            }
        }
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
            | "/admin/api/password"
            | "/admin/api/status"
            | "/admin/api/public_status"
            | "/admin/api/passkey/login/start"
            | "/admin/api/passkey/login/finish"
            | "/admin/api/session/refresh"
    )
}

/// VIEWER_ROLE_V1：檢視者被允許的寫入端點（僅限自助、且不觸及監控／他人資料）。
fn viewer_allowed_write_path(path: &str) -> bool {
    matches!(
        path,
        "/admin/api/login"
            | "/admin/api/logout"
            | "/admin/api/password"
            | "/admin/api/session/refresh"
            | "/admin/api/passkey/login/start"
            | "/admin/api/passkey/login/finish"
            | "/admin/api/passkey/register/start"
            | "/admin/api/passkey/register/finish"
            | "/admin/api/push/subscribe"
            | "/admin/api/push/unsubscribe"
            | "/admin/api/push/rotate"
            | "/admin/api/client-log"
            | "/admin/api/devices/revoke"
            | "/admin/api/bulletins/read"
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
    // VIEWER_ROLE_V1：先把 path／method 複製成 owned 值。`axum::body::Body` 不是 Sync，
    // 若把 `&Request` 借用跨過下面的 `.await`，整個 middleware future 會變成 !Send。
    let path_owned = path.to_string();
    let method = request.method().clone();
    let is_mutating = method != Method::GET && method != Method::HEAD && method != Method::OPTIONS;
    // LOGIN_DEVICES_V1：目前請求來源（owned），供 touch 時回填舊 session 的 ip / UA。
    let origin = {
        let peer = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip());
        let trusted = crate::trusted_ip::trusted_from_global_config();
        let ip = crate::trusted_ip::resolve_client_ip(peer, request.headers(), &trusted);
        SessionOrigin::from_request(request.headers(), &ip)
    };
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
            // VIEWER_ROLE_V1：檢視者唯讀閘門。所有會改變狀態的 HTTP 方法一律 403，
            // 只放行「自己登出／換密碼／裝置管理／session refresh／已讀標記」這類
            // 不影響其他帳號、也不影響監控狀態的自助操作。
            // 放在這個既有的單一 chokepoint，而不是散落到每個 handler：新 handler
            // 忘了檢查 is_viewer 也不會漏。GET 一律放行（handler 內自行遮罩）。
            if session.is_viewer && is_mutating && !viewer_allowed_write_path(&path_owned) {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": "viewer_read_only",
                        "message": "此帳號為檢視者（唯讀），不能執行此操作",
                    })),
                )
                    .into_response();
            }
            // LOGIN_DEVICES_V1：更新 session 最近活動時間（60 秒節流，避免每個
            // 請求都寫 SQLite），並回填舊 session 缺少的來源資訊。
            state.touch_session_last_seen(&token, origin).await;
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
///
/// CSRF_TOKEN_V1：在既有 Origin／Sec-Fetch-Site 比對之上，疊加一層顯式、
/// 不可被第三方猜到的 `X-CSRF-Token` 標頭檢查。
///
/// 只有「這次請求帶著的 cookie 剛好能解析出一個目前仍有效的 session」時才
/// 要求這個標頭——找不到有效 session 的情況（登入、passkey 登入、session 剛
/// 好過期需要刷新……）一律放行，交給下游真正的 handler 用它自己的邏輯回應
/// 「未登入/401」，CSRF 檢查不搶在它前面用一個容易誤導的 403 蓋過去。
/// `/admin/api/logout` 另外在 `csrf::is_exempt_path` 固定豁免：即使 session
/// 有效也不要求 token，確保使用者在任何情況下都能登出（跟
/// `reject_blacklisted_ip_middleware` 對登出端點的一貫處理方式相同）。
///
/// token = HMAC-SHA256(行程密鑰, account_id)——綁在帳號而非單一 access/refresh
/// token 上，背景靜默刷新（`/admin/api/session/refresh` 換發新 access token）
/// 不會讓頁面上已經嵌入的 `data-csrf` 失效，使用者不需要整頁重新整理。
///
/// 範圍刻意只到 `/admin/api/*`，不含 `/admin/stock-api/*`：StockGenie
/// 側車（`stock.rs`）是獨立前端（`stockgenie/static/index.html`），刻意跳過
/// `inject_theme`／`session-security.js` 等整套面板資產（見 `stock_page_html`
/// 與 `STOCK_SECURITY_HEADERS_V1` 的說明），沒有管道能拿到 `data-csrf`；把它
/// 一併納入會直接打斷現有功能。這段既有的 Origin／Sec-Fetch-Site 比對不變，
/// 新增的顯式 token 檢查就不擴及這個子路由。
async fn admin_same_origin_middleware(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = request.method();
    let mutating = method == Method::POST
        || method == Method::PUT
        || method == Method::PATCH
        || method == Method::DELETE;
    let path = request.uri().path();
    let is_admin_api = path.starts_with("/admin/api/");
    let same_origin_api = is_admin_api || path.starts_with("/admin/stock-api/");
    if mutating && same_origin_api {
        if !same_origin_request(&headers) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "cross-origin request rejected" })),
            )
                .into_response();
        }
        if is_admin_api && !csrf::is_exempt_path(path) {
            if let Some(session) = state.authorize_any_from_cookies(&headers).await {
                if !csrf::request_has_valid_token(&headers, &session.account_id) {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "error": "missing or invalid csrf token" })),
                    )
                        .into_response();
                }
            }
        }
    }
    next.run(request).await
}

/// CSP_STYLE_ATTR_STAGED_V1：內聯 style 屬性的策略（見 `AdminWebConfig::csp_style_attr_mode`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CspStyleAttrMode {
    /// 正式 CSP 允許 'unsafe-inline'，另送 Report-Only（預設）。
    Report,
    /// 正式 CSP 直接 'none'。
    Enforce,
    /// 舊行為：允許且不回報。
    Inline,
}

fn csp_style_attr_mode() -> CspStyleAttrMode {
    let raw = crate::config::try_get_config()
        .map(|cfg| cfg.adapters.admin_web.csp_style_attr_mode.trim().to_ascii_lowercase())
        .unwrap_or_default();
    match raw.as_str() {
        "enforce" | "strict" | "none" => CspStyleAttrMode::Enforce,
        "inline" | "off" | "unsafe-inline" => CspStyleAttrMode::Inline,
        _ => CspStyleAttrMode::Report,
    }
}

fn csp_style_attr_allow() -> &'static str {
    if csp_style_attr_mode() == CspStyleAttrMode::Enforce {
        "'none'"
    } else {
        "'unsafe-inline'"
    }
}

/// HTML_CACHE_HASH_CSP_V1：inline `<script>` / `<style>` 的 CSP extra
///（`'sha256-…'` 清單，含前導空白；空字串＝沒有 inline）。
struct InlineCspHashes {
    script: String,
    style: String,
}

fn format_csp(script_extra: &str, style_extra: &str, style_attr: &str) -> String {
    format!(
        "default-src 'self'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'; object-src 'none'; script-src 'self'{script_extra} https://static.cloudflareinsights.com; script-src-attr 'none'; style-src 'self'{style_extra}; style-src-attr {style_attr}; require-trusted-types-for 'script'; trusted-types default dompurify; img-src 'self' data: blob:; connect-src 'self' https://cloudflareinsights.com; font-src 'self' data:; manifest-src 'self'; media-src 'self' blob:; frame-src 'self' https://view.officeapps.live.com https://*.officeapps.live.com https://docs.google.com; worker-src 'self' blob:; block-all-mixed-content; upgrade-insecure-requests"
    )
}

/// JSON／靜態回應的嚴格 CSP（沒有 inline 腳本／樣式）。
fn csp_header_value(nonce: Option<&str>) -> String {
    // 非 HTML 路徑不再發 nonce；參數保留是為了既有 `csp_header_value(None)` 呼叫點。
    let _ = nonce;
    format_csp("", "", csp_style_attr_allow())
}

/// CSP_STYLE_ATTR_STAGED_V1：Report-Only 版本（style-src-attr 'none'），只在 `report` 模式
/// 對 HTML 頁面加送。違規由瀏覽器觸發 `securitypolicyviolation`（disposition=report），
/// client-log.js 會把它回報到 /admin/api/client-log。
fn csp_report_only_header_value(_script_extra: &str, style_extra: &str) -> Option<String> {
    if csp_style_attr_mode() != CspStyleAttrMode::Report {
        return None;
    }
    // CSP_RO_SCRIPT_NO_DUP_V1：正式 CSP 已用 hash 擋未知 inline script。
    // Report-Only 若複製同一組 script-src，Cloudflare 注入的 inline 會被瀏覽器
    // 報兩次（csp_violation + csp_report_only）。RO 只觀察 style-src-attr。
    Some(format_csp(" 'unsafe-inline'", style_extra, "'none'"))
}

/// CSP_STYLE_ATTR_STAGED_V1：把 Report-Only 標頭掛到 HTML 回應（有才掛）。
fn apply_csp_report_only(response: &mut Response, script_extra: &str, style_extra: &str) {
    if let Some(value) = csp_report_only_header_value(script_extra, style_extra) {
        if let Ok(value) = value.parse::<HeaderValue>() {
            response.headers_mut().insert(
                HeaderName::from_static("content-security-policy-report-only"),
                value,
            );
        }
    }
}

fn csp_sha256_token(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    format!("'sha256-{}'", B64_STANDARD.encode(hasher.finalize()))
}

fn html_attr_present(open_tag: &str, name: &str) -> bool {
    let lower = open_tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut rest = lower.as_str();
    while let Some(at) = rest.find(&needle) {
        let ok = at == 0 || rest.as_bytes()[at - 1].is_ascii_whitespace();
        if ok {
            return true;
        }
        rest = &rest[at + needle.len()..];
    }
    false
}

/// HTML_CACHE_HASH_CSP_V1：掃描 inline `<script>`（無 `src`）與 `<style>`，
/// 產出 CSP Level 2 的 `'sha256-…'`（標準 base64）。帶 `src` 的外部腳本由 `'self'` 放行，不雜湊。
fn collect_inline_csp_hashes(html: &str) -> InlineCspHashes {
    let lower = html.to_ascii_lowercase();
    let mut script_hashes: Vec<String> = Vec::new();
    let mut style_hashes: Vec<String> = Vec::new();
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
            break;
        };

        let is_script = lower[tag_at..].starts_with("<script");
        let name_len = if is_script {
            "<script".len()
        } else {
            "<style".len()
        };
        let next_byte = html.as_bytes().get(tag_at + name_len).copied();
        if !next_byte
            .map(|byte| byte.is_ascii_whitespace() || byte == b'>' || byte == b'/')
            .unwrap_or(false)
        {
            cursor = tag_at + name_len;
            continue;
        }
        let Some(relative_end) = lower[tag_at..].find('>') else {
            break;
        };
        let open_end = tag_at + relative_end;
        let open_tag = &html[tag_at..=open_end];
        let close_needle = if is_script { "</script>" } else { "</style>" };
        let Some(relative_close) = lower[open_end + 1..].find(close_needle) else {
            cursor = open_end + 1;
            continue;
        };
        let body_start = open_end + 1;
        let body_end = open_end + 1 + relative_close;
        let body = &html[body_start..body_end];
        cursor = body_end + close_needle.len();

        if is_script {
            if html_attr_present(open_tag, "src") {
                continue;
            }
            let open_lower = open_tag.to_ascii_lowercase();
            if open_lower.contains("application/json") || open_lower.contains("ld+json") {
                continue;
            }
        }
        let token = csp_sha256_token(body);
        let dest = if is_script {
            &mut script_hashes
        } else {
            &mut style_hashes
        };
        if !dest.iter().any(|existing| existing == &token) {
            dest.push(token);
        }
    }

    InlineCspHashes {
        script: script_hashes
            .into_iter()
            .map(|token| format!(" {token}"))
            .collect(),
        style: style_hashes
            .into_iter()
            .map(|token| format!(" {token}"))
            .collect(),
    }
}

/// Security headers for the externally reachable Admin Web. Inline `<script>` /
/// `<style>` are allowed by content hashes in `html_page_response` (HTML_CACHE_HASH_CSP_V1);
/// inline event attributes stay blocked (`script-src-attr 'none'`).
/// TRONCLASS_PROXY_PASSTHROUGH_V1（fix29）：頂層代理路由的路徑判定。
/// 這些前綴由 tc_proxy 轉發 iClass，回應不得套用面板的安全標頭/CSP。
fn is_tc_proxy_path(path: &str) -> bool {
    path == "/tc"
        || path.starts_with("/tc/")
        || path == "/static"
        || path.starts_with("/static/")
        || path == "/api"
        || path.starts_with("/api/")
        || path == "/course"
        || path.starts_with("/course/")
        || path == "/uploads"
        || path.starts_with("/uploads/")
        // TC_BULLETIN_LIST_ROUTE_V1（fix36）：iClass 課程公告頁（站內導覽／F5 會以
        // 根絕對路徑打進來）。同樣必須走 passthrough，否則會被套上
        // require-trusted-types-for 而讓 iClass 頁面整頁拋 TypeError。
        || path == "/bulletin-list"
        || path.starts_with("/bulletin-list/")
}

/// STOCKGENIE_EMBED_V1：智選頁用 innerHTML，不能套 require-trusted-types-for。
/// 與 iClass 代理一樣跳過面板 CSP；頁面 handler 自行送較鬆的 CSP。
fn is_stock_embed_path(path: &str) -> bool {
    path == "/admin/stock"
        || path == "/admin/stock-api"
        || path.starts_with("/admin/stock-api/")
}

/// PERMISSIONS_POLICY_PATH_V1：強大瀏覽器 API 預設關閉。
///
/// 舊值 `camera=(self), geolocation=()` 有兩頭不對：
/// - `camera=(self)` 掛在整份面板上（含 dashboard），但相機只給公開 `/scanner`
///   （那條路由不走這個 middleware）。
/// - `geolocation=()` 連同源都禁，管理員雷達頁（`/admin/radar`、`/admin/radar/input`）
///   的 `navigator.geolocation.getCurrentPosition` 會被瀏覽器直接拒絕。
///
/// 公開掃碼／雷達頁由 `scanner::tool_security_headers_middleware` 另外套。
/// 不關 `publickey-credentials-get`／clipboard，passkey 與複製按鈕才不會壞。
fn permissions_policy_for_path(path: &str) -> &'static str {
    const DENY: &str = "camera=(), geolocation=(), microphone=(), payment=(), usb=(), midi=(), bluetooth=(), display-capture=(), accelerometer=(), gyroscope=(), magnetometer=(), ambient-light-sensor=(), autoplay=(), encrypted-media=(), picture-in-picture=(), screen-wake-lock=(), web-share=(), xr-spatial-tracking=(), publickey-credentials-get=(self)";
    const RADAR: &str = "camera=(), geolocation=(self), microphone=(), payment=(), usb=(), midi=(), bluetooth=(), display-capture=(), accelerometer=(), gyroscope=(), magnetometer=(), ambient-light-sensor=(), autoplay=(), encrypted-media=(), picture-in-picture=(), screen-wake-lock=(), web-share=(), xr-spatial-tracking=(), publickey-credentials-get=(self)";
    if path == "/admin/radar" || path.starts_with("/admin/radar/") {
        RADAR
    } else {
        DENY
    }
}

async fn admin_security_headers_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // FAST_PAGE_LOAD_V1: remember path before moving the request so versioned
    // /admin/static assets can keep their Cache-Control instead of no-store.
    // PERFORMANCE_PATH_CHECK_V1: use &str directly, no allocation needed.
    let path = request.uri().path();
    // TRONCLASS_PROXY_PASSTHROUGH_V1（fix29）：頂層代理路由（/tc、/static、/api、
    // /course、/uploads）回傳的是 iClass 的原始頁面與資源。iClass 自己的 JS 沒有
    // Trusted Types policy，若上游回應沒帶 CSP，下面的 fallback 會給 iClass 的
    // HTML 裝上面板專用的 require-trusted-types-for —— iClass 頁面內所有
    // innerHTML 會整頁拋 TypeError（與 2026-09-14 /admin/homework 回報的
    // TrustedHTML 錯誤同型）。代理回應在 tc_proxy 內已做上游 header 白名單，
    // 這裡直接原樣放行，不加任何面板安全標頭。
    if is_tc_proxy_path(path) || is_stock_embed_path(path) {
        return next.run(request).await;
    }
    let is_admin_path = path.starts_with("/admin")
        || path == "/manifest.webmanifest"
        || path == "/service-worker.js";
    let is_root_path = path == "/";
    let cache_static = path.starts_with("/admin/static/") || path.starts_with("/admin/pwa/");
    // PDF_FRAME_PREVIEW_V1: 預覽端點的回應會被 Dashboard 自己的 <iframe> 內嵌
    // （PDF 走瀏覽器原生檢視器）。若沿用全站 `X-Frame-Options: DENY` +
    // `frame-ancestors 'none'`，瀏覽器會拒絕在框內顯示這份 PDF，預覽只剩空白。
    // 這裡只放寬到 same-origin：跨站仍然無法內嵌我們的預覽回應。
    // HOMEWORK_FILE_V1（fix43）：作業附件／繳交檔的預覽也是 <iframe> 內嵌，
    // 同樣只放寬到 same-origin（跨站仍無法內嵌）。
    let is_framable_preview =
        path == "/admin/api/files/preview" || path == "/admin/api/homework/file";
    // SHARE_V1c: /pub/ 是公開分享/串流端點，需被「跨域」檢視器（Microsoft Office
    // Online、外部 <img>/<iframe>）載入。若沿用 same-origin 的 CORP 與 X-Frame-Options
    // DENY，跨域檢視器會直接拒絕載入 → Office Online 顯示 "An error occurred"。
    let is_pub = path.starts_with("/pub/");
    // PREVIEW_REVALIDATE_V1（fix52）：檔案預覽端點自己帶 Cache-Control 與 ETag
    // （`private, max-age=0, must-revalidate`），不能被下面的全站 `no-store` 覆寫：
    // 一被覆寫，瀏覽器不留存內容、也不會送 `If-None-Match`，重開同一份教材就只能
    // 再整份下載一次。（作業附件端點自己仍送 no-store，不受影響。）
    //
    // BULLETIN_ATTACHMENT_CACHE_V1（fix61）：公告附件端點同一套理由——它每次都要先問
    // iClass 拿簽章 URL、再從 MinIO 串流，重看同一份附件原本等於整條路重跑。
    let keeps_own_cache =
        is_framable_preview || path == "/admin/api/bulletins/attachment";
    // PERMISSIONS_POLICY_PATH_V1：先取出 'static 政策字串，再 move request。
    let permissions_policy = permissions_policy_for_path(path);
    let mut response = next.run(request).await;

    // This middleware covers the whole Admin Web router, including `/` and
    // other redirect/error responses. Security headers must not depend on the
    // final status code being 200 or the path starting with `/admin`.
    //
    // Compute this before taking the mutable header borrow: reading `response`
    // afterwards would conflict with the `headers` borrow held below.
    let html_document = is_html_document(&response);
    // HTML_CACHE_HASH_CSP_V1：304 沒有 Content-Type，不能靠 is_html_document；
    // 若當成非 HTML 會被寫成 `no-store`，瀏覽器下次就不帶 If-None-Match。
    let not_modified = response.status() == StatusCode::NOT_MODIFIED;

    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    // SHARE_V1c: /pub/ 需可被跨域載入 → 不送 X-Frame-Options DENY。
    if !is_pub {
        headers.insert(
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static(if is_framable_preview {
                "SAMEORIGIN"
            } else {
                "DENY"
            }),
        );
    }
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static(if is_root_path {
            "strict-origin-when-cross-origin"
        } else {
            "no-referrer"
        }),
    );
    headers.insert(
        HeaderName::from_static("x-xss-protection"),
        HeaderValue::from_static("1; mode=block"),
    );
    headers.insert(
        HeaderName::from_static("x-permitted-cross-domain-policies"),
        HeaderValue::from_static("none"),
    );
    headers.insert(
        HeaderName::from_static("origin-agent-cluster"),
        HeaderValue::from_static("?1"),
    );
    headers.insert(
        HeaderName::from_static("x-dns-prefetch-control"),
        HeaderValue::from_static("off"),
    );
    headers.insert(
        header::SERVER,
        HeaderValue::from_static("jasonserver-security-core (shield-v5-max)"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static(permissions_policy),
    );
    if !is_pub {
        headers.insert(
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        );
        headers.insert(
            HeaderName::from_static("cross-origin-embedder-policy"),
            HeaderValue::from_static("credentialless"),
        );
    }
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static(if is_pub { "cross-origin" } else { "same-origin" }),
    );
    headers.insert(
        HeaderName::from_static("strict-transport-security"),
        HeaderValue::from_static(HSTS_HEADER_VALUE),
    );
    // Page handlers install a hash-bearing CSP. Use the strict no-inline
    // fallback only for JSON/static responses that do not contain inline code.
    if !headers.contains_key(header::CONTENT_SECURITY_POLICY) {
        // PDF_FRAME_PREVIEW_V1: 預覽回應需要能被同源 <iframe> 內嵌，把
        // frame-ancestors 從 'none' 放寬為 'self'（跨站仍被擋）。
        let csp = if is_framable_preview {
            csp_header_value(None).replace("frame-ancestors 'none'", "frame-ancestors 'self'")
        } else {
            csp_header_value(None)
        };
        if let Ok(value) = csp.parse::<HeaderValue>() {
            headers.insert(header::CONTENT_SECURITY_POLICY, value);
        }
    }
    if is_admin_path && !cache_static && !keeps_own_cache {
        // Admin HTML must always be revalidated (it is behind auth and may
        // change on redeploy), but `no-store` also forbade conditional
        // requests, so every navigation re-downloaded the full ~260 KB
        // dashboard. `no-cache` still revalidates on each load while allowing
        // the ETag below to answer with an empty 304.
        if html_document || not_modified {
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
///
/// HTML_CACHE_HASH_CSP_V1：inline 腳本改以 sha256 雜湊放行，HTML 本體不再插入
/// 每次不同的 nonce，所以同一份 HTML 的 ETag 穩定，If-None-Match 才能 304。
/// 隨機 nonce 與 304 不能共存（快取的 body nonce ≠ 新回應的 CSP nonce）。
fn html_page_response(headers: &HeaderMap, html: String) -> Response {
    // ASSET_VERSION_AUTO_V1：先統一改寫 /admin/static/... 版號（?sv=<build id>），
    // 再算雜湊 CSP 與 ETag——ETag 自然隨資產內容變動。
    let html = asset_versions::rewrite_asset_versions(&html).into_owned();
    let hashes = collect_inline_csp_hashes(&html);
    let csp = format_csp(&hashes.script, &hashes.style, csp_style_attr_allow());

    let mut hasher = Sha256::new();
    hasher.update(html.as_bytes());
    let etag = format!("\"{}\"", hex::encode(&hasher.finalize()[..16]));

    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|c| c.trim() == etag));

    const HTML_CACHE_CONTROL: &str = "private, no-cache, must-revalidate";

    if matches {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        if let Ok(value) = etag.parse::<HeaderValue>() {
            response.headers_mut().insert(header::ETAG, value);
        }
        if let Ok(value) = csp.parse::<HeaderValue>() {
            response
                .headers_mut()
                .insert(header::CONTENT_SECURITY_POLICY, value);
        }
        apply_csp_report_only(&mut response, &hashes.script, &hashes.style);
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(HTML_CACHE_CONTROL),
        );
        return response;
    }

    let mut response = Html(html).into_response();
    if let Ok(value) = etag.parse::<HeaderValue>() {
        response.headers_mut().insert(header::ETAG, value);
    }
    if let Ok(value) = csp.parse::<HeaderValue>() {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    apply_csp_report_only(&mut response, &hashes.script, &hashes.style);
    // Admin HTML 含帳號／session 狀態：private + 每次必須再驗證（304 可省 body）。
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(HTML_CACHE_CONTROL),
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
    /// ADMIN_PAGE_TRI_STATE_V1：`/admin?next=/admin/xxx`——已登入者直接被送去 next
    /// （登入頁則在登入成功後由前端 finishLogin 讀同一個參數）。只接受同源 /admin 路徑。
    #[serde(default)]
    next: Option<String>,
}

fn normalize_admin_ip(value: &str) -> Option<String> {
    // IP_BLACKLIST_V6_GROUP_V1：統一走 trusted_ip 的正規化——IPv4 原樣、
    // `::ffff:a.b.c.d` 收斂成 IPv4、IPv6 收斂為 /64 群組鍵；也接受既有清單
    // 裡的 "xxxx/64" 形式（解除封鎖時前端原樣送回）。
    crate::trusted_ip::normalize_blacklist_key(value)
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
        tokens.access_max_age,
    );
    // Refresh cookie is always set alongside the access cookie (bootstrap too,
    // so the mandatory password-change flow can keep refreshing).
    let response = append_refresh_cookie(
        response,
        &tokens.refresh,
        request_headers,
        tokens.refresh_max_age,
    );

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
    /// ADD_ACCOUNT_ROLE_V1：新增帳號時的角色 "admin" / "user" / "viewer"；
    /// 有給時優先於 is_admin。
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    mock_kind: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    poll_interval_secs: Option<Option<u64>>,
    #[serde(default)]
    group_submit_interval_ms: Option<u64>,
    /// GROUP_SUBMIT_RELAY_V1（fix67）：送出間隔的隨機上限（毫秒）。
    #[serde(default)]
    group_submit_jitter_ms: Option<u64>,
    /// QR_FRESH_PAYLOAD_V1（fix68）：QR Code 點名的送出間隔基準（毫秒）。
    #[serde(default)]
    group_submit_qrcode_interval_ms: Option<u64>,
    /// QR_FRESH_PAYLOAD_V1（fix68）：QR Code 點名的送出間隔隨機上限（毫秒）。
    #[serde(default)]
    group_submit_qrcode_jitter_ms: Option<u64>,
    /// GROUP_SUBMIT_RELAY_V1（fix67）：雷達點名的送出間隔基準（毫秒）。
    #[serde(default)]
    group_submit_radar_interval_ms: Option<u64>,
    /// GROUP_SUBMIT_RELAY_V1（fix67）：雷達點名送出間隔的隨機上限（毫秒）。
    #[serde(default)]
    group_submit_radar_jitter_ms: Option<u64>,
    #[serde(default)]
    auto_stop_time: Option<Option<String>>,
    #[serde(default)]
    rest_periods: Option<Vec<RuntimeRestPeriod>>,
    /// CURRENT_SEMESTER_OVERRIDE_V1：Some("1151") 覆蓋學期、Some("") 清除、None 不變。
    #[serde(default)]
    semester_override: Option<String>,
    /// MORNING_DIGEST_V1：晨間摘要設定（None 不變）。
    #[serde(default)]
    digest_enabled: Option<bool>,
    #[serde(default)]
    digest_time: Option<String>,
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

/// BLACKLIST_UX_V1（fix58）：`user_agent` ＝ 這次請求的 User-Agent（呼叫端用
/// `SessionOrigin::from_request(&headers, &ip).user_agent` 取；拿不到就傳空字串）。
/// 黑名單頁的「裝置」欄就是靠它——瀏覽器不會提供 MAC 位址，這是真正拿得到的裝置資訊。
async fn record_persistent_login_event(
    state: &AdminWebState,
    actor: &str,
    account_id: Option<&str>,
    action: &str,
    result: &str,
    details: &str,
    ip: &str,
    user_agent: &str,
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
            user_agent: user_agent.trim().to_string(),
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

/// SESSION_TOKEN_256_V1（fix128）：採 256-bit 作業系統亂數產生 session／導航
/// token（64 個十六進位字元）。統一由這支函式鑄造全部高價值 token。
/// 舊版 UUID v4（112-bit）格式仍被接受，新鑄造一律走此路。
fn mint_secure_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("SESSION_TOKEN_256_V1: OS RNG unavailable");
    hex::encode(buf)
}

/// Session token 格式驗證：SESSION_TOKEN_256_V1 起新 token = 64 個十六進位
/// 字元（256-bit）；舊 UUID v4 時代的 32 字元（112-bit）格式維持相容（既有
/// session 不會因升級被踢出）。任何在此之外的形狀（長度、字元集）都是偽造，
/// 在進 DB 查詢或寫 log 前一律擋下。
///
/// REFRESH_TOKEN_STRICT_FORMAT_V1 (M-2): this is the single definition shared
/// by `session_cookie_from_headers`, `refresh_cookie_from_headers` and
/// `session_token`, so the access and refresh paths cannot drift apart again.
fn is_valid_session_token_format(value: &str) -> bool {
    (value.len() == 32 || value.len() == 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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


