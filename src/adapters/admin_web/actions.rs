//! Admin action endpoint handlers for Admin Web.
//!
//! This module owns POST /admin/api/action and the helper logic behind
//! force/start/stop/reauth, account administration, runtime settings,
//! mock dispatch, and VM reboot.

use super::*;

pub(super) async fn admin_action_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    body: axum::body::Bytes,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };

    // Parse manually so Axum/serde's 422 rejection cannot expose Rust type
    // names, field names, or parser line/column details to API callers.
    let action: AdminActionRequest = match serde_json::from_slice(&body) {
        Ok(action) => action,
        Err(error) => {
            tracing::debug!(error = %error, "admin action request rejected as invalid JSON");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_request" })),
            )
                .into_response();
        }
    };

    for (value, max_bytes, field) in [
        (
            Some(action.action.as_str()),
            ADMIN_MAX_ACTION_BYTES,
            "action",
        ),
        (
            action.account_id.as_deref(),
            ADMIN_MAX_IDENTIFIER_BYTES,
            "account_id",
        ),
        (action.id.as_deref(), ADMIN_MAX_IDENTIFIER_BYTES, "id"),
        (
            action.provider.as_deref(),
            ADMIN_MAX_ACTION_BYTES,
            "provider",
        ),
        (
            action.username.as_deref(),
            ADMIN_MAX_USERNAME_BYTES,
            "username",
        ),
        (
            action.password.as_deref(),
            ADMIN_MAX_PASSWORD_BYTES,
            "password",
        ),
        (
            action.line_user_id.as_deref(),
            ADMIN_MAX_IDENTIFIER_BYTES,
            "line_user_id",
        ),
        (
            action.discord_user_id.as_deref(),
            ADMIN_MAX_IDENTIFIER_BYTES,
            "discord_user_id",
        ),
        (
            action.mock_kind.as_deref(),
            ADMIN_MAX_ACTION_BYTES,
            "mock_kind",
        ),
        (action.code.as_deref(), 1024, "code"),
    ] {
        if let Err(response) = validate_optional_admin_text(value, max_bytes, field) {
            return response;
        }
    }
    if let Some(Some(value)) = action.auto_stop_time.as_ref() {
        if let Err(response) = validate_admin_text(value, 16, "auto_stop_time") {
            return response;
        }
    }
    // CURRENT_SEMESTER_OVERRIDE_V1
    if let Some(value) = action.semester_override.as_deref() {
        if let Err(response) = validate_admin_text(value, 32, "semester_override") {
            return response;
        }
    }
    if let Some(rest_periods) = action.rest_periods.as_ref() {
        if rest_periods.len() > ADMIN_MAX_REST_PERIODS {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("too many rest periods; max {}", ADMIN_MAX_REST_PERIODS)
                })),
            )
                .into_response();
        }
        for period in rest_periods {
            if let Err(response) = validate_admin_text(&period.start_time, 16, "rest start_time") {
                return response;
            }
            if let Err(response) = validate_admin_text(&period.end_time, 16, "rest end_time") {
                return response;
            }
        }
    }

    let action_name = action.action.trim().to_string();
    let account_id = action
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned);

    // DIGEST_TEST_IN_TEST_CARD_V1：晨間摘要測試推送只送到「目前登入者」自己的裝置，
    // 與 /admin/api/push/test、/admin/api/bulletins/test 相同，不需管理員權限；
    // 提前處理，避免落入下方 targets 迴圈被判為 unknown action。
    if action_name == "digest_test" {
        // TEST_NOTIFICATION_RATE_LIMIT_V1
        if let Err(response) =
            check_test_notification_rate(&state, "digest", &session.account_id).await
        {
            return response;
        }
        crate::logs::record_audit(
            "web",
            &session.account_id,
            &action_name,
            None,
            None,
            "Web 設定頁：晨間摘要測試推送",
        );
        return handle_admin_account_action(&state, &session, &action_name, action).await;
    }

    // BINDING_SELF_SERVICE_V1：綁定／解除 Discord、LINE 通知管道。
    // 一般使用者只能改自己的帳號；管理員可指定 account_id。與 digest_test 一樣
    // 提前處理，避免落入下方 targets 迴圈被判為 unknown action。
    if action_name == "set_binding" {
        let target_id = account_id
            .clone()
            .unwrap_or_else(|| session.account_id.clone());
        if !session.is_admin && target_id != session.account_id {
            return unauthorized_response();
        }
        crate::logs::record_audit(
            "web",
            &session.account_id,
            &action_name,
            Some(target_id.as_str()),
            None,
            "Web 通知頁：更新 Discord／LINE 綁定",
        );
        return handle_set_binding(&state, &target_id, action).await;
    }

    if matches!(
        action_name.as_str(),
        "add_account"
            | "start_monitor"
            | "set_admin"
            | "unset_admin"
            | "set_viewer"
            | "unset_viewer"
            | "reset_password"
            | "mock_rollcall"
            | "runtime_settings"
            | "digest_settings"
    ) {
        if !session.is_admin {
            return unauthorized_response();
        }
        let target = account_id.as_deref().or(action.id.as_deref());
        crate::logs::record_audit(
            "web",
            &session.account_id,
            &action_name,
            target,
            None,
            "Web 管理頁管理員操作請求",
        );
        return handle_admin_account_action(&state, &session, &action_name, action).await;
    }

    let targets = if action_name == "force" && !session.is_admin {
        // 一般使用者按任一 Force 時，立即喚醒所有帳號一起檢查，避免等待
        // 先由該使用者帳號偵測到點名後才 cascade，造成速度過慢。
        state.accounts.all()
    } else {
        match resolve_targets(&state, &session, account_id.as_deref()) {
            Ok(targets) => targets,
            Err(resp) => return resp,
        }
    };

    // ACTION_FEEDBACK_V1：除了 affected 數字，額外回報「實際發生了什麼」——
    // 哪些帳號本來就已是目標狀態（start 對已在跑的帳號、stop 對已停的帳號）、
    // 哪些帳號被喚醒；前端把 message 直接當 toast，使用者不必再猜「操作完成」是什麼意思。
    let mut affected = 0usize;
    let mut changed: Vec<String> = Vec::new();
    let mut unchanged: Vec<String> = Vec::new();
    for account in &targets {
        match action_name.as_str() {
            "force" | "force_all" => {
                account.force_poll_tx.notify_one();
                affected += 1;
                changed.push(account.account_id.clone());
            }
            "reauth" | "reauth_all" => {
                account.reauth_tx.notify_one();
                affected += 1;
                changed.push(account.account_id.clone());
            }
            "start" | "start_all" => {
                let was_running = *account.is_running.lock().await;
                set_running(account, true).await;
                account.force_poll_tx.notify_one();
                affected += 1;
                if was_running {
                    unchanged.push(account.account_id.clone());
                } else {
                    changed.push(account.account_id.clone());
                }
            }
            "stop" | "stop_all" => {
                let was_running = *account.is_running.lock().await;
                set_running(account, false).await;
                affected += 1;
                if was_running {
                    changed.push(account.account_id.clone());
                } else {
                    unchanged.push(account.account_id.clone());
                }
            }
            other => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("unknown action: {other}") })),
                )
                    .into_response();
            }
        }
    }

    super::status::invalidate_admin_status_cache().await;

    let message = action_feedback_message(&action_name, &changed, &unchanged, session.is_admin);

    info!(
        action = %action_name,
        actor = %session.account_id,
        admin = session.is_admin,
        account = ?account_id,
        affected = affected,
        "admin web action executed"
    );
    let target = format!("affected={affected}");
    crate::logs::record_audit(
        "web",
        &session.account_id,
        &action_name,
        account_id.as_deref(),
        Some(target.as_str()),
        format!("Web 管理頁操作，影響 {affected} 個帳號"),
    );
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "affected": affected,
            // ACTION_FEEDBACK_V1
            "message": message,
            "changed": changed,
            "unchanged": unchanged,
        })),
    )
        .into_response()
}

