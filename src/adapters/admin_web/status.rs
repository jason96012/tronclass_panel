//! Dashboard 狀態（/admin/api/status 等）與點名 SSE 事件 handlers。

use super::*;

#[derive(Debug, Serialize)]
pub(super) struct AdminPublicStatusResponse {
    total_accounts: usize,
    running_accounts: usize,
    detected_accounts: usize,
}

#[derive(Debug, Serialize)]
pub(super) struct AdminStatusResponse {
    current_account_id: String,
    is_admin: bool,
    /// A bootstrap session has authenticated once with the upstream password
    /// and may only change its Dashboard password.
    must_change_password: bool,
    default_poll_interval_secs: u64,
    settings: RuntimeSettings,
    /// MORNING_DIGEST_V1：晨間摘要設定（設定頁顯示用）。
    digest: super::morning_digest::DigestStatusInfo,
    accounts: Vec<AdminAccountStatus>,
    /// ACCOUNT_HOT_ADD_FEEDBACK_V1：已在 accounts.db 且 enabled，但目前沒有監控循環的帳號
    /// （熱啟動登入失敗／CLI 離線新增）。只有管理員看得到；一般使用者永遠是空陣列。
    inactive_accounts: Vec<InactiveAccountStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct InactiveAccountStatus {
    account_id: String,
    username: String,
    provider: String,
    /// 最近一次熱啟動失敗原因；None 代表從未嘗試過（例如 CLI 新增後尚未重啟）。
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct AdminAccountStatus {
    account_id: String,
    username: String,
    line_user_id: String,
    discord_user_id: String,
    account_admin: bool,
    is_running: bool,
    user_name: String,
    last_poll_timestamp: Option<i64>,
    last_success_course: Option<String>,
    last_success_rollcall_id: Option<u64>,
    last_detected_timestamp: Option<i64>,
    last_detected_course: Option<String>,
    last_detected_rollcall_id: Option<u64>,
    last_detected_url: Option<String>,
    rollcall_history: Vec<RollcallHistoryEntry>,
    consecutive_failures: u32,
    started_at: i64,
}

// Cache admin flags for /admin/api/status to avoid querying accounts.db on every dashboard refresh.
static ADMIN_STATUS_ADMIN_FLAG_CACHE: once_cell::sync::Lazy<
    tokio::sync::Mutex<Option<(std::time::Instant, std::collections::HashMap<String, bool>)>>,
> = once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(None));

const ADMIN_STATUS_SHORT_CACHE_TTL: Duration = Duration::from_secs(2);
const ADMIN_STATUS_HISTORY_LIMIT: usize = 30;

// Cache status responses briefly to absorb burst dashboard refreshes.
// The key includes the session role/account because the payload contains
// `current_account_id` and regular users only see their own account.
static ADMIN_STATUS_SHORT_CACHE: once_cell::sync::Lazy<
    tokio::sync::Mutex<HashMap<String, (Instant, serde_json::Value)>>,
> = once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Clear status caches after an account permission change so the Dashboard
/// immediately renders the new administrator badge/button state.
pub(super) async fn invalidate_admin_status_cache() {
    *ADMIN_STATUS_ADMIN_FLAG_CACHE.lock().await = None;
    ADMIN_STATUS_SHORT_CACHE.lock().await.clear();
}

pub(super) fn admin_status_short_cache_key(
    session: &AdminSession,
    include_history: bool,
    history_limit: usize,
) -> String {
    let role = if session.is_admin { "admin" } else { "user" };
    format!(
        "{role}:{}:must_change={}:history={}:limit={}",
        session.account_id, session.must_change_password, include_history, history_limit
    )
}

pub(super) async fn admin_status_short_cache_get(key: &str) -> Option<serde_json::Value> {
    let mut cache = ADMIN_STATUS_SHORT_CACHE.lock().await;
    let now = Instant::now();
    if cache.len() > MAX_SESSIONS {
        cache.retain(|_, (updated_at, _)| {
            now.saturating_duration_since(*updated_at) <= ADMIN_STATUS_SHORT_CACHE_TTL
        });
    }
    cache.get(key).and_then(|(updated_at, value)| {
        (now.saturating_duration_since(*updated_at) <= ADMIN_STATUS_SHORT_CACHE_TTL)
            .then(|| value.clone())
    })
}

pub(super) async fn admin_status_short_cache_put(key: String, value: serde_json::Value) {
    let mut cache = ADMIN_STATUS_SHORT_CACHE.lock().await;
    cache.insert(key, (Instant::now(), value));
    if cache.len() > MAX_SESSIONS {
        let now = Instant::now();
        cache.retain(|_, (updated_at, _)| {
            now.saturating_duration_since(*updated_at) <= ADMIN_STATUS_SHORT_CACHE_TTL
        });
    }
}

pub(super) async fn admin_status_admin_flags_cached(
    state: &AdminWebState,
) -> std::result::Result<std::collections::HashMap<String, bool>, Response> {
    {
        let cache = ADMIN_STATUS_ADMIN_FLAG_CACHE.lock().await;
        if let Some((updated_at, flags)) = cache.as_ref() {
            if updated_at.elapsed() <= std::time::Duration::from_secs(30) {
                return Ok(flags.clone());
            }
        }
    }

    let raw_accounts = match state.accounts_db.list().await {
        Ok(accounts) => accounts,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("database error: {e}") })),
            )
                .into_response());
        }
    };
    let flags = raw_accounts
        .into_iter()
        .map(|account| (account.id, account.is_admin))
        .collect::<std::collections::HashMap<_, _>>();

    *ADMIN_STATUS_ADMIN_FLAG_CACHE.lock().await = Some((std::time::Instant::now(), flags.clone()));
    Ok(flags)
}

