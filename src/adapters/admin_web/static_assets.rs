//! 內嵌靜態資源（CSS/JS）的響應 handlers。

use super::*;
use super::pages;

const ADMIN_STATIC_CACHE_CONTROL: &str = "public, max-age=604800, immutable, stale-while-revalidate=86400";

pub(super) fn admin_static_asset(content_type: &'static str, body: impl IntoResponse) -> Response {
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
    admin_static_asset("text/css; charset=utf-8", pages::theme_css())
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

pub(super) async fn admin_dompurify_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DOMPURIFY_JS)
}

pub(super) async fn admin_trusted_types_init_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::TRUSTED_TYPES_INIT_JS,
    )
}