/// ACTION_FEEDBACK_V1：把 force／reauth／start／stop 的結果組成一句人看得懂的中文。
/// 帳號 ID 最多列 3 個，其餘以「等 N 個帳號」帶過，避免 toast 過長。
fn action_feedback_message(
    action: &str,
    changed: &[String],
    unchanged: &[String],
    is_admin: bool,
) -> String {
    fn list(ids: &[String]) -> String {
        const SHOW: usize = 3;
        if ids.is_empty() {
            return String::new();
        }
        if ids.len() == 1 {
            return ids[0].clone();
        }
        if ids.len() <= SHOW {
            return ids.join("、");
        }
        format!("{} 等 {} 個帳號", ids[..SHOW].join("、"), ids.len())
    }
    let total = changed.len() + unchanged.len();
    if total == 0 {
        return "沒有符合條件的帳號，未執行任何操作".to_string();
    }
    match action {
        "force" | "force_all" => {
            let scope = if total == 1 {
                list(changed)
            } else if action == "force" && !is_admin {
                // 一般使用者按 Force 會喚醒全部帳號一起檢查（見上方 targets 邏輯）。
                format!("全部 {total} 個帳號")
            } else {
                format!("{total} 個帳號")
            };
            format!("已要求 {scope} 立即檢查點名，結果會在下次輪詢後更新")
        }
        "reauth" | "reauth_all" => {
            let scope = if total == 1 { list(changed) } else { format!("{total} 個帳號") };
            format!("已要求 {scope} 重新登入學校系統，數秒內可在帳號卡看到結果")
        }
        "start" | "start_all" => {
            let mut parts = Vec::new();
            if !changed.is_empty() {
                parts.push(format!("已啟動 {} 的監控並立即檢查一次", list(changed)));
            }
            if !unchanged.is_empty() {
                parts.push(format!("{} 原本就在監控中", list(unchanged)));
            }
            parts.join("；")
        }
        "stop" | "stop_all" => {
            let mut parts = Vec::new();
            if !changed.is_empty() {
                parts.push(format!("已暫停 {} 的監控，期間不會偵測點名", list(changed)));
            }
            if !unchanged.is_empty() {
                parts.push(format!("{} 原本就是暫停狀態", list(unchanged)));
            }
            parts.join("；")
        }
        _ => format!("操作完成，影響 {total} 個帳號"),
    }
}

