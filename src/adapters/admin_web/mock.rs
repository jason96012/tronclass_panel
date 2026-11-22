//! Mock rollcall page/API handlers for Admin Web.

use super::*;
use crate::adapters::events::RollcallResultEvent;
// MOCK_RADAR_V1：雷達點名模擬需要「座標輸入通道」與「班上進度」型別。
use crate::adapters::requests::{create_radar_point_channel, RadarPointReceiver};
use crate::api::rollcall::{RollcallProgress, AUTO_SUBMIT_PERCENT};

#[derive(Debug, Deserialize)]
pub(super) struct AdminMockStartRequest {
    kind: String,
    selected_accounts: Vec<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    interval_secs: Option<u64>,
    // MOCK_RADAR_V1：雷達點名的「模擬教室」與判定條件（面板輸入，全部是測試值）。
    #[serde(default)]
    radar_latitude: Option<f64>,
    #[serde(default)]
    radar_longitude: Option<f64>,
    #[serde(default)]
    radar_radius_m: Option<f64>,
    #[serde(default)]
    radar_class_percent: Option<u32>,
}

/// MOCK_RADAR_V1：雷達模擬回給面板的參數摘要（其他點名類型不會有這個欄位）。
#[derive(Debug, Serialize)]
pub(super) struct AdminMockRadarSummary {
    latitude: f64,
    longitude: f64,
    radius_m: f64,
    class_percent: u32,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    radar: Option<AdminMockRadarSummary>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminMockQrQuery {
    #[serde(default)]
    token: String,
    data: String,
}

const MAX_MOCK_SELECTIONS: usize = 128;

// ─── MOCK_RADAR_V1：模擬雷達點名的「假學校」 ────────────────────────────────
// 真實流程＝監控循環把輸入頁送來的座標交給學校，學校用距離判斷在不在教室；
// 這裡由面板自己扮演學校：座標離「模擬教室」≤ 判定半徑＝成功，否則回報距離不足。
// 全部都是測試值，不會連到任何學校。

/// 預設判定半徑（公尺）。
const MOCK_RADAR_DEFAULT_RADIUS_M: f64 = 100.0;
/// 判定半徑可調範圍。
const MOCK_RADAR_MIN_RADIUS_M: f64 = 10.0;
const MOCK_RADAR_MAX_RADIUS_M: f64 = 5000.0;
/// 沒填座標時的模擬教室（台北車站附近，純測試值）。
const MOCK_RADAR_DEFAULT_CLASSROOM: [f64; 2] = [25.0478, 121.5170];
/// 模擬班上人數（給「班上已點到 x / y」與 15% 自動送出用）。
const MOCK_RADAR_ROSTER: usize = 40;
/// 模擬班上進度的檢查間隔（與 `AUTO_SUBMIT_POLL_SECS` 同一個精神：真流程是每 10 秒）。
const MOCK_RADAR_PROGRESS_TICK_SECS: u64 = 10;

/// MOCK_RADAR_V1：一場模擬雷達點名的參數。
#[derive(Debug, Clone, Copy)]
struct MockRadarPlan {
    /// 模擬教室座標（學校端的「正確答案」）
    classroom: [f64; 2],
    radius_m: f64,
    /// 模擬班上到課率（0＝模擬「查不到班上進度」）
    class_percent: u32,
}

/// 兩點距離（公尺）。與 `rollcalls::radar` 的 Haversine 同一個公式，這裡獨立一份
/// 是為了讓 admin_web 不必依賴點名模組（模擬器只需要「像學校一樣算距離」）。
fn mock_haversine_meters(from: [f64; 2], to: [f64; 2]) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_371_000.0;
    let phi1 = from[0].to_radians();
    let phi2 = to[0].to_radians();
    let d_phi = (to[0] - from[0]).to_radians();
    let d_lambda = (to[1] - from[1]).to_radians();
    let a = (d_phi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (d_lambda / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().min(1.0).asin()
}

/// 把面板輸入換成一場模擬計畫；座標不合法就明確拒收（不要把打錯的座標當教室）。
fn mock_radar_plan(
    latitude: Option<f64>,
    longitude: Option<f64>,
    radius_m: Option<f64>,
    class_percent: Option<u32>,
) -> Result<MockRadarPlan, Response> {
    let bad = |message: &str| -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": message })),
        )
            .into_response()
    };
    let classroom = match (latitude, longitude) {
        (None, None) => MOCK_RADAR_DEFAULT_CLASSROOM,
        (Some(lat), Some(lon)) => {
            let finite = lat.is_finite() && lon.is_finite();
            let in_range = (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon);
            if !finite || !in_range || (lat == 0.0 && lon == 0.0) {
                return Err(bad("radar classroom coordinates are invalid"));
            }
            [lat, lon]
        }
        _ => return Err(bad("radar classroom needs both latitude and longitude")),
    };
    let radius_m = radius_m.unwrap_or(MOCK_RADAR_DEFAULT_RADIUS_M);
    if !radius_m.is_finite() {
        return Err(bad("radar radius must be a number"));
    }
    Ok(MockRadarPlan {
        classroom,
        radius_m: radius_m.clamp(MOCK_RADAR_MIN_RADIUS_M, MOCK_RADAR_MAX_RADIUS_M),
        class_percent: class_percent.unwrap_or(0).min(100),
    })
}

