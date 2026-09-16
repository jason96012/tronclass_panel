//! IP 黑名單管理頁面與 API handlers。
//!
//! BLACKLIST_UX_V1（fix58）：黑名單頁的兩個長期問題
//!   ① 「最近登入失敗」每一列都有「加入黑名單」按鈕——即使那個 IP 已經在名單裡
//      （就在同一個畫面上方的表），按了只會得到「已在名單中」。
//      → `admin_blacklist_recent_logins_handler` 直接回 `blocked` 旗標，前端據此
//        改顯示「已封鎖」標籤，不再給按了沒用的按鈕。
//   ② 「位置」欄永遠是「未提供」（前端讀 `row.location`，但活動事件根本沒有這個
//      欄位）；「MAC 位址」欄更是永遠「無法取得」（瀏覽器不會把 MAC 給伺服器）。
//      → 位置改由伺服器用 `crate::geoip::resolve` 現查（24 小時快取，私網段直接
//        標「區域網路內」）；MAC 欄改成真正拿得到的「裝置」（User-Agent 摘要，
//        新的登入事件會記錄，舊資料顯示「未記錄」）。

use super::*;
use super::pages::{inject_theme, ADMIN_BLACKLIST_HTML};
// 編譯修正：`use super::*;` 只帶得進 mod.rs 的 `json!`（父模組只 `use serde_json::json;`），
// `Value` 不在 scope —— 同層其他模組（ai_chat.rs 等）也都是自己 import。
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub(super) struct AdminBlacklistIpRequest {
    ip: String,
}

/// 最近登入失敗查詢。`limit` 是「掃描的事件數」上限（與活動頁同一天花板）。
#[derive(Debug, Deserialize)]
pub(super) struct AdminBlacklistRecentQuery {
    #[serde(default)]
    token: String,
    limit: Option<i64>,
}

/// `BLACKLIST_RECENT_MAX_ROWS`：回給前端的列數上限（掃描更多，但只回最新的這些）。
const BLACKLIST_RECENT_MAX_ROWS: usize = 100;
/// 一次最多查幾個不同 IP 的位置。geoip 有 24 小時快取，這裡再設上限是為了避免
/// 第一次開啟頁面時對外部服務打出上百個請求（geoip 本身另有 4 秒逾時與短快取）。
const BLACKLIST_GEO_MAX_IPS: usize = 40;
/// 查位置時的並行數（外部服務，不宜太高）。
const BLACKLIST_GEO_CONCURRENCY: usize = 6;

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

/// 位置描述：私網／回送位址不必問外部服務，直接給確定的答案（不會再出現「無法取得」）。
fn is_private_or_local_ip(ip: &str) -> bool {
    match ip.trim().parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        Ok(std::net::IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
        Err(_) => true,
    }
}

/// 位置字串：私網 → 「區域網路內」；其他走 geoip（查不到才回「未知位置」）。
/// 只回文字、不回原始結構——前端不需要知道來源。
async fn location_label(ip: &str) -> String {
    let ip = ip.trim();
    if ip.is_empty() || ip == "-" {
        return String::new();
    }
    if is_private_or_local_ip(ip) {
        return "區域網路內".to_string();
    }
    crate::geoip::resolve(ip, None).await.label()
}

/// 一次查多個 IP 的位置：去重後限額、有限並行、回傳 ip → 位置。
/// 同一個 IP 在畫面上通常出現很多次（一直失敗），因此去重是省下最多工作的關鍵。
async fn location_labels(ips: Vec<String>) -> std::collections::HashMap<String, String> {
    use futures::stream::StreamExt;
    let mut unique: Vec<String> = Vec::new();
    for ip in ips {
        let ip = ip.trim().to_string();
        if ip.is_empty() || ip == "-" || unique.iter().any(|v| v == &ip) {
            continue;
        }
        unique.push(ip);
        if unique.len() >= BLACKLIST_GEO_MAX_IPS {
            break;
        }
    }
    let pairs: Vec<(String, String)> = futures::stream::iter(unique.into_iter().map(|ip| async move {
        let label = location_label(&ip).await;
        (ip, label)
    }))
    .buffer_unordered(BLACKLIST_GEO_CONCURRENCY)
    .collect()
    .await;
    pairs.into_iter().collect()
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
        Ok(items) => {
            // BLACKLIST_UX_V1（fix58）：被封鎖的 IP 也標上位置——管理員一眼看得出
            // 「這批自動封鎖是從哪裡打過來的」。
            let labels = location_labels(items.iter().map(|row| row.ip.clone()).collect()).await;
            let items: Vec<Value> = items
                .into_iter()
                .map(|row| {
                    let location = labels
                        .get(row.ip.trim())
                        .cloned()
                        .unwrap_or_else(|| "未知位置".to_string());
                    json!({
                        "ip": row.ip,
                        "blocked_at": row.blocked_at,
                        "blocked_time": row.blocked_time,
                        "blocked_by": row.blocked_by,
                        "reason": row.reason,
                        "failure_count": row.failure_count,
                        "location": location,
                    })
                })
                .collect();
            (StatusCode::OK, Json(json!({ "items": items }))).into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("讀取 IP 黑名單失敗：{error}") })),
        )
            .into_response(),
    }
}

