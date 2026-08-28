//! 作業清單與完成狀態 handlers。

use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
    /// 使用者按下「重新整理」時帶 `refresh=1`，略過快取強制重抓。
    /// 仍受 single-flight 保護，連點不會重複打上游。
    #[serde(default)]
    refresh: bool,
}

/// 使用者標記／取消標記「作業已完成」的請求。
#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkCompleteRequest {
    #[serde(default)]
    account_id: Option<String>,
    homework_id: u64,
    /// true=標記完成，false=取消標記。
    completed: bool,
}

/// 發送一則測試用的作業截止提醒，驗證各通知管道是否暢通。
///
/// HOMEWORK_TEST_NOTIFICATION_V1
/// 走與真實提醒完全相同的發送路徑（含通知偏好檢查），因此能真實反映
/// 使用者實際會不會收到通知。刻意**不寫入** `homework_reminder_sent`，
/// 所以測試不會影響真實作業的去重狀態。
pub(super) async fn admin_homework_test_notification_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };

    let account_id = session.account_id.clone();
    let Ok(Some(account)) = state.accounts_db.get(&account_id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "找不到帳號資料" })),
        )
            .into_response();
    };

    // 組一則「剩 45 分鐘」的假作業，對應最緊急的 1h 階段。
    let now_local = chrono::Local::now();
    let deadline = now_local + chrono::Duration::minutes(45);
    let event = crate::adapters::events::HomeworkDeadlineEvent {
        account: account_id.clone(),
        course_name: "測試課程".to_string(),
        title: "這是一則測試通知（非真實作業）".to_string(),
        stage_badge: "最後 1 小時".to_string(),
        deadline: deadline.format("%Y-%m-%d %H:%M").to_string(),
        time_remaining: "45 分鐘".to_string(),
    };
    let message = crate::adapters::events::OutboundMessage::HomeworkDeadline(event);

    let mut channels: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    // 1. Discord / LINE（走與真實提醒相同的偏好檢查路徑）
    if account.discord_user_id.trim().is_empty() && account.line_user_id.trim().is_empty() {
        skipped.push("Discord / LINE：此帳號尚未綁定".to_string());
    } else if let Some(messenger) = state.messenger.as_ref() {
        let target = crate::adapters::events::AdapterAccountTarget::new(
            account_id.clone(),
            account.line_user_id.clone(),
            account.discord_user_id.clone(),
        );
        match messenger.push_to_account_or_admin(&target, &message).await {
            Ok(()) => {
                if !account.discord_user_id.trim().is_empty() {
                    channels.push("Discord".to_string());
                }
                if !account.line_user_id.trim().is_empty() {
                    channels.push("LINE".to_string());
                }
            }
            Err(error) => {
                warn!(account = %account_id, %error, "測試通知發送失敗");
                skipped.push(format!("Discord / LINE 發送失敗：{error}"));
            }
        }
    } else {
        skipped.push("Discord / LINE：伺服器未啟用任何 Bot".to_string());
    }

    // 2. 手機 Web Push
    match state
        .accounts_db
        .count_web_push_subscriptions(&account_id)
        .await
    {
        Ok(count) if count > 0 => {
            if let Err(error) = crate::web_push_notifications::send_homework_test(
                &account_id,
                "測試課程",
                "這是一則測試通知（非真實作業）",
                "最後 1 小時",
            )
            .await
            {
                warn!(account = %account_id, %error, "測試 Web Push 發送失敗");
                skipped.push(format!("手機推播發送失敗：{error}"));
            } else {
                channels.push(format!("手機推播（{count} 台裝置）"));
            }
        }
        Ok(_) => skipped.push("手機推播：尚未在此裝置訂閱".to_string()),
        Err(error) => skipped.push(format!("手機推播：讀取訂閱失敗：{error}")),
    }

    info!(
        account = %account_id,
        channels = ?channels,
        "已發送作業提醒測試通知"
    );

    Json(json!({
        "ok": !channels.is_empty(),
        "channels": channels,
        "skipped": skipped,
    }))
    .into_response()
}

/// 實際向 Tronclass 抓取作業清單（快取未命中時才會執行）。
///
/// 先用已儲存的 session cookie 直接查詢；只有真的遇到認證錯誤才走完整登入，
/// 藉此避免多餘的 `/api/profile` 探測。
pub(super) async fn fetch_todos_for_account(
    account: &crate::account::AccountConfig,
    accounts_db: &crate::db::AccountsDb,
) -> miette::Result<Vec<crate::api::todos::TronclassTodoItem>> {
    if let Some(client) = build_client_from_saved_cookie(account) {
        let api = crate::api::ApiClient::new(client, account.base_url());
        match api.get_todos().await {
            Ok(items) => return Ok(items),
            Err(e) if !crate::api::is_auth_error(&e.to_string()) => return Err(e),
            Err(_) => {
                info!(account = %account.id, "作業頁：session 已失效，重新登入");
            }
        }
    }

    let (auth_client, session) = crate::auth::AuthClient::new(account).await?;
    let _ = accounts_db
        .set_session_cookie(&account.id, &session.cookie_string)
        .await;
    let api = crate::api::ApiClient::new(auth_client.client, account.base_url());
    api.get_todos().await
}

