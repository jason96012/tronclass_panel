//! 作業清單與完成狀態 handlers。

use super::*;
use tokio::sync::Semaphore;

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

/// HOMEWORK_DETAIL_V1（fix41）：作業詳情查詢。
///
/// `course_id` 可省略（部分來源的待辦沒有課程 id）；`item_id` 必填。
#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkDetailQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    course_id: Option<u64>,
    item_id: u64,
}

/// HOMEWORK_DETAIL_V1（fix41）：作業附件下載查詢。
///
/// **只接受 `upload_id`**（上游 upload 的數字 id），不接受任何由前端指定的 URL——
/// 否則這個端點會變成「帶著面板 session 的任意 URL 抓取器」（SSRF）。
/// `name` 只影響下載檔名，會再過 `safe_file_name`。
#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkAttachmentQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
    upload_id: u64,
    #[serde(default)]
    name: Option<String>,
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
    // TEST_NOTIFICATION_RATE_LIMIT_V1
    if let Err(response) = check_test_notification_rate(&state, "homework", &account_id).await {
        return response;
    }
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
        // HOMEWORK_NEW_NOTIFY_V1：測試通知走的是「截止提醒」文案。
        is_new: false,
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

/// HOMEWORK_DETAIL_V1（fix41）：作業相關端點共用的「目標帳號解析」。
///
/// 規則與 `admin_homework_list_handler` 完全一致（管理員可指定帳號；一般帳號只能
/// 看自己；檢視者沒有學校 session）。抽出來是為了讓詳情／附件兩個新端點與清單
/// 走同一套判定——三處各寫一份是先前 fix38 類型事故的溫床。
struct HomeworkTarget {
    raw: crate::account::RawAccountConfig,
    account: crate::account::AccountConfig,
    base_url: String,
}

async fn resolve_homework_target(
    state: &AdminWebState,
    session: &AdminSession,
    requested_account_id: Option<&str>,
) -> Result<HomeworkTarget, Response> {
    let target_account_id = if session.is_admin {
        requested_account_id.unwrap_or(&session.account_id)
    } else {
        if let Some(requested) = requested_account_id {
            if requested != session.account_id {
                return Err(unauthorized_response());
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
            // VIEWER_NO_SCHOOL_API_V1：只挑能登入學校系統的帳號。
            raw_account = accounts.into_iter().find(|a| a.enabled && !a.is_viewer);
        }
    }

    let Some(raw_account) = raw_account else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "找不到指定帳號，請至帳號設定確認" })),
        )
            .into_response());
    };

    if !raw_account.enabled {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "該帳號目前已被停用" })),
        )
            .into_response());
    }
    // VIEWER_NO_SCHOOL_API_V1：檢視者沒有學校憑證。
    if raw_account.is_viewer {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "檢視者帳號不會連線學校系統；請選擇一般帳號" })),
        )
            .into_response());
    }

    // HOMEWORK_CONFIG_PANIC_GUARD_V1：全域設定未初始化時回明確 JSON，不 panic。
    let Some(config) = crate::config::try_get_config() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "伺服器設定尚未就緒，請稍後再試或聯絡管理員" })),
        )
            .into_response());
    };
    let Some(provider) = config.providers.get(&raw_account.provider) else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("未知的 Provider 設定：{}", raw_account.provider) })),
        )
            .into_response());
    };

    let account = crate::account::AccountConfig {
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
    // COMPILE_FIX_V1（fix42 revB）：`AccountConfig::base_url()` 回的是 `&str`，
    // 而這裡的欄位是 `String`——先前交付時沒有編譯器可驗，這行是 rustc 抓到的
    // E0308。改為擁有所有權，後續 `&target.base_url` 仍會自動 deref 成 `&str`。
    let base_url = account.base_url().to_string();
    Ok(HomeworkTarget {
        raw: raw_account,
        account,
        base_url,
    })
}

/// HOMEWORK_DETAIL_V1（fix41）：取得 iClass `ApiClient`（**不做任何探測**）。
///
/// 有已保存的 session cookie 就直接沿用（呼叫端若遇到認證錯誤，再用
/// `iclass_api_after_relogin` 重登重試一次）；連 cookie 都沒有時才走完整登入。
///
/// 為什麼不先探測：詳情／附件都是「單一使用者動作」觸發的低頻請求，探測會多打
/// 一次上游（作業清單的探測成本更高——它是整個課程的批次抓取）。
async fn iclass_api_for_account(
    account: &crate::account::AccountConfig,
    accounts_db: &crate::db::AccountsDb,
) -> miette::Result<crate::api::ApiClient> {
    if let Some(client) = build_client_from_saved_cookie(account) {
        return Ok(crate::api::ApiClient::new(client, account.base_url()));
    }
    iclass_api_after_relogin(account, accounts_db).await
}

/// HOMEWORK_DETAIL_V1（fix41）：完整帳密登入一次，並把新 session cookie 存回 DB。
pub(super) async fn iclass_api_after_relogin(
    account: &crate::account::AccountConfig,
    accounts_db: &crate::db::AccountsDb,
) -> miette::Result<crate::api::ApiClient> {
    let (auth_client, session) = crate::auth::AuthClient::new(account).await?;
    let _ = accounts_db
        .set_session_cookie(&account.id, &session.cookie_string)
        .await;
    Ok(crate::api::ApiClient::new(
        auth_client.client,
        account.base_url(),
    ))
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
            // AUTH_ERROR_TYPED_V1：改用 report 版走完整條錯誤鏈。session 過期
            // 的 ApiError::Unauthorized 可能被上層 wrap_err 包住而 to_string
            // 看不到，舊寫法會誤把「可跳過的 session 過期」當成一般錯誤
            // 中斷整個作業列表；現在正確地只跳過該帳號、改用帳密重登。
            Err(e) if !crate::api::is_auth_error_report(&e) => return Err(e),
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

    // HOMEWORK_DETAIL_V1（fix41）：目標帳號解析抽成 `resolve_homework_target`，
    // 讓清單／詳情／附件三個端點共用同一套判定（管理員可指定帳號、一般帳號只能
    // 看自己、檢視者沒有學校 session），避免三份實作各自漂移。
    let target = match resolve_homework_target(
        &state,
        &session,
        query
            .account_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty()),
    )
    .await
    {
        Ok(target) => target,
        Err(response) => return response,
    };
    let raw_account = &target.raw;
    let resolved_account = target.account;
    // HOMEWORK_DETAIL_V1：把 iClass 的 base URL 一起給前端——「在 iClass 開啟」
    // 是按鈕直接開 iClass 網域的深連結（不再經過面板代理）。
    let iclass_base = resolved_account.base_url();

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
                    // UPDATE_TIMES_PAGE_V1
                    match &fetched {
                        Ok(items) => crate::update_times::record_account(
                            crate::update_times::KIND_HOMEWORK,
                            &bg_key,
                            true,
                            format!("{} 項", items.len()),
                        ),
                        Err(e) => crate::update_times::record_account(
                            crate::update_times::KIND_HOMEWORK,
                            &bg_key,
                            false,
                            e.to_string(),
                        ),
                    }

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
                    "iclass_base": iclass_base,
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
    // UPDATE_TIMES_PAGE_V1
    match &result {
        Ok(items) => crate::update_times::record_account(
            crate::update_times::KIND_HOMEWORK,
            &cache_key,
            true,
            format!("{} 項", items.len()),
        ),
        Err(e) => crate::update_times::record_account(
            crate::update_times::KIND_HOMEWORK,
            &cache_key,
            false,
            e.to_string(),
        ),
    }

    match result {
        Ok(items) => (
            StatusCode::OK,
            Json(json!({
                "account_id": raw_account.id,
                "user_name": cached_user_name,
                "items": items.as_ref(),
                "user_completed": user_completed,
                "iclass_base": iclass_base,
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
                    "iclass_base": iclass_base,
                    "warning": format!("{e}"),
                })),
            )
                .into_response()
        }
    }
}

