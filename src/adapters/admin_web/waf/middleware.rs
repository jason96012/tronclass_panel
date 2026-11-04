//! WAF 中介層組裝、私網告警與錯誤回應。

use super::*;
use std::sync::atomic::Ordering;
use super::fingerprint::{fingerprint_pair, is_fingerprint_signal_status, next_fingerprint_block, truncate_to};

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde_json::json;
use tracing::warn;

// WAF_AUTO_BLOCK_NOTIFY_V2：WAF 新建立自動封鎖時，透過 notify_waf_auto_block 只發 Discord 給
// admin_user_id（同一 IP 24 小時去重），附「解除封鎖」按鈕。不發 LINE、不發 Web Push。
use crate::adapters::admin_web::AdminWebState;
use crate::db::ActivityEventInput;


/// 私網（非 loopback）peer 會被 `is_local_or_trusted_ip` 整體豁免限速與自動封鎖。
/// 若它其實是反向代理，而 `trusted_proxies` 沒有設定，所有外部請求都會被當成代理的私網 IP，
/// 防護會靜默失效（WAF 審查 W5）。這裡只記一次 WARN，不改變豁免行為。
pub(super) fn warn_private_peer_without_trusted_proxies(waf: &WafEngine, peer: Option<std::net::IpAddr>) {
    let Some(peer) = peer.map(crate::trusted_ip::canonicalize_ip) else { return };
    if peer.is_loopback() || !is_local_or_trusted_ip(&peer.to_string()) {
        return;
    }
    if waf.private_peer_warned.load(Ordering::Relaxed) {
        return;
    }
    let proxies_configured = crate::config::try_get_config()
        .map(|cfg| !cfg.adapters.admin_web.trusted_proxies.is_empty())
        .unwrap_or(false);
    if !proxies_configured && !waf.private_peer_warned.swap(true, Ordering::Relaxed) {
        warn!(
            "🛡️ [WAF] 請求來自私網 IP（{}），但 trusted_proxies 未設定。若它是反向代理，所有外部請求都會被視為私網而豁免限速與自動封鎖；請在 config.toml 的 adapters.admin_web.trusted_proxies 明確列出代理。",
            peer
        );
    }
}

