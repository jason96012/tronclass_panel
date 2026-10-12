use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use serde_json::json;

use super::{
    admin_login_redirect, check_test_notification_rate, html_page_response,
    must_change_password_redirect, session_token, unauthorized_response, AdminQuery,
    AdminWebState,
};

const PAGE_HTML: &str = include_str!("static/push_status.html");

/// PUSH_SUBSCRIPTION_CAP_V1（fix121）：每帳號保留的推播訂閱裝置上限。
/// 一般使用者 1–3 台；超過者多半是舊裝置累積（或惡意灌入），要求先退訂。
const PUSH_SUBSCRIPTION_MAX_PER_ACCOUNT: i64 = 20;

pub(super) async fn page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // ADMIN_PAGE_TRI_STATE_V1：access 過期但 refresh 有效時由伺服器端輪換，不再閃登入頁。
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/push-status");
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

/// PUSH_GONE_VISIBLE_V1：`pushsubscriptionchange` 輪替請求。
#[derive(Debug, Deserialize)]
pub(super) struct PushRotateRequest {
    #[serde(default)]
    old_endpoint: String,
    new_subscription: PushSubscriptionRequest,
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
    // PUSH_HEALTH_V1：此帳號最近的推播發送結果（正常／連續失敗／尚無紀錄）。
    let health = state
        .accounts_db
        .get_web_push_health(&account_id)
        .await
        .ok()
        .flatten();
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
            "health": health,
        }))
        .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": super::public_error("讀取推播狀態失敗：", &error)})),
        )
            .into_response(),
    }
}

/// PUSH_OVERVIEW_V1：管理員查看所有帳號的推播綁定狀態。
pub(super) async fn overview_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "僅管理員可查看全部帳號的綁定狀態"})),
        )
            .into_response();
    }
    let accounts = match state.accounts_db.list().await {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": super::public_error("讀取帳號清單失敗：", &error)})),
            )
                .into_response()
        }
    };
    let overview = match state.accounts_db.list_web_push_binding_overview().await {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": super::public_error("讀取綁定總覽失敗：", &error)})),
            )
                .into_response()
        }
    };
    // PUSH_HEALTH_V1：每個帳號的發送健康與上游輪詢狀態（太久沒登入＝輪詢連續失敗）。
    let health_rows = state
        .accounts_db
        .list_web_push_health()
        .await
        .unwrap_or_default();
    let mut health_by_account: std::collections::BTreeMap<String, serde_json::Value> =
        health_rows.into_iter().collect();
    let mut monitor_by_account: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();
    for account in super::status::resolve_visible_accounts(&state, &session) {
        let consecutive_failures = account.monitor_status.lock().await.consecutive_failures;
        let last_poll_timestamp = account.monitor_status.lock().await.last_poll_timestamp;
        let is_running = *account.is_running.lock().await;
        monitor_by_account.insert(
            account.account_id.clone(),
            json!({
                "is_running": is_running,
                "consecutive_failures": consecutive_failures,
                "last_poll_timestamp": last_poll_timestamp,
            }),
        );
    }
    let mut by_account: std::collections::BTreeMap<String, (i64, Option<i64>)> =
        std::collections::BTreeMap::new();
    for row in overview {
        by_account.insert(row.account_id, (row.bindings, row.last_updated));
    }
    let mut items = Vec::new();
    for account in &accounts {
        let (bindings, last_updated) = by_account
            .remove(&account.id)
            .unwrap_or((0, None));
        let health = health_by_account.remove(&account.id);
        let monitor = monitor_by_account.get(&account.id).cloned();
        items.push(json!({
            "account_id": account.id,
            "username": account.username,
            "enabled": account.enabled,
            "bindings": bindings,
            "bound": bindings > 0,
            "last_updated": last_updated,
            "health": health,
            "monitor": monitor,
        }));
    }
    // 殘留訂閱：帳號已刪除但裝置訂閱還在（供管理員判斷是否清理）。
    let orphans: Vec<serde_json::Value> = by_account
        .into_iter()
        .map(|(account_id, (bindings, last_updated))| {
            json!({"account_id": account_id, "bindings": bindings, "last_updated": last_updated})
        })
        .collect();
    Json(json!({"is_admin": true, "accounts": items, "orphans": orphans})).into_response()
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
    // PUSH_SUBSCRIPTION_CAP_V1（fix121）：原先沒有訂閱數上限——任一登入
    // session 都能以不重複端點對自己帳號灌入無限量訂閱列（SQLite 漲大 +
    // 每次發送都要遍歷它們，工具人向量）。一般使用者只有 1–3 台裝置：
    // 每帳號上限 20；額滿後先退訂舊裝置才能訂閱新裝置。
    match state
        .accounts_db
        .count_web_push_subscriptions(&account_id)
        .await
    {
        Ok(current) if current >= PUSH_SUBSCRIPTION_MAX_PER_ACCOUNT => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "推播訂閱裝置已達上限（20），請先移除不需要的訂閱",
                    "code": "PUSH_SUBSCRIPTION_CAP"
                })),
            )
                .into_response();
        }
        _ => {}
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
        Ok(()) => {
            // PUSH_GONE_VISIBLE_V1：重新啟用即清掉「訂閱曾被移除」的橫幅標記。
            let _ = state.accounts_db.clear_web_push_removed(&account_id).await;
            Json(json!({"ok": true, "account_id": account_id})).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": super::public_error("儲存推播訂閱失敗：", &error)})),
        )
            .into_response(),
    }
}

