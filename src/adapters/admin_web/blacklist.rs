//! IP 黑名單管理頁面與 API handlers。

use super::*;
use super::pages::{inject_theme, ADMIN_BLACKLIST_HTML};

#[derive(Debug, Deserialize)]
pub(super) struct AdminBlacklistIpRequest {
    ip: String,
}

pub(super) async fn admin_blacklist_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return admin_login_redirect("/admin/blacklist");
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    html_page_response(&headers, inject_theme(ADMIN_BLACKLIST_HTML))
}

pub(super) async fn admin_blacklist_list_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    match state.accounts_db.list_ip_blacklist(4096).await {
        Ok(items) => (StatusCode::OK, Json(json!({ "items": items }))).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("讀取 IP 黑名單失敗：{error}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_blacklist_block_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<AdminBlacklistIpRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    let Some(ip) = normalize_admin_ip(&request.ip) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid IP address" })),
        )
            .into_response();
    };
    match state
        .accounts_db
        .block_ip(&ip, &session.account_id, "Web Admin manual block")
        .await
    {
        Ok(created) => {
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "ip_blacklist_block",
                None,
                Some(&ip),
                "Web 管理頁封鎖 IP",
            );
            let _ = state
                .accounts_db
                .record_activity_event(crate::db::ActivityEventInput {
                    kind: "security".to_string(),
                    actor: session.account_id.clone(),
                    action: "ip_blacklist_block".to_string(),
                    account_id: String::new(),
                    path: ip.clone(),
                    result: "success".to_string(),
                    details: "Web Admin manual block".to_string(),
                    ip: ip.clone(),
                })
                .await;
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "blocked": created, "ip": ip })),
            )
                .into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("封鎖 IP 失敗：{error}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_blacklist_unblock_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<AdminBlacklistIpRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    let Some(ip) = normalize_admin_ip(&request.ip) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid IP address" })),
        )
            .into_response();
    };
    match state.accounts_db.unblock_ip(&ip).await {
        Ok(removed) => {
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "ip_blacklist_unblock",
                None,
                Some(&ip),
                "Web 管理頁解除 IP 封鎖",
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "unblocked": removed, "ip": ip })),
            )
                .into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("解除 IP 封鎖失敗：{error}") })),
        )
            .into_response(),
    }
}