/// WAF 防火牆核心中介層。
pub async fn waf_firewall_middleware(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // WAF_ENGINE_DI_V1：所有狀態經由注入的引擎實例取得（不再使用全域 static）。
    let waf = state.waf.clone();
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());

    // WAF_TRUSTED_PROXY_CONFIG_V1：必須與全站同一份信任清單
    // （`trusted_from_global_config`），不能寫死 `from_config(&[])`——
    // 否則營運者在 config.toml 額外宣告的信任代理（例如直連 Cloudflare 時
    // `trusted_proxies = ["cloudflare"]`）在 WAF 眼裡會失效：所有請求都被當成
    // 「代理自己的 IP」，真實攻擊者洗成別人；更糟的是自動封鎖會把**代理本身**
    // 關進黑名單 → 全站對所有人斷線。
    let trusted = crate::trusted_ip::trusted_from_global_config();
    warn_private_peer_without_trusted_proxies(&waf, peer);
    let ip = crate::trusted_ip::resolve_client_ip(peer, &headers, &trusted);
    let uri = request.uri().clone();
    let method = request.method().clone();
    let raw_path = uri.path().to_string();

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // 0. WAF 白名單：白名單 IP/CIDR 或路徑前綴直接免檢查放行。
    {
        let ip_str = ip.to_string();
        let path_whitelisted = waf.whitelist.is_path_whitelisted(&raw_path).await;
        let ip_whitelisted = waf.whitelist.is_ip_whitelisted(&ip_str).await;
        if ip_whitelisted || path_whitelisted {
            waf.telemetry.whitelist_bypass.fetch_add(1, Ordering::Relaxed);
            return next.run(request).await;
        }
    }

    // 0b. 總體請求大小限制：依 Content-Length 標頭擋下過大請求。
    if let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > WAF_MAX_REQUEST_SIZE {
            waf.telemetry.total_blocked.fetch_add(1, Ordering::Relaxed);
            warn!(ip = %ip, path = %raw_path, size = %len, "🛡️ [WAF] 請求大小超過上限，已拒絕");
            return waf_error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({
                    "error": "payload_too_large",
                    "message": "WAF: 請求大小超過系統上限，已拒絕"
                }),
            );
        }
    }

    // 0c. 標頭欄位數量限制：防止標頭洪水攻擊。
    if headers.len() > WAF_MAX_HEADERS_COUNT {
        waf.telemetry.total_blocked.fetch_add(1, Ordering::Relaxed);
        warn!(ip = %ip, path = %raw_path, headers = %headers.len(), "🛡️ [WAF] 標頭欄位數量過多，已拒絕");
        return waf_error_response(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "too_many_headers",
                "message": "WAF: 請求標頭欄位數量超過上限，已拒絕"
            }),
        );
    }

    // 0c. 請求指紋（指紋＋IP 群組）。使用者規則（2026-11-04）：
    //   指紋命中 → 403，並依次數指數延長的時間封鎖（5 分鐘起，每次加倍，上限 24 小時）。
    //   時間封鎖不是永久封鎖；管理員收到通知可按「放行此組合」，放行後 30 天內不因指紋被封鎖（R-03）。
    //   鍵綁定 IP 群組，所以放行只影響該 IP 群組，不影響同指紋的其他人。
    let fingerprint = request_fingerprint(&headers);
    let fp_pair = fingerprint_pair(&fingerprint, &ip.to_string());
    let fingerprint_suspicious = waf.fingerprint
        .record_and_check(&fingerprint, &ip.to_string())
        .await;
    let now_unix = chrono::Local::now().timestamp();
    let fp_row = match state.accounts_db.get_fingerprint_block(&fp_pair).await {
        Ok(row) => row,
        Err(error) => {
            // 與黑名單檢查一致：讀取失敗時略過本次封鎖查詢（會記錄警告）。
            warn!(ip = %ip, error = %error, "WAF 指紋封鎖紀錄讀取失敗，本次略過指紋封鎖檢查");
            None
        }
    };
    // 已驗證的 Google 官方爬蟲 IP（依 Google 公布的範圍，不看 UA，無法偽造）不套用指紋封鎖：
    // 否則 Googlebot 分散在大量 IP 上，會讓指紋被判可疑並逐次延長封鎖。只豁免這一層；
    // URL／標頭特徵、本體與限流檢查照常執行。
    let fp_exempt = crate::hosting_ip::is_verified_google_crawler(&ip.to_string())
        || fp_row.as_ref().is_some_and(|r| r.exempt_active(now_unix));
    if !fp_exempt {
        let blocked_now = fp_row.as_ref().is_some_and(|r| r.blocked_until > now_unix);
        if blocked_now {
            waf.telemetry.fingerprint_blocked.fetch_add(1, Ordering::Relaxed);
            return waf_forbidden_response(json!({
                "error": "waf_access_denied",
                "message": "WAF: 請求指紋已被暫時封鎖，請稍後再試"
            }));
        }
        if fingerprint_suspicious && waf.fingerprint.has_recent_signal(&fp_pair).await {
            let prev_strikes = fp_row.as_ref().map_or(0, |r| r.strikes);
            let prev_until = fp_row.as_ref().map_or(0, |r| r.blocked_until);
            let (strikes, blocked_until) = next_fingerprint_block(prev_strikes, prev_until, now_unix);
            let ip_group = crate::trusted_ip::blacklist_group_key(&ip.to_string());
            let preview = truncate_to(&fingerprint, 200);
            if let Err(error) = state
                .accounts_db
                .record_fingerprint_block(&fp_pair, &ip_group, &preview, strikes, blocked_until, now_unix)
                .await
            {
                warn!(ip = %ip, error = %error, "WAF 指紋封鎖紀錄寫入失敗（本次仍回 403）");
            }
            waf.telemetry.fingerprint_blocked.fetch_add(1, Ordering::Relaxed);
            warn!(
                ip = %ip,
                ip_group = %ip_group,
                strikes = %strikes,
                blocked_until = %blocked_until,
                "🛡️ [WAF] 指紋可疑，已暫時封鎖（指數時間，可由管理員放行）"
            );
            let blocked_until_label = chrono::TimeZone::timestamp_opt(&chrono::Local, blocked_until, 0)
                .single()
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();
            let event = crate::adapters::events::FingerprintBlockEvent {
                pair: fp_pair.clone(),
                ip_group,
                fingerprint_preview: preview,
                strikes,
                blocked_until: blocked_until_label,
                timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            };
            let notify_state = state.clone();
            tokio::spawn(async move {
                crate::adapters::admin_web::notify_fingerprint_block(&notify_state, event).await;
            });
            return waf_forbidden_response(json!({
                "error": "waf_access_denied",
                "message": "WAF: 請求指紋可疑，已被系統防火牆暫時攔截"
            }));
        }
    }

    // 1. 深度威脅特徵、蜜罐、黑客工具與惡意標頭檢測
    if let Some(threat) = waf.inspect_threat(&uri, &method, &headers) {
        // R-02：威脅命中是攻擊信號，記到組合上（指紋封鎖需要信號才會生效）。
        waf.fingerprint.mark_signal(&fp_pair).await;
        threat.record_telemetry(&waf.telemetry);
        let threat_desc = threat.description();
        warn!(
            ip = %ip,
            path = %raw_path,
            threat = %threat_desc,
            "🛡️ [最高級 WAF 防火牆] 攔截並阻斷惡意攻擊請求！"
        );

        // 非本機/私網 IP 自動觸發自動黑名單封鎖與安全告警
        let is_local = is_local_or_trusted_ip(&ip) || ip == "unknown";
        if !is_local && threat.allows_auto_ban() {
            // WAF_BAN_DEDUP_V1：黑名單中介層在 WAF **之後**才跑，因此已被封鎖的
            // IP 之後的每個請求仍會走到這裡。若不看「是否已封鎖」就照單全收地
            // spawn，攻擊者拿著已封鎖的 IP 對蜜罐路徑狂打，就能造成：
            // ① 每請求一個 spawn　② 每請求一筆活動日誌　③ 每請求一則管理員
            // Discord／Web Push 告警——變成對告警與資料庫的放大 DoS。
            // 先查一次黑名單（主鍵 EXISTS），已封鎖者靜默擋下（回應照舊 403），
            // 把「封鎖＋稽核＋告警」留給第一次。
            let ban_key = crate::trusted_ip::blacklist_group_key(&ip);
            let already_banned = state
                .accounts_db
                .is_ip_blacklisted(&ban_key)
                .await
                .unwrap_or(false);
            if !already_banned {
                let db = state.accounts_db.clone();
                let block_ip = ban_key.clone();
                let alert_desc = threat_desc.clone();
                let path_for_log = raw_path.clone();
                let path_for_notice = raw_path.clone();
                let ua_for_log = user_agent.clone();
                let alert_state = state.clone();

                tokio::spawn(async move {
                    // 自動將惡意攻擊者 IP 寫入永久黑名單；只有「新建立」的封鎖
                    // 才留稽核軌跡（INSERT OR IGNORE 回傳是否新列），
                    // 與上面的 already_banned 併用可擋掉併發下的重複寫入。
                    if let Ok(true) = db
                        .block_ip_with_failure_count(
                            &block_ip,
                            "system:waf-firewall-max",
                            &format!("最高級 WAF 防火牆自動封鎖: {alert_desc}"),
                            1,
                        )
                        .await
                    {
                        // 寫入後台安全活動日誌
                        let _ = db
                            .record_activity_event(ActivityEventInput {
                                kind: "security".to_string(),
                                actor: "system:waf-firewall-max".to_string(),
                                action: "waf_threat_blocked".to_string(),
                                account_id: "unknown".to_string(),
                                path: path_for_log,
                                result: "blocked".to_string(),
                                details: format!("最高級 WAF 攔截威脅並自動封鎖: {alert_desc}"),
                                ip: block_ip.clone(),
                                user_agent: ua_for_log,
                            })
                            .await;

                        // WAF_AUTO_BLOCK_NOTIFY_V2：只有「新建立」的封鎖才通知（上面 Ok(true) 已保證）。
                        // 攻擊特徵類為永久封鎖，因此通知說明「需按按鈕才會解除」。
                        // 同一 IP 24 小時內只通知一次（去重在 notify_waf_auto_block 內），
                        // 避免全網掃描器反覆觸發。
                        crate::adapters::admin_web::notify_waf_auto_block(
                            &alert_state,
                            &block_ip,
                            &alert_desc,
                            &path_for_notice,
                            true,
                        )
                        .await;
                    }
                });
            }
        }

        if matches!(threat, WafThreat::DangerousMethod(_)) {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                Json(json!({
                    "error": "method_not_allowed",
                    "message": "WAF: 危險 HTTP 方法已被系統最高級防火牆阻斷"
                })),
            )
                .into_response();
        }

        return waf_forbidden_response(json!({
                    "error": "waf_access_denied",
                    "message": "WAF: 請求包含惡意攻擊特徵或探測高危路徑，已被系統最高級防火牆攔截並永久封鎖"
                }));
    }

    // 2. 全域 Layer-7 請求限速與防 DoS 洪水防護。
    // IP_BLACKLIST_V6_GROUP_V1：限速鍵與黑名單同一把尺（IPv6 收斂為 /64 群組）——
    // 否則攻擊者在同前綴內輪換位址，每換一個位址就是全新的 240/分鐘額度，
    // 全域限速形同虛設。
    let is_exempt = is_local_or_trusted_ip(&ip) || ip == "unknown";
    if !is_exempt
        && !waf.rate_limiter
            .check_limit(&crate::trusted_ip::blacklist_group_key(&ip))
            .await
    {
        waf.telemetry.rate_limited.fetch_add(1, Ordering::Relaxed);
        warn!(ip = %ip, "🛡️ [最高級 WAF 防火牆] 客戶端觸發全域 L7 速率上限，予以暫時限流！");
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": "rate_limited",
                "message": "WAF: 請求頻率過高，已觸發最高級安全防禦限流，請稍候再試"
            })),
        )
            .into_response();
        response.headers_mut().insert(
            HeaderName::from_static("retry-after"),
            HeaderValue::from_static("60"),
        );
        return response;
    }

    // 3. 請求本體檢查（WAF_BODY_SCAN_V1）。命中只回 403，不寫入黑名單：
    // 本體是使用者輸入（貼程式碼、教材），誤判率高於 URL，不應變成永久封鎖。
    if waf_body_needs_scan(&method, &headers) {
        let limit = waf_body_scan_limit(&raw_path);
        // C5：宣告的 Content-Length 已超過上限，直接 413，不緩衝本體。
        if declared_content_length(&headers).map_or(false, |n| n > limit as u64) {
            return waf_payload_too_large();
        }
        // C5：掃描記憶體預算。等不到就回 503（失敗即關閉）；預算在本區塊結束時釋放。
        let Some(_scan_slot) = acquire_body_scan_slot(&waf, limit).await else {
            return waf_error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({
                    "error": "waf_busy",
                    "message": "WAF: 本體檢查忙碌中，請稍後再試"
                }),
            );
        };
        let (parts, body) = request.into_parts();
        let bytes = match tokio::time::timeout(WAF_BODY_READ_TIMEOUT, axum::body::to_bytes(body, limit)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => return waf_payload_too_large(),
            Err(_) => {
                return waf_error_response(
                    StatusCode::REQUEST_TIMEOUT,
                    json!({
                        "error": "body_read_timeout",
                        "message": "WAF: 請求本體讀取逾時，已拒絕"
                    }),
                );
            }
        };
        let is_form = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().starts_with("application/x-www-form-urlencoded"))
            .unwrap_or(false);
        if let Some(threat) = inspect_waf_body(&String::from_utf8_lossy(&bytes), is_form) {
            // R-02：本體威脅命中也是攻擊信號。
            waf.fingerprint.mark_signal(&fp_pair).await;
            threat.record_telemetry(&waf.telemetry);
            // WAF_BODY_NO_BAN_V1：本體是使用者輸入（程式碼、教材），誤判率高，
            // 依原註解只回 403，不做永久封鎖（先前誤呼叫 ban_for_body_threat 會誤封 IP）。
            warn!(
                ip = %ip,
                path = %raw_path,
                threat = %threat.description(),
                "🛡️ [最高級 WAF 防火牆] 請求本體含攻擊特徵，已攔截（不封鎖）"
            );
            return waf_forbidden_response(json!({
                "error": "waf_access_denied",
                "message": "WAF: 請求內容含攻擊特徵，已被攔截"
            }));
        }
        request = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
    }

    // 安全校驗通過，放行至下游處理器。
    let response = next.run(request).await;

    // 9. 異常行為偵測：追蹤 4xx 回應（不含 403），短時間內異常頻繁則自動封鎖。
    // 
    // 設計考量：
    // - 403 (FORBIDDEN) 是我們自己的中介層（地理封鎖、機房 IP 封鎖、黑名單、認證失敗）合法回傳的存取控制回應，
    //   不應視為異常探測行為。若計入會導致正常被封鎖的用戶重複觸發異常偵測而被二次封鎖。
    // - 真正的掃描器異常行為主要表現為：404 (路徑探測)、400 (惡意請求)、429 (頻率超限) 等。
    // - 已知搜尋引擎爬蟲豁免，避免 Googlebot/Bingbot 被誤封導致「未建立索引」。
    // - WAF_ANOMALY_WHITELIST_V1：聯絡管理員端點預期回 403/429，不應計入異常行為；
    //   與其他中介層的豁免清單保持一致。
    let status = response.status();
    let is_crawler = is_known_crawler(&user_agent);
    // WAF_ANOMALY_EXCLUDE_403_V1：403 是合法存取控制回應，不計入異常偵測
    let is_forbidden = status == StatusCode::FORBIDDEN;
    // WAF_ANOMALY_EXCLUDE_429_V1（R-06）：429 是限流回應，不計入異常偵測。
    // 限流本身若再計入，會形成「限流 → 異常 → 封鎖」的連鎖。
    let is_rate_limited = status == StatusCode::TOO_MANY_REQUESTS;
    // R-02：探測類 4xx（404、400 等）記到組合上。白名單路徑（聯絡管理員等）不算。
    // 放在異常計數條件之外：不因 UA 是爬蟲而豁免（UA 可偽造，豁免會讓攻擊者避開信號）。
    if is_fingerprint_signal_status(status) && !waf_anomaly_whitelisted(&raw_path) {
        waf.fingerprint.mark_signal(&fp_pair).await;
    }
    if status.is_client_error()
        && !is_forbidden
        && !is_rate_limited
        && !is_local_or_trusted_ip(&ip)
        && !is_crawler
        && !waf_anomaly_whitelisted(&raw_path)
    {
        // WAF_ANOMALY_GROUP_KEY_V1（R-01）：異常計數與封鎖使用同一把尺（IPv6 收斂為 /64）。
        // 若以完整位址計數，攻擊者可在同一 /64 內輪換位址，每個位址都低於門檻。
        let anomaly_key = crate::trusted_ip::blacklist_group_key(&ip.to_string());
        // 401 獨立計數（WAF_ANOMALY_401_SEPARATE_V1）：session 過期不應與掃描共用 4xx 門檻。
        let should_ban = if status == StatusCode::UNAUTHORIZED {
            waf.anomaly_401.record_and_check(&anomaly_key).await
        } else {
            waf.anomaly.record_and_check(&anomaly_key).await
        };
        if should_ban {
            waf.telemetry.anomaly_blocked.fetch_add(1, Ordering::Relaxed);
            let ban_key = crate::trusted_ip::blacklist_group_key(&ip.to_string());
            let already_banned = state
                .accounts_db
                .is_ip_blacklisted(&ban_key)
                .await
                .unwrap_or(false);
            if !already_banned {
                let db = state.accounts_db.clone();
                let block_ip = ban_key.clone();
                let path_for_log = raw_path.clone();
                let path_for_notice = raw_path.clone();
                let ua_for_log = user_agent.clone();
                let anomaly_state = state.clone();
                tokio::spawn(async move {
                    if let Ok(true) = db
                        .block_ip_with_failure_count(
                            &block_ip,
                            "system:waf-firewall-max",
                            &format!(
                                "{}: {} 觸發",
                                crate::db::WAF_ANOMALY_REASON_PREFIX,
                                status
                            ),
                            1,
                        )
                        .await
                    {
                        let _ = db
                            .record_activity_event(ActivityEventInput {
                                kind: "security".to_string(),
                                actor: "system:waf-firewall-max".to_string(),
                                action: "waf_anomaly_blocked".to_string(),
                                account_id: "unknown".to_string(),
                                path: path_for_log,
                                result: "blocked".to_string(),
                                details: format!(
                                    "WAF 攔截異常 4xx 行為並自動封鎖: {} (status={})",
                                    ip, status
                                ),
                                ip: block_ip.clone(),
                                user_agent: ua_for_log,
                            })
                            .await;
                        // WAF_AUTO_BLOCK_NOTIFY_V2：異常 4xx 類 30 分鐘後自動解除，通知說明這點。
                        crate::adapters::admin_web::notify_waf_auto_block(
                            &anomaly_state,
                            &block_ip,
                            &format!("異常 4xx 行為（HTTP {}）", status),
                            &path_for_notice,
                            false,
                        )
                        .await;
                    }
                });
            }
        }
    }

    response
}


