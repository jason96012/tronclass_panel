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

    if matches!(
        action_name.as_str(),
        "add_account"
            | "set_admin"
            | "unset_admin"
            | "reset_password"
            | "mock_rollcall"
            | "runtime_settings"
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
        return handle_admin_account_action(&state, &action_name, action).await;
    }

    let targets = if action_name == "force" && !session.is_admin {
        // 一般使用者按任一 Force 時，立即喚醒所有帳號一起檢查，避免等待
        // 先由該使用者帳號偵測到點名後才 cascade，造成速度過慢。
        state.accounts.iter().collect::<Vec<_>>()
    } else {
        match resolve_targets(&state, &session, account_id.as_deref()) {
            Ok(targets) => targets,
            Err(resp) => return resp,
        }
    };

    let mut affected = 0usize;
    for account in targets {
        match action_name.as_str() {
            "force" | "force_all" => {
                account.force_poll_tx.notify_one();
                affected += 1;
            }
            "reauth" | "reauth_all" => {
                account.reauth_tx.notify_one();
                affected += 1;
            }
            "start" | "start_all" => {
                set_running(account, true).await;
                account.force_poll_tx.notify_one();
                affected += 1;
            }
            "stop" | "stop_all" => {
                set_running(account, false).await;
                affected += 1;
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

    super::invalidate_admin_status_cache().await;

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
        Json(json!({ "ok": true, "affected": affected })),
    )
        .into_response()
}

pub(super) async fn handle_admin_account_action(
    state: &AdminWebState,
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
            let is_admin = action.is_admin.unwrap_or(false);

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
                dashboard_password_hash: String::new(),
            };

            match state.accounts_db.insert(&account).await {
                Ok(()) => (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "affected": 1,
                        "message": format!("已新增帳號 {account_id}；請重啟服務後才會開始監控。網站登入需由管理員另行設定 Dashboard 密碼。")
                    })),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("新增帳號失敗：{e}") })),
                )
                    .into_response(),
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
                    super::invalidate_admin_status_cache().await;
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
                    super::invalidate_admin_status_cache().await;
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
                rest_periods: action.rest_periods.unwrap_or(current.rest_periods),
            };
            crate::runtime_settings::update(settings.clone()).await;
            super::invalidate_admin_status_cache().await;
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

pub(super) fn resolve_targets<'a>(
    state: &'a AdminWebState,
    session: &AdminSession,
    account_id: Option<&str>,
) -> Result<Vec<&'a RequestAccountState>, Response> {
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
            Ok(state.accounts.iter().collect())
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