/// HOMEWORK_DETAIL_V1（fix41）：面板內顯示作業詳情——**完全不經過 iClass 頁面代理**。
///
/// 動機：代理路線（`/admin/tc-go` ＋ `/tc/...`，fix22→fix40）反覆出問題，而使用者
/// 真正需要的是「按一個按鈕就看到作業內容」。這裡由伺服器端用面板自己保存的
/// iClass session 抓資料，面板自己渲染：
///   * 不碰瀏覽器 cookie → 面板登入狀態零風險（fix38 那類事故不可能重演）
///   * 不需要 iClass 的 CSRF token（伺服器端自帶 cookie 呼叫 API）
///   * 上游沒有說明／附件時優雅降級，不影響清單本身
pub(super) async fn admin_homework_detail_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminHomeworkDetailQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    let target = match resolve_homework_target(
        &state,
        &session,
        query
            .account_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty()),
    )
    .await
    {
        Ok(target) => target,
        Err(response) => return response,
    };

    // 深連結（「在 iClass 開啟」）。沒有 course_id 就沒有可開啟的頁面。
    let deep_link = query
        .course_id
        .map(|course_id| {
            crate::api::todos::iclass_activity_deep_link(
                &target.base_url,
                course_id,
                query.item_id,
            )
        });

    // 說明：這裡刻意不用 closure 包 fetch——`async move` 會把捕獲值移進 future，
    // 兩次呼叫（首次＋重登後重試）就無法成立。直接寫兩次呼叫最清楚也最安全。
    let course_id = query.course_id;
    let item_id = query.item_id;

    let api = match iclass_api_for_account(&target.account, &state.accounts_db).await {
        Ok(api) => api,
        Err(error) => {
            warn!(account = %target.raw.id, %error, "作業詳情：取得 iClass client 失敗");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("無法連線學校系統：{error}") })),
            )
                .into_response();
        }
    };
    let result = match api.get_homework_detail(course_id, item_id).await {
        // 認證錯誤 → 完整重登一次再試（與作業清單同一條策略）。
        Err(error) if crate::api::is_auth_error_report(&error) => {
            info!(account = %target.raw.id, "作業詳情：session 已失效，重新登入後重試");
            match iclass_api_after_relogin(&target.account, &state.accounts_db).await {
                Ok(fresh) => fresh.get_homework_detail(course_id, item_id).await,
                Err(relogin_error) => Err(relogin_error),
            }
        }
        other => other,
    };

    match result {
        Ok(Some(detail)) => {
            // 說明同時給「原始 HTML」與「純文字」兩版：前端有 DOMPurify 時渲染
            // HTML（消毒後才 innerHTML），沒有時退回純文字（與 bulletins.html 一致）。
            let description_text = super::bulletins::to_plain_text(&detail.description_html);
            let attachments: Vec<serde_json::Value> = detail
                .attachments
                .iter()
                .map(|attachment| {
                    json!({
                        "upload_id": attachment.upload_id,
                        "name": attachment.name,
                        "size": attachment.size,
                        // 有 id 就能由面板伺服器端代抓（見 attachment handler）。
                        "downloadable": attachment.upload_id.is_some(),
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "account_id": target.raw.id,
                    "description_html": detail.description_html,
                    "description_text": description_text,
                    "has_images": detail.has_images,
                    // HOMEWORK_SUBMIT_V1：小組作業不代繳，前端據此收起繳交表單。
                    "submit_by_group": detail.submit_by_group,
                    "attachments": attachments,
                    "iclass_url": deep_link,
                    "item": detail.item,
                    "source": detail.source,
                })),
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::OK,
            Json(json!({
                "ok": false,
                "account_id": target.raw.id,
                "iclass_url": deep_link,
                "error": "上游沒有回傳這份作業的內容（可能已下架或課程不在本學期）",
            })),
        )
            .into_response(),
        Err(error) => {
            warn!(account = %target.raw.id, item_id = query.item_id, %error, "讀取作業詳情失敗");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "ok": false,
                    "account_id": target.raw.id,
                    "iclass_url": deep_link,
                    "error": format!("讀取作業內容失敗：{error}"),
                })),
            )
                .into_response()
        }
    }
}

/// HOMEWORK_FILE_V1（fix43）：`GET /admin/api/homework/file` 的查詢參數。
///
/// 與附件的差別只有 `disposition`：`inline` ＝ 面板內預覽（PDF／圖片／影音／文字、
/// 以及前端本機解析的 Office），其餘（含省略）＝ 下載。
#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkFileQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
    upload_id: u64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    disposition: Option<String>,
}

/// 附件與繳交檔共用的請求內容（兩個入口各自組出來後交給同一個實作）。
struct HomeworkFileRequest {
    token: String,
    account_id: Option<String>,
    upload_id: u64,
    name: Option<String>,
    inline: bool,
}

/// HOMEWORK_FILE_SANITY_V1（fix47）：嗅探上游回應開頭所需的 bytes 數。
///
/// 只需要認出「這是不是檔案」——HTML 頁面與 JSON 錯誤都出現在最前面幾個 bytes。
const FILE_SNIFF_BYTES: usize = 512;