pub(super) async fn handle_admin_account_action(
    state: &AdminWebState,
    session: &AdminSession,
    action_name: &str,
    action: AdminActionRequest,
) -> Response {
    match action_name {
        "add_account" => {
            let id = action
                .id
                .or(action.account_id)
                .unwrap_or_default()
                .trim()
                .to_string();
            let provider = action.provider.unwrap_or_default().trim().to_string();
            let username = action.username.unwrap_or_default().trim().to_string();
            let password = action.password.unwrap_or_default().trim().to_string();
            let line_user_id = action.line_user_id.unwrap_or_default().trim().to_string();
            let discord_user_id = action
                .discord_user_id
                .unwrap_or_default()
                .trim()
                .to_string();
            // ADD_ACCOUNT_ROLE_V1：role 欄位優先；沒給時退回舊的 is_admin 布林。
            let role = action
                .role
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| {
                    if action.is_admin.unwrap_or(false) { "admin".to_string() } else { "user".to_string() }
                });
            let (is_admin, is_viewer) = match role.as_str() {
                "admin" => (true, false),
                "viewer" => (false, true),
                "user" => (false, false),
                other => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("未知的角色：{other}（可用 admin / user / viewer）") })),
                    )
                        .into_response();
                }
            };

            let account_id = if id.is_empty() { username.clone() } else { id };
            if account_id.is_empty()
                || provider.is_empty()
                || username.is_empty()
                || password.is_empty()
            {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "id/provider/username/password are required" })),
                )
                    .into_response();
            }

            // PROVIDER_KIND_NO_PANIC_V1：provider 不存在或 kind 無登入流程時直接 400，
            // 不寫入 DB（否則下次重啟會因這個帳號無法通過 resolve 而整個起不來）。
            if let Err(message) = crate::auth::providers::check_provider_supported(&provider) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": message })),
                )
                    .into_response();
            }

            let account = RawAccountConfig {
                id: account_id.clone(),
                provider,
                username,
                password,
                enabled: true,
                line_user_id,
                discord_user_id,
                session_cookie: String::new(),
                is_admin,
                is_viewer: false,
                dashboard_password_hash: String::new(),
            };

            if let Err(e) = state.accounts_db.insert(&account).await {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("新增帳號失敗：{e}") })),
                )
                    .into_response();
            }
            // ADD_ACCOUNT_ROLE_V1 / VIEWER_ROLE_V1：檢視者旗標是獨立欄位，insert 後補設。
            if is_viewer {
                if let Err(e) = state.accounts_db.set_viewer(&account_id, true).await {
                    warn!(account = %account_id, error = %e, "新增帳號後設定檢視者旗標失敗");
                }
            }

            // VIEWER_BOOT_EXCLUDE_V1：檢視者只看儀表板，不需要監控循環，也不登入學校系統。
            if is_viewer {
                super::status::invalidate_admin_status_cache().await;
                let message = format!(
                    "已新增檢視者帳號 {account_id}（唯讀）。此帳號不會登入學校系統、不做點名監控；網站登入需由管理員另行設定 Dashboard 密碼。"
                );
                crate::logs::record_audit(
                    "web",
                    &session.account_id,
                    "add_account_viewer",
                    Some(account_id.as_str()),
                    Some("viewer"),
                    message.clone(),
                );
                return (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "affected": 1,
                        "account_id": account_id,
                        "monitor_started": false,
                        "viewer": true,
                        "message": message,
                    })),
                )
                    .into_response();
            }

            // ACCOUNT_HOT_ADD_V1：寫入 DB 後直接熱啟動監控循環（會實際登入一次）。
            // 登入失敗時帳號仍保留在 DB（密碼可能只是暫時打錯／學校系統暫時異常），
            // 使用者可修正後用「重登」或重啟服務再試；訊息中明確說明目前狀態。
            let (monitor_started, message) = match state.monitor_spawner.as_ref() {
                Some(spawner) => match spawner.spawn_account(&account_id).await {
                    Ok(started) => {
                        info!(account = %started.account_id, actor = %session.account_id, "Web 新增的帳號已熱啟動監控");
                        super::status::invalidate_admin_status_cache().await;
                        let who = if started.user_name.trim().is_empty() {
                            String::new()
                        } else {
                            format!("（{}）", started.user_name.trim())
                        };
                        (
                            true,
                            format!(
                                "已新增帳號 {account_id}{who}，登入成功並已開始監控，不需重啟服務。網站登入需由管理員另行設定 Dashboard 密碼。"
                            ),
                        )
                    }
                    Err(reason) => (
                        false,
                        format!(
                            "已新增帳號 {account_id} 到資料庫，但即時啟動監控失敗：{reason}。請確認帳號密碼後重啟服務再試。"
                        ),
                    ),
                },
                None => (
                    false,
                    format!(
                        "已新增帳號 {account_id}；請重啟服務後才會開始監控。網站登入需由管理員另行設定 Dashboard 密碼。"
                    ),
                ),
            };
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "add_account_hot_start",
                Some(account_id.as_str()),
                Some(if monitor_started { "started" } else { "not_started" }),
                message.clone(),
            );
            // ACCOUNT_HOT_ADD_FEEDBACK_V1：帳號已寫入 DB 所以仍是 200，但前端要能
            // 明確分辨「已開始監控」與「已入庫但沒在監控」：後者以 `warning` 帶原因，
            // 前端會用紅字留在對話框內，而不是綠色 toast 一閃而過。
            let mut body = json!({
                "ok": true,
                "affected": 1,
                "account_id": account_id,
                "monitor_started": monitor_started,
                "message": message,
            });
            if !monitor_started {
                body["warning"] = json!(message);
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        // ACCOUNT_HOT_ADD_FEEDBACK_V1：帳號已在 DB 但沒有監控循環（熱啟動登入失敗、
        // 或 CLI 離線新增）時，管理員可在狀態頁按「啟動監控」重試，不必重啟服務。
        "start_monitor" => {
            let target = action
                .account_id
                .or(action.id)
                .unwrap_or_default()
                .trim()
                .to_string();
            if target.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "account_id is required" })),
                )
                    .into_response();
            }
            let Some(spawner) = state.monitor_spawner.as_ref() else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "此執行環境沒有監控 runtime，請重啟服務" })),
                )
                    .into_response();
            };
            match spawner.spawn_account(&target).await {
                Ok(started) => {
                    super::status::invalidate_admin_status_cache().await;
                    crate::logs::record_audit(
                        "web",
                        &session.account_id,
                        "start_monitor",
                        Some(target.as_str()),
                        Some("started"),
                        format!("Web 管理頁：手動啟動帳號 {} 的監控", started.account_id),
                    );
                    let who = if started.user_name.trim().is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", started.user_name.trim())
                    };
                    (
                        StatusCode::OK,
                        Json(json!({
                            "ok": true,
                            "affected": 1,
                            "monitor_started": true,
                            "message": format!("帳號 {target}{who} 登入成功，已開始監控。"),
                        })),
                    )
                        .into_response()
                }
                Err(reason) => {
                    crate::logs::record_audit(
                        "web",
                        &session.account_id,
                        "start_monitor",
                        Some(target.as_str()),
                        Some("failed"),
                        format!("Web 管理頁：啟動帳號 {target} 的監控失敗：{reason}"),
                    );
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("啟動監控失敗：{reason}") })),
                    )
                        .into_response()
                }
            }
        }
        "reset_password" => {
            let Some(account_id) = action
                .account_id
                .or(action.id)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
            else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "account_id is required" })),
                )
                    .into_response();
            };
            match state
                .accounts_db
                .set_dashboard_password_hash(&account_id, "")
                .await
            {
                Ok(true) => {
                    state.revoke_sessions_for_account(&account_id).await;
                    super::status::invalidate_admin_status_cache().await;
                    (
                        StatusCode::OK,
                        Json(json!({
                            "ok": true,
                            "affected": 1,
                            "message": "已重製密碼，該帳號可用 Tronclass 密碼登入，並須重新設定 Dashboard 密碼"
                        })),
                    )
                        .into_response()
                }
                Ok(false) => (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": format!("account not found: {account_id}") })),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("重製密碼失敗：{e}") })),
                )
                    .into_response(),
            }
        }
        // VIEWER_ROLE_V1：設定／取消檢視者（唯讀）。與 set_admin 相同：改完清快取、
        // 撤銷該帳號所有 session 強制重新登入，讓新的角色立即生效。
        "set_viewer" | "unset_viewer" => {
            let Some(account_id) = action
                .account_id
                .or(action.id)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
            else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "account_id is required" })),
                )
                    .into_response();
            };
            if account_id == session.account_id {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "不能變更自己的檢視者角色" })),
                )
                    .into_response();
            }
            let is_viewer = action_name == "set_viewer";
            match state.accounts_db.set_viewer(&account_id, is_viewer).await {
                Ok(true) => {
                    super::status::invalidate_admin_status_cache().await;
                    state.revoke_sessions_for_account(&account_id).await;
                    // VIEWER_NO_SCHOOL_API_V1：改為檢視者後立即暫停其監控循環
                    // （重啟後 resolve() 會直接排除，不再登入學校系統）。
                    if is_viewer {
                        if let Some(runtime) = state.account_by_id(&account_id) {
                            set_running(&runtime, false).await;
                        }
                    }
                    crate::logs::record_audit(
                        "web",
                        &session.account_id,
                        action_name,
                        Some(&account_id),
                        None,
                        if is_viewer {
                            "已設定為檢視者（唯讀），該帳號所有 session 已撤銷"
                        } else {
                            "已取消檢視者，該帳號所有 session 已撤銷"
                        },
                    );
                    (
                        StatusCode::OK,
                        Json(json!({
                            "ok": true,
                            "affected": 1,
                            "message": if is_viewer { "已設定為檢視者（唯讀）" } else { "已取消檢視者" }
                        })),
                    )
                        .into_response()
                }
                Ok(false) => (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": format!("account not found: {account_id}") })),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("更新檢視者失敗：{e}") })),
                )
                    .into_response(),
            }
        }
        "set_admin" | "unset_admin" => {
            let Some(account_id) = action
                .account_id
                .or(action.id)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
            else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "account_id is required" })),
                )
                    .into_response();
            };
            let is_admin = action_name == "set_admin";
            match state.accounts_db.set_admin(&account_id, is_admin).await {
                Ok(true) => {
                    // Do not keep the old admin badge/button state for the cache TTL.
                    super::status::invalidate_admin_status_cache().await;
                    // AdminSession caches is_admin at login time and lives for
                    // 8 hours, so a demoted account would otherwise keep full
                    // admin API access until its session expired. Force a
                    // re-login so the new privilege level is picked up.
                    state.revoke_sessions_for_account(&account_id).await;
                    (
                        StatusCode::OK,
                        Json(json!({
                            "ok": true,
                            "affected": 1,
                            "message": if is_admin { "已設定為管理員" } else { "已取消管理員" }
                        })),
                    )
                        .into_response()
                }
                Ok(false) => (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": format!("account not found: {account_id}") })),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("更新管理員失敗：{e}") })),
                )
                    .into_response(),
            }
        }
        "mock_rollcall" => super::mock::handle_mock_rollcall_action(state, action).await,
        "digest_settings" => {
            // MORNING_DIGEST_V1：晨間摘要設定（持久化於 config/digest.json）。
            let current = super::morning_digest::digest_settings();
            let enabled = action.digest_enabled.unwrap_or(current.enabled);
            let time = action
                .digest_time
                .clone()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or(current.time);
            match super::morning_digest::set_digest_settings(enabled, time).await {
                Ok(normalized) => {
                    super::status::invalidate_admin_status_cache().await;
                    (
                        StatusCode::OK,
                        Json(json!({
                            "ok": true,
                            "digest": { "enabled": enabled, "time": normalized }
                        })),
                    )
                        .into_response()
                }
                Err(error) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("{error}") })),
                )
                    .into_response(),
            }
        }
        "digest_test" => {
            // MORNING_DIGEST_V1：立即推送一則測試摘要到「目前登入者」的裝置。
            match super::morning_digest::send_test_now(&state, &session.account_id).await {
                Ok(devices) => (
                    StatusCode::OK,
                    Json(json!({ "ok": true, "devices": devices })),
                )
                    .into_response(),
                Err(error) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("{error}") })),
                )
                    .into_response(),
            }
        }
        "runtime_settings" => {
            let current = crate::runtime_settings::snapshot().await;
            let settings = RuntimeSettings {
                poll_interval_secs: match action.poll_interval_secs {
                    Some(value) => value,
                    None => current.poll_interval_secs,
                },
                group_submit_interval_ms: action
                    .group_submit_interval_ms
                    .unwrap_or(current.group_submit_interval_ms),
                auto_stop_time: match action.auto_stop_time {
                    Some(value) => value,
                    None => current.auto_stop_time,
                },
                rest_periods: action.rest_periods.clone().unwrap_or(current.rest_periods),
                // CURRENT_SEMESTER_OVERRIDE_V1：Some(值)＝設定／Some("")＝清除、None＝不變
                semester_override: match action.semester_override.clone() {
                    Some(value) => Some(value),
                    None => current.semester_override.clone(),
                },
            };
            crate::runtime_settings::update(settings.clone()).await;
            super::status::invalidate_admin_status_cache().await;
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "affected": 1,
                    "message": "運行參數已更新",
                    "settings": settings,
                })),
            )
                .into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown action: {action_name}") })),
        )
            .into_response(),
    }
}

