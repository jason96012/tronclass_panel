//! System/audit log page/API handlers for Admin Web.

use super::pages::{inject_theme, ADMIN_LOGS_HTML};
use super::*;

pub(super) async fn admin_logs_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // ADMIN_PAGE_TRI_STATE_V1：伺服器端驗證（含 refresh 續期），未登入導回登入頁。
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return super::admin_login_redirect("/admin/logs");
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(super::must_change_password_redirect(), &page, &headers);
    }
    super::pages::attach_fresh_cookies(
        super::html_page_response(&headers, inject_theme(ADMIN_LOGS_HTML)),
        &page,
        &headers,
    )
}

pub(super) async fn admin_logs_api_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminLogsQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }

    let limit = query.limit.unwrap_or(200).clamp(1, 1_000);
    let entries = crate::logs::snapshot(query.kind.as_deref(), limit);
    (
        StatusCode::OK,
        Json(json!({ "entries": entries.clone(), "items": entries })),
    )
        .into_response()
}
