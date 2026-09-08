//! Mock rollcall page/API handlers for Admin Web.

use super::pages::inject_theme;
use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct AdminMockStartRequest {
    kind: String,
    selected_accounts: Vec<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    interval_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(super) struct AdminMockStartResponse {
    kind: String,
    provider: String,
    rollcall_id: u64,
    account_id: String,
    token: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    qr_base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interval_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminMockQrQuery {
    #[serde(default)]
    token: String,
    data: String,
}

const MAX_MOCK_SELECTIONS: usize = 128;

fn normalize_mock_selections(values: Vec<String>) -> Result<Vec<String>, Response> {
    if values.len() > MAX_MOCK_SELECTIONS {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("too many selected accounts; max {MAX_MOCK_SELECTIONS}")
            })),
        )
            .into_response());
    }

    let mut seen = std::collections::HashSet::with_capacity(values.len());
    let mut selected = Vec::with_capacity(values.len());
    for value in values {
        validate_admin_text(&value, ADMIN_MAX_IDENTIFIER_BYTES, "selected account")?;
        let value = value.trim();
        if value.is_empty() || !seen.insert(value.to_string()) {
            continue;
        }
        selected.push(value.to_string());
    }
    Ok(selected)
}

pub(super) async fn admin_mock_page_handler(headers: HeaderMap) -> Response {
    // 模擬點名已整合到 /admin 的彈出視窗，/admin/mock 也回同一個管理頁。
    super::html_page_response(&headers, inject_theme(super::pages::ADMIN_DASHBOARD_HTML))
}

