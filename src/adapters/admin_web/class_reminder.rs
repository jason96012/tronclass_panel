use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use super::{
    unauthorized_response, validate_optional_admin_text, AdminQuery, AdminSession, AdminWebState,
    ADMIN_MAX_IDENTIFIER_BYTES,
};

#[derive(Debug, Deserialize)]
pub(super) struct AdminClassReminderQuery {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminClassReminderUpdateRequest {
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default = "default_discord_enabled")]
    pub discord_enabled: bool,
    /// CLASS_REMINDER_PUSH_V1：手機 Web Push 渠道開關（省略時維持預設開啟）。
    #[serde(default = "default_push_enabled")]
    pub push_enabled: bool,
    #[serde(default = "default_minutes_before")]
    pub minutes_before: i64,
}

fn default_discord_enabled() -> bool {
    true
}
fn default_push_enabled() -> bool {
    true
}
fn default_minutes_before() -> i64 {
    10
}

/// DIGEST_PER_ACCOUNT_V1：晨間摘要偏好端點沿用同一套「管理員可指定帳號、一般使用者僅限自己」規則。
pub(super) fn resolve_class_reminder_target(
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
                Json(json!({ "error": "管理員 token 請先選擇 account_id" })),
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

pub(super) async fn admin_class_reminder_get_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminClassReminderQuery>,
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
    let target = match resolve_class_reminder_target(&session, query.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    match state
        .accounts_db
        .get_class_reminder_preference(&target)
        .await
    {
        Ok(pref) => (
            StatusCode::OK,
            Json(json!({
                "account_id": pref.account_id,
                "discord_enabled": pref.discord_enabled,
                "push_enabled": pref.push_enabled,
                "minutes_before": pref.minutes_before,
                "max_minutes": 180,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": super::public_error("class reminder database error: ", &e) })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_class_reminder_update_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(req): Json<AdminClassReminderUpdateRequest>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_optional_admin_text(
        req.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_class_reminder_target(&session, req.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    let minutes_before = req.minutes_before.clamp(0, 180);
    match state
        .accounts_db
        .set_class_reminder_preference(
            &target,
            req.discord_enabled,
            req.push_enabled,
            minutes_before,
        )
        .await
    {
        Ok(()) => {
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "class_reminder_settings",
                Some(&target),
                Some(&format!(
                    "discord_enabled={} push_enabled={} minutes_before={}",
                    req.discord_enabled, req.push_enabled, minutes_before
                )),
                "更新上課前提醒設定（Discord／Web Push）",
            );
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "account_id": target,
                    "discord_enabled": req.discord_enabled,
                    "push_enabled": req.push_enabled,
                    "minutes_before": minutes_before,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": super::public_error("save class reminder failed: ", &e) })),
        )
            .into_response(),
    }
}