/// 這個檔名本來就是文字／網頁類嗎？
///
/// 文字類的開頭本來就可能長得像錯誤頁（`.html` 是 `<`、`.json` 是 `{`、
/// `.rtf` 是 `{\rtf`、`.url` 是 `[InternetShortcut]`），這些一律不套用
/// 「上游回傳錯誤頁」的檢查，否則正常檔案會被誤擋。
fn is_texty_file_name(file_name: &str) -> bool {
    let ext = std::path::Path::new(file_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "html"
            | "htm"
            | "xhtml"
            | "txt"
            | "text"
            | "md"
            | "markdown"
            | "csv"
            | "tsv"
            | "json"
            | "xml"
            | "svg"
            | "yaml"
            | "yml"
            | "ini"
            | "conf"
            | "cfg"
            | "toml"
            | "sql"
            | "sh"
            | "rs"
            | "js"
            | "mjs"
            | "ts"
            | "jsx"
            | "tsx"
            | "css"
            | "py"
            | "java"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "go"
            | "php"
            | "log"
            | "srt"
            | "vtt"
            | "url"
            | "rtf"
    )
}

/// 上游回應「明顯不是檔案」的判斷（回傳原因供 log／錯誤訊息使用）。
///
/// 這是教材匯入 `fetch_json` 同一套判斷（首個非空白字元是 `<` 即 HTML 登入頁）的延伸：
/// 這裡多了 JSON——iClass 的 API 錯誤常以 JSON、HTTP 200 回覆，
/// 而面板端點若原封不動轉出去，使用者拿到的「.docx」其實是一段 JSON，
/// 前端只會說「內容與副檔名不符」，完全查不出原因。
fn looks_like_recognition_page(prefix: &[u8]) -> Option<&'static str> {
    let first = prefix
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())?;
    match first {
        b'<' => Some("HTML 頁面（多半是登入頁或錯誤頁）"),
        b'{' | b'[' => Some("JSON 回應（多半是 API 錯誤）"),
        _ => None,
    }
}

/// Range 是否從第 0 個 byte 開始；只有這種情況開頭才是檔案真正的開頭，可以嗅探。
fn range_starts_at_zero(value: &axum::http::HeaderValue) -> bool {
    match value.to_str() {
        Ok(text) => text
            .trim()
            .trim_start_matches("bytes=")
            .trim()
            .starts_with("0-"),
        Err(_) => false,
    }
}

/// HOMEWORK_FILE_SANITY_V1（fix47）：一次「依序試候選網址」的串流結果。
///
/// BULLETIN_ATTACHMENT_V1（fix55）：公告附件走完全相同的代理路徑，因此這個型別
/// 與下面的 `stream_iclass_upload` 一起開放給同層模組（原本名稱為
/// `HomeworkFileAttempt`／`stream_homework_file`，作業與公告共用後改名）。
///
/// 注意：Rust 的**欄位**預設是「模組私有」，struct 本身標了 `pub(super)` 並不會
/// 連帶開放欄位——`bulletins.rs` 會讀 `response`／`saw_page`／`last_error` 這三個
/// 欄位（判斷是否要重登重試、以及重試後仍失敗時的錯誤訊息），所以每個欄位都要
/// 各別標 `pub(super)`，否則編譯會直接 E0616（field is private）。
pub(super) struct IclassUploadAttempt {
    pub(super) response: Option<Response>,
    pub(super) last_error: String,
    /// 至少有一個候選網址回傳「明顯不是檔案」的內容（HTML／JSON）。
    pub(super) saw_page: bool,
}

/// 決定回給瀏覽器的 Content-Type。
///
/// **以副檔名優先**：上游對同一種檔案常常只回 `application/octet-stream`，而預覽要能
/// 運作就得給對型別（PDF、圖片、影音都靠這個）。`material_content_type` 不認得的
/// 副檔名（例如 `.asta`）才退回上游給的型別。
///
/// 註：`material_content_type` 刻意把 html／xml 對成 text/plain，避免使用者上傳的檔案
/// 在面板同一個 origin 上被當成文件執行（stored XSS）；預覽沿用同一條規則，不多開後門。
fn homework_file_content_type(file_name: &str, upstream: Option<&str>) -> String {
    let inferred = super::utils::material_content_type(std::path::Path::new(file_name));
    if inferred != "application/octet-stream" {
        return inferred.to_string();
    }
    upstream
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string()
}

