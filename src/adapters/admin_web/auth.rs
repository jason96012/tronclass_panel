//! Dashboard 登入/登出/session 刷新/改密碼與登入審計 handlers。

use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct AdminActivityQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminLoginRequest {
    pub(super) username: String,
    pub(super) password: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminPasswordChangeRequest {
    #[serde(default)]
    current_password: String,
    #[serde(default)]
    new_password: String,
}

#[derive(Debug, Serialize)]
pub(super) struct AdminLoginResponse {
    /// Compatibility marker for legacy static pages. The actual random token is
    /// in a HttpOnly cookie and is never returned to browser JavaScript.
    pub(super) token: String,
    pub(super) account_id: String,
    pub(super) is_admin: bool,
    /// True only for a first login that used the upstream Tronclass password.
    /// The browser must immediately show the mandatory password-change flow.
    pub(super) must_change_password: bool,
}

pub(super) async fn admin_login_activity_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminActivityQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }

    let limit = query.limit.unwrap_or(200).clamp(1, 1000) as i64;
    match state
        .accounts_db
        .list_activity_events(Some("login"), limit)
        .await
    {
        Ok(events) => (StatusCode::OK, Json(json!({ "events": events }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("activity database error: {e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_login_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    body: axum::body::Bytes,
) -> Response {
    let login: AdminLoginRequest = match serde_json::from_slice(&body) {
        Ok(login) => login,
        Err(error) => {
            // Keep parser details in server logs only; never expose field names,
            // line numbers or the serde/axum implementation to a caller.
            tracing::debug!(error = %error, "admin login request rejected as invalid JSON");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_request" })),
            )
                .into_response();
        }
    };
    if let Err(response) =
        validate_admin_text(&login.username, ADMIN_MAX_USERNAME_BYTES, "username")
    {
        return response;
    }
    if let Err(response) =
        validate_admin_text(&login.password, ADMIN_MAX_PASSWORD_BYTES, "password")
    {
        return response;
    }
    if ip_is_blacklisted(&state, &ip).await {
        warn!(ip = %ip, "password login rejected for blacklisted IP");
        return blocked_ip_response();
    }
    if !state
        .rate_limiter
        .check_limit(&format!("login-ip:{ip}"), IP_RATE_LIMIT_MAX_ATTEMPTS)
        .await
    {
        warn!(ip = %ip, "login rate limited by ip");
        alert_abnormal_traffic(&state, "密碼登入", "", &ip).await;
        return rate_limited_response();
    }

    let username = login.username.trim();
    // Passwords are opaque strings: do not trim user-entered whitespace.
    let password = login.password.as_str();
    if username.is_empty() || password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "username and password are required" })),
        )
            .into_response();
    }

    let client = ip.clone();
    let rate_key = format!("login:{}:{client}", username.to_lowercase());
    if !state.rate_limiter.check(&rate_key).await {
        warn!(username = %username, client = %client, "admin web login rate limited");
        // The sixth request is rejected by the five-attempt login rate limiter
        // before password verification. It must still count as a failed login;
        // otherwise the auto-blacklist threshold (>5 failures) is impossible
        // to reach. Run this off the response path just like a normal failed
        // password verification.
        {
            let state = state.clone();
            let user = username.to_string();
            let ip = ip.clone();
            tokio::spawn(async move {
                record_persistent_login_event(
                    &state,
                    &user,
                    None,
                    "login_rate_limited",
                    "failed",
                    "Dashboard login rejected by rate limit",
                    &ip,
                )
                .await;
                alert_password_failures(&state, &user, &ip).await;
            });
        }
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "too many attempts; please wait" })),
        )
            .into_response();
    }

    // Try exact account-id match first, then fall back to username search.
    // LOGIN_PERF_V1：兩個查詢彼此獨立，同時發出（SQLite 讀取池有 10 條連線），
    // 以學號登入（id 查不到、要走 username）的常見路徑少等一趟。
    let (by_id, by_username) = tokio::join!(
        state.accounts_db.get(username),
        state.accounts_db.find_by_username(username, None),
    );
    let mut candidates = Vec::new();
    if let Ok(Some(account)) = by_id {
        candidates.push(account);
    }
    if candidates.is_empty() {
        if let Ok(accounts) = by_username {
            candidates = accounts;
        }
    }

    // LOGIN_TIERED_LOCKOUT_V1 第一層：帳號密碼登入若在鎖定中，直接回 423，不做驗證。
    // 鍵含輸入名稱與所有候選帳號的 id / 學號（與上鎖時一致）。這次嘗試仍計入失敗，
    // 攻擊者持續打會走到第二層（IP 黑名單）；持有人可由 Discord「取消這次鎖定」。
    {
        let mut lock_keys: Vec<String> = vec![username.to_string()];
        for account in &candidates {
            lock_keys.push(account.id.clone());
            lock_keys.push(account.username.clone());
        }
        if let Some(remaining) = login_lock_remaining(&lock_keys).await {
            warn!(username = %username, client = %client, remaining, "admin web login rejected: account temporarily locked");
            {
                let state = state.clone();
                let user = username.to_string();
                let ip = ip.clone();
                tokio::spawn(async move {
                    record_persistent_login_event(
                        &state,
                        &user,
                        None,
                        "login_locked",
                        "failed",
                        "Dashboard login rejected: account temporarily locked",
                        &ip,
                    )
                    .await;
                    alert_password_failures(&state, &user, &ip).await;
                });
            }
            return (
                StatusCode::LOCKED,
                Json(json!({
                    "error": "account_locked",
                    "retry_after_secs": remaining,
                })),
            )
                .into_response();
        }
    }

    // LOGIN_CANDIDATE_FILTER_V1：先「篩選」再「驗證」——不再對每個候選帳號都跑一次
    // 慢雜湊。規則：
    //   * 停用帳號直接排除；
    //   * 有 Dashboard 密碼者走正式驗證；沒有者才允許一次性的 Tronclass 密碼 bootstrap；
    //   * 有 Dashboard 密碼的候選優先（同學號多帳號時，已完成設定的那個先驗）；
    //   * 只驗到第一個成功為止。
    // 慢雜湊一律以 spawn_blocking 執行（LOGIN_VERIFY_OFFLOAD_V1），不佔 tokio worker。
    let mut ordered: Vec<crate::account::RawAccountConfig> = candidates
        .into_iter()
        .filter(|account| account.enabled)
        .collect();
    ordered.sort_by_key(|account| account.dashboard_password_hash.is_empty());

    let mut matched = None;
    let mut ran_slow_verify = false;
    for account in ordered {
        if !account.dashboard_password_hash.is_empty() {
            ran_slow_verify = true;
            if crate::dashboard_auth::verify_dashboard_password_async(
                password.to_string(),
                account.dashboard_password_hash.clone(),
            )
            .await
            {
                matched = Some((account, false));
                break;
            }
            continue;
        }
        if crate::dashboard_auth::verify_first_login_password(password, &account.password) {
            matched = Some((account, true));
            break;
        }
    }
    // LOGIN_TIMING_EQUALIZE_V1：沒有任何候選跑過慢雜湊（帳號不存在／停用／只有 bootstrap
    // 比對）時，補做一次固定假雜湊驗證，讓回應時間與「帳號存在但密碼錯」相近。
    if matched.is_none() && !ran_slow_verify {
        crate::dashboard_auth::burn_dummy_verification(password.to_string()).await;
    }

    let Some((account, must_change_password)) = matched else {
        warn!(username = %username, client = %client, "admin web login failed");
        crate::logs::record_audit(
            "web",
            username,
            "login_failed",
            None,
            None,
            "Web 管理頁登入失敗",
        );
        // PERFORMANCE_LOGIN_RESPONSE_V1:
        // Fire-and-forget the failure audit and security alert so the user
        // gets the 401 response immediately instead of waiting for SQLite
        // writes and potential Discord notification round-trips.
        {
            let state = state.clone();
            let user = username.to_string();
            let ip = ip.clone();
            tokio::spawn(async move {
                record_persistent_login_event(
                    &state,
                    &user,
                    None,
                    "login",
                    "failed",
                    "Web 管理頁登入失敗",
                    &ip,
                )
                .await;
                alert_password_failures(&state, &user, &ip).await;
            });
        }
        return unauthorized_response();
    };

    state.rate_limiter.clear(&rate_key).await;
    // LOGIN_GEO_ALERT_V1：先讀出成功前的失敗次數（疑似撞對密碼的訊號），再清零。
    let recent_failures = peek_recent_auth_failures(username, &ip).await;
    // 登入成功即重置失敗計數，確保只有「連續」失敗才會觸發告警。
    clear_auth_failures(username, &ip).await;

    // ARGON2_MIGRATION_V1
    // 平滑遷移：成功登入且既有雜湊仍是舊 PBKDF2 時，用這次的明文密碼
    // 重算成 Argon2id 並回寫，下次登入即走新的抗暴力破解雜湊。bootstrap
    // （首登改用上游密碼、強制改密碼）不在此升級，改密碼流程會寫 Argon2id。
    //
    // LOGIN_UPGRADE_BACKGROUND_V1：升級改為背景任務（不 await）——重算 Argon2id 又是一次
    // 慢雜湊，沒必要讓使用者多等；失敗只記 log，下次登入再試。明文只活在這個 task 內。
    if !must_change_password && crate::dashboard_auth::is_legacy_pbkdf2_hash(&account.dashboard_password_hash) {
        let state = state.clone();
        let upgrade_id = account.id.clone();
        let provider_password = account.password.clone();
        let plain = password.to_string();
        tokio::spawn(async move {
            match crate::dashboard_auth::hash_dashboard_password_async(plain, provider_password).await {
                Ok(new_hash) => {
                    match state
                        .accounts_db
                        .set_dashboard_password_hash(&upgrade_id, &new_hash)
                        .await
                    {
                        Ok(_) => info!(account = %upgrade_id, "Dashboard 密碼雜湊已由 PBKDF2 升級為 Argon2id"),
                        Err(error) => warn!(account = %upgrade_id, error = %error, "Argon2id 雜湊回寫失敗，下次登入再試"),
                    }
                }
                Err(error) => warn!(account = %upgrade_id, error = %error, "Argon2id 升級雜湊計算失敗"),
            }
        });
    }

    let account_id = account.id.clone();
    let is_admin = account.is_admin;
    // VIEWER_ROLE_V1：viewer 旗標寫進 session（管理員永遠不是 viewer）。
    // LOGIN_PERF_V1：`RawAccountConfig` 本來就帶 `is_viewer`（accounts 表同一列），
    // 不必再多查一次 SQLite。
    let is_viewer = !is_admin && account.is_viewer;
    let tokens = state
        .create_session_with(NewSession {
            account_id: account_id.clone(),
            is_admin,
            is_viewer,
            must_change_password,
            origin: SessionOrigin::from_request(&headers, &ip),
        })
        .await;
    let login_action = if must_change_password {
        "login_bootstrap"
    } else {
        "login"
    };
    let login_message = if must_change_password {
        "首次使用 Tronclass 密碼登入；必須立即設定新的 Dashboard 密碼"
    } else if is_admin {
        "Web 管理頁管理員登入成功"
    } else {
        "Web 管理頁登入成功"
    };
    info!(account = %account_id, is_admin = is_admin, bootstrap = must_change_password, client = %client, "admin web login success");
    crate::logs::record_audit(
        "web",
        &account_id,
        login_action,
        Some(&account_id),
        None,
        login_message,
    );
    // PERFORMANCE_LOGIN_RESPONSE_V1:
    // Fire-and-forget the persistent login event so the SQLite INSERT does
    // not delay the response. The session cookie is already set; the audit
    // row is best-effort and must not block the user.
    {
        let state = state.clone();
        let actor = account_id.clone();
        let acct = account_id.clone();
        let action = login_action.to_string();
        let msg = login_message.to_string();
        let ip = ip.clone();
        let user_agent = SessionOrigin::from_request(&headers, &ip).user_agent;
        // LOGIN_GEO_ALERT_V1：Cloudflare 國家標頭（不可信來源已被 sanitize middleware 移除）。
        let cf_ipcountry = cf_ipcountry_header(&headers);
        tokio::spawn(async move {
            // LOGIN_LOCATION_ALERT_V1：先比對「這個 IP 對這個帳號是否第一次出現」，
            // 再寫入登入事件與 IP 記憶（順序不能反，否則永遠都是「已知」）。
            check_unusual_login_location(
                &state,
                &acct,
                &ip,
                &user_agent,
                "密碼",
                cf_ipcountry.as_deref(),
                recent_failures,
            )
            .await;
            record_persistent_login_event(
                &state,
                &actor,
                Some(&acct),
                &action,
                "success",
                &msg,
                &ip,
            )
            .await;
        });
    }
    session_login_response(
        &headers,
        &tokens,
        account_id,
        is_admin,
        must_change_password,
    )
}