/// PUSH_GONE_VISIBLE_V1：瀏覽器觸發 `pushsubscriptionchange`（推播服務輪替端點）時，
/// service worker 以新訂閱呼叫這裡，把舊端點換成新端點。沒有這條路徑時，舊端點下次發送
/// 會收到 404/410 → 被移除 → 使用者得手動再開一次推播。
///
/// 安全性：需登入 cookie（service worker 的 fetch 會帶同源 cookie）；只會動「這個帳號」的列，
/// 舊端點不屬於此帳號時不會刪別人的訂閱（`delete_web_push_subscription` 以 account_id 過濾）。
pub(super) async fn rotate_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<PushRotateRequest>,
) -> Response {
    let account_id = match authenticated_account(&state, &headers, &query).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(message) = validate_subscription(&request.new_subscription) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
    }
    if request.old_endpoint.len() > 2048 {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "old_endpoint 過長"}))).into_response();
    }
    match state
        .accounts_db
        .replace_web_push_endpoint(
            &account_id,
            request.old_endpoint.trim(),
            &request.new_subscription.endpoint,
            &request.new_subscription.keys.p256dh,
            &request.new_subscription.keys.auth,
        )
        .await
    {
        Ok(replaced) => {
            let _ = state.accounts_db.clear_web_push_removed(&account_id).await;
            tracing::info!(account = %account_id, replaced, "Web Push 端點已依 pushsubscriptionchange 輪替");
            Json(json!({"ok": true, "replaced": replaced})).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": super::public_error("更新推播訂閱失敗：", &error)})),
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
            Json(json!({"error": super::public_error("刪除推播訂閱失敗：", &error)})),
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
    // TEST_NOTIFICATION_RATE_LIMIT_V1
    if let Err(response) = check_test_notification_rate(&state, "push", &account_id).await {
        return response;
    }
    const TEST_DELAY_SECS: u64 = 10;
    let subscription_count = match state
        .accounts_db
        .count_web_push_subscriptions(&account_id)
        .await
    {
        Ok(value) => value.max(0) as usize,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "sent": 0,
                "error": super::public_error("讀取推播訂閱失敗：", &error)
            }))
            .into_response()
        }
    };
    if subscription_count == 0 {
        return Json(json!({
            "ok": false,
            "sent": 0,
            "error": "這個帳號尚未啟用手機推播"
        }))
        .into_response();
    }
    let delayed_account = account_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(TEST_DELAY_SECS)).await;
        if let Err(error) = crate::web_push_notifications::send_test(&delayed_account).await {
            tracing::warn!(
                account = %delayed_account,
                error = %error,
                "延遲背景推播測試失敗"
            );
        }
    });
    Json(json!({
        "ok": true,
        "sent": subscription_count,
        "scheduled": true,
        "delay_secs": TEST_DELAY_SECS
    }))
    .into_response()
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