pub(super) async fn admin_mock_start_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    body: axum::body::Bytes,
) -> Response {
    // 授權檢查優先，避免未認證請求觸發 body 解析洩漏框架指紋（F-01）
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    let req: AdminMockStartRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_request" })),
            )
                .into_response();
        }
    };
    if let Err(response) = validate_admin_text(&req.kind, ADMIN_MAX_ACTION_BYTES, "kind") {
        return response;
    }
    if let Err(response) = validate_optional_admin_text(req.code.as_deref(), 1024, "code") {
        return response;
    }

    let Some(scanner) = state.scanner.as_ref().map(Arc::clone) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "scanner/public_base_url is not enabled" })),
        )
            .into_response();
    };

    // Enforce cardinality and deduplicate before account resolution, scanner
    // registration, outbound messages, or task creation.
    let selected = match normalize_mock_selections(req.selected_accounts) {
        Ok(selected) => selected,
        Err(response) => return response,
    };
    if selected.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "請至少選擇一個測試帳號" })),
        )
            .into_response();
    }

    // Administrators may exercise a batch of accounts. A regular Dashboard
    // user may use Mock 點名 too, but only for the account tied to their own
    // session; never trust the checkbox values supplied by the browser.
    if !session.is_admin
        && (selected.len() != 1 || selected.first() != Some(&session.account_id))
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "一般使用者只能操作自己的帳號" })),
        )
            .into_response();
    }

    let mut targets = Vec::with_capacity(selected.len());
    for id in &selected {
        let Some(account) = state.accounts.find(id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("account not found: {id}") })),
            )
                .into_response();
        };
        targets.push(account);
    }

    let kind = req.kind.trim().to_ascii_lowercase();
    let rollcall_id = mock_rollcall_id();
    let timeout = Duration::from_secs(300);
    let provider = "mock";
    let first_account_id = targets[0].account_id.clone();

    match kind.as_str() {
        "number" | "num" | "password" => {
            let code = req
                .code
                .unwrap_or_else(|| "1234".to_string())
                .trim()
                .to_string();
            if code.len() != 4 || !code.chars().all(|c| c.is_ascii_digit()) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "mock number code must be 4 digits" })),
                )
                    .into_response();
            }

            let mut first_url = String::new();
            let mut first_token = String::new();
            let mut number_receivers = Vec::new();
            for account in &targets {
                let (tx, rx) = mpsc::channel(8);
                let link = match scanner
                    .register_number_pending(
                        provider,
                        rollcall_id,
                        &account.account_id,
                        tx,
                        timeout,
                    )
                    .await
                {
                    Ok(link) => link,
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": e.to_string() })),
                        )
                            .into_response();
                    }
                };
                if first_url.is_empty() {
                    first_url = link.input_url.clone();
                    first_token = link.token.clone();
                }
                set_mock_detected_status(
                    &account.monitor_status,
                    rollcall_id,
                    "Mock 數字點名",
                    &link.input_url,
                )
                .await;
                crate::web_push_notifications::notify_rollcall_with_url(
                    &account.account_id,
                    rollcall_id,
                    "Mock 數字點名",
                    "數字簽到",
                    Some(link.input_url.as_str()),
                );
                number_receivers.push((
                    account.account_id.clone(),
                    Arc::clone(&account.monitor_status),
                    rx,
                ));
            }

            if let Some(messenger) = state.messenger.as_ref() {
                for account in &targets {
                    let target = account.target.lock().await.clone();
                    let _ = messenger
                        .push_to_account_or_admin(
                            &target,
                            &OutboundMessage::NumberCodeRequested(NumberCodeRequest {
                                rollcall_id,
                                account: account.account_id.clone(),
                                course_name: "Mock 數字點名".to_string(),
                                teacher_name: "Mock Admin".to_string(),
                                input_url: first_url.clone(),
                                alt_input_url: String::new(),
                                timeout_secs: timeout.as_secs(),
                            }),
                        )
                        .await;
                }
            }

            for (account_id_task, status_task, mut rx) in number_receivers {
                let scanner_task = Arc::clone(&scanner);
                let expected_code = code.clone();
                tokio::spawn(async move {
                    let deadline = tokio::time::Instant::now() + timeout;
                    loop {
                        let now = tokio::time::Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let remaining = deadline.saturating_duration_since(now);
                        match tokio::time::timeout(remaining, rx.recv()).await {
                            Ok(Some(NumberCodeInput::Code(input_code)))
                                if input_code == expected_code =>
                            {
                                scanner_task
                                    .mark_number_completed("mock", rollcall_id, &account_id_task)
                                    .await;
                                push_mock_history(
                                    &status_task,
                                    rollcall_id,
                                    "Mock 數字點名",
                                    "數字簽到",
                                    true,
                                    format!("Mock 數字碼 `{input_code}` 正確"),
                                )
                                .await;
                                break;
                            }
                            Ok(Some(NumberCodeInput::Code(input_code))) => {
                                let reason = format!(
                                    "Mock 數字碼 `{input_code}` 錯誤，正確為 `{expected_code}`"
                                );
                                scanner_task
                                    .mark_number_failed(
                                        "mock",
                                        rollcall_id,
                                        &account_id_task,
                                        &reason,
                                    )
                                    .await;
                                push_mock_history(
                                    &status_task,
                                    rollcall_id,
                                    "Mock 數字點名",
                                    "數字簽到",
                                    false,
                                    reason,
                                )
                                .await;
                            }
                            Ok(Some(NumberCodeInput::AutoLookup)) => {
                                scanner_task
                                    .mark_number_completed("mock", rollcall_id, &account_id_task)
                                    .await;
                                push_mock_history(
                                    &status_task,
                                    rollcall_id,
                                    "Mock 數字點名",
                                    "數字簽到",
                                    true,
                                    format!("Mock 自動讀取代碼 `{expected_code}` 成功"),
                                )
                                .await;
                                break;
                            }
                            Ok(None) | Err(_) => break,
                        }
                    }
                });
            }

            (
                StatusCode::OK,
                Json(json!(AdminMockStartResponse {
                    kind: "number".to_string(),
                    provider: provider.to_string(),
                    rollcall_id,
                    account_id: first_account_id,
                    token: first_token,
                    url: first_url,
                    qr_base: None,
                    interval_secs: None,
                })),
            )
                .into_response()
        }
        "qr" | "qrcode" | "quick" => {
            let interval_secs = req.interval_secs.unwrap_or(15).clamp(1, 300);
            let qr_base = format!("MOCK_QR_{rollcall_id}");
            let mut first_url = String::new();
            let mut first_token = String::new();
            let mut qr_receivers = Vec::new();
            for account in &targets {
                let (tx, rx) = mpsc::channel(8);
                let link = match scanner
                    .register_pending(provider, rollcall_id, &account.account_id, tx, timeout)
                    .await
                {
                    Ok(link) => link,
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": e.to_string() })),
                        )
                            .into_response();
                    }
                };
                if first_url.is_empty() {
                    first_url = link.scan_url.clone();
                    first_token = link.token.clone();
                }
                set_mock_detected_status(
                    &account.monitor_status,
                    rollcall_id,
                    "Mock QR Code 點名",
                    &link.scan_url,
                )
                .await;
                crate::web_push_notifications::notify_rollcall_with_url(
                    &account.account_id,
                    rollcall_id,
                    "Mock QR Code 點名",
                    "QR Code 簽到",
                    Some(link.scan_url.as_str()),
                );
                qr_receivers.push((
                    account.account_id.clone(),
                    Arc::clone(&account.monitor_status),
                    rx,
                ));
            }

            if let Some(messenger) = state.messenger.as_ref() {
                for account in &targets {
                    let target = account.target.lock().await.clone();
                    let _ = messenger
                        .push_to_account_or_admin(
                            &target,
                            &OutboundMessage::QrCodeRequested(QrCodeRequest {
                                rollcall_id,
                                account: account.account_id.clone(),
                                course_name: "Mock QR Code 點名".to_string(),
                                teacher_name: "Mock Admin".to_string(),
                                scan_url: first_url.clone(),
                                alt_scan_url: String::new(),
                                timeout_secs: timeout.as_secs(),
                            }),
                        )
                        .await;
                }
            }

            for (account_id_task, status_task, mut rx) in qr_receivers {
                let scanner_task = Arc::clone(&scanner);
                tokio::spawn(async move {
                    if let Ok(Some(qr_data)) = tokio::time::timeout(timeout, rx.recv()).await {
                        scanner_task
                            .mark_completed("mock", rollcall_id, &account_id_task)
                            .await;
                        push_mock_history(
                            &status_task,
                            rollcall_id,
                            "Mock QR Code 點名",
                            "QR Code 簽到",
                            true,
                            format!("Mock QR payload 已收到（長度：{}）", qr_data.len()),
                        )
                        .await;
                    }
                });
            }

            (
                StatusCode::OK,
                Json(json!(AdminMockStartResponse {
                    kind: "qr".to_string(),
                    provider: provider.to_string(),
                    rollcall_id,
                    account_id: first_account_id,
                    token: first_token,
                    url: first_url,
                    qr_base: Some(qr_base),
                    interval_secs: Some(interval_secs),
                })),
            )
                .into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "mock kind must be number or qr" })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_mock_qr_svg_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminMockQrQuery>,
) -> Response {
    let Some(_session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    // Any authenticated user can render the QR preview for a Mock 點名 they
    // started. The start endpoint separately restricts regular users to their
    // own account, and rendering arbitrary QR text has no account side effect.
    if query.data.len() > 2048 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "QR data too large" })),
        )
            .into_response();
    }

    let code = match QrCode::new(query.data.as_bytes()) {
        Ok(code) => code,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("QR generate failed: {e}") })),
            )
                .into_response();
        }
    };
    let image = code.render::<svg::Color>().min_dimensions(360, 360).build();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")],
        image,
    )
        .into_response()
}