/// 模擬班上進度：固定 [`MOCK_RADAR_ROSTER`] 人，成功人數依面板輸入的百分比換算。
/// `0` 故意回 `None`——用來測「查不到班上進度時，頁面與流程都要維持原本行為」。
fn mock_class_progress(percent: u32) -> Option<RollcallProgress> {
    if percent == 0 {
        return None;
    }
    let completed = (MOCK_RADAR_ROSTER * percent as usize + 50) / 100;
    Some(RollcallProgress {
        completed: completed.min(MOCK_RADAR_ROSTER),
        total: MOCK_RADAR_ROSTER,
        source: "mock:class_percent".to_string(),
    })
}

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

pub(super) async fn admin_mock_page_handler() -> Response {
    // 模擬點名已整合到 /admin 的彈出視窗。ADMIN_PAGE_TRI_STATE_V1：Dashboard HTML 現在必須
    // 帶伺服器注入的驗證狀態，這裡不再直接回靜態 HTML，改為導到 /admin 走三態判斷。
    (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, "/admin")]).into_response()
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
                    account.target.lock().await.clone(),
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

            for (account_id_task, status_task, target_task, mut rx) in number_receivers {
                let messenger_task = state.messenger.clone();
                let scanner_task = Arc::clone(&scanner);
                let expected_code = code.clone();
                tokio::spawn(async move {
                    let deadline = tokio::time::Instant::now() + timeout;
                    loop {
                        let now = tokio::time::Instant::now();
                        if now >= deadline {
                                                        // MOCK_FIDELITY_V1：與真實流程一致——逾時視為失敗，記錄並推播。
                            scanner_task
                                .mark_number_failed("mock", rollcall_id, &account_id_task, "等待輸入逾時")
                                .await;
                            push_mock_history(
                                &status_task,
                                rollcall_id,
                                "Mock 數字點名",
                                "數字簽到",
                                false,
                                "等待輸入逾時".to_string(),
                            )
                            .await;
                            crate::web_push_notifications::notify_rollcall_finished(
                                &account_id_task,
                                rollcall_id,
                                "Mock 數字點名",
                                "數字簽到",
                                false,
                                "❌ 失敗：等待輸入逾時",
                            );
                            if let Some(messenger) = messenger_task.as_ref() {
                                let _ = messenger
                                    .push_to_account_or_admin(
                                        &target_task,
                                        &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                            rollcall_id,
                                            account: account_id_task.clone(),
                                            course_name: "Mock 數字點名".to_string(),
                                            attendance_type: "數字簽到".to_string(),
                                            success: false,
                                            result: "❌ 失敗：等待輸入逾時".to_string(),
                                            elapsed_ms: 0,
                                        }),
                                    )
                                    .await;
                            }
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
                                // MOCK_FINISH_NOTIFY_V1：與真實流程一致，成功結果推播。
                                crate::web_push_notifications::notify_rollcall_finished(
                                    &account_id_task,
                                    rollcall_id,
                                    "Mock 數字點名",
                                    "數字簽到",
                                    true,
                                    &format!("✅ 成功：Mock 數字碼 `{input_code}` 正確"),
                                );
                                if let Some(messenger) = messenger_task.as_ref() {
                                    let _ = messenger
                                        .push_to_account_or_admin(
                                            &target_task,
                                            &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                                rollcall_id,
                                                account: account_id_task.clone(),
                                                course_name: "Mock 數字點名".to_string(),
                                                attendance_type: "數字簽到".to_string(),
                                                success: true,
                                                result: format!("✅ 成功：Mock 數字碼 `{input_code}` 正確").to_string(),
                                                elapsed_ms: 0,
                                            }),
                                        )
                                        .await;
                                }
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
                                // MOCK_FINISH_NOTIFY_V1：與真實流程一致，成功結果推播。
                                crate::web_push_notifications::notify_rollcall_finished(
                                    &account_id_task,
                                    rollcall_id,
                                    "Mock 數字點名",
                                    "數字簽到",
                                    true,
                                    &format!("✅ 成功：Mock 自動讀取代碼 `{expected_code}`"),
                                );
                                if let Some(messenger) = messenger_task.as_ref() {
                                    let _ = messenger
                                        .push_to_account_or_admin(
                                            &target_task,
                                            &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                                rollcall_id,
                                                account: account_id_task.clone(),
                                                course_name: "Mock 數字點名".to_string(),
                                                attendance_type: "數字簽到".to_string(),
                                                success: true,
                                                result: format!("✅ 成功：Mock 自動讀取代碼 `{expected_code}`").to_string(),
                                                elapsed_ms: 0,
                                            }),
                                        )
                                        .await;
                                }
                                break;
                            }
                            Ok(None) | Err(_) => {                            // MOCK_FIDELITY_V1：與真實流程一致——逾時視為失敗，記錄並推播。
                            scanner_task
                                .mark_number_failed("mock", rollcall_id, &account_id_task, "等待輸入逾時")
                                .await;
                            push_mock_history(
                                &status_task,
                                rollcall_id,
                                "Mock 數字點名",
                                "數字簽到",
                                false,
                                "等待輸入逾時".to_string(),
                            )
                            .await;
                            crate::web_push_notifications::notify_rollcall_finished(
                                &account_id_task,
                                rollcall_id,
                                "Mock 數字點名",
                                "數字簽到",
                                false,
                                "❌ 失敗：等待輸入逾時",
                            );
                            if let Some(messenger) = messenger_task.as_ref() {
                                let _ = messenger
                                    .push_to_account_or_admin(
                                        &target_task,
                                        &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                            rollcall_id,
                                            account: account_id_task.clone(),
                                            course_name: "Mock 數字點名".to_string(),
                                            attendance_type: "數字簽到".to_string(),
                                            success: false,
                                            result: "❌ 失敗：等待輸入逾時".to_string(),
                                            elapsed_ms: 0,
                                        }),
                                    )
                                    .await;
                            }
                            break;
                            }
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
                    radar: None,
                })),
            )
                .into_response()
        }
        "radar" | "radar_rollcall" | "location" => {
            let plan = match mock_radar_plan(
                req.radar_latitude,
                req.radar_longitude,
                req.radar_radius_m,
                req.radar_class_percent,
            ) {
                Ok(plan) => plan,
                Err(response) => return response,
            };
            let (first_token, first_url) =
                match start_mock_radar(&state, &scanner, &targets, &plan, rollcall_id, timeout)
                    .await
                {
                    Ok(result) => result,
                    Err(response) => return response,
                };

            (
                StatusCode::OK,
                Json(json!(AdminMockStartResponse {
                    kind: "radar".to_string(),
                    provider: provider.to_string(),
                    rollcall_id,
                    account_id: first_account_id,
                    token: first_token,
                    url: first_url,
                    qr_base: None,
                    interval_secs: None,
                    radar: Some(AdminMockRadarSummary {
                        latitude: plan.classroom[0],
                        longitude: plan.classroom[1],
                        radius_m: plan.radius_m,
                        class_percent: plan.class_percent,
                    }),
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
                    account.target.lock().await.clone(),
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

            for (account_id_task, status_task, target_task, mut rx) in qr_receivers {
                let messenger_task = state.messenger.clone();
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
                        // MOCK_FINISH_NOTIFY_V1：與真實流程一致，成功結果推播。
                        crate::web_push_notifications::notify_rollcall_finished(
                            &account_id_task,
                            rollcall_id,
                            "Mock QR Code 點名",
                            "QR Code 簽到",
                            true,
                            &format!("✅ 成功：Mock QR payload 已收到（長度：{}）", qr_data.len()),
                        );
                        if let Some(messenger) = messenger_task.as_ref() {
                            let _ = messenger
                                .push_to_account_or_admin(
                                    &target_task,
                                    &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                        rollcall_id,
                                        account: account_id_task.clone(),
                                        course_name: "Mock QR Code 點名".to_string(),
                                        attendance_type: "QR Code 簽到".to_string(),
                                        success: true,
                                        result: format!("✅ 成功：Mock QR payload 已收到（長度：{}）", qr_data.len()).to_string(),
                                        elapsed_ms: 0,
                                    }),
                                )
                                .await;
                        }
                    } else {
                        // MOCK_FIDELITY_V1：與真實流程一致——逾時視為失敗，記錄並推播。
                        push_mock_history(
                            &status_task,
                            rollcall_id,
                            "Mock QR Code 點名",
                            "QR Code 簽到",
                            false,
                            "等待掃碼逾時".to_string(),
                        )
                        .await;
                        crate::web_push_notifications::notify_rollcall_finished(
                            &account_id_task,
                            rollcall_id,
                            "Mock QR Code 點名",
                            "QR Code 簽到",
                            false,
                            "❌ 失敗：等待掃碼逾時",
                        );
                        if let Some(messenger) = messenger_task.as_ref() {
                            let _ = messenger
                                .push_to_account_or_admin(
                                    &target_task,
                                    &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                        rollcall_id,
                                        account: account_id_task.clone(),
                                        course_name: "Mock QR Code 點名".to_string(),
                                        attendance_type: "QR Code 簽到".to_string(),
                                        success: false,
                                        result: "❌ 失敗：等待掃碼逾時".to_string(),
                                        elapsed_ms: 0,
                                    }),
                                )
                                .await;
                        }

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
                    radar: None,
                })),
            )
                .into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "mock kind must be number, qr or radar" })),
        )
            .into_response(),
    }
}

