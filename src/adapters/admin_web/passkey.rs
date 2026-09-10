//! WebAuthn 通行密鑰（passkey）註冊與登入 handlers。

use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct AdminPasskeyLoginStartRequest {
    username: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminPasskeyRegisterFinishRequest {
    ceremony_id: String,
    response: RegistrationResponse,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminPasskeyLoginFinishRequest {
    ceremony_id: String,
    response: AuthenticationResponse,
}

pub(super) struct PasskeyRegistrationCeremony {
    account_id: String,
    state: RegistrationState,
    expires_at: Instant,
}

pub(super) struct PasskeyAuthenticationCeremony {
    account_id: String,
    state: AuthenticationState,
    expires_at: Instant,
}

static PASSKEY_REG_STATES: once_cell::sync::Lazy<
    Mutex<HashMap<String, PasskeyRegistrationCeremony>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
static PASSKEY_AUTH_STATES: once_cell::sync::Lazy<
    Mutex<HashMap<String, PasskeyAuthenticationCeremony>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
const MAX_PENDING_PASSKEY_STATES: usize = 128;
const MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT: usize = 32;

pub(super) fn passkey_user_handle(account_id: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"tronclass-rollcall-passkey-user-handle-v1");
    hasher.update(account_id.as_bytes());
    hasher.finalize().to_vec()
}

pub(super) fn passkey_webauthn_from_headers(headers: &HeaderMap) -> Result<Webauthn, Response> {
    for (name, max_bytes) in [
        (header::ORIGIN.as_str(), 512usize),
        (header::HOST.as_str(), 255usize),
        ("x-forwarded-proto", 16usize),
    ] {
        if headers
            .get(name)
            .map(|value| value.as_bytes().len() > max_bytes)
            .unwrap_or(false)
        {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("{name} header is too long") })),
            )
                .into_response());
        }
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            let host = headers.get(header::HOST)?.to_str().ok()?;
            let proto = headers
                .get("x-forwarded-proto")
                .and_then(|value| value.to_str().ok())
                .unwrap_or(
                    if host.starts_with("127.0.0.1") || host.starts_with("localhost") {
                        "http"
                    } else {
                        "https"
                    },
                );
            Some(format!("{proto}://{host}"))
        })
        .unwrap_or_else(|| "https://dashboard.jasoncloudserver.me".to_string());
    let parsed = match url::Url::parse(&origin) {
        Ok(parsed) => parsed,
        Err(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid Origin for passkey" })),
            )
                .into_response());
        }
    };
    let Some(rp_id) = parsed.host_str() else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing host for passkey rp_id" })),
        )
            .into_response());
    };
    // SEC_PASSKEY_REQUIRE_UV_V1（審查 M-08）：伺服器端強制 UV 旗標——沒有生物辨識／PIN
    // 的「按一下即過」roaming key 不再算通過登入；challenge 也會改為
    // `userVerification: "required"`。平台 passkey（Face ID／Touch ID／Windows Hello／
    // iCloud 鑰匙圈）本來就會做 UV，既有註冊不受影響。若真有無 PIN 的硬體金鑰需求，
    // 可用環境變數 `TRONCLASS_PASSKEY_REQUIRE_UV=0` 暫時關閉（不建議）。
    Ok(Webauthn::new(rp_id, "Tronclass Rollcall", &origin)
        .authenticator_attachment(Attachment::Any)
        .require_user_verification(passkey_require_uv()))
}

fn passkey_require_uv() -> bool {
    !matches!(
        std::env::var("TRONCLASS_PASSKEY_REQUIRE_UV")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off"
    )
}

pub(super) async fn cleanup_passkey_states() {
    let now = Instant::now();
    PASSKEY_REG_STATES
        .lock()
        .await
        .retain(|_, state| state.expires_at > now);
    PASSKEY_AUTH_STATES
        .lock()
        .await
        .retain(|_, state| state.expires_at > now);
}