/// 以帳號已儲存的 session cookie 組出 HTTP client，**不做任何驗證探測**。
///
/// cookie 為空或 client 建立失敗時回傳 `None`，呼叫端會退回完整登入流程。
///
/// 由 `crate::session_client` 提供快取：每個帳號只在 session cookie（或
/// base_url／timeout）變更時才重建，其餘呼叫共享同一連線池，避免每個 API
/// 請求都重打一次 TCP／TLS 握手。
pub(super) fn build_client_from_saved_cookie(
    account: &crate::account::AccountConfig,
) -> Option<reqwest::Client> {
    crate::session_client::session_http_client(account).map(|shared| (*shared).clone())
}

pub(super) async fn admin_homework_list_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminHomeworkQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };

    let requested_account_id = query
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let target_account_id = if session.is_admin {
        requested_account_id.unwrap_or(&session.account_id)
    } else {
        if let Some(requested) = requested_account_id {
            if requested != session.account_id {
                return unauthorized_response();
            }
        }
        &session.account_id
    };

    let mut raw_account = state.accounts_db.get(target_account_id).await.ok().flatten();
    if raw_account.is_none() {
        if let Ok(accounts) = state.accounts_db.find_by_username(target_account_id, None).await {
            raw_account = accounts.into_iter().next();
        }
    }
    if raw_account.is_none() && session.is_admin {
        if let Ok(accounts) = state.accounts_db.list().await {
            raw_account = accounts.into_iter().find(|a| a.enabled);
        }
    }

    let Some(raw_account) = raw_account else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "找不到指定帳號，請至帳號設定確認" })),
        )
            .into_response();
    };

    if !raw_account.enabled {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "該帳號目前已被停用" })),
        )
            .into_response();
    }

    // HOMEWORK_CONFIG_PANIC_GUARD_V1
    // 這裡以前直接呼叫 get_config()，全域設定未初始化時會 panic。axum 沒有掛
    // CatchPanicLayer，panic 會讓連線直接斷掉而不回應，反向代理因此合成 502。
    // 改成回傳明確的 JSON 錯誤，讓前端顯示可讀訊息而不是 HTTP 502。
    let Some(config) = crate::config::try_get_config() else {
        error!("全域設定尚未初始化，無法提供作業清單（啟動時應已由 AppConfig::load() 填入）");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "伺服器設定尚未就緒，請稍後再試或聯絡管理員" })),
        )
            .into_response();
    };

    let Some(provider) = config.providers.get(&raw_account.provider) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("未知的 Provider 設定：{}", raw_account.provider) })),
        )
            .into_response();
    };

    let resolved_account = crate::account::AccountConfig {
        id: raw_account.id.clone(),
        provider: raw_account.provider.clone(),
        username: raw_account.username.clone(),
        password: raw_account.password.clone(),
        enabled: raw_account.enabled,
        line_user_id: raw_account.line_user_id.clone(),
        discord_user_id: raw_account.discord_user_id.clone(),
        session_cookie: raw_account.session_cookie.clone(),
        is_admin: raw_account.is_admin,
        provider_config: provider.clone(),
        request_timeout_secs: provider.api.request_timeout_secs,
    };

    // NO_REDUNDANT_SESSION_PROBE_V1
    // 這裡以前無條件呼叫 AuthClient::new()，它會主動打一次 /api/profile。作業頁
    // 每 5 分鐘自動刷新一次，於是每次刷新都在日誌刷出一輪 session 驗證訊息。
    // 監控器早已在記憶體裡保有驗證過的 user_name，這個探測純屬多餘。
    //
    // 改為先用已儲存的 cookie 直接查詢；只有真的遇到認證錯誤才走完整登入。
    let cached_user_name = state
        .lookup_monitor_user_name(&resolved_account.id)
        .await
        .unwrap_or_else(|| raw_account.id.clone());

    // HOMEWORK_USER_COMPLETED_V1
    // 載入該帳號「使用者手動標記完成」的作業 id 集合，隨回應一起給前端，
    // 讓作業卡顯示已標記狀態。
    let user_completed = state
        .accounts_db
        .list_homework_user_completed(&resolved_account.id)
        .await
        .unwrap_or_default();

    // DEMAND_DRIVEN_HOMEWORK_CACHE_V1 + INSTANT_FIRST_PAINT_V1
    // 開頁體驗優先：若有任何既有資料（記憶體快取或 SQLite 快照），先立刻回傳，
    // 同時在背景把資料更新到最新（stale-while-revalidate）。
    // 使用者一進頁面就看得到作業與截止日，不必空等 Tronclass 回應。
    let cache_key = resolved_account.id.clone();

    if !query.refresh {
        if let Some((items, fetched_at)) =
            crate::homework_cache::peek_stale(&cache_key, &state.accounts_db).await
        {
            // 背景預抓器每 4 分鐘刷新一次，因此多數情況下這裡拿到的就是新資料。
            // is_fresh 為真時回報 stale=false，前端就不會顯示「更新中」也不會回抓。
            let is_fresh = crate::homework_cache::peek(&cache_key).await.is_some();

            // 資料已過期才需要背景更新；仍在 TTL 內就不必打擾上游。
            if !is_fresh {
                let bg_account = resolved_account.clone();
                let bg_db = state.accounts_db.clone();
                let bg_key = cache_key.clone();
                let bg_messenger = state.messenger.clone();
                tokio::spawn(async move {
                    let fetch_account = bg_account.clone();
                    let fetch_db = bg_db.clone();
                    let persist_key = bg_key.clone();
                    let fetched =
                        crate::homework_cache::get_or_fetch(&bg_key, true, || async move {
                            let items = fetch_todos_for_account(&fetch_account, &fetch_db).await?;
                            crate::homework_cache::persist(&persist_key, &items, &fetch_db).await;
                            Ok(items)
                        })
                        .await;

                    // REMINDERS_ON_ANY_REFRESH_V1
                    // 使用者開頁觸發的背景更新也評估一次截止提醒，讓通知不必
                    // 等到下一輪 15 分鐘排程。去重表確保不會重複發送。
                    if let Ok(items) = fetched {
                        if let Err(error) = crate::homework_reminders::evaluate_and_send_reminders(
                            &bg_account,
                            &bg_db,
                            bg_messenger.as_deref(),
                            &items,
                            chrono::Utc::now(),
                        )
                        .await
                        {
                            warn!(account = %bg_account.id, %error, "背景更新評估作業提醒失敗");
                        }
                    }
                });
            }

            return (
                StatusCode::OK,
                Json(json!({
                    "account_id": raw_account.id,
                    "user_name": cached_user_name,
                    "items": items.as_ref(),
                    "fetched_at": fetched_at,
                    "stale": !is_fresh,
                    "user_completed": user_completed,
                })),
            )
                .into_response();
        }
    }

    // 沒有任何既有資料（首次使用該帳號）或使用者要求強制刷新 → 同步抓取。
    let fetch_account = resolved_account.clone();
    let fetch_db = state.accounts_db.clone();
    let persist_key = cache_key.clone();
    let result = crate::homework_cache::get_or_fetch(&cache_key, query.refresh, || async move {
        let items = fetch_todos_for_account(&fetch_account, &fetch_db).await?;
        crate::homework_cache::persist(&persist_key, &items, &fetch_db).await;
        Ok(items)
    })
    .await;

    match result {
        Ok(items) => (
            StatusCode::OK,
            Json(json!({
                "account_id": raw_account.id,
                "user_name": cached_user_name,
                "items": items.as_ref(),
                "user_completed": user_completed,
            })),
        )
            .into_response(),
        Err(e) => {
            warn!(account = %resolved_account.id, error = %e, "讀取作業清單失敗");
            // 確保任何非預期情況皆安全回傳結構化 JSON，絕不丟出 502
            (
                StatusCode::OK,
                Json(json!({
                    "account_id": raw_account.id,
                    "user_name": cached_user_name,
                    "items": Vec::<crate::api::todos::TronclassTodoItem>::new(),
                    "user_completed": user_completed,
                    "warning": format!("{e}"),
                })),
            )
                .into_response()
        }
    }
}