/// MOCK_RADAR_V1：對每個選取帳號註冊一場「等待座標」的雷達點名，並各開一個任務扮演學校。
///
/// 回傳 `(第一個帳號的 token, 第一個帳號的輸入頁網址)`（與數字／QR 的形狀一致）。
async fn start_mock_radar(
    state: &AdminWebState,
    scanner: &Arc<QrScannerRegistry>,
    targets: &[RequestAccountState],
    plan: &MockRadarPlan,
    rollcall_id: u64,
    timeout: Duration,
) -> Result<(String, String), Response> {
    let provider = "mock";
    let course_name = "Mock 雷達點名";
    let attendance_type = "雷達簽到";
    let progress = mock_class_progress(plan.class_percent);
    let mut first_token = String::new();
    let mut first_url = String::new();
    let mut receivers = Vec::with_capacity(targets.len());

    for account in targets {
        let (tx, rx) = create_radar_point_channel(8);
        let link = match scanner
            .register_radar_pending(
                provider,
                rollcall_id,
                &account.account_id,
                tx,
                timeout,
                &[plan.classroom],
                course_name,
            )
            .await
        {
            Ok(link) => link,
            Err(e) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response());
            }
        };
        if first_url.is_empty() {
            first_url = link.input_url.clone();
            first_token = link.token.clone();
        }
        // fix72：頁面會顯示「班上：已點到 x / y」與「超過 15% 系統會自動送出」，
        // 這裡把面板輸入的模擬到課率餵給它（0% ＝ 模擬查不到班上進度）。
        scanner
            .set_radar_class_progress(provider, rollcall_id, &account.account_id, progress.clone())
            .await;
        set_mock_detected_status(
            &account.monitor_status,
            rollcall_id,
            course_name,
            &link.input_url,
        )
        .await;
        crate::web_push_notifications::notify_rollcall_with_url(
            &account.account_id,
            rollcall_id,
            course_name,
            attendance_type,
            Some(link.input_url.as_str()),
        );
        if let Some(messenger) = state.messenger.as_ref() {
            let target = account.target.lock().await.clone();
            let _ = messenger
                .push_to_account_or_admin(
                    &target,
                    &OutboundMessage::Text(format!(
                        "🎯 需要雷達座標\n\n課程：{course_name}\n帳號：{}\n教師：Mock Admin\n點名 ID：{rollcall_id}\n\n\
                         開這一頁按「🧮 系統演算」，系統就會用教室位置幫你送出點名：{}",
                        account.account_id, link.input_url
                    )),
                )
                .await;
        }
        receivers.push((
            account.account_id.clone(),
            Arc::clone(&account.monitor_status),
            account.target.lock().await.clone(),
            rx,
        ));
    }

    for (account_id_task, status_task, target_task, rx) in receivers {
        let scanner_task = Arc::clone(scanner);
        let messenger_task = state.messenger.clone();
        let plan_task = *plan;
        tokio::spawn(async move {
            run_mock_radar_account(
                scanner_task,
                account_id_task,
                status_task,
                target_task,
                rx,
                messenger_task,
                rollcall_id,
                timeout,
                plan_task,
            )
            .await;
        });
    }

    Ok((first_token, first_url))
}