/// Adds non-sensitive timing information to `/admin/api/status` responses.
///
/// These metrics are intentionally exposed through the standard `Server-Timing`
/// response header so they can be inspected in the browser Network panel without
/// logging a session token, account name, or any upstream credential.
pub(super) fn with_admin_status_timing(
    mut response: Response,
    total: Duration,
    phases: &[(&str, Duration)],
) -> Response {
    let mut value = format!("app;dur={:.1}", total.as_secs_f64() * 1000.0);
    for (name, duration) in phases {
        value.push_str(&format!("; {name};dur={:.1}", duration.as_secs_f64() * 1000.0));
    }
    if let Ok(value) = HeaderValue::try_from(value.as_str()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("server-timing"), value);
    }
    response
}

/// Server-Sent Events stream for immediate Dashboard rollcall alerts.
///
/// The stream reads only the accounts visible to this session. It emits a
/// rollcall event as soon as the monitor exposes a direct input URL, avoiding
/// the dashboard's normal refresh interval for time-sensitive attendance.
pub(super) async fn admin_rollcall_events_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if session.must_change_password {
        return unauthorized_response();
    }

    // ACCOUNT_HOT_ADD_V1：持有登錄簿而非固定快照，熱新增的帳號也會進入 SSE。
    let accounts = state.accounts.clone();
    let account_id = session.account_id;
    let is_admin = session.is_admin;
    let stream = futures::stream::unfold(
        (accounts, account_id, is_admin, HashMap::<String, String>::new()),
        |(accounts, account_id, is_admin, mut last_seen)| async move {
            tokio::time::sleep(Duration::from_millis(350)).await;
            let mut detected = Vec::new();
            let runtime_accounts = accounts.snapshot();
            for account in runtime_accounts.iter() {
                if !is_admin && account.account_id != account_id {
                    continue;
                }
                let status = account.monitor_status.lock().await;
                let (timestamp, course, rollcall_id, url) = (
                    status.last_detected_timestamp,
                    status.last_detected_course.clone(),
                    status.last_detected_rollcall_id,
                    status.last_detected_url.clone(),
                );
                drop(status);
                let Some(timestamp) = timestamp else { continue };
                let Some(url) = url.filter(|value| !value.trim().is_empty()) else { continue };
                let event_key = format!("{}:{}:{}", account.account_id, rollcall_id.unwrap_or(0), timestamp);
                if last_seen.get(&account.account_id) == Some(&event_key) {
                    continue;
                }
                last_seen.insert(account.account_id.clone(), event_key.clone());
                detected.push(json!({
                    "key": event_key,
                    "account": account.account_id,
                    "course": course.unwrap_or_else(|| "新點名".to_string()),
                    "rollcall_id": rollcall_id,
                    "timestamp": timestamp,
                    "url": url,
                }));
            }
            let event = if detected.is_empty() {
                SseEvent::default().event("ping").data("{}")
            } else {
                SseEvent::default()
                    .event("rollcall")
                    .data(serde_json::to_string(&detected).unwrap_or_else(|_| "[]".to_string()))
            };
            Some((Ok::<_, std::convert::Infallible>(event), (accounts, account_id, is_admin, last_seen)))
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("keepalive"))
        .into_response()
}

