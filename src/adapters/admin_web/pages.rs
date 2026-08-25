//! HTML templates — Tronclass Admin Web UI.

use std::sync::RwLock;

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

pub(super) const TIMETABLE_UX_JS: &str = include_str!("static/timetable_ux.js");
pub(super) const DOMPURIFY_JS: &str = include_str!("static/js/dompurify.min.js");
pub(super) const TRUSTED_TYPES_INIT_JS: &str = include_str!("static/js/trusted-types-init.js");
pub(super) const SESSION_SECURITY_JS: &str = include_str!("static/js/session-security.js");
pub(super) const CORE_API_JS: &str = include_str!("static/js/core-api.js");
pub(super) const WEB_PUSH_JS: &str = include_str!("static/js/web-push.js");
pub(super) const ACCOUNTS_MODULE_JS: &str = include_str!("static/js/accounts.js");
pub(super) const MOCK_ROLLCALL_MODULE_JS: &str = include_str!("static/js/mock-rollcall.js");
pub(super) const NOTIFICATIONS_MODULE_JS: &str = include_str!("static/js/notifications.js");
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
pub(super) const DASHBOARD_RUNTIME_JS: &str = include_str!("static/js/dashboard-runtime.js");
pub(super) const DASHBOARD_FEATURES_JS: &str = include_str!("static/js/dashboard-modules.js");

pub(super) const ADMIN_LOGS_HTML: &str = include_str!("static/logs.html");
pub(super) const ADMIN_FILES_HTML: &str = include_str!("static/files.html");
pub(super) const ADMIN_HEALTH_HTML: &str = include_str!("static/health.html");
pub(super) const ADMIN_DASHBOARD_HTML: &str = include_str!("static/dashboard.html");
pub(super) const ADMIN_LOGIN_HTML: &str = include_str!("static/login.html");
pub(super) const ADMIN_ACTIVITY_HTML: &str = include_str!("static/activity.html");
pub(super) const ADMIN_PASSWORD_HTML: &str = include_str!("static/password.html");
pub(super) const ADMIN_PASSKEY_HTML: &str = include_str!("static/passkey.html");
pub(super) const ADMIN_BLACKLIST_HTML: &str = include_str!("static/blacklist.html");
pub(super) const ADMIN_BLOCKED_HTML: &str = include_str!("static/blocked.html");

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
    const SHARED_ASSETS: &str = concat!(
        r#"<link rel="preload" href="/admin/static/theme.css?v=20260821fast" as="style">"#,
        r#"<link rel="preload" href="/admin/static/js/dompurify.min.js?v=20260823trustedtypes" as="script">"#,
        r#"<script src="/admin/static/js/dompurify.min.js?v=20260823trustedtypes"></script>"#,
        r#"<script src="/admin/static/js/trusted-types-init.js?v=20260823trustedtypes"></script>"#,
        r#"<link rel="preload" href="/admin/static/session-security.js?v=20260821fast" as="script">"#,
        r#"<script id="sessionSecurityHardeningV1" src="/admin/static/session-security.js?v=20260821fast"></script>"#,
        r#"<link rel="stylesheet" href="/admin/static/theme.css?v=20260821fast">"#,
        r#"<script defer src="/admin/static/web-push.js?v=20260825push"></script>"#
    );
    const TIMETABLE_UX_ASSET: &str =
        r#"<script defer src="/admin/static/timetable-ux.js?v=20260821fast"></script>"#;

    let assets = if html.contains("tron-auth") || html.contains("dash-auth-ok") {
        SHARED_ASSETS.to_string()
    } else {
        String::from(SHARED_ASSETS) + TIMETABLE_UX_ASSET
    };
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
