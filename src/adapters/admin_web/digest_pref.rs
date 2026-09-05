//! DIGEST_PER_ACCOUNT_V1：晨間摘要每帳號開關。
//!
//! GET  /admin/api/digest/preference?account_id=…  → { account_id, enabled }
//! POST /admin/api/digest/preference { account_id?, enabled } → { ok, account_id, enabled }
//!
//! 儲存於 notification_preferences（channel=`digest`、key=`morning`），
//! 未設定視為開啟。管理員可指定 account_id；一般使用者只能操作自己。
//! 全域開關／時間仍在設定頁（config/digest.json），兩者皆開才會發送。

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use super::class_reminder::resolve_class_reminder_target;
use super::morning_digest::{DIGEST_PREF_CHANNEL, DIGEST_PREF_KEY};
use super::{
    unauthorized_response, validate_optional_admin_text, AdminQuery, AdminWebState,
    ADMIN_MAX_IDENTIFIER_BYTES,
};

#[derive(Debug, Deserialize)]
pub(super) struct DigestPreferenceQuery {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct DigestPreferenceUpdateRequest {
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

pub(super) async fn get_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<DigestPreferenceQuery>,
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
        .notification_preference_enabled(&target, DIGEST_PREF_CHANNEL, DIGEST_PREF_KEY)
        .await
    {
        Ok(enabled) => {
            let global = super::morning_digest::digest_status();
            (
                StatusCode::OK,
                Json(json!({
                    "account_id": target,
                    "enabled": enabled,
                    "global_enabled": global.enabled,
                    "time": global.time,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("digest preference database error: {e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn update_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(req): Json<DigestPreferenceUpdateRequest>,
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
    match state
        .accounts_db
        .set_notification_preference(&target, DIGEST_PREF_CHANNEL, DIGEST_PREF_KEY, req.enabled)
        .await
    {
        Ok(()) => {
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "digest_preference",
                Some(&target),
                Some(&format!("enabled={}", req.enabled)),
                "更新晨間摘要每帳號開關",
            );
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "account_id": target,
                    "enabled": req.enabled,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("save digest preference failed: {e}") })),
        )
            .into_response(),
    }
}
