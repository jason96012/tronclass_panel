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
//!
//! ADMIN_GEO_RESTRICT_V1：「僅限台灣 IP 存取 /admin」開關（`admin_geo_restrict_
//! get_handler`／`admin_geo_restrict_set_handler`），放在同一個檔案、同一個頁面
//! ——與 IP 黑名單同屬「誰可以連進管理後台」這個問題，不另開一支模組或一個頁面。
//! 實際擋人的中介層在 `mod.rs` 的 `reject_foreign_admin_ip_middleware`。

use super::*;
use super::pages::{inject_auth_attrs, inject_theme, ADMIN_BLACKLIST_HTML};
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
    // CSRF_TOKEN_V1：這頁原本不注入任何 session 屬性到 HTML。黑名單/地理限制/
    // 放行審核等按鈕都是寫入操作，一併補上 data-csrf，讓它們能通過新的顯式
    // CSRF 檢查。
    html_page_response(
        &headers,
        inject_auth_attrs(&inject_theme(ADMIN_BLACKLIST_HTML), &session),
    )
}

/// 位置描述：私網／回送位址不必問外部服務，直接給確定的答案（不會再出現「無法取得」）。
///
/// `pub(super)`：ADMIN_GEO_RESTRICT_V1 的國外 IP 封鎖中介層（`mod.rs`）也要判斷
/// 「這個 IP 是不是區網內部」，刻意重用這裡既有的邏輯，不要再長出第三份一樣的
/// 私網判斷（`mod.rs` 的 `check_unusual_login_location` 裡其實已經有一份幾乎一樣
/// 但獨立內嵌的邏輯，是既有的技術債，非本次改動範圍，不在這裡一併處理）。
pub(super) fn is_private_or_local_ip(ip: &str) -> bool {
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
    // IP_BLACKLIST_V6_GROUP_V1：黑名單列可能是 IPv6 /64 群組鍵（例如
    // `2001:db8:...::/64`）；前綴位址本身就足以代表這段的地理位置。
    let ip = ip.split_once('/').map(|(addr, _)| addr).unwrap_or(ip);
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

/// IP_BLACKLIST_AUTO_REDIRECT_V1：黑名單擋下頁面（`blocked.html`）輪詢用——
/// 跟 geo-restrict 那支 `admin_geo_access_status_handler` 是同一個道理，只是
/// 這裡查的是「IP 還在不在黑名單」。一樣刻意不需要 session（被擋的人本來就
/// 登入不了），一樣要排進 `reject_blacklisted_ip_middleware` 與
/// `reject_foreign_admin_ip_middleware` 兩層中介層的豁免清單，否則被擋的人
/// 連這支都連不到；限流數字一樣給寬鬆一點，因為是設計成被定時輪詢。
const BLACKLIST_STATUS_RATE_LIMIT: u32 = 30;

pub(super) async fn admin_blacklist_status_handler(
    State(state): State<AdminWebState>,
    ClientIp(ip): ClientIp,
) -> Response {
    if ip == "unknown" || is_private_or_local_ip(&ip) {
        return (StatusCode::OK, Json(json!({ "blacklisted": false }))).into_response();
    }
    if !state
        .rate_limiter
        .check_limit(
            &format!(
                "blacklist-status-ip:{}",
                crate::trusted_ip::blacklist_group_key(&ip)
            ),
            BLACKLIST_STATUS_RATE_LIMIT,
        )
        .await
    {
        return rate_limited_response();
    }
    match state.accounts_db.is_ip_blacklisted(&ip).await {
        Ok(blacklisted) => {
            (StatusCode::OK, Json(json!({ "blacklisted": blacklisted }))).into_response()
        }
        Err(error) => {
            // 查詢失敗：誠實回報「還在黑名單」（不要讓前端因為這一次查詢失敗
            // 就誤判成已經解除、觸發不該發生的自動跳轉），下一次輪詢再試。
            warn!(error = %error, "查詢 IP 黑名單狀態失敗");
            (StatusCode::OK, Json(json!({ "blacklisted": true }))).into_response()
        }
    }
}

// ─── ADMIN_GEO_RESTRICT_V1：「僅限台灣 IP 存取 /admin」開關 ─────────────────

#[derive(Debug, Deserialize)]
pub(super) struct AdminGeoRestrictRequest {
    enabled: bool,
}

/// 讀取目前的開關狀態＋生效中的「本國」國家清單（供前端顯示，例如
/// 「目前僅允許：TW」），不需要查任何外部服務、單純讀資料庫＋設定檔。
pub(super) async fn admin_geo_restrict_get_handler(
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
    let enabled = super::geo_restrict_admin_enabled(&state).await;
    let home_countries = super::geo_restrict_home_countries();
    (
        StatusCode::OK,
        Json(json!({ "enabled": enabled, "home_countries": home_countries })),
    )
        .into_response()
}

pub(super) async fn admin_geo_restrict_set_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<AdminGeoRestrictRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    let value = if request.enabled { "1" } else { "0" };
    match state
        .accounts_db
        .set_security_setting(crate::db::GEO_RESTRICT_ADMIN_KEY, value, &session.account_id)
        .await
    {
        Ok(()) => {
            // SECURITY_SETTING_CACHE_V1：立刻清快取，下一個請求就看得到新值，
            // 不必被動等 10 秒 TTL 過期。
            super::invalidate_security_toggle_cache(crate::db::GEO_RESTRICT_ADMIN_KEY).await;
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "geo_restrict_admin_toggle",
                None,
                None,
                if request.enabled {
                    "Web 管理頁開啟「僅限台灣 IP 存取」"
                } else {
                    "Web 管理頁關閉「僅限台灣 IP 存取」"
                },
            );
            let _ = state
                .accounts_db
                .record_activity_event(crate::db::ActivityEventInput {
                    kind: "security".to_string(),
                    actor: session.account_id.clone(),
                    action: "geo_restrict_admin_toggle".to_string(),
                    account_id: String::new(),
                    path: String::new(),
                    result: "success".to_string(),
                    details: format!(
                        "僅限台灣 IP 存取 /admin：{}",
                        if request.enabled { "開啟" } else { "關閉" }
                    ),
                    ip: String::new(),
                    user_agent: String::new(),
                })
                .await;
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "enabled": request.enabled })),
            )
                .into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("更新「僅限台灣 IP 存取」開關失敗：{error}") })),
        )
            .into_response(),
    }
}

