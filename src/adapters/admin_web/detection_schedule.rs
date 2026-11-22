use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Datelike, Local};
use serde::Deserialize;
use serde_json::json;

use super::{
    admin_login_redirect, html_page_response, must_change_password_redirect,
    unauthorized_response, validate_optional_admin_text, AdminQuery, AdminSession, AdminWebState,
    ADMIN_MAX_IDENTIFIER_BYTES,
};

const PAGE_HTML: &str = include_str!("static/detection_schedule.html");

#[derive(Debug, Deserialize)]
pub(super) struct DetectionScheduleQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct DetectionScheduleUpdateRequest {
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_ten")]
    minutes_before: i64,
    #[serde(default = "default_ten")]
    minutes_after: i64,
    #[serde(default = "default_outside_interval")]
    outside_interval_secs: i64,
}

fn default_true() -> bool {
    true
}
fn default_ten() -> i64 {
    10
}
fn default_outside_interval() -> i64 {
    300
}

fn resolve_target(session: &AdminSession, requested: Option<String>) -> Result<String, Response> {
    let requested = requested
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if session.is_admin {
        let target = requested.unwrap_or_else(|| session.account_id.clone());
        if target == "<token-admin>" {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "請先選擇帳號"
                })),
            )
                .into_response());
        }
        Ok(target)
    } else {
        if requested
            .as_deref()
            .is_some_and(|value| value != session.account_id)
        {
            return Err(unauthorized_response());
        }
        Ok(session.account_id.clone())
    }
}

pub(super) async fn page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // ADMIN_PAGE_TRI_STATE_V1：access 過期但 refresh 有效時由伺服器端輪換，不再閃登入頁。
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/detection-schedule");
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    super::pages::attach_fresh_cookies(
        html_page_response(&headers, super::pages::inject_auth_attrs(&super::pages::inject_theme(PAGE_HTML), &page.session)),
        &page,
        &headers,
    )
}

pub(super) async fn get_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<DetectionScheduleQuery>,
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
    let target = match resolve_target(&session, query.account_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let preference = match state
        .accounts_db
        .get_detection_schedule_preference(&target)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": super::public_error("讀取偵測設定失敗：", &error)
                })),
            )
                .into_response()
        }
    };
    let entries = match state
        .accounts_db
        .list_timetable_entries_for_account(&target)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": super::public_error("讀取課表失敗：", &error)
                })),
            )
                .into_response()
        }
    };
    let now = Local::now();
    let evaluation =
        crate::detection_schedule::evaluate(&entries, now.weekday(), now.time(), &preference);
    (
        StatusCode::OK,
        Json(json!({
            "preference": preference,
            "evaluation": evaluation,
            "entries": entries,
            "night_pause_active": crate::detection_schedule::is_night_pause(now.time()),
            "night_pause": "19:00–07:00",
            "server_time": now.format("%Y-%m-%d %H:%M:%S").to_string(),
        })),
    )
        .into_response()
}

pub(super) async fn update_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(request): Json<DetectionScheduleUpdateRequest>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_optional_admin_text(
        request.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_target(&session, request.account_id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let before = request.minutes_before.clamp(0, 180);
    let after = request.minutes_after.clamp(0, 180);
    let outside = request.outside_interval_secs.clamp(60, 3600);
    if let Err(error) = state
        .accounts_db
        .set_detection_schedule_preference(&target, request.enabled, before, after, outside)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": super::public_error("儲存偵測設定失敗：", &error)
            })),
        )
            .into_response();
    }

    // Wake this account so the new window/interval is applied immediately.
    // The resulting force poll is also useful confirmation that monitoring is alive.
    if let Some(account) = state.account_by_id(&target) {
        account.force_poll_tx.notify_one();
    }
    crate::logs::record_audit(
        "web",
        &session.account_id,
        "detection_schedule_settings",
        Some(&target),
        Some(&format!(
            "enabled={} before={} after={} outside_secs={}",
            request.enabled, before, after, outside
        )),
        "更新依課表點名偵測設定",
    );
    (
        StatusCode::OK,
        Json(json!({
            "ok": true, "account_id": target, "enabled": request.enabled,
            "minutes_before": before, "minutes_after": after,
            "outside_interval_secs": outside,
        })),
    )
        .into_response()
}