pub(super) async fn admin_logout_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    if let Some(session) = state.authorize(&query.token).await {
        if session.account_id != "<token-admin>" {
            state.revoke_session(&query.token).await;
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "logout",
                Some(&session.account_id),
                None,
                "Web 管理頁登出",
            );
        }
    }

    // LOGOUT_SERVER_SIDE_V1：API 登出也撤銷 refresh 列，與導覽式登出行為一致。
    if let Some(refresh) = refresh_cookie_from_headers(&headers) {
        let _ = state
            .accounts_db
            .delete_dashboard_session_by_refresh(&refresh)
            .await;
    }
    fb_sso::append_expired_fb_sso_cookie(append_expired_session_cookies(
        (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
    ))
}

/// LOGOUT_SERVER_SIDE_V1：瀏覽器導覽式登出。
///
/// `POST /admin/logout`（同源表單或 fetch 後跟隨導向）：伺服器撤銷 session、清掉
/// access / refresh / Filebrowser SSO 三組 cookie，然後 302 回 `/admin`。因為 cookie
/// 在 302 回應裡就被清掉，`/admin` 的三態判斷（見 `pages::admin_page_handler`）
/// 自然落到「無 session → 登入頁」，前端不再需要 `?logout=1`、
/// `tronclass_dashboard_manual_logout`、`tronclass_session_hint` 或 referrer 推測。
///
/// 用 POST 而非 GET：避免預抓取 / 連結掃描器一碰就把使用者登出（CSRF-logout）。
/// 同源檢查由 `admin_same_origin_middleware` 只覆蓋 `/admin/api/`，這裡自行檢查一次。
pub(super) async fn admin_logout_redirect_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    if !same_origin_request(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "cross-origin request rejected" })),
        )
            .into_response();
    }
    if let Some(session) = state.authorize_any(&query.token).await {
        if session.account_id != "<token-admin>" {
            state.revoke_session(&query.token).await;
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "logout",
                Some(&session.account_id),
                None,
                "Web 管理頁登出",
            );
        }
    }
    // 同時讓 refresh token 失效：否則清掉 cookie 後，DB 裡的 refresh 列仍可被重放。
    if let Some(refresh) = refresh_cookie_from_headers(&headers) {
        let _ = state
            .accounts_db
            .delete_dashboard_session_by_refresh(&refresh)
            .await;
    }
    let response = (StatusCode::FOUND, [(header::LOCATION, "/admin")]).into_response();
    fb_sso::append_expired_fb_sso_cookie(append_expired_session_cookies(response))
}