/// 標記／取消標記「作業已完成」。僅影響該帳號自己的提醒與顯示狀態；
/// 不會寫入上游，也不改動提醒「已送出」紀錄。
pub(super) async fn admin_homework_complete_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let req: AdminHomeworkCompleteRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_request" })),
            )
                .into_response();
        }
    };

    // 授權：session_cookie_to_query middleware 已把 HttpOnly cookie 注入 query
    // token，但這裡是 POST body 而非 GET，需自行從 Cookie header 讀。
    let token = session_cookie_from_headers(&headers).unwrap_or_default();
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };

    // 目標帳號：管理員可指定其他帳號，一般使用者只能標記自己。
    let target_account_id = if session.is_admin {
        req.account_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(&session.account_id)
    } else {
        if let Some(requested) = req.account_id.as_deref().map(str::trim).filter(|v| !v.is_empty())
        {
            if requested != session.account_id {
                return unauthorized_response();
            }
        }
        &session.account_id
    };

    let homework_id = req.homework_id.to_string();
    let result = if req.completed {
        state
            .accounts_db
            .mark_homework_user_completed(target_account_id, &homework_id)
            .await
    } else {
        state
            .accounts_db
            .unmark_homework_user_completed(target_account_id, &homework_id)
            .await
    };

    match result {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "account_id": target_account_id,
                "homework_id": req.homework_id,
                "completed": req.completed,
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("標記作業狀態失敗：{error}") })),
        )
            .into_response(),
    }
}