/// HOSTING_IP_BLOCK_V1：讀取「封鎖雲端／VPS機房 IP」開關目前狀態，與
/// `admin_geo_restrict_get_handler` 同一種風格、不需要額外參數（沒有「本國
/// 清單」這種概念，是或否而已）。
pub(super) async fn admin_hosting_restrict_get_handler(
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
    let enabled = super::hosting_ip_block_enabled(&state).await;
    (StatusCode::OK, Json(json!({ "enabled": enabled }))).into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminHostingRestrictRequest {
    enabled: bool,
}

/// HOSTING_IP_BLOCK_V1：切換「封鎖雲端／VPS機房 IP」開關。
pub(super) async fn admin_hosting_restrict_set_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<AdminHostingRestrictRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    let value = if request.enabled { "1" } else { "0" };
    match state
        .accounts_db
        .set_security_setting(crate::db::HOSTING_IP_BLOCK_KEY, value, &session.account_id)
        .await
    {
        Ok(()) => {
            // SECURITY_SETTING_CACHE_V1：立刻清快取，下一個請求就看得到新值，
            // 不必被動等 10 秒 TTL 過期。
            super::invalidate_security_toggle_cache(crate::db::HOSTING_IP_BLOCK_KEY).await;
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "hosting_ip_block_toggle",
                None,
                None,
                if request.enabled {
                    "Web 管理頁開啟「封鎖雲端／VPS機房 IP」"
                } else {
                    "Web 管理頁關閉「封鎖雲端／VPS機房 IP」"
                },
            );
            let _ = state
                .accounts_db
                .record_activity_event(crate::db::ActivityEventInput {
                    kind: "security".to_string(),
                    actor: session.account_id.clone(),
                    action: "hosting_ip_block_toggle".to_string(),
                    account_id: String::new(),
                    path: String::new(),
                    result: "success".to_string(),
                    details: format!(
                        "封鎖雲端／VPS機房 IP：{}",
                        if request.enabled { "開啟" } else { "關閉" }
                    ),
                    ip: String::new(),
                    user_agent: String::new(),
                })
                .await;
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "enabled": request.enabled })),
            )
                .into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("更新「封鎖雲端／VPS機房 IP」開關失敗：{error}") })),
        )
            .into_response(),
    }
}