/// HOMEWORK_DETAIL_V1（fix41）／HOMEWORK_FILE_V1（fix43）：作業附件與「自己繳交的
/// 檔案」共用同一條串流實作——兩個入口的差異只有 `inline` 與是否轉送 Range。
///
/// 上游 upload → `GET {base}/api/uploads/{id}/preview` 取得簽章 URL（
/// `MATERIALS_IMPORT_S3_V1` 已實測的路徑，與教材匯入共用同一個 resolver），
/// 再把檔案**串流**回瀏覽器（不整包進記憶體）。
///
/// 安全：只接受數字 `upload_id`，前端無法指定 URL（不然就是帶著 session 的任意抓取器）；
/// 回應一律 `nosniff` ＋ `Cache-Control: private, no-store`，Content-Type 由面板自己決定
/// （見 `homework_file_content_type`），不讓上游的 HTML 被當頁面執行。
async fn homework_file_response(
    state: AdminWebState,
    headers: HeaderMap,
    request: HomeworkFileRequest,
) -> Response {
    let token = session_token(&headers, &request.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    let target = match resolve_homework_target(
        &state,
        &session,
        request
            .account_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty()),
    )
    .await
    {
        Ok(target) => target,
        Err(response) => return response,
    };

    let api = match iclass_api_for_account(&target.account, &state.accounts_db).await {
        Ok(api) => api,
        Err(error) => {
            warn!(account = %target.raw.id, %error, "讀取作業檔案：取得 iClass client 失敗");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("無法連線學校系統：{error}") })),
            )
                .into_response();
        }
    };
    // 前端給的檔名只當顯示用（同樣過安全檢查），真正的來源是 upload id。
    let file_name = request
        .name
        .as_deref()
        .and_then(super::utils::safe_file_name)
        .unwrap_or_else(|| format!("作業檔案-{}", request.upload_id));

    let upload = json!({ "id": request.upload_id });
    let mut urls = super::materials_import::resolve_download_urls(
        &api.client,
        &target.base_url,
        &upload,
    )
    .await;
    // S3 簽章網址（`/api/uploads/{id}/preview` 取得）是唯一可靠的下載通道；
    // 只剩 `…/download/{key}` 那種備援、或完全沒有，通常代表 **iClass session 已過期**
    // （preview 端點回登入頁）——此時重登一次再解一次，而不是讓使用者看到 404。
    if !urls.iter().any(|url| !url.contains("/download/")) {
        if let Ok(fresh) = iclass_api_after_relogin(&target.account, &state.accounts_db).await {
            let retried = super::materials_import::resolve_download_urls(
                &fresh.client,
                &target.base_url,
                &upload,
            )
            .await;
            if retried.iter().any(|url| !url.contains("/download/")) {
                urls = retried;
            } else if urls.is_empty() {
                urls = retried;
            }
        }
    }
    if urls.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "找不到這個檔案的下載位址" })),
        )
            .into_response();
    }

    // RANGE_STREAM_V1：影音要能邊下邊播、拖時間軸，PDF 檢視器也常先要一小段。
    // 這裡不自己實作分段，直接把瀏覽器的 Range 原樣轉給上游，再把上游的
    // 206／Content-Range 照傳回去（上游就是它的儲存後端，語意最準）。
    let range = headers
        .get(axum::http::header::RANGE)
        .cloned();

    let mut attempt =
        stream_iclass_upload(&api.client, &urls, &file_name, request.inline, range.as_ref()).await;

    // HOMEWORK_FILE_SANITY_V1（fix47）：上游回「HTML 登入頁／JSON 錯誤」時，
    // 以前會原封不動當成 .docx 串回瀏覽器，前端只看到「檔案內容與副檔名不符」。
    // 最常見的成因是 session 過期，這裡重新登入後再試一輪（與上面「只剩 /download/
    // 備援」同一個處置），仍然失敗才回錯誤，不再把錯誤頁當檔案送出去。
    if attempt.response.is_none() && attempt.saw_page {
        match iclass_api_after_relogin(&target.account, &state.accounts_db).await {
            Ok(fresh) => {
                let retried_urls = super::materials_import::resolve_download_urls(
                    &fresh.client,
                    &target.base_url,
                    &upload,
                )
                .await;
                if !retried_urls.is_empty() {
                    info!(account = %target.raw.id, "作業檔案疑似 session 過期：重新登入後重試");
                    attempt = stream_iclass_upload(
                        &fresh.client,
                        &retried_urls,
                        &file_name,
                        request.inline,
                        range.as_ref(),
                    )
                    .await;
                }
            }
            Err(error) => {
                warn!(account = %target.raw.id, %error, "作業檔案：重新登入失敗");
            }
        }
    }

    match attempt.response {
        Some(response) => response,
        None => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("讀取檔案失敗：{}", attempt.last_error) })),
        )
            .into_response(),
    }
}

/// HOMEWORK_FILE_SANITY_V1（fix47）：依序嘗試候選網址，成功就回傳可串流的回應。
///
/// 與舊版的差別只有一處：送出回應**之前**先看開頭 [`FILE_SNIFF_BYTES`] bytes，
/// 若內容明顯是 HTML 頁面或 JSON（登入頁／API 錯誤），就當成這個網址失敗、
/// 換下一個候選，而不是把錯誤頁當成檔案交給瀏覽器。
///
/// 已經讀進來的前綴會原樣接回串流前端，因此 Content-Length／Content-Range
/// 與位元組內容都與上游一致（影音分段、PDF 分段照常運作）。
/// BULLETIN_ATTACHMENT_V1（fix55）：公告附件（`bulletins.rs`）與作業附件／繳交檔
/// 共用這支串流器——同一個上游（iClass 簽章 URL）、同一套嗅探與 Range 轉發，
/// 兩邊行為才不會分岔。因此收 `reqwest::Client`（呼叫端自己決定帶哪個 session）。
pub(super) async fn stream_iclass_upload(
    client: &reqwest::Client,
    urls: &[String],
    file_name: &str,
    inline: bool,
    range: Option<&axum::http::HeaderValue>,
) -> IclassUploadAttempt {
    let mut last_error = String::from("沒有可用的下載位址");
    let mut saw_page = false;

    for url in urls {
        let mut upstream_request = client
            .get(url)
            .timeout(std::time::Duration::from_secs(600))
            .header(reqwest::header::ACCEPT, "application/octet-stream, */*");
        if let Some(range) = range {
            upstream_request = upstream_request.header(reqwest::header::RANGE, range.clone());
        }
        let response = upstream_request.send().await;
        let upstream = match response {
            Ok(upstream) if upstream.status().is_success() => upstream,
            Ok(upstream) => {
                last_error = format!("HTTP {}（{url}）", upstream.status());
                warn!(url = %url, status = upstream.status().as_u16(), "讀取作業檔案：非 2xx，換下一個位址");
                continue;
            }
            Err(error) => {
                last_error = format!("連線失敗（{url}）：{error}");
                warn!(url = %url, %error, "讀取作業檔案：連線失敗，換下一個位址");
                continue;
            }
        };

        let status = upstream.status();
        let upstream_type = upstream
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok());
        let content_type = homework_file_content_type(file_name, upstream_type);
        let content_length = upstream.content_length();
        let content_range = upstream
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        // 只有「整份檔案」或「從第 0 byte 起的分段」才嗅探；中段的 bytes 沒有魔數。
        let sniffable = !is_texty_file_name(file_name)
            && range
                .map(|value| range_starts_at_zero(value))
                .unwrap_or(true);

        let mut stream = upstream.bytes_stream();
        let mut prefix: Vec<u8> = Vec::new();
        // 已讀進來的 chunk（型別由 `bytes_stream()` 推得，不需要直接依賴 bytes crate）。
        let mut kept = Vec::new();
        let mut peek_error: Option<String> = None;
        if sniffable {
            while prefix.len() < FILE_SNIFF_BYTES {
                match futures::StreamExt::next(&mut stream).await {
                    Some(Ok(chunk)) => {
                        prefix.extend_from_slice(&chunk);
                        kept.push(chunk);
                    }
                    Some(Err(error)) => {
                        peek_error = Some(error.to_string());
                        break;
                    }
                    None => break,
                }
            }
        }
        if let Some(error) = peek_error {
            last_error = format!("讀取回應失敗（{url}）：{error}");
            warn!(url = %url, %error, "讀取作業檔案：讀取前置內容失敗，換下一個位址");
            continue;
        }
        if sniffable {
            if let Some(reason) = looks_like_recognition_page(&prefix) {
                saw_page = true;
                last_error = format!("iClass 回傳的是{reason}（{url}）");
                warn!(
                    url = %url,
                    reason,
                    prefix_bytes = prefix.len(),
                    "讀取作業檔案：上游回傳的不是檔案內容，換下一個位址"
                );
                continue;
            }
        }

        let disposition = if inline { "inline" } else { "attachment" };
        let mut builder = axum::http::Response::builder()
            .status(status)
            .header(axum::http::header::CONTENT_TYPE, content_type)
            .header(
                axum::http::header::CONTENT_DISPOSITION,
                format!(
                    "{disposition}; filename*=UTF-8''{}",
                    percent_encode_filename(file_name)
                ),
            )
            // 沒有這個標頭時，影音播放器不會嘗試分段抓取。
            .header(axum::http::header::ACCEPT_RANGES, "bytes")
            // 上游內容一律不讓瀏覽器 sniff 成 HTML 執行。
            .header("x-content-type-options", "nosniff")
            .header(axum::http::header::CACHE_CONTROL, "private, max-age=0, no-store");
        if let Some(length) = content_length {
            // 比照 files.rs：以字串設定（避免 TryInto<HeaderValue> 的邊角情況）。
            builder = builder.header(axum::http::header::CONTENT_LENGTH, length.to_string());
        }
        if let Some(content_range) = content_range {
            builder = builder.header(axum::http::header::CONTENT_RANGE, content_range);
        }

        // CLOUD_FILE_STREAMING_V1 的同一套理由：串流而不是整包進記憶體。
        // 已嗅探的前綴接回最前面，內容與上游完全一致。
        let prefix_stream = futures::stream::iter(kept.into_iter().map(Ok::<_, reqwest::Error>));
        let body_stream = futures::StreamExt::chain(prefix_stream, stream);
        return match builder.body(axum::body::Body::from_stream(body_stream)) {
            Ok(response) => IclassUploadAttempt {
                response: Some(response),
                last_error,
                saw_page,
            },
            Err(error) => {
                warn!(%error, "讀取作業檔案：回應組裝失敗");
                IclassUploadAttempt {
                    response: None,
                    last_error: "回應組裝失敗".to_string(),
                    saw_page,
                }
            }
        };
    }

    IclassUploadAttempt {
        response: None,
        last_error,
        saw_page,
    }
}

