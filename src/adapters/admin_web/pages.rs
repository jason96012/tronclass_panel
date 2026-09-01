//! HTML templates — Tronclass Admin Web UI.

use std::sync::RwLock;

// Page handlers below reuse the shared auth/cookie/response helpers from the
// parent module, so pull in the parent namespace (same pattern as files.rs).
use super::*;

static RUNTIME_THEME: RwLock<Option<String>> = RwLock::new(None);

pub(super) fn load_runtime_theme() {
    let paths = [
        "src/adapters/admin_web/static/theme.css",
        "theme.css",
        "static/theme.css",
    ];
    for p in &paths {
        if let Ok(css) = std::fs::read_to_string(p) {
            if !css.trim().is_empty() {
                *RUNTIME_THEME.write().unwrap() = Some(css);
                tracing::info!(path = %p, "runtime theme.css loaded");
                return;
            }
        }
    }
}

pub(super) fn theme_css() -> String {
    if let Some(ref css) = *RUNTIME_THEME.read().unwrap() {
        return css.clone();
    }
    include_str!("static/theme.css").to_string()
}

pub(super) const DOMPURIFY_JS: &str = include_str!("static/js/dompurify.min.js");
pub(super) const TRUSTED_TYPES_INIT_JS: &str = include_str!("static/js/trusted-types-init.js");
pub(super) const JSZIP_MIN_JS: &str = include_str!("static/js/jszip.min.js");
pub(super) const OFFICE_VIEW_JS: &str = include_str!("static/js/office-view.js");
pub(super) const QR_CODE_MIN_JS: &str = include_str!("static/js/qrcode.min.js");
// SHARE_ADMIN_V1: 分享連結管理頁（管理員專用）與其前端。
pub(super) const SHARES_HTML: &str = include_str!("static/shares.html");
pub(super) const SHARES_ADMIN_JS: &str = include_str!("static/js/shares-admin.js");
// SHARE_V1: 免登入分享預覽頁的外掛 JS/CSS（符合 CSP，不用 inline）。
pub(super) const SHARE_PREVIEW_JS: &str = include_str!("static/js/share-preview.js");
pub(super) const SHARE_CSS: &str = include_str!("static/css/share.css");
// PDFJS_PREVIEW_V1: 套用 Firefox 同款 PDF.js 檢視器（純 JS canvas 渲染，
// 不靠瀏覽器原生外掛，所有瀏覽器一致，解決 iframe 空白）。
pub(super) const PDF_MIN_JS: &str = include_str!("static/js/pdf.min.js");
pub(super) const PDF_WORKER_MIN_JS: &str = include_str!("static/js/pdf.worker.min.js");
pub(super) const SESSION_SECURITY_JS: &str = include_str!("static/js/session-security.js");
pub(super) const CORE_API_JS: &str = include_str!("static/js/core-api.js");
pub(super) const WEB_PUSH_JS: &str = include_str!("static/js/web-push.js");
pub(super) const ACCOUNTS_MODULE_JS: &str = include_str!("static/js/accounts.js");
pub(super) const MOCK_ROLLCALL_MODULE_JS: &str = include_str!("static/js/mock-rollcall.js");
pub(super) const TIMETABLE_UI_MODULE_JS: &str = include_str!("static/js/timetable.js");
pub(super) const MAINTENANCE_IMPLEMENTATION_JS: &str =
    include_str!("static/js/maintenance-implementation.js");
pub(super) const MAINTENANCE_MODULE_JS: &str = include_str!("static/js/maintenance.js");
// MAINTENANCE_COMPONENTS_CSS_PHASE3B4
pub(super) const MAINTENANCE_COMPONENTS_CSS: &str =
    include_str!("static/css/maintenance-components.css");