pub(super) async fn admin_session_refresh_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Query(query): Query<AdminQuery>,
) -> Response {
    // Prefer the HttpOnly refresh cookie; fall back to a token query param
    // (only for compatibility with clients that cannot send cookies).
    let refresh_token = refresh_cookie_from_headers(&headers)
        .filter(|t| !t.is_empty())
        .or_else(|| {
            let t = query.token.trim();
            if t.is_empty() { None } else { Some(t.to_string()) }
        });

    let Some(refresh_token) = refresh_token else {
        return unauthorized_response();
    };

    // LOGIN_DEVICES_V1：帶入這次請求的來源，讓功能上線前建立的 session
    // （ip / UA 為空）在輪換時補齊。
    let Some(tokens) = state
        .rotate_session(&refresh_token, Some(SessionOrigin::from_request(&headers, &ip)))
        .await
    else {
        // Invalid/expired refresh token: clear the cookies so the frontend
        // stops trying.
        return fb_sso::append_expired_fb_sso_cookie(append_expired_session_cookies(
            unauthorized_response(),
        ));
    };

    // Resolve account id / is_admin for the response.
    let mut account_id = String::new();
    let mut must_change = false;
    let mut is_admin = false;
    if let Ok(Some(row)) = state.accounts_db.get_dashboard_session(&tokens.access).await {
        account_id = row.account_id;
        is_admin = row.is_admin;
        must_change = row.must_change_password;
    }

    let response = append_session_cookie(
        (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "account_id": account_id,
                "is_admin": is_admin,
            })),
        )
            .into_response(),
        &tokens.access,
        &headers,
    );
    let response = append_refresh_cookie(response, &tokens.refresh, &headers);

    // Rotation invalidates the old access token the Filebrowser SSO ticket was
    // bound to, so re-sign the SSO cookie with the new access token. Bootstrap
    // sessions must not receive a Filebrowser ticket.
    if !must_change && !account_id.is_empty() && account_id != "<token-admin>" {
        fb_sso::append_fb_sso_cookie(response, &account_id, is_admin, &tokens.access, &headers)
    } else {
        response
    }
}