/// HOMEWORK_DETAIL_V1（fix41）：作業附件下載（保留原本的專用路徑，行為不變）。
pub(super) async fn admin_homework_attachment_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminHomeworkAttachmentQuery>,
) -> Response {
    homework_file_response(
        state,
        headers,
        HomeworkFileRequest {
            token: query.token,
            account_id: query.account_id,
            upload_id: query.upload_id,
            name: query.name,
            inline: false,
        },
    )
    .await
}

/// HOMEWORK_FILE_V1（fix43）：附件／自己繳交的檔案，預覽（`disposition=inline`）
/// 與下載（預設）共用同一個實作。
///
/// 為什麼要新的端點：fix41 的附件端點一律 `attachment`，瀏覽器只會下載；而繳交後要
/// 看「自己交了什麼」得先有可預覽的回應（`inline` ＋ 正確的 Content-Type ＋ Range）。
pub(super) async fn admin_homework_file_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminHomeworkFileQuery>,
) -> Response {
    let inline = matches!(
        query.disposition.as_deref().map(str::trim),
        Some(value) if value.eq_ignore_ascii_case("inline")
    );
    homework_file_response(
        state,
        headers,
        HomeworkFileRequest {
            token: query.token,
            account_id: query.account_id,
            upload_id: query.upload_id,
            name: query.name,
            inline,
        },
    )
    .await
}

/// HOMEWORK_DETAIL_V1（fix41）：`Content-Disposition` 的 RFC 5987 檔名編碼。
/// 與 `ai_chat::percent_encode_filename` 同一個規則（各自模組內私有，避免跨模組耦合）。
fn percent_encode_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
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

// ─── HOMEWORK_SUBMIT_V1（fix42）：面板內直接繳交作業 ───────────────────────────
//
// 完整流程與實測依據寫在 `src/api/todos.rs` 的 `HOMEWORK_SUBMIT_V1` 區塊。這裡
// 只負責「面板這一側」的職責：授權、目標帳號判定、multipart 收檔與大小把關、
// 逐檔上傳、建立繳交、稽核紀錄，以及把結果收斂成前端看得懂的 JSON。
//
// 安全邊界（與教材上傳同一套規矩）：
//   * 檔名一律過 `safe_file_name`（無路徑分隔、無控制字元、≤255 bytes）。
//   * 檔案只在記憶體中暫存，且「單檔 50 MiB／單次總量 100 MiB／最多 10 檔」
//     三重上限；同時只允許一件繳交在跑（`HOMEWORK_SUBMIT_SEMAPHORE`），
//     併發也撐不爆記憶體。
//   * 上傳網址只接受上游回的簽章網址，且必須是 http(s) 公網位址
//     （`is_safe_upload_url` 擋掉 file://、localhost、內網 IP）。
//   * 這裡不碰、也不種任何 iClass cookie：全程用面板已保存的 session 代打。

/// 單次最多幾個檔案。
pub(super) const HOMEWORK_SUBMIT_MAX_FILES: usize = 10;
/// 單檔上限：沿用教材上傳的 50 MiB，使用者只要記一個數字。
// HOMEWORK_SUBMIT_MAX_FILE_BYTES：作業「繳交」的單檔上限（50 MiB）。
// fix54 之前這裡共用教材的 MATERIALS_MAX_FILE_SIZE；教材改成不限單檔大小後，
// 作業繳交維持原本的 50 MiB，因此拆成獨立常數（要調整作業上限不必動教材）。
pub(super) const HOMEWORK_SUBMIT_MAX_FILE_BYTES: usize = 50 * 1024 * 1024;
/// 單次繳交的總量上限（伺服器記憶體用量直接受此限制）。
pub(super) const HOMEWORK_SUBMIT_MAX_TOTAL_BYTES: usize = 100 * 1024 * 1024;
/// axum 的 body 上限：總量＋multipart 邊界／欄位名稱的餘裕。
pub(super) const HOMEWORK_SUBMIT_BODY_LIMIT: usize =
    HOMEWORK_SUBMIT_MAX_TOTAL_BYTES + 4 * 1024 * 1024;