/// BINDING_SELF_SERVICE_V1：更新帳號的 Discord／LINE 綁定。
///
/// 只更新請求中有帶的欄位（`discord_user_id` / `line_user_id`），帶空字串代表解除。
/// 寫入 accounts.db 後同步更新 runtime `AdapterAccountTarget`，讓下一次通知立即生效；
/// 與 Discord `/bind-account` 走同一組 DB／runtime 寫入路徑。
async fn handle_set_binding(
    state: &AdminWebState,
    target_id: &str,
    action: AdminActionRequest,
) -> Response {
    let discord = action
        .discord_user_id
        .as_deref()
        .map(str::trim)
        .map(ToOwned::to_owned);
    let line = action
        .line_user_id
        .as_deref()
        .map(str::trim)
        .map(ToOwned::to_owned);
    if discord.is_none() && line.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "discord_user_id 或 line_user_id 至少需要一個" })),
        )
            .into_response();
    }
    if let Some(value) = discord.as_deref() {
        if !value.is_empty() && !value.chars().all(|c| c.is_ascii_digit()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Discord User ID 必須是純數字（在 Discord 開啟開發者模式後右鍵使用者 → 複製使用者 ID）" })),
            )
                .into_response();
        }
    }
    if let Some(value) = line.as_deref() {
        if !value.is_empty()
            && !(value.starts_with('U') && value.len() == 33 && value[1..].chars().all(|c| c.is_ascii_hexdigit()))
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "LINE User ID 格式錯誤：應為 U 開頭、共 33 碼（可在 LINE Bot 對話輸入「我的ID」取得）" })),
            )
                .into_response();
        }
    }

    // 先確認帳號存在（inactive 帳號也允許綁定，所以查 DB 而不是 runtime）。
    match state.accounts_db.get(target_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("account not found: {target_id}") })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("讀取帳號失敗：{e}") })),
            )
                .into_response();
        }
    }

    let runtime = state.account_by_id(target_id);
    if let Some(value) = discord.as_deref() {
        if let Err(e) = state.accounts_db.set_discord_user_id(target_id, value).await {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("更新 Discord 綁定失敗：{e}") })),
            )
                .into_response();
        }
        if let Some(account) = runtime.as_ref() {
            account.target.lock().await.discord_user_id = value.to_string();
        }
    }
    if let Some(value) = line.as_deref() {
        if let Err(e) = state.accounts_db.set_line_user_id(target_id, value).await {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("更新 LINE 綁定失敗：{e}") })),
            )
                .into_response();
        }
        if let Some(account) = runtime.as_ref() {
            account.target.lock().await.line_user_id = value.to_string();
        }
    }
    super::status::invalidate_admin_status_cache().await;
    info!(
        account = %target_id,
        discord = ?discord,
        line = ?line,
        "BINDING_SELF_SERVICE_V1 通知管道綁定已更新"
    );
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "account_id": target_id,
            "discord_user_id": discord,
            "line_user_id": line,
            "message": "綁定已更新，之後的通知會立即使用新的設定"
        })),
    )
        .into_response()
}

pub(super) fn resolve_targets(
    state: &AdminWebState,
    session: &AdminSession,
    account_id: Option<&str>,
) -> Result<Vec<RequestAccountState>, Response> {
    if session.is_admin {
        if let Some(account_id) = account_id {
            let Some(account) = state.account_by_id(account_id) else {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": format!("account not found: {account_id}") })),
                )
                    .into_response());
            };
            Ok(vec![account])
        } else {
            Ok(state.accounts.all())
        }
    } else {
        let target_id = account_id.unwrap_or(&session.account_id);
        if target_id != session.account_id {
            return Err(unauthorized_response());
        }
        let Some(account) = state.account_by_id(&session.account_id) else {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "runtime account not found" })),
            )
                .into_response());
        };
        Ok(vec![account])
    }
}

pub(super) async fn set_running(account: &RequestAccountState, is_running: bool) {
    *account.is_running.lock().await = is_running;
    let mut status = account.monitor_status.lock().await;
    status.is_running = is_running;
}