/// HOSTING_IP_RECENT_BLOCKS_V1：黑名單頁「最近被雲端／VPS機房 IP 擋下」清單，
/// 比照 `admin_blacklist_recent_logins_handler` 的風格，但資料來源是
/// `crate::hosting_ip` 模組內的記憶體環狀緩衝區，不是 `activity_events`
/// （原因見該模組文件 HOSTING_IP_RECENT_BLOCKS_V1 一節）。純展示用途、
/// 重啟後會清空，不是正式稽核紀錄。
pub(super) async fn admin_hosting_recent_blocks_handler(
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
    let limit = query.limit.unwrap_or(100).clamp(1, 200) as usize;
    let items: Vec<Value> = crate::hosting_ip::recent_blocks(limit)
        .into_iter()
        .map(|row| {
            json!({
                "time": row.time,
                "timestamp": row.timestamp,
                "ip": row.ip,
                "path": row.path,
                "source": row.source,
            })
        })
        .collect();
    (StatusCode::OK, Json(json!({ "items": items }))).into_response()
}

/// ADMIN_GEO_RESTRICT_V1：訪客在 `geo_blocked.html` 按「聯絡管理員」打的端點。
/// **刻意不檢查 session**——會被這層擋下的人，定義上就是登入不了的人，若要求
/// 先登入就等於這顆按鈕永遠按不到。安全性靠三點保證，不是「要不要登入」：
/// ① 這裡只會新增一筆「待處理」申請並發通知，不會讓任何人多拿到任何存取權限；
/// ② 真正放行一定要管理員本人在 Discord 按鈕或 `/admin/blacklist` 頁面按過
///    才會生效；③ 速率限制＋15 分鐘內同 IP 去重，防止被拿來洗版灌爆通知。
#[derive(Debug, Deserialize, Default)]
pub(super) struct AdminGeoAccessRequestBody {
    #[serde(default)]
    path: String,
}

/// 同一來源 IP（以黑名單群組鍵計）一分鐘最多幾次——防止誤觸發連點或惡意腳本
/// 對這支公開端點灌爆請求；真正避免「重複通知」靠的是 DB 層 15 分鐘去重。
const GEO_ACCESS_REQUEST_IP_RATE_LIMIT: u32 = 3;

/// GEO_ACCESS_BURST_MUTE_V1：單一 IP 限流擋不住「很多不同 IP 一起打」的洗版
/// ——這裡換個角度看「這段時間內有幾個不同 IP 在申請」，超過視為異常。
const GEO_ACCESS_BURST_WINDOW_SECS: i64 = 10 * 60;
/// 10 分鐘內超過這個數字的不同 IP 申請，視為洗版，之後新申請改成靜音
/// （只在觸發當下發一次彙總警示），直到管理員按「清空並恢復通知」為止。
const GEO_ACCESS_BURST_THRESHOLD: i64 = 5;