/// 構建帶有安全響應標頭的 WAF 403 回應（參考 rust-web-security SKILL：防止內容嗅探、點擊劫持、引用洩漏）。
pub(super) fn waf_forbidden_response(body: serde_json::Value) -> Response {
    (
        StatusCode::FORBIDDEN,
        [
            (header::HeaderName::from_static("x-content-type-options"), header::HeaderValue::from_static("nosniff")),
            (header::HeaderName::from_static("x-frame-options"), header::HeaderValue::from_static("DENY")),
            (header::HeaderName::from_static("referrer-policy"), header::HeaderValue::from_static("no-referrer")),
            (header::HeaderName::from_static("content-security-policy"), header::HeaderValue::from_static("default-src 'none'")),
        ],
        Json(body),
    )
        .into_response()
}

/// 構建帶有安全響應標頭的 WAF 錯誤回應。
/// 本體超過檢查上限（C5 預檢或緩衝時超限）：413，不掃描、不放行。
fn waf_payload_too_large() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(json!({
            "error": "payload_too_large",
            "message": "WAF: 請求本體超過檢查上限，已拒絕"
        })),
    )
        .into_response()
}

pub(super) fn waf_error_response(status: StatusCode, body: serde_json::Value) -> Response {
    (
        status,
        [
            (header::HeaderName::from_static("x-content-type-options"), header::HeaderValue::from_static("nosniff")),
            (header::HeaderName::from_static("x-frame-options"), header::HeaderValue::from_static("DENY")),
            (header::HeaderName::from_static("referrer-policy"), header::HeaderValue::from_static("no-referrer")),
            (header::HeaderName::from_static("content-security-policy"), header::HeaderValue::from_static("default-src 'none'")),
        ],
        Json(body),
    )
        .into_response()
}