/// multipart 欄位數上限（檔案＋文字欄位）。
const HOMEWORK_SUBMIT_MAX_FIELDS: usize = HOMEWORK_SUBMIT_MAX_FILES + 8;
/// 單一文字欄位的位元組上限（`comment` 另受字元數上限約束）。
const HOMEWORK_SUBMIT_MAX_FIELD_BYTES: usize = 8 * 1024;
/// 同時只允許一次繳交：位元組會暫存在記憶體，序列化才不會被併發撐爆。
const HOMEWORK_SUBMIT_CONCURRENCY: usize = 1;

static HOMEWORK_SUBMIT_SEMAPHORE: once_cell::sync::Lazy<Semaphore> =
    once_cell::sync::Lazy::new(|| Semaphore::new(HOMEWORK_SUBMIT_CONCURRENCY));

/// `POST /admin/api/homework/submit` 的查詢參數（授權與帳號判定比照其他端點）。
#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkSubmitQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
}

/// `GET /admin/api/homework/submission` 的查詢參數。
#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkSubmissionQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
    item_id: u64,
}

/// 從 multipart 收到、還沒上傳到 iClass 的一個檔案。
struct PendingSubmitFile {
    name: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// 收斂後的繳交請求內容。
struct HomeworkSubmitForm {
    item_id: u64,
    course_id: Option<u64>,
    comment: String,
    is_draft: bool,
    files: Vec<PendingSubmitFile>,
}

fn parse_submit_bool(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// 讀完整個 multipart：文字欄位進 `HomeworkSubmitForm`，檔案進 `files`。
///
/// 回傳的 `String` 是「給使用者看的錯誤訊息」——呼叫端一律轉成 400，不揭露細節。
async fn parse_homework_submit_form(
    multipart: &mut axum::extract::Multipart,
) -> std::result::Result<HomeworkSubmitForm, String> {
    let mut item_id: Option<u64> = None;
    let mut course_id: Option<u64> = None;
    let mut comment = String::new();
    let mut is_draft = false;
    let mut files: Vec<PendingSubmitFile> = Vec::new();
    let mut total_bytes: usize = 0usize;
    let mut field_count: usize = 0usize;

    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => return Err(format!("讀取 multipart 失敗：{e}")),
        };
        field_count = field_count.saturating_add(1);
        if field_count > HOMEWORK_SUBMIT_MAX_FIELDS {
            return Err(format!("欄位數量超過上限（{HOMEWORK_SUBMIT_MAX_FIELDS}）"));
        }
        let field_name = field.name().unwrap_or_default().to_string();
        if let Err(response) = validate_admin_text(&field_name, 64, "multipart field name") {
            // validate_admin_text 直接回 HTTP 回應；這裡只需要訊息。
            let _ = response;
            return Err("欄位名稱不合法".to_string());
        }

        if field_name == "file" {
            if files.len() >= HOMEWORK_SUBMIT_MAX_FILES {
                return Err(format!("一次最多上傳 {HOMEWORK_SUBMIT_MAX_FILES} 個檔案"));
            }
            let raw_name = field.file_name().unwrap_or_default().to_string();
            if let Err(_) = validate_admin_text(&raw_name, 255, "file name") {
                return Err("檔名不合法".to_string());
            }
            let Some(file_name) = safe_file_name(&raw_name) else {
                return Err(format!("檔名不合法：{raw_name}"));
            };
            let content_type = field.content_type().map(str::to_string);
            let mut bytes: Vec<u8> = Vec::new();
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        if bytes.len().saturating_add(chunk.len())
                            > HOMEWORK_SUBMIT_MAX_FILE_BYTES
                        {
                            return Err(format!(
                                "檔案「{file_name}」超過單檔上限 {} MB",
                                HOMEWORK_SUBMIT_MAX_FILE_BYTES / (1024 * 1024)
                            ));
                        }
                        if total_bytes.saturating_add(chunk.len())
                            > HOMEWORK_SUBMIT_MAX_TOTAL_BYTES
                        {
                            return Err(format!(
                                "這次繳交的總容量超過 {} MB，請分批上傳",
                                HOMEWORK_SUBMIT_MAX_TOTAL_BYTES / (1024 * 1024)
                            ));
                        }
                        bytes.extend_from_slice(&chunk);
                        total_bytes = total_bytes.saturating_add(chunk.len());
                    }
                    Ok(None) => break,
                    Err(e) => return Err(format!("讀取檔案「{file_name}」失敗：{e}")),
                }
            }
            if bytes.is_empty() {
                return Err(format!("檔案「{file_name}」是空的"));
            }
            files.push(PendingSubmitFile {
                name: file_name,
                content_type,
                bytes,
            });
            continue;
        }

        // 逐段讀文字欄位並套上限。**不用 `field.text()`**：惡意用戶端可以讓單一
        // 欄位吃滿整個 body 額度，先讀成一大段字串再檢查反而更糟。
        let mut raw_bytes: Vec<u8> = Vec::new();
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if raw_bytes.len().saturating_add(chunk.len())
                        > HOMEWORK_SUBMIT_MAX_FIELD_BYTES
                    {
                        return Err(format!(
                            "欄位「{field_name}」超過上限（{HOMEWORK_SUBMIT_MAX_FIELD_BYTES} bytes）"
                        ));
                    }
                    raw_bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => return Err(format!("讀取欄位「{field_name}」失敗：{e}")),
            }
        }
        let value = String::from_utf8_lossy(&raw_bytes).trim().to_string();
        match field_name.as_str() {
            "item_id" => {
                item_id = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "item_id 不是有效的數字".to_string())?,
                );
            }
            "course_id" => {
                if !value.is_empty() {
                    course_id = Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| "course_id 不是有效的數字".to_string())?,
                    );
                }
            }
            "comment" => comment = value,
            "is_draft" => is_draft = parse_submit_bool(&value),
            // token 由 query／cookie 提供；未知欄位直接忽略（前端版本不同也不致失敗）。
            _ => {}
        }
    }

    let Some(item_id) = item_id.filter(|v| *v > 0) else {
        return Err("缺少作業 id（item_id）".to_string());
    };
    if files.is_empty() && comment.is_empty() {
        return Err("請至少選擇一個檔案，或填寫要繳交的文字／連結".to_string());
    }
    if comment.chars().count() > crate::api::todos::HOMEWORK_SUBMIT_MAX_COMMENT_CHARS {
        return Err(format!(
            "繳交說明超過 {} 字",
            crate::api::todos::HOMEWORK_SUBMIT_MAX_COMMENT_CHARS
        ));
    }

    Ok(HomeworkSubmitForm {
        item_id,
        course_id,
        comment,
        is_draft,
        files,
    })
}