pub(super) async fn admin_geo_access_request_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    body: axum::body::Bytes,
) -> Response {
    // 請求體是可選的（純點按鈕也可能完全不帶 body）；解析失敗一律當作沒有帶
    // path，不要因為格式錯誤就把整個申請擋掉——這支端點本來就不相信呼叫端。
    let request_path = serde_json::from_slice::<AdminGeoAccessRequestBody>(&body)
        .map(|b| b.path)
        .unwrap_or_default();
    // 私網／本機位址或解析不出 IP：這層限制本來就不會擋這種來源，沒有「申請」
    // 的意義，也避免內部呼叫誤觸發通知。
    if ip == "unknown" || is_private_or_local_ip(&ip) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "not_applicable" })),
        )
            .into_response();
    }
    if !state
        .rate_limiter
        .check_limit(
            &format!(
                "geo-access-request-ip:{}",
                crate::trusted_ip::blacklist_group_key(&ip)
            ),
            GEO_ACCESS_REQUEST_IP_RATE_LIMIT,
        )
        .await
    {
        return rate_limited_response();
    }
    let cf_ipcountry = super::cf_ipcountry_header(&headers);
    let geo = crate::geoip::resolve(&ip, cf_ipcountry.as_deref()).await;
    let location = geo.label();
    match state
        .accounts_db
        .create_or_reuse_geo_access_request(&ip, &location, &request_path)
        .await
    {
        Ok((row, is_new)) => {
            if is_new {
                // GEO_ACCESS_BURST_MUTE_V1：先看是不是已經在靜音中（有人之前
                // 觸發過洗版警示、管理員還沒按「清空並恢復通知」）——靜音中
                // 完全不發任何通知，只留稽核紀錄，避免管理員被連續灌爆。
                let muted = state
                    .accounts_db
                    .get_security_setting(crate::db::GEO_ACCESS_NOTIFY_MUTED_KEY)
                    .await
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some("1");
                if muted {
                    crate::logs::record_audit(
                        "web",
                        "",
                        "geo_access_request_created_muted",
                        None,
                        None,
                        &format!("國外 IP {ip}（{location}）申請暫時放行（通知靜音中）"),
                    );
                } else {
                    let recent_ip_count = state
                        .accounts_db
                        .count_recent_geo_access_ips(GEO_ACCESS_BURST_WINDOW_SECS)
                        .await
                        .unwrap_or(0);
                    if recent_ip_count > GEO_ACCESS_BURST_THRESHOLD {
                        // 觸發靜音：寫旗標 + 發一次彙總警示，取代這次（以及之後）
                        // 原本要逐筆發的個別通知。
                        if let Err(error) = state
                            .accounts_db
                            .set_security_setting(
                                crate::db::GEO_ACCESS_NOTIFY_MUTED_KEY,
                                "1",
                                "system",
                            )
                            .await
                        {
                            warn!(error = %error, "寫入國外存取申請靜音旗標失敗");
                        }
                        super::notify_admins_geo_access_burst(&state, recent_ip_count).await;
                        crate::logs::record_audit(
                            "web",
                            "",
                            "geo_access_request_burst_muted",
                            None,
                            None,
                            &format!(
                                "10 分鐘內已有 {recent_ip_count} 個不同來源 IP 申請暫時放行，\
                                 已靜音逐筆通知（本次：{ip} / {location}）"
                            ),
                        );
                    } else {
                        let event = crate::adapters::events::GeoAccessRequestEvent {
                            request_id: row.id.clone(),
                            ip: ip.clone(),
                            location: row.location.clone(),
                            path: row.path.clone(),
                            timestamp: row.requested_time.clone(),
                        };
                        super::notify_admins_geo_access_request(&state, event).await;
                        crate::logs::record_audit(
                            "web",
                            "",
                            "geo_access_request_created",
                            None,
                            None,
                            &format!("國外 IP {ip}（{location}）申請暫時放行"),
                        );
                    }
                }
            }
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "already_pending": !is_new })),
            )
                .into_response()
        }
        Err(error) => {
            warn!(error = %error, "建立國外存取申請失敗");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal_error" })),
            )
                .into_response()
        }
    }
}

/// GEO_ACCESS_AUTO_REDIRECT_V1：被擋頁面輪詢用——每隔幾秒問一次「我現在算不算
/// 已被放行」，拿到 `true` 就自己重新整理，不用等使用者手動重新整理。這支
/// 查詢頻率比申請按鈕高很多（被擋頁面一開就會定時打），限流數字刻意抓寬鬆
/// 一點；查不到 IP／私網位址一律回 `false`（沒有「放行」的意義，不是錯誤）。
const GEO_ACCESS_STATUS_RATE_LIMIT: u32 = 30;

pub(super) async fn admin_geo_access_status_handler(
    State(state): State<AdminWebState>,
    ClientIp(ip): ClientIp,
) -> Response {
    if ip == "unknown" || is_private_or_local_ip(&ip) {
        return (StatusCode::OK, Json(json!({ "allowed": false }))).into_response();
    }
    if !state
        .rate_limiter
        .check_limit(
            &format!(
                "geo-access-status-ip:{}",
                crate::trusted_ip::blacklist_group_key(&ip)
            ),
            GEO_ACCESS_STATUS_RATE_LIMIT,
        )
        .await
    {
        return rate_limited_response();
    }
    match state.accounts_db.geo_access_is_ip_allowed(&ip).await {
        Ok(allowed) => (StatusCode::OK, Json(json!({ "allowed": allowed }))).into_response(),
        Err(error) => {
            // 查詢失敗：回報「還沒放行」，讓前端繼續輪詢，不要因為這一次查詢
            // 失敗就誤導使用者（這裡不是要擋存取，只是誠實回報查不到）。
            warn!(error = %error, "查詢國外存取放行狀態失敗");
            (StatusCode::OK, Json(json!({ "allowed": false }))).into_response()
        }
    }
}

