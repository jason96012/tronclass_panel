//! System/audit log page/API handlers for Admin Web.

use super::pages::{inject_theme, ADMIN_LOGS_HTML};
use super::*;

pub(super) async fn admin_logs_page_handler(headers: HeaderMap) -> Response {
    super::html_page_response(&headers, inject_theme(ADMIN_LOGS_HTML))
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