pub(super) fn password_json_pick(value: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(|v| v.as_str()) {
            return text.to_string();
        }
    }
    String::new()
}

pub(super) fn password_change_from_body(body: &[u8]) -> AdminPasswordChangeRequest {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).unwrap_or(serde_json::json!({}));
    let new_password =
        password_json_pick(&parsed, &["new_password", "password", "newPassword", "new"]);
    let current_password = password_json_pick(
        &parsed,
        &[
            "current_password",
            "old_password",
            "currentPassword",
            "oldPassword",
        ],
    );
    AdminPasswordChangeRequest {
        current_password,
        new_password,
    }
}

pub(super) async fn admin_password_change_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    body: axum::body::Bytes,
) -> Response {
    let request = password_change_from_body(&body);
    if let Err(response) = validate_admin_text(
        &request.current_password,
        ADMIN_MAX_PASSWORD_BYTES,
        "current password",
    ) {
        return response;
    }
    if let Err(response) = validate_admin_text(
        &request.new_password,
        ADMIN_MAX_PASSWORD_BYTES,
        "new password",
    ) {
        return response;
    }
    let Some(session) = state.authorize_any(&query.token).await else {
        return unauthorized_response();
    };
    if session.account_id == "<token-admin>" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "emergency token cannot change a Dashboard password" })),
        )
            .into_response();
    }

    let account = match state.accounts_db.get(&session.account_id).await {
        Ok(Some(account)) if account.enabled => account,
        Ok(_) => return unauthorized_response(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {error}") })),
            )
                .into_response();
        }
    };
    let first_setup =
        session.must_change_password || account.dashboard_password_hash.trim().is_empty();

    // The first-login session has already verified the upstream Tronclass
    // password. It may set a Dashboard password without asking the user to
    // submit that upstream password a second time. Every normal password change
    // still requires the current Dashboard password.
    if !first_setup
        && (!session.must_change_password
            && (account.dashboard_password_hash.is_empty()
                // LOGIN_VERIFY_OFFLOAD_V1：改密碼時驗目前密碼同樣走 blocking 執行緒池。
                || !crate::dashboard_auth::verify_dashboard_password_async(
                    request.current_password.clone(),
                    account.dashboard_password_hash.clone(),
                )
                .await))
    {
        crate::logs::record_audit(
            "web",
            &account.id,
            "dashboard_password_change_failed",
            Some(&account.id),
            None,
            "Dashboard 密碼變更驗證失敗",
        );
        return unauthorized_response();
    }

    let hash = match crate::dashboard_auth::hash_dashboard_password_async(
        request.new_password.clone(),
        account.password.clone(),
    )
    .await
    {
        Ok(hash) => hash,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };
    match state
        .accounts_db
        .set_dashboard_password_hash(&account.id, &hash)
        .await
    {
        Ok(true) => {}
        Ok(false) => return unauthorized_response(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("save password failed: {error}") })),
            )
                .into_response();
        }
    }

    // Revoke all prior sessions, including this one, then issue a fresh cookie.
    state.revoke_account_sessions(&account.id).await;
    // VIEWER_ROLE_V1 / LOGIN_DEVICES_V1：改密碼後的新 session 沿用 viewer 旗標與
    // 這次請求的來源（改密碼不是角色變更；裝置頁也要能看到這台裝置）。
    let tokens = state
        .create_session_with(NewSession {
            account_id: account.id.clone(),
            is_admin: account.is_admin,
            is_viewer: session.is_viewer,
            must_change_password: false,
            origin: SessionOrigin::from_request(&headers, &ip),
        })
        .await;
    crate::logs::record_audit(
        "web",
        &account.id,
        "dashboard_password_change",
        Some(&account.id),
        None,
        "Dashboard 密碼已變更，先前登入工作階段已失效",
    );
    session_login_response(
        &headers,
        &tokens,
        account.id,
        account.is_admin,
        false,
    )
}