// ACCOUNT_COMPONENTS_CSS_PHASE3B3
pub(super) const ACCOUNT_COMPONENTS_CSS: &str = include_str!("static/css/account-components.css");
// MOBILE_TIMETABLE_CSS_PHASE3B2
pub(super) const MOBILE_TIMETABLE_CSS: &str = include_str!("static/css/mobile-timetable.css");
// MOBILE_NAVIGATION_CSS_PHASE3B1
pub(super) const MOBILE_NAVIGATION_CSS: &str = include_str!("static/css/mobile-navigation.css");
// DASHBOARD_MODULE_LOADER_PHASE1
pub(super) const DASHBOARD_MODULE_CSS: &str = include_str!("static/css/dashboard.css");
// DASHBOARD_LATE_FIXES_CSS_PHASE1_V1
pub(super) const DASHBOARD_LATE_FIXES_CSS: &str =
    include_str!("static/css/dashboard-late-fixes.css");
// DASHBOARD_CSS_CLEANUP_PHASE2
pub(super) const DASHBOARD_MOBILE_CSS: &str = include_str!("static/css/dashboard-mobile.css");
pub(super) const DASHBOARD_COMPONENTS_CSS: &str =
    include_str!("static/css/dashboard-components.css");
pub(super) const DASHBOARD_ACCESS_CSS: &str = include_str!("static/css/dashboard-access.css");
pub(super) const DASHBOARD_RUNTIME_JS: &str = include_str!("static/js/dashboard-runtime.js");
pub(super) const DASHBOARD_FEATURES_JS: &str = include_str!("static/js/dashboard-modules.js");

pub(super) const ADMIN_LOGS_HTML: &str = include_str!("static/logs.html");
pub(super) const ADMIN_HEALTH_HTML: &str = include_str!("static/health.html");
pub(super) const ADMIN_DASHBOARD_HTML: &str = include_str!("static/dashboard.html");
pub(super) const ADMIN_LOGIN_HTML: &str = include_str!("static/login.html");
pub(super) const ADMIN_ACTIVITY_HTML: &str = include_str!("static/activity.html");
pub(super) const ADMIN_PASSWORD_HTML: &str = include_str!("static/password.html");
pub(super) const ADMIN_PASSKEY_HTML: &str = include_str!("static/passkey.html");
pub(super) const ADMIN_BLACKLIST_HTML: &str = include_str!("static/blacklist.html");
pub(super) const ADMIN_BLOCKED_HTML: &str = include_str!("static/blocked.html");
pub(super) const ADMIN_HOMEWORK_HTML: &str = include_str!("static/homework.html");
pub(super) const ADMIN_FILES_HTML: &str = include_str!("static/files.html");
pub(super) const ADMIN_SETTINGS_HTML: &str = include_str!("static/settings.html");
pub(super) const ADMIN_NOTIFICATIONS_HTML: &str = include_str!("static/notifications.html");
pub(super) const APP_SETTINGS_JS: &str = include_str!("static/js/app-settings.js");
pub(super) const NOTIFICATIONS_PAGE_JS: &str = include_str!("static/js/notifications-page.js");

/// Add shared frontend assets without copying about 50 KiB of CSS/JavaScript
/// into every HTML response. The security script intentionally remains
/// parser-blocking: it must install its Web Storage guards before legacy body
/// scripts execute. Shared URLs are browser-cacheable by their route handlers.
/// Build-time asset version. Static assets are served with
/// `immutable, max-age=604800`, so a stale `?v=` means the browser will not
/// even revalidate for a week -- edited JS/CSS silently keeps running the old
/// copy. Stamping it from the build makes every rebuild bust the cache
/// automatically instead of relying on hand-edited version strings.
pub(super) fn asset_version() -> &'static str {
    option_env!("TRONCLASS_BUILD_STAMP").unwrap_or("dev")
}