/// 最近登入失敗（含位置、裝置，以及「是否已封鎖」）。
///
/// 為什麼不直接用 `/admin/api/activity/login`：那個端點是活動頁共用的原始事件，
/// 沒有位置（每次查 geoip 都要外部服務，活動頁一次 200 筆會打爆），也不知道黑名單。
/// 這裡只回失敗事件、只查去重後的少量 IP，並把「已封鎖」算好給前端。
pub(super) async fn admin_blacklist_recent_logins_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminBlacklistRecentQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    let scan = query.limit.unwrap_or(300).clamp(1, 1000);
    let events = match state.accounts_db.list_activity_events(Some("login"), scan).await {
        Ok(events) => events,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("讀取登入事件失敗：{error}") })),
            )
                .into_response();
        }
    };
    // 只留「失敗」——與前端原本的判斷同一套規則（action=login_failed 或 result 非成功）。
    let failed: Vec<_> = events
        .into_iter()
        .filter(|row| {
            let action = row.action.trim().to_ascii_lowercase();
            let result = row.result.trim().to_ascii_lowercase();
            action == "login_failed"
                || matches!(result.as_str(), "fail" | "failed" | "error" | "false")
                || (action == "login" && result != "success")
        })
        .take(BLACKLIST_RECENT_MAX_ROWS)
        .collect();

    // 已封鎖名單（同一個畫面下方的表）；黑名單本身有上限，這裡直接整份載入。
    let blocked_ips: std::collections::HashSet<String> = state
        .accounts_db
        .list_ip_blacklist(4096)
        .await
        .map(|rows| rows.into_iter().map(|row| row.ip.trim().to_string()).collect())
        .unwrap_or_default();

    let labels = location_labels(failed.iter().map(|row| row.ip.clone()).collect()).await;

    let items: Vec<Value> = failed
        .iter()
        .map(|row| {
            let ip = row.ip.trim().to_string();
            let location = labels
                .get(&ip)
                .cloned()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "未知位置".to_string());
            // 裝置：User-Agent 摘要（瀏覽器不會給 MAC，這是真正拿得到的裝置資訊）。
            // 兩邊都認不得時不要顯示「其他 · 其他瀏覽器」這種廢話，直接說「未知裝置」；
            // 只認得一半時就只顯示認得的那一半。
            let device = if row.user_agent.trim().is_empty() {
                "未記錄".to_string()
            } else {
                let (os, browser) = super::devices::summarize_user_agent(&row.user_agent);
                match (os.as_str(), browser.as_str()) {
                    ("其他", "其他瀏覽器") => "未知裝置".to_string(),
                    ("其他", known_browser) => known_browser.to_string(),
                    (known_os, "其他瀏覽器") => known_os.to_string(),
                    (known_os, known_browser) => format!("{known_os} · {known_browser}"),
                }
            };
            json!({
                "id": row.id,
                "time": row.time,
                "timestamp": row.timestamp,
                "account": if row.account_id.trim().is_empty() { row.actor.clone() } else { row.account_id.clone() },
                "ip": ip,
                // 位置：伺服器先算好（前端不必、也不能自己查）。
                "location": location,
                "device": device,
                // 已封鎖的 IP：前端據此不顯示「加入黑名單」按鈕（改顯示已封鎖標籤）。
                "blocked": blocked_ips.contains(ip.trim()),
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "items": items,
            "blocked_count": blocked_ips.len(),
            "scanned": scan,
        })),
    )
        .into_response()
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
                    user_agent: String::new(),
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