pub(super) async fn admin_passkey_register_start_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if session.account_id == "<token-admin>" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "emergency token cannot register passkey; login as a real account first" })),
        )
            .into_response();
    }
    let wa = match passkey_webauthn_from_headers(&headers) {
        Ok(wa) => wa,
        Err(resp) => return resp,
    };

    let account = match state.accounts_db.get(&session.account_id).await {
        Ok(Some(account)) if account.enabled => account,
        Ok(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "account not found or disabled" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {e}") })),
            )
                .into_response();
        }
    };
    match state
        .accounts_db
        .count_passkey_credentials(&account.id)
        .await
    {
        Ok(count) if count >= MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT as i64 => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "too many passkeys for this account; max {}",
                        MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT
                    )
                })),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {e}") })),
            )
                .into_response();
        }
    }
    let existing_credentials = state
        .accounts_db
        .list_passkey_credentials(&account.id)
        .await
        .unwrap_or_default();
    let existing: Vec<CredentialId> = existing_credentials
        .iter()
        .map(|credential| credential.id.clone())
        .collect();
    let display_name = if account.id == account.username || account.username.trim().is_empty() {
        account.id.clone()
    } else {
        format!("{} ({})", account.id, account.username)
    };
    let (challenge, reg_state) = wa.start_registration(
        &passkey_user_handle(&account.id),
        &account.id,
        &display_name,
        &existing,
    );
    let ceremony_id = Uuid::new_v4().simple().to_string();
    cleanup_passkey_states().await;
    let mut pending = PASSKEY_REG_STATES.lock().await;
    if pending.len() >= MAX_PENDING_PASSKEY_STATES {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "too many pending passkey registrations; retry shortly" })),
        )
            .into_response();
    }
    pending.insert(
        ceremony_id.clone(),
        PasskeyRegistrationCeremony {
            account_id: account.id.clone(),
            state: reg_state,
            expires_at: Instant::now() + Duration::from_secs(300),
        },
    );
    drop(pending);
    (
        StatusCode::OK,
        Json(json!({ "ceremony_id": ceremony_id, "publicKey": challenge })),
    )
        .into_response()
}

pub(super) async fn admin_passkey_register_finish_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(req): Json<AdminPasskeyRegisterFinishRequest>,
) -> Response {
    if let Err(response) =
        validate_admin_text(&req.ceremony_id, ADMIN_MAX_IDENTIFIER_BYTES, "ceremony_id")
    {
        return response;
    }
    let wa = match passkey_webauthn_from_headers(&headers) {
        Ok(wa) => wa,
        Err(resp) => return resp,
    };
    cleanup_passkey_states().await;
    let ceremony = match PASSKEY_REG_STATES.lock().await.remove(&req.ceremony_id) {
        Some(ceremony) if ceremony.expires_at > Instant::now() => ceremony,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "passkey registration expired; please try again" })),
            )
                .into_response();
        }
    };
    let credential = match wa.finish_registration(&ceremony.state, &req.response) {
        Ok(credential) => credential,
        Err(e) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": format!("passkey registration verify failed: {e}") })),
            )
                .into_response();
        }
    };
    let credential_id = credential.id.to_b64url();
    match state
        .accounts_db
        .upsert_passkey_credential(&ceremony.account_id, &credential)
        .await
    {
        Ok(()) => {
            // ONBOARDING_PASSKEY_STATE_V1：status 有短快取，清掉讓 passkey_count 立即更新。
            super::status::invalidate_admin_status_cache().await;
            crate::logs::record_audit(
                "web",
                &ceremony.account_id,
                "passkey_register",
                Some(&ceremony.account_id),
                Some(&credential_id),
                "Web 管理頁新增通行密鑰",
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "credential_id": credential_id })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("save passkey failed: {e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn resolve_passkey_login_account(
    state: &AdminWebState,
    username: &str,
) -> Option<crate::account::RawAccountConfig> {
    let username = username.trim();
    if username.is_empty() {
        return None;
    }
    let mut candidates = Vec::new();
    if let Ok(Some(account)) = state.accounts_db.get(username).await {
        candidates.push(account);
    }
    if candidates.is_empty() {
        if let Ok(accounts) = state.accounts_db.find_by_username(username, None).await {
            candidates = accounts;
        }
    }
    let enabled = candidates
        .into_iter()
        .filter(|account| account.enabled)
        .collect::<Vec<_>>();
    match enabled.as_slice() {
        [account] => Some(account.clone()),
        _ => None,
    }
}

pub(super) fn passkey_login_decoy_response(wa: &Webauthn) -> Response {
    // Usernameless/discoverable flow: never put a per-user credential ID in
    // the response. Unknown, disabled and passkey-less accounts all receive
    // the same empty allowCredentials challenge; the decoy state is not
    // persisted, so a follow-up finish request can only fail authentication.
    let (challenge, _) = wa.start_authentication(&[]);
    (
        StatusCode::OK,
        Json(json!({
            "ceremony_id": Uuid::new_v4().simple().to_string(),
            "publicKey": challenge,
        })),
    )
        .into_response()
}

pub(super) async fn admin_passkey_login_start_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Json(req): Json<AdminPasskeyLoginStartRequest>,
) -> Response {
    if ip_is_blacklisted(&state, &ip).await {
        warn!(ip = %ip, "passkey login rejected for blacklisted IP");
        return blocked_ip_response();
    }
    if !state
        .rate_limiter
        .check_limit(&format!("passkey-ip:{ip}"), IP_RATE_LIMIT_MAX_ATTEMPTS)
        .await
    {
        warn!(ip = %ip, "passkey login rate limited by ip");
        alert_abnormal_traffic(&state, "通行密鑰登入", &req.username, &ip).await;
        return rate_limited_response();
    }
    if let Err(response) = validate_admin_text(&req.username, ADMIN_MAX_USERNAME_BYTES, "username")
    {
        return response;
    }
    let username_key = req.username.trim().to_lowercase();
    if !username_key.is_empty()
        && !state
            .rate_limiter
            .check_limit(&format!("passkey-user:{username_key}"), RATE_LIMIT_MAX_ATTEMPTS)
            .await
    {
        warn!(ip = %ip, "passkey login rate limited by username");
        alert_abnormal_traffic(&state, "通行密鑰帳號探測", &req.username, &ip).await;
        return rate_limited_response();
    }
    let wa = match passkey_webauthn_from_headers(&headers) {
        Ok(wa) => wa,
        Err(resp) => return resp,
    };
    // PASSKEY_USERNAMELESS_V1：帳號留空 → 免輸帳號流程；finish 階段改以
    // assertion 的 credential id 反查擁有者。挑戰內容與有帳號流程完全相同。
    let usernameless = req.username.trim().is_empty();
    let account = if usernameless {
        None
    } else {
        match resolve_passkey_login_account(&state, &req.username).await {
            Some(account) => Some(account),
            None => return passkey_login_decoy_response(&wa),
        }
    };
    if let Some(account) = &account {
        let credentials = match state
            .accounts_db
            .list_passkey_credentials(&account.id)
            .await
        {
            Ok(credentials) => credentials,
            Err(_) => return passkey_login_decoy_response(&wa),
        };
        if credentials.len() > MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT || credentials.is_empty() {
            return passkey_login_decoy_response(&wa);
        }
    }
    // Keep allowCredentials empty even after finding the account. This makes
    // the public challenge shape identical for existing and unknown accounts;
    // the browser uses a discoverable passkey, and the finish step is scoped
    // to the named account — or, in the usernameless flow, resolved there
    // from the asserted credential id.
    let (challenge, auth_state) = wa.start_authentication(&[]);
    let ceremony_id = Uuid::new_v4().simple().to_string();
    cleanup_passkey_states().await;
    let mut pending = PASSKEY_AUTH_STATES.lock().await;
    if pending.len() >= MAX_PENDING_PASSKEY_STATES {
        return passkey_login_decoy_response(&wa);
    }
    pending.insert(
        ceremony_id.clone(),
        PasskeyAuthenticationCeremony {
            // 空字串＝免輸帳號儀式：finish 階段以 credential id 反查。
            account_id: account.map(|account| account.id).unwrap_or_default(),
            state: auth_state,
            expires_at: Instant::now() + Duration::from_secs(300),
        },
    );
    drop(pending);
    (
        StatusCode::OK,
        Json(json!({ "ceremony_id": ceremony_id, "publicKey": challenge })),
    )
        .into_response()
}