pub(super) async fn admin_status_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // Temporary production profiling for first-paint diagnosis. Keep the
    // measurements around lock acquisition and SQLite, rather than around
    // individual accounts, to avoid leaking account-specific timing.
    let request_started = Instant::now();
    // PERFORMANCE_STATUS_AUTH_V1:
    // The session_cookie_to_query_middleware already injected the HttpOnly
    // cookie into query.token, so we can skip redundant header parsing.
    // Fall back to direct cookie read only if middleware was bypassed.
    let token = if query.token.trim().is_empty() {
        session_cookie_from_headers(&headers)
            .unwrap_or_default()
    } else {
        query.token.trim().to_string()
    };
    let Some(session) = state.authorize_any(&token).await else {
        // Session expired or missing — this is a normal event (e.g., after
        // server restart or cookie expiry), not a warning. Log at debug level
        // to avoid flooding the console during routine re-authentication.
        tracing::debug!(
            parsed_token_len = token.len(),
            uses_https = request_uses_https(&headers),
            "STATUS 401: no valid session"
        );
        return append_expired_session_cookies(unauthorized_response());
    };
    let auth_elapsed = request_started.elapsed();

    let include_history = query.include_history.unwrap_or(true);
    let history_limit = if include_history {
        query
            .history_limit
            .unwrap_or(ADMIN_STATUS_HISTORY_LIMIT)
            .min(ADMIN_STATUS_HISTORY_LIMIT)
    } else {
        0
    };

    let cache_key = admin_status_short_cache_key(&session, include_history, history_limit);
    if let Some(cached) = admin_status_short_cache_get(&cache_key).await {
        let total = request_started.elapsed();
        info!(
            total_ms = total.as_millis(),
            auth_ms = auth_elapsed.as_millis(),
            include_history,
            history_limit,
            "admin status profile: cache hit"
        );
        return with_admin_status_timing(
            (StatusCode::OK, Json(cached)).into_response(),
            total,
            &[("auth", auth_elapsed), ("cache", total.saturating_sub(auth_elapsed))],
        );
    }

    // A first-login session may use this endpoint only to learn that it must
    // change its Dashboard password. Do not expose account status, schedules,
    // or management data until the password is changed.
    if session.must_change_password {
        let response_json = json!(AdminStatusResponse {
            current_account_id: session.account_id.clone(),
            is_admin: session.is_admin,
            must_change_password: true,
            default_poll_interval_secs: state.default_poll_interval_secs,
            settings: crate::runtime_settings::snapshot().await,
            digest: super::morning_digest::digest_status(),
            accounts: vec![],
            // ACCOUNT_HOT_ADD_FEEDBACK_V1：首次登入尚未改密碼前不暴露任何帳號資料。
            inactive_accounts: vec![],
        });
        admin_status_short_cache_put(cache_key, response_json.clone()).await;
        return (StatusCode::OK, Json(response_json)).into_response();
    }

    let admin_flags_started = Instant::now();
    let admin_by_id = if session.is_admin {
        match admin_status_admin_flags_cached(&state).await {
            Ok(flags) => flags,
            Err(resp) => return resp,
        }
    } else {
        std::collections::HashMap::new()
    };
    let admin_flags_elapsed = admin_flags_started.elapsed();

    let visible_accounts = resolve_visible_accounts(&state, &session);
    // PERFORMANCE_STATUS_PAYLOAD_V1:
    // Build the response directly while each account status is locked instead
    // of cloning the full MonitorStatus.  The dashboard only renders the first
    // 30 history entries, so cap history in the hot /admin/api/status payload.
    let status_futures: Vec<_> = visible_accounts
        .iter()
        .map(|account| {
            let account_admin = admin_by_id
                .get(&account.account_id)
                .copied()
                .unwrap_or(false);
            async move {
                let status = account.monitor_status.lock().await;
                let rollcall_history = status
                    .rollcall_history
                    .iter()
                    .take(history_limit)
                    .cloned()
                    .collect::<Vec<_>>();
                let user_name = status.user_name.clone();
                let last_success_course = status.last_success_course.clone();
                let last_detected_course = status.last_detected_course.clone();
                let last_detected_url = status.last_detected_url.clone();
                let last_poll_timestamp = status.last_poll_timestamp;
                let last_success_rollcall_id = status.last_success_rollcall_id;
                let last_detected_timestamp = status.last_detected_timestamp;
                let last_detected_rollcall_id = status.last_detected_rollcall_id;
                let consecutive_failures = status.consecutive_failures;
                let started_at = status.started_at;
                drop(status);

                // PERFORMANCE_STATUS_TARGET_V1:
                // Extract only the fields we need from the target lock instead
                // of cloning the entire AdapterAccountTarget struct.
                let (line_user_id, discord_user_id) = {
                    let target = account.target.lock().await;
                    (target.line_user_id.clone(), target.discord_user_id.clone())
                };
                let running = *account.is_running.lock().await;

                AdminAccountStatus {
                    account_id: account.account_id.clone(),
                    username: account.username.clone(),
                    line_user_id,
                    discord_user_id,
                    account_admin,
                    is_running: running,
                    user_name,
                    last_poll_timestamp,
                    last_success_course,
                    last_success_rollcall_id,
                    last_detected_timestamp,
                    last_detected_course,
                    last_detected_rollcall_id,
                    last_detected_url,
                    rollcall_history,
                    consecutive_failures,
                    started_at,
                }
            }
        })
        .collect();
    let account_locks_started = Instant::now();
    let mut accounts = futures::future::join_all(status_futures).await;
    let account_locks_elapsed = account_locks_started.elapsed();
    accounts.sort_by(|a, b| a.account_id.cmp(&b.account_id));

    let settings_started = Instant::now();
    let settings = crate::runtime_settings::snapshot().await;
    let settings_elapsed = settings_started.elapsed();
    let account_count = accounts.len();
    // ACCOUNT_HOT_ADD_FEEDBACK_V1：管理員額外列出「在 DB 但沒在監控」的帳號與原因。
    let inactive_accounts = if session.is_admin {
        collect_inactive_accounts(&state).await
    } else {
        Vec::new()
    };
    let response_json = json!(AdminStatusResponse {
        current_account_id: session.account_id.clone(),
        is_admin: session.is_admin,
        must_change_password: false,
        default_poll_interval_secs: state.default_poll_interval_secs,
        settings,
        digest: super::morning_digest::digest_status(),
        accounts,
        inactive_accounts,
    });

    admin_status_short_cache_put(cache_key, response_json.clone()).await;

    let total = request_started.elapsed();
    info!(
        total_ms = total.as_millis(),
        auth_ms = auth_elapsed.as_millis(),
        admin_flags_ms = admin_flags_elapsed.as_millis(),
        account_locks_ms = account_locks_elapsed.as_millis(),
        settings_ms = settings_elapsed.as_millis(),
        account_count,
        include_history,
        history_limit,
        "admin status profile: cache miss"
    );
    with_admin_status_timing(
        (StatusCode::OK, Json(response_json)).into_response(),
        total,
        &[
            ("auth", auth_elapsed),
            ("admin_flags", admin_flags_elapsed),
            ("account_locks", account_locks_elapsed),
            ("settings", settings_elapsed),
        ],
    )
}

