//! LOGIN_DEVICES_V1：「登入裝置管理」頁面與 API。
//!
//! 列出目前帳號所有仍有效的 Dashboard session（登入時間、最近活動、來源 IP、
//! 裝置／瀏覽器摘要），可個別登出或「登出其他所有裝置」。
//!
//! 安全邊界：
//! * 任何已登入帳號（含檢視者）只能看／管自己的 session；管理員也一樣——
//!   要踢別人請用既有的 reset_password（會撤銷該帳號所有 session）。
//! * 前端永遠拿不到 session token；每一列以 token 的 SHA-256 前 8 bytes 當識別碼
//!   （`session_id`），撤銷時由後端反查目前帳號的 session 清單再比對。
//! * `/admin/api/devices/revoke` 已列入 `viewer_allowed_write_path`。

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    admin_login_redirect, html_page_response, must_change_password_redirect, session_token,
    unauthorized_response, AdminQuery, AdminWebState,
};

const PAGE_HTML: &str = include_str!("static/devices.html");

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// token → 不可逆的短識別碼（前端顯示／撤銷用）。
pub(super) fn session_public_id(token: &str) -> String {
    let digest = Sha256::digest(token.trim().as_bytes());
    hex::encode(&digest[..8])
}

/// 從 User-Agent 粗略歸納「裝置／瀏覽器」摘要，純字串比對、不引入 UA parser。
fn summarize_user_agent(ua: &str) -> (String, String) {
    let ua_l = ua.to_ascii_lowercase();
    let os = if ua_l.contains("iphone") {
        "iPhone"
    } else if ua_l.contains("ipad") {
        "iPad"
    } else if ua_l.contains("android") {
        "Android"
    } else if ua_l.contains("windows") {
        "Windows"
    } else if ua_l.contains("mac os x") || ua_l.contains("macintosh") {
        "macOS"
    } else if ua_l.contains("cros") {
        "ChromeOS"
    } else if ua_l.contains("linux") {
        "Linux"
    } else if ua.trim().is_empty() {
        "未知裝置"
    } else {
        "其他"
    };
    let browser = if ua_l.contains("edg/") || ua_l.contains("edge/") {
        "Edge"
    } else if ua_l.contains("opr/") || ua_l.contains("opera") {
        "Opera"
    } else if ua_l.contains("samsungbrowser") {
        "Samsung Internet"
    } else if ua_l.contains("firefox/") || ua_l.contains("fxios/") {
        "Firefox"
    } else if ua_l.contains("crios/") {
        "Chrome (iOS)"
    } else if ua_l.contains("chrome/") || ua_l.contains("chromium/") {
        "Chrome"
    } else if ua_l.contains("safari/") && ua_l.contains("version/") {
        "Safari"
    } else if ua_l.contains("line/") {
        "LINE 內建瀏覽器"
    } else if ua.trim().is_empty() {
        "未知瀏覽器"
    } else {
        "其他瀏覽器"
    };
    (os.to_string(), browser.to_string())
}

pub(super) async fn page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return admin_login_redirect("/admin/devices");
    };
    if session.must_change_password {
        return must_change_password_redirect();
    }
    html_page_response(&headers, super::pages::inject_theme(PAGE_HTML))
}

pub(super) async fn list_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if session.account_id == "<token-admin>" {
        return (
            StatusCode::OK,
            Json(json!({ "sessions": [], "current_id": "", "account_id": session.account_id })),
        )
            .into_response();
    }
    let now = now_unix();
    let rows = match state
        .accounts_db
        .list_dashboard_sessions_by_account(&session.account_id, now)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("讀取登入裝置失敗：{error}") })),
            )
                .into_response();
        }
    };
    let current_id = session_public_id(&token);
    let sessions: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let id = session_public_id(&row.token);
            let (os, browser) = summarize_user_agent(&row.user_agent);
            let last_seen_at = if row.last_seen_at > 0 {
                row.last_seen_at
            } else {
                row.created_at
            };
            let current = id == current_id;
            json!({
                "id": id,
                "current": current,
                "ip": row.ip,
                "os": os,
                "browser": browser,
                "user_agent": row.user_agent,
                "created_at": row.created_at,
                "last_seen_at": last_seen_at,
                "expires_at": row.expires_at,
                "refresh_expires_at": row.refresh_expires_at,
                "access_expired": row.expires_at <= now,
                "is_admin": row.is_admin,
                "is_viewer": row.is_viewer,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "sessions": sessions,
            "current_id": current_id,
            "account_id": session.account_id,
            "server_time": now,
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct RevokeRequest {
    /// 要登出的 session 識別碼；`all_others` 為 true 時忽略。
    #[serde(default)]
    id: String,
    #[serde(default)]
    all_others: bool,
}

pub(super) async fn revoke_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(req): Json<RevokeRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if session.account_id == "<token-admin>" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "emergency token has no persistent sessions" })),
        )
            .into_response();
    }

    if req.all_others {
        let revoked = state.revoke_other_sessions(&session.account_id, &token).await;
        crate::logs::record_audit(
            "web",
            &session.account_id,
            "devices_revoke_others",
            Some(&session.account_id),
            None,
            &format!("登入裝置管理：登出其他 {revoked} 個裝置"),
        );
        return (
            StatusCode::OK,
            Json(json!({ "ok": true, "revoked": revoked })),
        )
            .into_response();
    }

    let target_id = req.id.trim();
    if target_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "id is required" })),
        )
            .into_response();
    }
    if target_id == session_public_id(&token) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "要登出目前這台裝置請直接按「登出」" })),
        )
            .into_response();
    }
    let rows = state
        .accounts_db
        .list_dashboard_sessions_by_account(&session.account_id, now_unix())
        .await
        .unwrap_or_default();
    let Some(target) = rows
        .into_iter()
        .find(|row| session_public_id(&row.token) == target_id)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "找不到這個裝置（可能已登出或過期）" })),
        )
            .into_response();
    };
    state.revoke_session(&target.token).await;
    crate::logs::record_audit(
        "web",
        &session.account_id,
        "devices_revoke",
        Some(&session.account_id),
        Some(target.ip.as_str()),
        "登入裝置管理：登出單一裝置",
    );
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "revoked": 1 })),
    )
        .into_response()
}