pub(super) async fn admin_passkey_login_finish_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Json(req): Json<AdminPasskeyLoginFinishRequest>,
) -> Response {
    if ip_is_blacklisted(&state, &ip).await {
        warn!(ip = %ip, "passkey finish rejected for blacklisted IP");
        return blocked_ip_response();
    }
    if !state
        .rate_limiter
        .check_limit(&format!("passkey-finish-ip:{ip}"), IP_RATE_LIMIT_MAX_ATTEMPTS)
        .await
    {
        warn!(ip = %ip, "passkey finish rate limited by ip");
        alert_abnormal_traffic(&state, "通行密鑰驗證", "", &ip).await;
        return rate_limited_response();
    }
    if let Err(response) =
        validate_admin_text(&req.ceremony_id, ADMIN_MAX_IDENTIFIER_BYTES, "ceremony_id")
    {
        return response;
    }
    if let Err(response) = validate_admin_text(&req.response.id, 4096, "credential id") {
        return response;
    }
    let wa = match passkey_webauthn_from_headers(&headers) {
        Ok(wa) => wa,
        Err(resp) => return resp,
    };
    cleanup_passkey_states().await;
    let ceremony = match PASSKEY_AUTH_STATES.lock().await.remove(&req.ceremony_id) {
        Some(ceremony) if ceremony.expires_at > Instant::now() => ceremony,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "passkey authentication expired; please try again" })),
            )
                .into_response();
        }
    };
    let asserted_id = match CredentialId::from_b64url(&req.response.id) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "bad passkey credential id" })),
            )
                .into_response();
        }
    };
    // PASSKEY_USERNAMELESS_V1：儀式未綁帳號（免輸帳號流程）時，以 assertion
    // 的 credential id 反查擁有者；查無 → 與帳密錯誤同一個 401，不洩漏密鑰
    // 是否存在。有綁帳號 → 維持原本的帳號範圍限制。
    let (account_id, mut stored) = if ceremony.account_id.is_empty() {
        match state
            .accounts_db
            .find_passkey_credential_owner(&asserted_id.to_b64url())
            .await
        {
            Ok(Some((account_id, stored))) => {
                match state
                    .accounts_db
                    .count_passkey_credentials(&account_id)
                    .await
                {
                    Ok(count) if count <= MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT as i64 => {}
                    Ok(_) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": "this account has too many stored passkeys; remove legacy entries before login" })),
                        )
                            .into_response();
                    }
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": format!("database error: {e}") })),
                        )
                            .into_response();
                    }
                }
                (account_id, stored)
            }
            Ok(None) => {
                warn!(ip = %ip, "usernameless passkey login with unknown credential");
                return unauthorized_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("database error: {e}") })),
                )
                    .into_response();
            }
        }
    } else {
        let credentials = match state
            .accounts_db
            .list_passkey_credentials(&ceremony.account_id)
            .await
        {
            Ok(credentials) => credentials,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("database error: {e}") })),
                )
                    .into_response();
            }
        };
        if credentials.len() > MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "this account has too many stored passkeys; remove legacy entries before login" })),
            )
                .into_response();
        }
        match credentials
            .into_iter()
            .find(|credential| credential.id == asserted_id)
        {
            Some(stored) => (ceremony.account_id.clone(), stored),
            None => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "unknown passkey credential" })),
                )
                    .into_response();
            }
        }
    };
    if !state
        .rate_limiter
        .check_limit(
            &format!("passkey-finish-account:{account_id}"),
            RATE_LIMIT_MAX_ATTEMPTS,
        )
        .await
    {
        warn!(account = %account_id, ip = %ip, "passkey finish rate limited by account");
        alert_abnormal_traffic(&state, "通行密鑰驗證", &account_id, &ip).await;
        return rate_limited_response();
    }
    let outcome = match wa.finish_authentication(&ceremony.state, &req.response, &mut stored) {
        Ok(outcome) => outcome,
        Err(e) => {
            record_persistent_login_event(
                &state,
                &account_id,
                Some(&account_id),
                "login_failed",
                "fail",
                "Web 管理頁通行密鑰驗證失敗",
                &ip,
            )
            .await;
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": format!("passkey verify failed: {e}") })),
            )
                .into_response();
        }
    };
    stored.counter = outcome.new_counter;
    let _ = state
        .accounts_db
        .upsert_passkey_credential(&account_id, &stored)
        .await;

    let account = match state.accounts_db.get(&account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "account not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {e}") })),
            )
                .into_response();
        }
    };
    // Password login refuses disabled accounts; passkey login must too,
    // otherwise disabling an account leaves any registered passkey as a
    // working back door.
    if !account.enabled {
        record_persistent_login_event(
            &state,
            &account.id,
            Some(&account.id),
            "login_failed",
            "fail",
            "帳號已停用，通行密鑰登入遭拒",
            &ip,
        )
        .await;
        return unauthorized_response();
    }

    let account_id = account.id.clone();
    let is_admin = account.is_admin;
    // VIEWER_ROLE_V1（LOGIN_PERF_V1：直接用同一列的 `is_viewer`，省一次查詢）
    let is_viewer = !is_admin && account.is_viewer;
    let origin = SessionOrigin::from_request(&headers, &ip);
    let tokens = state
        .create_session_with(NewSession {
            account_id: account_id.clone(),
            is_admin,
            is_viewer,
            must_change_password: false,
            origin: origin.clone(),
        })
        .await;
    crate::logs::record_audit(
        "web",
        &account_id,
        "passkey_login",
        Some(&account_id),
        None,
        "Web 管理頁通行密鑰登入成功",
    );
    // LOGIN_LOCATION_ALERT_V1 / LOGIN_DEVICES_V1：通行密鑰登入過去沒有寫入持久登入
    // 事件，登入審計頁與「已知 IP」比對都看不到它；補上（與密碼登入同格式，
    // action=passkey_login），並做異常地點判斷。
    {
        let state = state.clone();
        let acct = account_id.clone();
        let ip = ip.clone();
        let user_agent = origin.user_agent.clone();
        // LOGIN_GEO_ALERT_V1：通行密鑰不會被「猜」，失敗次數訊號不適用（傳 0）；
        // 仍看國家：境外／Tor 的通行密鑰登入依舊通知。
        let cf_ipcountry = cf_ipcountry_header(&headers);
        tokio::spawn(async move {
            check_unusual_login_location(
                &state,
                &acct,
                &ip,
                &user_agent,
                "通行密鑰",
                cf_ipcountry.as_deref(),
                0,
            )
            .await;
            record_persistent_login_event(
                &state,
                &acct,
                Some(&acct),
                "passkey_login",
                "success",
                "Web 管理頁通行密鑰登入成功",
                &ip,
            )
            .await;
        });
    }
    session_login_response(&headers, &tokens, account_id, is_admin, false)
}