/// 取得「可以用來繳交」的 API client，並先抓一次作業內容。
///
/// 抓作業內容有兩個目的：(1) 用第一個上游呼叫驗證 session，若失效就在**還沒上傳
/// 任何位元組之前**重登一次（重新登入後才開始傳檔，避免傳到一半才失敗）；(2) 檔掉
/// 小組作業——小組作業的繳交端點不同（`/api/activities/{id}/groups/{gid}/submission`），
/// 用個人端點送出會被上游拒絕或記到錯的地方，寧可先講清楚。
async fn prepare_submit_api(
    target: &HomeworkTarget,
    state: &AdminWebState,
    course_id: Option<u64>,
    item_id: u64,
) -> std::result::Result<(crate::api::ApiClient, Option<crate::api::todos::HomeworkDetail>), String> {
    let api = iclass_api_for_account(&target.account, &state.accounts_db)
        .await
        .map_err(|error| format!("無法連線學校系統：{error}"))?;

    // 回傳型別是 `HomeworkDetail`（`item` 是強型別 `TronclassTodoItem`）——小組作業
    // 旗標就掛在 detail 上（`submit_by_group`），不是從 `item` 撈 JSON 欄位。
    match api.get_homework_detail(course_id, item_id).await {
        Ok(detail) => Ok((api, detail)),
        Err(error) if crate::api::is_auth_error_report(&error) => {
            info!(account = %target.raw.id, "作業繳交：session 已失效，重新登入後重試");
            let fresh = iclass_api_after_relogin(&target.account, &state.accounts_db)
                .await
                .map_err(|error| format!("重新登入學校系統失敗：{error}"))?;
            let detail = fresh
                .get_homework_detail(course_id, item_id)
                .await
                .map_err(|error| format!("讀取作業資料失敗：{error}"))?;
            Ok((fresh, detail))
        }
        Err(error) => Err(format!("讀取作業資料失敗：{error}")),
    }
}

/// `POST /admin/api/homework/submit`：面板內直接繳交（含重新繳交）。
///
/// HOMEWORK_SUBMIT_V1（fix42）
/// 檔案以 multipart 欄位名 `file` 上傳（可多個）；其餘為文字欄位
/// `item_id`／`course_id`／`comment`／`is_draft`。
/// 管理員可帶 `account_id` 代其他帳號繳交（授權判定沿用 `resolve_homework_target`）。
pub(super) async fn admin_homework_submit_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminHomeworkSubmitQuery>,
    mut multipart: Multipart,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    let target = match resolve_homework_target(
        &state,
        &session,
        query
            .account_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty()),
    )
    .await
    {
        Ok(target) => target,
        Err(response) => return response,
    };

    let _submit_permit = match HOMEWORK_SUBMIT_SEMAPHORE.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": "已有其他繳交正在處理，請稍後再試" })),
            )
                .into_response();
        }
    };

    let form = match parse_homework_submit_form(&mut multipart).await {
        Ok(form) => form,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": message })),
            )
                .into_response();
        }
    };

    let item_id = form.item_id;
    let course_id = form.course_id;
    let deep_link = course_id.map(|course_id| {
        crate::api::todos::iclass_activity_deep_link(&target.base_url, course_id, item_id)
    });

    let (api, detail) = match prepare_submit_api(&target, &state, course_id, item_id).await {
        Ok(prepared) => prepared,
        Err(message) => {
            warn!(account = %target.raw.id, item_id, %message, "作業繳交前置檢查失敗");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "ok": false, "iclass_url": deep_link, "error": message })),
            )
                .into_response();
        }
    };

    if detail
        .as_ref()
        .is_some_and(|detail| detail.submit_by_group)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "iclass_url": deep_link,
                "error": "這是小組作業，請在小組頁面或 iClass 上傳（面板目前只代繳個人作業）",
            })),
        )
            .into_response();
    }

    let file_count = form.files.len();
    let total_bytes: usize = form.files.iter().map(|f| f.bytes.len()).sum();
    let comment = form.comment.clone();
    let is_draft = form.is_draft;

    // ── 逐檔上傳 ────────────────────────────────────────────────────────────
    let mut uploaded: Vec<(u64, String, u64)> = Vec::new();
    for file in form.files {
        let size = file.bytes.len() as u64;
        let ticket = match api.create_upload_ticket(&file.name, size).await {
            Ok(ticket) => ticket,
            Err(error) => {
                warn!(account = %target.raw.id, item_id, file = %file.name, %error, "建立 upload 佔位失敗");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "ok": false,
                        "account_id": target.raw.id,
                        "iclass_url": deep_link,
                        "uploaded": uploaded_json(&uploaded),
                        "error": format!("上傳「{}」失敗：{error}", file.name),
                    })),
                )
                    .into_response();
            }
        };
        let upload_id = ticket.id;
        // 記錄上游給的儲存型態：淡江是 `S3 + MEDIA`（直接 multipart PUT，不需要
        // `/internal-api/upload/callback`）；若哪天變成 `S3 + WRPC` 就得補那一步，
        // 這行 log 是最快的線索。
        tracing::debug!(
            account = %target.raw.id,
            upload_id,
            storage_type = ?ticket.storage_type,
            transcoder = ?ticket.transcoder,
            "已建立 upload 佔位"
        );
        if let Err(error) = api
            .put_upload_content(
                &ticket,
                &file.name,
                file.content_type.as_deref(),
                file.bytes,
            )
            .await
        {
            warn!(account = %target.raw.id, item_id, upload_id, %error, "上傳檔案位元組失敗");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "ok": false,
                    "account_id": target.raw.id,
                    "iclass_url": deep_link,
                    "uploaded": uploaded_json(&uploaded),
                    // 已經建立的 upload 佔位留在上游（沒有綁到繳交，不會出現在作業上）；
                    // 記錄 id 以便必要時由管理員清理。
                    "orphan_upload_ids": upload_ids_json(&uploaded, upload_id),
                    "error": format!("上傳「{}」失敗：{error}", file.name),
                })),
            )
                .into_response();
        }
        uploaded.push((upload_id, file.name.clone(), size));
    }

    let upload_ids: Vec<u64> = uploaded.iter().map(|(id, _, _)| *id).collect();
    let mut submission = match api
        .create_homework_submission(item_id, &upload_ids, &comment, is_draft)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            warn!(account = %target.raw.id, item_id, %error, "建立繳交失敗");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "ok": false,
                    "account_id": target.raw.id,
                    "iclass_url": deep_link,
                    "uploaded": uploaded_json(&uploaded),
                    "error": format!("學校系統拒絕這次繳交：{error}"),
                })),
            )
                .into_response();
        }
    };

    // 上游若只回空 body，就再讀一次目前版本，讓前端一定拿得到可顯示的狀態。
    if submission.get("id").and_then(|v| v.as_u64()).is_none() {
        if let Ok(profile) = api.get_profile().await {
            if let Ok(Some(fresh)) = api
                .get_my_homework_submission(item_id, profile.id)
                .await
            {
                submission = fresh;
            }
        }
    }
    let summary = crate::api::todos::summarize_submission(&submission);

    info!(
        account = %target.raw.id,
        item_id,
        course_id = ?course_id,
        files = file_count,
        total_bytes,
        is_draft,
        submission_id = ?summary.submission_id,
        "作業繳交完成"
    );
    let target_label = if comment.is_empty() {
        format!("item={item_id} files={file_count}")
    } else {
        format!("item={item_id} files={file_count} comment={}字", comment.chars().count())
    };
    crate::logs::record_audit(
        "homework",
        &session.account_id,
        "homework.submit",
        Some(target.raw.id.as_str()),
        Some(target_label.as_str()),
        format!(
            "面板內繳交作業（{}，{} 個檔案 / {} bytes，{}{}）",
            if is_draft { "草稿" } else { "正式送出" },
            file_count,
            total_bytes,
            summary
                .submitted_at
                .as_deref()
                .unwrap_or("未提供繳交時間"),
            summary
                .submission_id
                .map(|id| format!("，submission={id}"))
                .unwrap_or_default()
        ),
    );

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "account_id": target.raw.id,
            "item_id": item_id,
            "course_id": course_id,
            "iclass_url": deep_link,
            "is_draft": is_draft,
            "uploaded": uploaded_json(&uploaded),
            "submission": summary,
            "limits": submit_limits_json(),
        })),
    )
        .into_response()
}