/// `/admin/blacklist` 頁面的備用清單——Discord 收不到通知、或管理員比較常看
/// 網頁時，一樣可以在這裡核准／忽略，不是只能靠 Discord 按鈕。
const GEO_ACCESS_PENDING_LIST_LIMIT: i64 = 100;

pub(super) async fn admin_geo_access_pending_handler(
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
    let muted = state
        .accounts_db
        .get_security_setting(crate::db::GEO_ACCESS_NOTIFY_MUTED_KEY)
        .await
        .ok()
        .flatten()
        .as_deref()
        == Some("1");
    match state
        .accounts_db
        .list_pending_geo_access_requests(GEO_ACCESS_PENDING_LIST_LIMIT)
        .await
    {
        Ok(rows) => {
            let items: Vec<Value> = rows
                .into_iter()
                .map(|row| {
                    json!({
                        "id": row.id,
                        "ip": row.ip,
                        "location": row.location,
                        "path": row.path,
                        "requested_time": row.requested_time,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({ "items": items, "muted": muted })),
            )
                .into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("讀取待處理申請失敗：{error}") })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminGeoAccessActionRequest {
    id: String,
}

pub(super) async fn admin_geo_access_approve_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<AdminGeoAccessActionRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    match state
        .accounts_db
        .approve_geo_access_request(
            &request.id,
            crate::db::GEO_ACCESS_ALLOW_SECONDS,
            &session.account_id,
        )
        .await
    {
        Ok(Some(row)) => {
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "geo_access_request_approved",
                None,
                None,
                &format!("Web 管理頁核准國外 IP {} 暫時放行", row.ip),
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "ip": row.ip, "allowed_until": row.allowed_until })),
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "not_found_or_already_handled" })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("核准失敗：{error}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_geo_access_dismiss_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<AdminGeoAccessActionRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    match state
        .accounts_db
        .dismiss_geo_access_request(&request.id, &session.account_id)
        .await
    {
        Ok(true) => {
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "geo_access_request_dismissed",
                None,
                None,
                "Web 管理頁忽略一筆國外存取申請",
            );
            (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "not_found_or_already_handled" })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("忽略失敗：{error}") })),
        )
            .into_response(),
    }
}

/// GEO_ACCESS_BURST_MUTE_V1：洗版時的「清空並恢復通知」按鈕——把目前所有
/// 待處理申請一次標成忽略（維持擋下，不放行任何人）並解除靜音旗標，之後
/// 新申請會恢復逐筆通知。跟核准／忽略一樣要求已登入的管理員身分。
pub(super) async fn admin_geo_access_clear_all_handler(
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
    let cleared = match state
        .accounts_db
        .dismiss_all_pending_geo_access_requests(&session.account_id)
        .await
    {
        Ok(count) => count,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("清空失敗：{error}") })),
            )
                .into_response();
        }
    };
    // 不論清空前靜音與否都重設成「未靜音」，恢復逐筆通知；寫入失敗只記警告，
    // 不讓已經成功的清空動作因此回報失敗（清單本身確實清乾淨了）。
    if let Err(error) = state
        .accounts_db
        .set_security_setting(crate::db::GEO_ACCESS_NOTIFY_MUTED_KEY, "0", &session.account_id)
        .await
    {
        warn!(error = %error, "解除國外存取申請靜音旗標失敗");
    }
    crate::logs::record_audit(
        "web",
        &session.account_id,
        "geo_access_request_cleared_all",
        None,
        None,
        &format!("Web 管理頁一鍵清空 {cleared} 筆待處理國外存取申請並恢復通知"),
    );
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "cleared": cleared })),
    )
        .into_response()
}