pub(super) async fn handle_mock_rollcall_action(
    state: &AdminWebState,
    action: AdminActionRequest,
) -> Response {
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

    let Some(account) = state.accounts.find(&account_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("account not found: {account_id}") })),
        )
            .into_response();
    };

    let Some(scanner) = state.scanner.as_ref().map(Arc::clone) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "scanner/public_base_url is not enabled" })),
        )
            .into_response();
    };

    let kind = action
        .mock_kind
        .unwrap_or_else(|| "number".to_string())
        .trim()
        .to_ascii_lowercase();
    let rollcall_id = mock_rollcall_id();
    let timeout = Duration::from_secs(300);
    let status = Arc::clone(&account.monitor_status);
    let target = account.target.lock().await.clone();

    match kind.as_str() {
        "number" | "num" | "password" => {
            let code = action
                .code
                .unwrap_or_else(|| "1234".to_string())
                .trim()
                .to_string();
            if code.len() != 4 || !code.chars().all(|c| c.is_ascii_digit()) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "mock number code must be 4 digits" })),
                )
                    .into_response();
            }

            let (tx, mut rx) = mpsc::channel(8);
            let link = match scanner
                .register_number_pending("mock", rollcall_id, &account.account_id, tx, timeout)
                .await
            {
                Ok(link) => link,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
            };

            set_mock_detected_status(&status, rollcall_id, "Mock 數字點名", &link.input_url).await;
            crate::web_push_notifications::notify_rollcall_with_url(
                &account.account_id,
                rollcall_id,
                "Mock 數字點名",
                "數字簽到",
                Some(link.input_url.as_str()),
            );
            if let Some(messenger) = state.messenger.as_ref() {
                let _ = messenger
                    .push_to_account_or_admin(
                        &target,
                        &OutboundMessage::NumberCodeRequested(NumberCodeRequest {
                            rollcall_id,
                            account: account.account_id.clone(),
                            course_name: "Mock 數字點名".to_string(),
                            teacher_name: "Mock Admin".to_string(),
                            input_url: link.input_url.clone(),
                            alt_input_url: String::new(),
                            timeout_secs: timeout.as_secs(),
                        }),
                    )
                    .await;
            }

            let scanner_task = Arc::clone(&scanner);
            let status_task = Arc::clone(&status);
            let account_id_task = account.account_id.clone();
            let expected_code = code.clone();
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + timeout;
                loop {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    match tokio::time::timeout(remaining, rx.recv()).await {
                        Ok(Some(NumberCodeInput::Code(input_code)))
                            if input_code == expected_code =>
                        {
                            scanner_task
                                .mark_number_completed("mock", rollcall_id, &account_id_task)
                                .await;
                            push_mock_history(
                                &status_task,
                                rollcall_id,
                                "Mock 數字點名",
                                "數字簽到",
                                true,
                                format!("Mock 數字碼 `{input_code}` 正確"),
                            )
                            .await;
                            break;
                        }
                        Ok(Some(NumberCodeInput::Code(input_code))) => {
                            let reason = format!(
                                "Mock 數字碼 `{input_code}` 錯誤，正確為 `{expected_code}`"
                            );
                            scanner_task
                                .mark_number_failed("mock", rollcall_id, &account_id_task, &reason)
                                .await;
                            push_mock_history(
                                &status_task,
                                rollcall_id,
                                "Mock 數字點名",
                                "數字簽到",
                                false,
                                reason,
                            )
                            .await;
                        }
                        Ok(Some(NumberCodeInput::AutoLookup)) => {
                            scanner_task
                                .mark_number_completed("mock", rollcall_id, &account_id_task)
                                .await;
                            push_mock_history(
                                &status_task,
                                rollcall_id,
                                "Mock 數字點名",
                                "數字簽到",
                                true,
                                format!("Mock 自動讀取代碼 `{expected_code}` 成功"),
                            )
                            .await;
                            break;
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            });

            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "affected": 1,
                    "message": format!("已觸發 Mock 數字點名：{account_id}"),
                    "url": link.input_url,
                })),
            )
                .into_response()
        }
        "qr" | "qrcode" | "quick" => {
            let (tx, mut rx) = mpsc::channel(8);
            let link = match scanner
                .register_pending("mock", rollcall_id, &account.account_id, tx, timeout)
                .await
            {
                Ok(link) => link,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
            };

            set_mock_detected_status(&status, rollcall_id, "Mock QR Code 點名", &link.scan_url)
                .await;
            crate::web_push_notifications::notify_rollcall_with_url(
                &account.account_id,
                rollcall_id,
                "Mock QR Code 點名",
                "QR Code 簽到",
                Some(link.scan_url.as_str()),
            );
            if let Some(messenger) = state.messenger.as_ref() {
                let _ = messenger
                    .push_to_account_or_admin(
                        &target,
                        &OutboundMessage::QrCodeRequested(QrCodeRequest {
                            rollcall_id,
                            account: account.account_id.clone(),
                            course_name: "Mock QR Code 點名".to_string(),
                            teacher_name: "Mock Admin".to_string(),
                            scan_url: link.scan_url.clone(),
                            alt_scan_url: String::new(),
                            timeout_secs: timeout.as_secs(),
                        }),
                    )
                    .await;
            }

            let scanner_task = Arc::clone(&scanner);
            let status_task = Arc::clone(&status);
            let account_id_task = account.account_id.clone();
            tokio::spawn(async move {
                if let Ok(Some(qr_data)) = tokio::time::timeout(timeout, rx.recv()).await {
                    scanner_task
                        .mark_completed("mock", rollcall_id, &account_id_task)
                        .await;
                    push_mock_history(
                        &status_task,
                        rollcall_id,
                        "Mock QR Code 點名",
                        "QR Code 簽到",
                        true,
                        format!("Mock QR payload 已收到（長度：{}）", qr_data.len()),
                    )
                    .await;
                }
            });

            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "affected": 1,
                    "message": format!("已觸發 Mock QR Code 點名：{account_id}"),
                    "url": link.scan_url,
                })),
            )
                .into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "mock_kind must be number or qr" })),
        )
            .into_response(),
    }
}

