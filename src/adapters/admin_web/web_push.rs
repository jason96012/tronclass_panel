use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use serde_json::json;

use super::{session_token, unauthorized_response, AdminQuery, AdminWebState};

#[derive(Debug, Deserialize)]
pub(super) struct PushSubscriptionRequest {
    endpoint: String,
    keys: PushSubscriptionKeys,
}

#[derive(Debug, Deserialize)]
struct PushSubscriptionKeys {
    p256dh: String,
    auth: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct PushUnsubscribeRequest {
    endpoint: String,
}

fn authenticated_account(
    state: &AdminWebState,
    headers: &HeaderMap,
    query: &AdminQuery,
) -> impl std::future::Future<Output = Result<String, Response>> {
    let token = session_token(headers, &query.token);
    let state = state.clone();
    async move {
        let Some(session) = state.authorize(&token).await else {
            return Err(unauthorized_response());
        };
        if session.account_id == "<token-admin>" {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "請使用實際帳號登入後再啟用手機推播"})),
            )
                .into_response());
        }
        Ok(session.account_id)
    }
}

pub(super) async fn config_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let account_id = match authenticated_account(&state, &headers, &query).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(public_key) = crate::web_push_notifications::public_key() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "Web Push 尚未初始化"})),
        )
            .into_response();
    };
    match state
        .accounts_db
        .count_web_push_subscriptions(&account_id)
        .await
    {
        Ok(count) => Json(json!({
            "public_key": public_key,
            "subscribed": count > 0,
            "subscription_count": count,
            "account_id": account_id,
        }))
        .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("讀取推播狀態失敗：{error}")})),
        )
            .into_response(),
    }
}

pub(super) async fn subscribe_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<PushSubscriptionRequest>,
) -> Response {
    let account_id = match authenticated_account(&state, &headers, &query).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(message) = validate_subscription(&request) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
    }
    match state
        .accounts_db
        .upsert_web_push_subscription(
            &account_id,
            &request.endpoint,
            &request.keys.p256dh,
            &request.keys.auth,
        )
        .await
    {
        Ok(()) => Json(json!({"ok": true, "account_id": account_id})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("儲存推播訂閱失敗：{error}")})),
        )
            .into_response(),
    }
}

pub(super) async fn unsubscribe_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<PushUnsubscribeRequest>,
) -> Response {
    let account_id = match authenticated_account(&state, &headers, &query).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match state
        .accounts_db
        .delete_web_push_subscription(&account_id, request.endpoint.trim())
        .await
    {
        Ok(deleted) => Json(json!({"ok": true, "deleted": deleted})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("刪除推播訂閱失敗：{error}")})),
        )
            .into_response(),
    }
}

pub(super) async fn test_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let account_id = match authenticated_account(&state, &headers, &query).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match crate::web_push_notifications::send_test(&account_id).await {
        Ok(0) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "這個帳號尚未啟用手機推播"})),
        )
            .into_response(),
        Ok(sent) => Json(json!({"ok": true, "sent": sent})).into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("測試推播失敗：{error}")})),
        )
            .into_response(),
    }
}

fn validate_subscription(request: &PushSubscriptionRequest) -> Result<(), &'static str> {
    if request.endpoint.len() > 4096 {
        return Err("推播 endpoint 太長");
    }
    let endpoint = reqwest::Url::parse(request.endpoint.trim()).map_err(|_| "推播 endpoint 無效")?;
    if endpoint.scheme() != "https" || endpoint.host_str() != Some("web.push.apple.com") {
        return Err("目前只接受 iPhone/iOS 的 Apple Web Push 訂閱");
    }
    let p256dh = URL_SAFE_NO_PAD
        .decode(request.keys.p256dh.trim())
        .map_err(|_| "p256dh 格式無效")?;
    let auth = URL_SAFE_NO_PAD
        .decode(request.keys.auth.trim())
        .map_err(|_| "auth 格式無效")?;
    if p256dh.len() != 65 || auth.len() != 16 {
        return Err("推播金鑰長度無效");
    }
    Ok(())
}