fn uploaded_json(uploaded: &[(u64, String, u64)]) -> serde_json::Value {
    serde_json::Value::Array(
        uploaded
            .iter()
            .map(|(id, name, size)| json!({ "upload_id": id, "name": name, "size": size }))
            .collect(),
    )
}

/// 上傳中途失敗時，列出「已建立但沒綁進繳交」的 upload id，方便日後清理。
fn upload_ids_json(uploaded: &[(u64, String, u64)], current: u64) -> serde_json::Value {
    let mut ids: Vec<u64> = uploaded.iter().map(|(id, _, _)| *id).collect();
    ids.push(current);
    serde_json::Value::Array(ids.into_iter().map(|id| json!(id)).collect())
}

fn submit_limits_json() -> serde_json::Value {
    json!({
        "max_files": HOMEWORK_SUBMIT_MAX_FILES,
        "max_file_bytes": HOMEWORK_SUBMIT_MAX_FILE_BYTES,
        "max_total_bytes": HOMEWORK_SUBMIT_MAX_TOTAL_BYTES,
        "max_comment_chars": crate::api::todos::HOMEWORK_SUBMIT_MAX_COMMENT_CHARS,
    })
}

/// `GET /admin/api/homework/submission`：目前繳交狀態（面板開彈窗時顯示）。
///
/// HOMEWORK_SUBMIT_V1（fix42）
/// 上游「沒繳交」是 404，這裡轉成 `submission.submitted = false`，不是錯誤。
pub(super) async fn admin_homework_submission_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminHomeworkSubmissionQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    let target = match resolve_homework_target(
        &state,
        &session,
        query
            .account_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty()),
    )
    .await
    {
        Ok(target) => target,
        Err(response) => return response,
    };

    let item_id = query.item_id;
    let api = match iclass_api_for_account(&target.account, &state.accounts_db).await {
        Ok(api) => api,
        Err(error) => {
            warn!(account = %target.raw.id, %error, "讀取繳交狀態：取得 iClass client 失敗");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "ok": false, "error": format!("無法連線學校系統：{error}") })),
            )
                .into_response();
        }
    };

    // 需要 user id 才能讀「我」的繳交；先拿 profile，認證失效就重登一次。
    let profile = match api.get_profile().await {
        Ok(profile) => Ok(profile),
        Err(error) if crate::api::is_auth_error_report(&error) => {
            match iclass_api_after_relogin(&target.account, &state.accounts_db).await {
                Ok(fresh) => fresh.get_profile().await,
                Err(relogin_error) => Err(relogin_error),
            }
        }
        Err(error) => Err(error),
    };
    let profile = match profile {
        Ok(profile) => profile,
        Err(error) => {
            warn!(account = %target.raw.id, %error, "讀取繳交狀態：profile 失敗");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "ok": false, "error": format!("讀取帳號資料失敗：{error}") })),
            )
                .into_response();
        }
    };

    let submission = match api
        .get_my_homework_submission(item_id, profile.id)
        .await
    {
        Ok(Some(value)) => crate::api::todos::summarize_submission(&value),
        Ok(None) => crate::api::todos::HomeworkSubmissionSummary::default(),
        Err(error) if crate::api::is_auth_error_report(&error) => {
            match iclass_api_after_relogin(&target.account, &state.accounts_db).await {
                Ok(fresh) => match fresh
                    .get_my_homework_submission(item_id, profile.id)
                    .await
                {
                    Ok(Some(value)) => crate::api::todos::summarize_submission(&value),
                    Ok(None) => crate::api::todos::HomeworkSubmissionSummary::default(),
                    Err(error) => {
                        warn!(account = %target.raw.id, item_id, %error, "讀取繳交狀態失敗（重登後）");
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({
                                "ok": false,
                                "error": format!("讀取繳交狀態失敗：{error}"),
                            })),
                        )
                            .into_response();
                    }
                },
                Err(relogin_error) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "ok": false,
                            "error": format!("重新登入學校系統失敗：{relogin_error}"),
                        })),
                    )
                        .into_response();
                }
            }
        }
        Err(error) => {
            warn!(account = %target.raw.id, item_id, %error, "讀取繳交狀態失敗");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "ok": false, "error": format!("讀取繳交狀態失敗：{error}") })),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "account_id": target.raw.id,
            "item_id": item_id,
            "submission": submission,
            "limits": submit_limits_json(),
        })),
    )
        .into_response()
}