pub(super) fn mock_rollcall_id() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    // Keep every Mock trigger unique for practical purposes. The previous
    // modulo-100000 value repeated every 100 seconds and was then suppressed by
    // the two-hour Web Push duplicate guard.
    1_000_000_000_000 + millis
}

pub(super) async fn set_mock_detected_status(
    status: &Arc<Mutex<crate::adapters::events::MonitorStatus>>,
    rollcall_id: u64,
    course_name: &str,
    url: &str,
) {
    let mut status = status.lock().await;
    status.last_detected_timestamp = Some(mock_unix_secs());
    status.last_detected_course = Some(course_name.to_string());
    status.last_detected_rollcall_id = Some(rollcall_id);
    status.last_detected_url = Some(url.to_string());
}

pub(super) async fn push_mock_history(
    status: &Arc<Mutex<crate::adapters::events::MonitorStatus>>,
    rollcall_id: u64,
    course_name: &str,
    attendance_type: &str,
    success: bool,
    result: String,
) {
    let mut status = status.lock().await;
    status.rollcall_history.insert(
        0,
        RollcallHistoryEntry {
            timestamp: mock_unix_secs(),
            rollcall_id,
            course_name: course_name.to_string(),
            attendance_type: attendance_type.to_string(),
            success,
            result,
        },
    );
    if success {
        status.last_success_course = Some(course_name.to_string());
        status.last_success_rollcall_id = Some(rollcall_id);
        if status.last_detected_rollcall_id == Some(rollcall_id) {
            status.last_detected_timestamp = None;
            status.last_detected_course = None;
            status.last_detected_rollcall_id = None;
            status.last_detected_url = None;
        }
    }
    if status.rollcall_history.len() > 50 {
        status.rollcall_history.truncate(50);
    }
}

pub(super) fn mock_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
