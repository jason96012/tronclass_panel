//! 內嵌靜態資源（CSS/JS）的響應 handlers。

use super::*;
use super::pages;

const ADMIN_STATIC_CACHE_CONTROL: &str = "public, max-age=604800, immutable, stale-while-revalidate=86400";

pub(super) fn admin_static_asset(content_type: &'static str, body: &str) -> Response {
    // ASSET_VERSION_AUTO_V1：JS/CSS 內部互相引用的 /admin/static/... 版號
    // （如 mobile-nav.js 注入 mobile-navigation.css?v=…）也在輸出時統一改寫。
    let body = if content_type.starts_with("text/") || content_type.contains("javascript") {
        super::asset_versions::rewrite_asset_versions(body).into_owned()
    } else {
        body.to_string()
    };
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

pub(super) async fn admin_theme_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", &pages::theme_css())
}

pub(super) async fn admin_session_security_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::SESSION_SECURITY_JS,
    )
}

pub(super) async fn admin_notifications_page_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::NOTIFICATIONS_PAGE_JS,
    )
}

pub(super) async fn admin_app_settings_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::APP_SETTINGS_JS,
    )
}

pub(super) async fn admin_dashboard_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::DASHBOARD_MODULE_CSS)
}

pub(super) async fn admin_dashboard_late_fixes_css_handler() -> Response {
    admin_static_asset(
        "text/css; charset=utf-8",
        pages::DASHBOARD_LATE_FIXES_CSS,
    )
}

pub(super) async fn admin_dashboard_mobile_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::DASHBOARD_MOBILE_CSS)
}

pub(super) async fn admin_dashboard_components_css_handler() -> Response {
    admin_static_asset(
        "text/css; charset=utf-8",
        pages::DASHBOARD_COMPONENTS_CSS,
    )
}

pub(super) async fn admin_dashboard_access_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::DASHBOARD_ACCESS_CSS)
}

pub(super) async fn admin_dashboard_runtime_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::DASHBOARD_RUNTIME_JS,
    )
}

pub(super) async fn admin_dashboard_modules_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::DASHBOARD_FEATURES_JS,
    )
}

pub(super) async fn admin_mobile_navigation_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::MOBILE_NAVIGATION_CSS)
}

pub(super) async fn admin_mobile_timetable_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::MOBILE_TIMETABLE_CSS)
}

pub(super) async fn admin_account_components_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::ACCOUNT_COMPONENTS_CSS)
}

pub(super) async fn admin_maintenance_components_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::MAINTENANCE_COMPONENTS_CSS)
}

pub(super) async fn admin_maintenance_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::MAINTENANCE_MODULE_JS,
    )
}

pub(super) async fn admin_maintenance_implementation_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::MAINTENANCE_IMPLEMENTATION_JS,
    )
}

pub(super) async fn admin_timetable_ui_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::TIMETABLE_UI_MODULE_JS,
    )
}

pub(super) async fn admin_mock_rollcall_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::MOCK_ROLLCALL_MODULE_JS,
    )
}

pub(super) async fn admin_accounts_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::ACCOUNTS_MODULE_JS,
    )
}

pub(super) async fn admin_core_api_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::CORE_API_JS)
}

pub(super) async fn admin_web_push_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::WEB_PUSH_JS)
}
// DASHBOARD_LIVE_EVENTS_V1
pub(super) async fn admin_live_events_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::LIVE_EVENTS_JS)
}

// SUBPAGE_FETCH_SHARED_V1
pub(super) async fn admin_subpage_fetch_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::SUBPAGE_FETCH_JS)
}

// CLIENT_ERROR_REPORT_V1
pub(super) async fn admin_client_log_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::CLIENT_LOG_JS)
}

pub(super) async fn admin_dompurify_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DOMPURIFY_JS)
}

pub(super) async fn admin_trusted_types_init_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::TRUSTED_TYPES_INIT_JS,
    )
}

pub(super) async fn admin_jszip_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::JSZIP_MIN_JS)
}

pub(super) async fn admin_office_view_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::OFFICE_VIEW_JS)
}

// QR_SHARE_V1: 分享彈窗的 QR code 產生函式庫（離線內建）。
pub(super) async fn admin_qrcode_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::QR_CODE_MIN_JS)
}

// SHARE_ADMIN_V1: 分享連結管理頁前端。
pub(super) async fn admin_shares_admin_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::SHARES_ADMIN_JS)
}

// DASH_ASSET_V1: dashboard 資產化模組。
pub(super) async fn admin_dashboard_particles_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_PARTICLES_JS)
}
pub(super) async fn admin_dashboard_login_card_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_LOGIN_CARD_JS)
}
pub(super) async fn admin_dashboard_live_alerts_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_LIVE_ALERTS_JS)
}
// DASH_ASSET_V2
pub(super) async fn admin_dashboard_permissions_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_PERMISSIONS_JS)
}
pub(super) async fn admin_dashboard_cookie_badge_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_COOKIE_BADGE_JS)
}
pub(super) async fn admin_dashboard_modal_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MODAL_JS)
}
// DASH_ASSET_V3
pub(super) async fn admin_dashboard_header_more_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_HEADER_MORE_JS)
}
pub(super) async fn admin_dashboard_more_buttons_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MORE_BUTTONS_JS)
}
pub(super) async fn admin_dashboard_more_links_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MORE_LINKS_JS)
}
pub(super) async fn admin_dashboard_more_pages_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MORE_PAGES_JS)
}
pub(super) async fn admin_dashboard_notify_purge_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_NOTIFY_PURGE_JS)
}
pub(super) async fn admin_dashboard_desktop_size_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_DESKTOP_SIZE_JS)
}
pub(super) async fn admin_dashboard_header_spacious_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_HEADER_SPACIOUS_JS)
}
pub(super) async fn admin_dashboard_more_actions_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MORE_ACTIONS_JS)
}
// SHARED_MOBILE_NAV_V1
pub(super) async fn shared_mobile_nav_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::SHARED_MOBILE_NAV_JS)
}
// ONBOARDING_GUIDE_V1
pub(super) async fn admin_dashboard_onboarding_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_ONBOARDING_JS)
}

// SHARE_V1: 免登入分享預覽頁的外掛 JS/CSS。
pub(super) async fn admin_share_preview_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::SHARE_PREVIEW_JS)
}
pub(super) async fn admin_share_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::SHARE_CSS)
}

// PDFJS_PREVIEW_V1: PDF.js 主程式與 worker。worker 由 pdf.js 在執行期以
// workerSrc 同源載入（worker-src 'self' 合法）。
pub(super) async fn admin_pdf_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::PDF_MIN_JS)
}

pub(super) async fn admin_pdf_worker_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::PDF_WORKER_MIN_JS)
}