fn stamp_asset_urls(html: &str) -> String {
    // Rewrite every ?v=<token> that belongs to an /admin/static/ URL.
    let stamp: String = asset_version()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if stamp.is_empty() {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("?v=") {
        out.push_str(&rest[..pos]);
        out.push_str("?v=");
        let after = &rest[pos + 3..];
        let end = after
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(after.len());
        let token = &after[..end];
        // Leave functional markers such as ?v=auth / ?v=logout alone.
        if token.starts_with("2026") {
            out.push_str(&stamp);
        } else {
            out.push_str(token);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

pub(super) fn inject_theme(html: &str) -> String {
    // FAST_PAGE_LOAD_V1: preload + put CSS at the start of <head> so parsing
    // the large inline dashboard scripts does not delay theme.css.
    //
    // APP_SETTINGS_BOOT_V1: app-settings.js 排在最前面、先於 theme.css 執行，
    // 在 body 解析前把 data-theme / data-accent / data-fontscale / data-motion
    // 掛到 <html>，使用者選的主題不會在載入時閃爍（FOUC）。
    // 外部 <script src> 為同源 'self'，符合 CSP script-src 'self'。
    //
    // APP_SETTINGS_DYNAMIC_CSS_V1: 依 <html data-app-settings> 屬性套用的外觀覆蓋。
    // 排在 theme.css link 之後：兩者皆為 !important 時，同 specificity 由順序決勝；
    // accent 規則再用雙屬性選擇器拉高 specificity，穩定壓過 theme.css 的
    // :root / html[data-theme] 區塊與其手機 media-query 覆蓋。
    // （此 inline <style> 由 html_page_response() 統一補上 CSP nonce。）
    // 注意：concat!() 只接受字面值（literal），不能展開 const，因此全部內聯在此。
    const SHARED_ASSETS: &str = concat!(
        r#"<script src="/admin/static/app-settings.js?v=20260829settings9"></script>"#,
        r#"<link rel="preload" href="/admin/static/theme.css?v=20260821fast" as="style">"#,
        r#"<link rel="preload" href="/admin/static/js/dompurify.min.js?v=20260826dompurify3414" as="script">"#,
        r#"<script src="/admin/static/js/dompurify.min.js?v=20260826dompurify3414"></script>"#,
        r#"<script src="/admin/static/js/trusted-types-init.js?v=20260823trustedtypes"></script>"#,
        r#"<link rel="preload" href="/admin/static/session-security.js?v=20260829nopwbtn" as="script">"#,
        r#"<script id="sessionSecurityHardeningV1" src="/admin/static/session-security.js?v=20260829nopwbtn"></script>"#,
        r#"<link rel="stylesheet" href="/admin/static/theme.css?v=20260821fast">"#,
        r#"<script defer src="/admin/static/web-push.js?v=20260825push"></script>"#,
        r#"<style id="appSettingsDynamicCss">
html[data-app-settings][data-theme="dark"]{color-scheme:dark!important}
html[data-app-settings][data-theme="light"]{color-scheme:light!important}
html[data-app-settings][data-accent="blue"]{--primary:#2563EB!important}
html[data-app-settings][data-accent="indigo"]{--primary:#6366F1!important}
html[data-app-settings][data-accent="green"]{--primary:#059669!important}
html[data-app-settings][data-accent="amber"]{--primary:#D97706!important}
html[data-app-settings][data-accent="purple"]{--primary:#7C3AED!important}
html[data-app-settings][data-accent="rose"]{--primary:#E11D48!important}
html[data-app-settings][data-accent="cyan"]{--primary:#0E7490!important}
html[data-app-settings][data-theme="dark"][data-accent="blue"]{--primary:#5B9BFF!important}
html[data-app-settings][data-theme="dark"][data-accent="indigo"]{--primary:#818CF8!important}
html[data-app-settings][data-theme="dark"][data-accent="green"]{--primary:#34D399!important}
html[data-app-settings][data-theme="dark"][data-accent="amber"]{--primary:#FBBF24!important}
html[data-app-settings][data-theme="dark"][data-accent="purple"]{--primary:#A78BFA!important}
html[data-app-settings][data-theme="dark"][data-accent="rose"]{--primary:#FB7185!important}
html[data-app-settings][data-theme="dark"][data-accent="cyan"]{--primary:#22D3EE!important}
html[data-app-settings][data-fontscale="lg"] body{zoom:1.08}
html[data-app-settings][data-fontscale="xl"] body{zoom:1.16}
html[data-app-settings][data-motion="reduced"] *,html[data-app-settings][data-motion="reduced"] *::before,html[data-app-settings][data-motion="reduced"] *::after{animation-duration:.001ms!important;animation-iteration-count:1!important;transition-duration:.001ms!important;scroll-behavior:auto!important}
.app-settings-tab.active{background:var(--card)!important;color:var(--text)!important;border-color:var(--line)!important}
/* 使用者要求:隱藏 Dashboard 右上角的重新整理圖示(#refreshAll)。
   用 CSS 隱藏而非刪除 markup:dashboard 內聯 script 有未加防護的
   q('refreshAll').onclick=...,刪除元素會讓整段腳本拋錯掛掉。 */
#refreshAll{display:none!important}
/* 保險:若瀏覽器/service worker 仍快取舊版 app-settings.js 並把「設定」
   連結塞進 header,此規則(每次導頁都隨 no-cache HTML 刷新)保證將其隱藏。 */
#appSettingsGearLink,.app-settings-gear{display:none!important}
</style>"#
    );
    let assets = SHARED_ASSETS.to_string();
    if let Some(head_at) = html.find("<head") {
        if let Some(rel) = html[head_at..].find('>') {
            let at = head_at + rel + 1;
            let mut out = String::with_capacity(html.len() + assets.len());
            out.push_str(&html[..at]);
            out.push_str(&assets);
            out.push_str(&html[at..]);
            return stamp_asset_urls(&out);
        }
    }
    stamp_asset_urls(&html.replacen("</head>", &(assets + "</head>"), 1))
}
pub(super) async fn admin_dashboard_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        // `/admin` remains the public login shell, but `/admin/dashboard` must
        // never return a 200 login SPA to an unauthenticated caller.
        return admin_login_redirect("/admin/dashboard");
    };
    if session.must_change_password {
        return must_change_password_redirect();
    }
    // ETag-aware: repeat navigations get an empty 304 instead of ~260 KB.
    html_page_response(&headers, inject_theme(ADMIN_DASHBOARD_HTML))
}

pub(super) async fn admin_activity_page_handler(headers: HeaderMap) -> Response {
    html_page_response(&headers, inject_theme(ADMIN_ACTIVITY_HTML))
}

pub(super) async fn admin_homework_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return admin_login_redirect("/admin/homework");
    };
    if session.must_change_password {
        return must_change_password_redirect();
    }
    html_page_response(&headers, inject_theme(ADMIN_HOMEWORK_HTML))
}

// FILES_NATIVE_V1
// 廢除 FileBrowser：/admin/files 改為直接服務同源的「教材雲端硬碟」頁
// （static/files.html），不再重導向到外部 files 主機。教材存取一律走
// /admin/api/files/* 的原有端點，因此網路與 Dashboard 同源、不需跨 app。
pub(super) async fn admin_files_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return admin_login_redirect("/admin/files");
    };
    if session.must_change_password {
        return must_change_password_redirect();
    }
    let html = inject_theme(ADMIN_FILES_HTML);
    html_page_response(&headers, inject_files_office_assets(&html))
}

// SHARE_ADMIN_V1: 分享連結管理頁（僅管理員）。列出所有已發連結、可撤銷。
pub(super) async fn admin_shares_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return admin_login_redirect("/admin/shares");
    };
    if !session.is_admin {
        return (StatusCode::FORBIDDEN, "僅管理員可存取").into_response();
    }
    if session.must_change_password {
        return must_change_password_redirect();
    }
    html_page_response(&headers, SHARES_HTML.to_string())
}

// PREVIEW_ENGINES_V1: 只在「雲端硬碟」頁注入 PDF.js（同源 'self'，合法於 CSP），
// 不要無差別注入到每個 admin 頁面。
// 預覽引擎：PDF/Office(轉PDF後) → PDF.js；Office 未轉時 → Microsoft Office Online。
pub(super) fn inject_files_office_assets(html: &str) -> String {
    const OFFICE_ASSETS: &str =
        // 版本號必須隨內容更新：static_assets.rs 對靜態檔下
        // `max-age=604800, immutable`，不換 query string 瀏覽器會沿用舊檔一週。
        r#"<script src="/admin/static/js/jszip.min.js?v=officeview6"></script><script src="/admin/static/js/office-view.js?v=officeview10"></script><script src="/admin/static/js/pdf.min.js?v=pdfjs1"></script><script src="/admin/static/js/qrcode.min.js?v=qrshare1"></script>"#;
    // OFFICE_INJECT_GUARD_V1: 防重複注入必須用「真正的 <script src> 標籤」，
    // 不能用裸檔名子字串（files.html 註解也會提到檔名，會讓 guard 永遠成立）。
    if html.contains(r#"src="/admin/static/js/jszip.min.js"#) {
        return html.to_string();
    }
    if let Some(head_at) = html.find("</head>") {
        let mut out = String::with_capacity(html.len() + OFFICE_ASSETS.len());
        out.push_str(&html[..head_at]);
        out.push_str(OFFICE_ASSETS);
        out.push_str(&html[head_at..]);
        out
    } else {
        html.replacen("<head>", &format!("<head>{OFFICE_ASSETS}"), 1)
    }
}

const ADMIN_CLEAN_TIMETABLE_HTML: &str = include_str!("static/timetable.html");

pub(super) async fn admin_clean_timetable_page_handler(headers: HeaderMap) -> Response {
    html_page_response(&headers, inject_theme(ADMIN_CLEAN_TIMETABLE_HTML))
}

pub(super) async fn admin_root_redirect_handler(uri: axum::http::Uri) -> Response {
    // TOKEN_QUERY_STRIP_ALL_ADMIN_V1 (H-2): `/` is outside the scrubbing
    // middleware's `/admin` scope, so forwarding the query verbatim would
    // bounce an attacker-supplied `?token=` straight into the address bar of
    // `/admin`. Drop `token` (and the transient `logout` marker is preserved,
    // since the login shell reads it) while forwarding everything else.
    let loc = match uri.query() {
        Some(q) if !q.is_empty() => {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            let mut kept = 0usize;
            for (name, value) in url::form_urlencoded::parse(q.as_bytes()) {
                if name == "token" {
                    continue;
                }
                serializer.append_pair(&name, &value);
                kept += 1;
            }
            if kept == 0 {
                "/admin".to_string()
            } else {
                format!("/admin?{}", serializer.finish())
            }
        }
        _ => "/admin".to_string(),
    };
    (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, loc)]).into_response()
}

pub(super) async fn admin_password_page_handler(headers: HeaderMap) -> Response {
    html_page_response(&headers, inject_theme(ADMIN_PASSWORD_HTML))
}

/// /admin/notifications — 通知設定獨立頁（NOTIFICATIONS_STANDALONE_PAGE_V1）。
/// 原 Dashboard「通知」分頁移除，通知偏好/上課提醒/提示音集中於此頁。
pub(super) async fn admin_notifications_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return admin_login_redirect("/admin/notifications");
    };
    if session.must_change_password {
        return must_change_password_redirect();
    }
    html_page_response(&headers, inject_theme(ADMIN_NOTIFICATIONS_HTML))
}

/// /admin/settings — 外觀與本機偏好設定頁。
/// 比照 dashboard：伺服器端先驗證 session，未登入導回登入頁並帶 next 路徑。
pub(super) async fn admin_settings_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return admin_login_redirect("/admin/settings");
    };
    if session.must_change_password {
        return must_change_password_redirect();
    }
    html_page_response(&headers, inject_theme(ADMIN_SETTINGS_HTML))
}

pub(super) async fn admin_passkey_page_handler(headers: HeaderMap) -> Response {
    html_page_response(&headers, inject_theme(ADMIN_PASSKEY_HTML))
}

pub(super) async fn session_must_change_password(
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

pub(super) async fn admin_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    if session_must_change_password(&state, &headers, &query.token).await {
        return must_change_password_redirect();
    }
    // ETag-aware: repeat navigations get an empty 304 instead of ~260 KB.
    html_page_response(&headers, inject_theme(ADMIN_DASHBOARD_HTML))
}