pub(super) async fn admin_public_status_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // PERFORMANCE_STATUS_AUTH_V1: prefer middleware-injected query.token
    let token = if query.token.trim().is_empty() {
        session_cookie_from_headers(&headers)
            .unwrap_or_default()
    } else {
        query.token.trim().to_string()
    };
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    let visible = resolve_visible_accounts(&state, &session);
    let total_accounts = visible.len();
    let statuses: Vec<_> = visible
        .into_iter()
        .map(|account| async move {
            let running = *account.is_running.lock().await;
            let detected = account
                .monitor_status
                .lock()
                .await
                .last_detected_timestamp
                .is_some();
            (running, detected)
        })
        .collect();
    let all = futures::future::join_all(statuses).await;
    let running_accounts = all.iter().filter(|(r, _)| *r).count();
    let detected_accounts = all.iter().filter(|(_, d)| *d).count();

    (
        StatusCode::OK,
        Json(json!(AdminPublicStatusResponse {
            total_accounts,
            running_accounts,
            detected_accounts,
        })),
    )
        .into_response()
}

// ACCOUNT_HOT_ADD_FEEDBACK_V1
async fn collect_inactive_accounts(state: &AdminWebState) -> Vec<InactiveAccountStatus> {
    let Some(spawner) = state.monitor_spawner.as_ref() else {
        return Vec::new();
    };
    let Ok(rows) = state.accounts_db.list().await else {
        return Vec::new();
    };
    let mut inactive = rows
        .into_iter()
        .filter(|row| row.enabled && !spawner.is_active(&row.id))
        .map(|row| InactiveAccountStatus {
            last_error: spawner.last_error(&row.id),
            account_id: row.id,
            username: row.username,
            provider: row.provider,
        })
        .collect::<Vec<_>>();
    inactive.sort_by(|a, b| a.account_id.cmp(&b.account_id));
    inactive
}

pub(super) fn resolve_visible_accounts(
    state: &AdminWebState,
    session: &AdminSession,
) -> Vec<RequestAccountState> {
    if session.is_admin {
        state.accounts.all()
    } else {
        state
            .account_by_id(&session.account_id)
            .into_iter()
            .collect()
    }
}
