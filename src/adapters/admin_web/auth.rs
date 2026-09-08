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

    let mut candidates = Vec::new();
    // Try exact account-id match first, then fall back to username search.
    if let Ok(Some(account)) = state.accounts_db.get(username).await {
        candidates.push(account);
    }
    if candidates.is_empty() {
        if let Ok(accounts) = state.accounts_db.find_by_username(username, None).await {
            candidates = accounts;
        }
    }

    // Normal logins use the separate Dashboard verifier. If no Dashboard
    // password exists yet, allow the stored Tronclass password once and create
    // a restricted bootstrap session that can only set a new Dashboard password.
    let mut matched = None;
    for account in candidates {
        if !account.enabled {
            continue;
        }
        if !account.dashboard_password_hash.is_empty()
            && crate::dashboard_auth::verify_dashboard_password(
                password,
                &account.dashboard_password_hash,
            )
        {
            matched = Some((account, false));
            break;
        }
        if account.dashboard_password_hash.is_empty()
            && crate::dashboard_auth::verify_first_login_password(password, &account.password)
        {
            matched = Some((account, true));
            break;
        }
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
    // 登入成功即重置失敗計數，確保只有「連續」失敗才會觸發告警。
    clear_auth_failures(username, &ip).await;

    // ARGON2_MIGRATION_V1
    // 平滑遷移：成功登入且既有雜湊仍是舊 PBKDF2 時，用這次的明文密碼
    // 重算成 Argon2id 並回寫，下次登入即走新的抗暴力破解雜湊。bootstrap
    // （首登改用上游密碼、強制改密碼）不在此升級，改密碼流程會寫 Argon2id。
    if !must_change_password && crate::dashboard_auth::is_legacy_pbkdf2_hash(&account.dashboard_password_hash) {
        let account_for_upgrade = account.clone();
        if let Ok(new_hash) =
            crate::dashboard_auth::hash_dashboard_password(password, &account_for_upgrade.password)
        {
            let _ = state
                .accounts_db
                .set_dashboard_password_hash(&account_for_upgrade.id, &new_hash)
                .await;
            info!(account = %account_for_upgrade.id, "Dashboard 密碼雜湊已由 PBKDF2 升級為 Argon2id");
        }
    }

    let account_id = account.id.clone();
    let is_admin = account.is_admin;
    // VIEWER_ROLE_V1：登入時查一次 viewer 旗標寫進 session（管理員永遠不是 viewer）。
    let is_viewer = !is_admin
        && state
            .accounts_db
            .is_viewer(&account_id)
            .await
            .unwrap_or(false);
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
        tokio::spawn(async move {
            // LOGIN_LOCATION_ALERT_V1：先比對「這個 IP 對這個帳號是否第一次出現」，
            // 再寫入登入事件與 IP 記憶（順序不能反，否則永遠都是「已知」）。
            check_unusual_login_location(&state, &acct, &ip, &user_agent, "密碼").await;
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

    fb_sso::append_expired_fb_sso_cookie(append_expired_session_cookies(
        (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
    ))
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
                || !crate::dashboard_auth::verify_dashboard_password(
                    request.current_password.as_str(),
                    &account.dashboard_password_hash,
                )))
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

    let hash = match crate::dashboard_auth::hash_dashboard_password(
        &request.new_password,
        &account.password,
    ) {
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