/// MOCK_RADAR_V1：一個帳號的「假學校」流程（＝監控循環那一側）。
///
/// - 收到座標：距離 ≤ 判定半徑 → 成功；否則把「距離不足」寫回等待項目
///   （輸入頁輪詢狀態時就看得到，和真實流程一樣），然後繼續等下一組。
/// - 這一段沒收到座標：檢查模擬的班上進度，超過 `AUTO_SUBMIT_PERCENT` 就照 fix72 的
///   雷達語意「用教室座標自動送出一次」，並推播說明。
/// - 逾時：標記失敗、寫歷史、推播（與數字／QR 模擬一致）。
#[allow(clippy::too_many_arguments)]
async fn run_mock_radar_account(
    scanner: Arc<QrScannerRegistry>,
    account_id: String,
    status: Arc<Mutex<crate::adapters::events::MonitorStatus>>,
    target: crate::adapters::events::AdapterAccountTarget,
    rx: RadarPointReceiver,
    messenger: Option<Arc<dyn AdapterMessenger>>,
    rollcall_id: u64,
    timeout: Duration,
    plan: MockRadarPlan,
) {
    let course_name = "Mock 雷達點名";
    let attendance_type = "雷達簽到";
    let progress = mock_class_progress(plan.class_percent);
    let deadline = tokio::time::Instant::now() + timeout;
    let tick = Duration::from_secs(MOCK_RADAR_PROGRESS_TICK_SECS);
    let mut rejected = 0usize;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            scanner
                .mark_radar_failed("mock", rollcall_id, &account_id, "等待輸入逾時")
                .await;
            push_mock_history(
                &status,
                rollcall_id,
                course_name,
                attendance_type,
                false,
                "等待輸入逾時".to_string(),
            )
            .await;
            crate::web_push_notifications::notify_rollcall_finished(
                &account_id,
                rollcall_id,
                course_name,
                attendance_type,
                false,
                "❌ 失敗：等待輸入逾時",
            );
            if let Some(messenger) = messenger.as_ref() {
                let _ = messenger
                    .push_to_account_or_admin(
                        &target,
                        &OutboundMessage::RollcallFinished(RollcallResultEvent {
                            rollcall_id,
                            account: account_id.clone(),
                            course_name: course_name.to_string(),
                            attendance_type: attendance_type.to_string(),
                            success: false,
                            result: "❌ 失敗：等待輸入逾時".to_string(),
                            elapsed_ms: 0,
                        }),
                    )
                    .await;
            }
            return;
        }

        // 這一段最多等到「下一次檢查班上進度」的時間點（與 fix72 的 10 秒一次同節奏）。
        let segment = deadline.saturating_duration_since(now).min(tick);
        let received = {
            let mut guard = rx.lock().await;
            match tokio::time::timeout(segment, guard.recv()).await {
                Ok(Some(point)) => Some(point),
                // 通道關閉或這一段沒收到，都走同一條「看看班上進度」的路。
                _ => None,
            }
        };

        if let Some(point) = received {
            let distance = mock_haversine_meters(plan.classroom, [point.latitude, point.longitude]);
            let source = point.source_label();
            if distance <= plan.radius_m {
                let detail = format!(
                    "座標 ({:.6}, {:.6})（{source}），距教室 {distance:.0} 公尺（判定半徑 {:.0} 公尺）",
                    point.latitude,
                    point.longitude,
                    plan.radius_m
                );
                scanner
                    .mark_radar_completed("mock", rollcall_id, &account_id)
                    .await;
                push_mock_history(
                    &status,
                    rollcall_id,
                    course_name,
                    attendance_type,
                    true,
                    format!("✅ 成功：{detail}"),
                )
                .await;
                crate::web_push_notifications::notify_rollcall_finished(
                    &account_id,
                    rollcall_id,
                    course_name,
                    attendance_type,
                    true,
                    &format!("✅ 成功：{detail}"),
                );
                if let Some(messenger) = messenger.as_ref() {
                    let _ = messenger
                        .push_to_account_or_admin(
                            &target,
                            &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                rollcall_id,
                                account: account_id.clone(),
                                course_name: course_name.to_string(),
                                attendance_type: attendance_type.to_string(),
                                success: true,
                                result: format!("✅ 成功：{detail}"),
                                elapsed_ms: 0,
                            }),
                        )
                        .await;
                }
                return;
            }
            // 距離不足：真實流程這裡會拿到學校回報的距離並繼續重試，
            // 所以模擬只把原因寫回等待項目（輸入頁看得到），不當成整場失敗。
            rejected += 1;
            let reason = format!(
                "距離不足：還差 {distance:.0} 公尺（判定半徑 {:.0} 公尺）",
                plan.radius_m
            );
            warn!(
                account = %account_id,
                rollcall_id = rollcall_id,
                distance_m = distance,
                "Mock 雷達點名：座標距離不足，已寫回頁面並繼續等下一組"
            );
            scanner
                .mark_radar_failed("mock", rollcall_id, &account_id, &reason)
                .await;
            continue;
        }

        // 沒有輸入 → 檢查模擬的班上進度（fix72 的雷達語意：過門檻就自動送一次）。
        let passed = match progress.as_ref() {
            Some(progress) => progress.passed(AUTO_SUBMIT_PERCENT),
            None => false,
        };
        // 送出後立刻 return（不會回到迴圈），所以同一場只會自動送出一次。
        if passed {
            let (completed, total, percent) = match progress.as_ref() {
                Some(progress) => (
                    progress.completed,
                    progress.total,
                    progress.percent_rounded(),
                ),
                None => (0, MOCK_RADAR_ROSTER, 0),
            };
            let detail = format!(
                "🌐 班上已有 {completed}/{total} 人點名成功（{percent}%，超過 {AUTO_SUBMIT_PERCENT}%），\
                 系統自動用教室座標 ({:.6}, {:.6}) 送出",
                plan.classroom[0], plan.classroom[1]
            );
            info!(
                account = %account_id,
                rollcall_id = rollcall_id,
                rejected = rejected,
                "Mock 雷達點名：班上進度過門檻，自動改用教室位置送出"
            );
            scanner
                .mark_radar_completed("mock", rollcall_id, &account_id)
                .await;
            push_mock_history(
                &status,
                rollcall_id,
                course_name,
                attendance_type,
                true,
                detail.clone(),
            )
            .await;
            crate::web_push_notifications::notify_rollcall_finished(
                &account_id,
                rollcall_id,
                course_name,
                attendance_type,
                true,
                &detail,
            );
            if let Some(messenger) = messenger.as_ref() {
                let _ = messenger
                    .push_to_account_or_admin(
                        &target,
                        &OutboundMessage::RollcallFinished(RollcallResultEvent {
                            rollcall_id,
                            account: account_id.clone(),
                            course_name: course_name.to_string(),
                            attendance_type: attendance_type.to_string(),
                            success: true,
                            result: detail,
                            elapsed_ms: 0,
                        }),
                    )
                    .await;
            }
            return;
        }
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
                Json(json!({ "error": super::public_error("QR generate failed: ", &e) })),
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
            let messenger_task = state.messenger.clone();
            let target_task = target.clone();
            let expected_code = code.clone();
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + timeout;
                loop {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                                                // MOCK_FIDELITY_V1：與真實流程一致——逾時視為失敗，記錄並推播。
                        scanner_task
                            .mark_number_failed("mock", rollcall_id, &account_id_task, "等待輸入逾時")
                            .await;
                        push_mock_history(
                            &status_task,
                            rollcall_id,
                            "Mock 數字點名",
                            "數字簽到",
                            false,
                            "等待輸入逾時".to_string(),
                        )
                        .await;
                        crate::web_push_notifications::notify_rollcall_finished(
                            &account_id_task,
                            rollcall_id,
                            "Mock 數字點名",
                            "數字簽到",
                            false,
                            "❌ 失敗：等待輸入逾時",
                        );
                        if let Some(messenger) = messenger_task.as_ref() {
                            let _ = messenger
                                .push_to_account_or_admin(
                                    &target_task,
                                    &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                        rollcall_id,
                                        account: account_id_task.clone(),
                                        course_name: "Mock 數字點名".to_string(),
                                        attendance_type: "數字簽到".to_string(),
                                        success: false,
                                        result: "❌ 失敗：等待輸入逾時".to_string(),
                                        elapsed_ms: 0,
                                    }),
                                )
                                .await;
                        }
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
                            // MOCK_FINISH_NOTIFY_V1：與真實流程一致，成功結果推播。
                            crate::web_push_notifications::notify_rollcall_finished(
                                &account_id_task,
                                rollcall_id,
                                "Mock 數字點名",
                                "數字簽到",
                                true,
                                &format!("✅ 成功：Mock 數字碼 `{input_code}` 正確"),
                            );
                            if let Some(messenger) = messenger_task.as_ref() {
                                let _ = messenger
                                    .push_to_account_or_admin(
                                        &target_task,
                                        &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                            rollcall_id,
                                            account: account_id_task.clone(),
                                            course_name: "Mock 數字點名".to_string(),
                                            attendance_type: "數字簽到".to_string(),
                                            success: true,
                                            result: format!("✅ 成功：Mock 數字碼 `{input_code}` 正確").to_string(),
                                            elapsed_ms: 0,
                                        }),
                                    )
                                    .await;
                            }
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
                            // MOCK_FINISH_NOTIFY_V1：與真實流程一致，成功結果推播。
                            crate::web_push_notifications::notify_rollcall_finished(
                                &account_id_task,
                                rollcall_id,
                                "Mock 數字點名",
                                "數字簽到",
                                true,
                                &format!("✅ 成功：Mock 自動讀取代碼 `{expected_code}`"),
                            );
                            if let Some(messenger) = messenger_task.as_ref() {
                                let _ = messenger
                                    .push_to_account_or_admin(
                                        &target_task,
                                        &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                            rollcall_id,
                                            account: account_id_task.clone(),
                                            course_name: "Mock 數字點名".to_string(),
                                            attendance_type: "數字簽到".to_string(),
                                            success: true,
                                            result: format!("✅ 成功：Mock 自動讀取代碼 `{expected_code}`").to_string(),
                                            elapsed_ms: 0,
                                        }),
                                    )
                                    .await;
                            }
                            break;
                        }
                        Ok(None) | Err(_) => {                        // MOCK_FIDELITY_V1：與真實流程一致——逾時視為失敗，記錄並推播。
                        scanner_task
                            .mark_number_failed("mock", rollcall_id, &account_id_task, "等待輸入逾時")
                            .await;
                        push_mock_history(
                            &status_task,
                            rollcall_id,
                            "Mock 數字點名",
                            "數字簽到",
                            false,
                            "等待輸入逾時".to_string(),
                        )
                        .await;
                        crate::web_push_notifications::notify_rollcall_finished(
                            &account_id_task,
                            rollcall_id,
                            "Mock 數字點名",
                            "數字簽到",
                            false,
                            "❌ 失敗：等待輸入逾時",
                        );
                        if let Some(messenger) = messenger_task.as_ref() {
                            let _ = messenger
                                .push_to_account_or_admin(
                                    &target_task,
                                    &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                        rollcall_id,
                                        account: account_id_task.clone(),
                                        course_name: "Mock 數字點名".to_string(),
                                        attendance_type: "數字簽到".to_string(),
                                        success: false,
                                        result: "❌ 失敗：等待輸入逾時".to_string(),
                                        elapsed_ms: 0,
                                    }),
                                )
                                .await;
                        }
                        break;
                        }
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
            let messenger_task = state.messenger.clone();
            let target_task = target.clone();
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
                    // MOCK_FINISH_NOTIFY_V1：與真實流程一致，成功結果推播。
                    crate::web_push_notifications::notify_rollcall_finished(
                        &account_id_task,
                        rollcall_id,
                        "Mock QR Code 點名",
                        "QR Code 簽到",
                        true,
                        &format!("✅ 成功：Mock QR payload 已收到（長度：{}）", qr_data.len()),
                    );
                    if let Some(messenger) = messenger_task.as_ref() {
                        let _ = messenger
                            .push_to_account_or_admin(
                                &target_task,
                                &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                    rollcall_id,
                                    account: account_id_task.clone(),
                                    course_name: "Mock QR Code 點名".to_string(),
                                    attendance_type: "QR Code 簽到".to_string(),
                                    success: true,
                                    result: format!("✅ 成功：Mock QR payload 已收到（長度：{}）", qr_data.len()).to_string(),
                                    elapsed_ms: 0,
                                }),
                            )
                            .await;
                    }
                } else {
                    // MOCK_FIDELITY_V1：與真實流程一致——逾時視為失敗，記錄並推播。
                    push_mock_history(
                        &status_task,
                        rollcall_id,
                        "Mock QR Code 點名",
                        "QR Code 簽到",
                        false,
                        "等待掃碼逾時".to_string(),
                    )
                    .await;
                    crate::web_push_notifications::notify_rollcall_finished(
                        &account_id_task,
                        rollcall_id,
                        "Mock QR Code 點名",
                        "QR Code 簽到",
                        false,
                        "❌ 失敗：等待掃碼逾時",
                    );
                    if let Some(messenger) = messenger_task.as_ref() {
                        let _ = messenger
                            .push_to_account_or_admin(
                                &target_task,
                                &OutboundMessage::RollcallFinished(RollcallResultEvent {
                                    rollcall_id,
                                    account: account_id_task.clone(),
                                    course_name: "Mock QR Code 點名".to_string(),
                                    attendance_type: "QR Code 簽到".to_string(),
                                    success: false,
                                    result: "❌ 失敗：等待掃碼逾時".to_string(),
                                    elapsed_ms: 0,
                                }),
                            )
                            .await;
                    }

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
        "radar" | "radar_rollcall" | "location" => {
            // MOCK_RADAR_V1：這個入口（單一帳號的 action）沒有座標欄位，
            // 一律用預設模擬教室與判定半徑；要調參數請用面板的模擬點名表單。
            let plan = match mock_radar_plan(None, None, None, None) {
                Ok(plan) => plan,
                Err(response) => return response,
            };
            let targets = vec![account.clone()];
            let (_, url) =
                match start_mock_radar(state, &scanner, &targets, &plan, rollcall_id, timeout).await
                {
                    Ok(result) => result,
                    Err(response) => return response,
                };
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "affected": 1,
                    "message": format!("已觸發 Mock 雷達點名：{account_id}"),
                    "url": url,
                    "classroom": {
                        "latitude": plan.classroom[0],
                        "longitude": plan.classroom[1],
                    },
                    "radius_m": plan.radius_m,
                })),
            )
                .into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "mock_kind must be number, qr or radar" })),
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
