//! Discord 通知偏好設定 handlers。

use super::*;
// HOMEWORK_LADDER_V1：作業提醒階梯的限制常數（定義在 db::preferences）。
use crate::db::{
    DEFAULT_HOMEWORK_LADDER_HOURS as DEFAULT_LADDER, HOMEWORK_LADDER_MAX_HOURS as MAX_LADDER_HOURS,
    HOMEWORK_LADDER_MAX_STAGES as MAX_LADDER_STAGES,
    HOMEWORK_LADDER_MIN_HOURS as MIN_LADDER_HOURS,
};

#[derive(Debug, Deserialize)]
pub(super) struct AdminNotificationPreferencesQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminNotificationPreferenceInput {
    key: String,
    enabled: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminNotificationPreferencesUpdateRequest {
    #[serde(default)]
    account_id: Option<String>,
    preferences: Vec<AdminNotificationPreferenceInput>,
}

const DISCORD_NOTIFICATION_KEYS: &[(&str, &str, &str)] = &[
    ("system_started", "系統啟動", "服務啟動或帳號監控開始時通知"),
    ("rollcall_detected", "偵測到點名", "偵測到新點名時通知"),
    ("number_code", "數字點名", "需要輸入數字點名碼時通知"),
    ("qr_code", "QR Code 點名", "需要掃描或送出 QR Code 時通知"),
    (
        "rollcall_finished",
        "點名完成結果",
        "點名成功或失敗結果通知",
    ),
    ("status", "狀態回覆", "主動推送狀態摘要通知"),
    (
        "control",
        "控制操作",
        "暫停、恢復、Force、Reauth 等控制通知",
    ),
    ("other", "其他通知", "其他 Discord 通知"),
];

pub(super) fn resolve_notification_target(
    session: &AdminSession,
    requested: Option<String>,
) -> Result<String, Response> {
    if session.is_admin {
        let target = requested
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| session.account_id.clone());
        if target == "<token-admin>" {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "請指定 account_id" })),
            )
                .into_response());
        }
        Ok(target)
    } else {
        if let Some(requested) = requested
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            if requested != session.account_id {
                return Err(unauthorized_response());
            }
        }
        Ok(session.account_id.clone())
    }
}

pub(super) async fn admin_notification_preferences_get_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminNotificationPreferencesQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_optional_admin_text(
        query.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_notification_target(&session, query.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    let rows = match state
        .accounts_db
        .list_notification_preferences(&target, "discord")
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("notification database error: {e}") })),
            )
                .into_response();
        }
    };
    let mut saved = std::collections::HashMap::new();
    for row in rows {
        saved.insert(row.notification_key, row.enabled);
    }
    let preferences = DISCORD_NOTIFICATION_KEYS
        .iter()
        .map(|(key, label, description)| {
            json!({
                "key": key,
                "label": label,
                "description": description,
                "enabled": saved.get(*key).copied().unwrap_or(true),
            })
        })
        .collect::<Vec<_>>();
    (
        StatusCode::OK,
        Json(json!({
            "account_id": target,
            "channel": "discord",
            "is_admin": session.is_admin,
            "preferences": preferences,
        })),
    )
        .into_response()
}

pub(super) async fn admin_notification_preferences_update_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    body: axum::body::Bytes,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    let req: AdminNotificationPreferencesUpdateRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_request" })),
            )
                .into_response();
        }
    };
    if req.preferences.len() > ADMIN_MAX_NOTIFICATION_PREFERENCES {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "too many notification preferences; max {}",
                    ADMIN_MAX_NOTIFICATION_PREFERENCES
                )
            })),
        )
            .into_response();
    }
    if let Err(response) = validate_optional_admin_text(
        req.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_notification_target(&session, req.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    let allowed = DISCORD_NOTIFICATION_KEYS
        .iter()
        .map(|(key, _, _)| *key)
        .collect::<std::collections::HashSet<_>>();
    let mut preferences = Vec::with_capacity(req.preferences.len());
    let mut seen = std::collections::HashSet::with_capacity(req.preferences.len());
    for pref in req.preferences {
        if let Err(response) =
            validate_admin_text(&pref.key, ADMIN_MAX_ACTION_BYTES, "notification key")
        {
            return response;
        }
        let key = pref.key.trim().to_string();
        if !seen.insert(key.clone()) {
            continue;
        }
        if !allowed.contains(key.as_str()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unknown notification key: {key}") })),
            )
                .into_response();
        }
        preferences.push((key, pref.enabled));
    }
    if let Err(e) = state
        .accounts_db
        .set_notification_preferences(&target, "discord", &preferences)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("save notification preference failed: {e}") })),
        )
            .into_response();
    }
    crate::logs::record_audit(
        "web",
        &session.account_id,
        "notification_preferences",
        Some(&target),
        Some("discord"),
        "更新 Discord 通知設定",
    );
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "account_id": target })),
    )
        .into_response()
}

// ─── 作業截止提醒階梯（HOMEWORK_LADDER_V1）──────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkLadderQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkLadderUpdateRequest {
    #[serde(default)]
    account_id: Option<String>,
    /// 小時陣列（1～168、最多 8 個）。空陣列＝恢復預設 24/6/1。
    #[serde(default)]
    stages: Vec<u64>,
}

pub(super) async fn admin_homework_ladder_get_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminHomeworkLadderQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_optional_admin_text(
        query.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_notification_target(&session, query.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    match state
        .accounts_db
        .get_homework_reminder_ladder_raw(&target)
        .await
    {
        Ok(custom) => {
            let effective =
                custom.clone().unwrap_or_else(|| DEFAULT_LADDER.to_vec());
            (
                StatusCode::OK,
                Json(json!({
                    "account_id": target,
                    "stages": effective,
                    "use_default": custom.is_none(),
                    "default_stages": DEFAULT_LADDER,
                    "min_hours": MIN_LADDER_HOURS,
                    "max_hours": MAX_LADDER_HOURS,
                    "max_stages": MAX_LADDER_STAGES,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("homework ladder database error: {e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_homework_ladder_update_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    body: axum::body::Bytes,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    let req: AdminHomeworkLadderUpdateRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_request" })),
            )
                .into_response();
        }
    };
    if let Err(response) = validate_optional_admin_text(
        req.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_notification_target(&session, req.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };

    // 驗證：最多 8 個階段、每個 1～168 小時。空陣列視為「恢復預設」。
    if req.stages.len() > MAX_LADDER_STAGES {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("too many stages; max {MAX_LADDER_STAGES}") })),
        )
            .into_response();
    }
    if let Some(&bad) = req
        .stages
        .iter()
        .find(|&&h| !(MIN_LADDER_HOURS..=MAX_LADDER_HOURS).contains(&h))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("stage {bad} out of range {MIN_LADDER_HOURS}..={MAX_LADDER_HOURS} hours") })),
        )
            .into_response();
    }

    match state
        .accounts_db
        .set_homework_reminder_ladder(&target, &req.stages)
        .await
    {
        Ok(saved) => {
            let effective = if saved.is_empty() {
                DEFAULT_LADDER.to_vec()
            } else {
                saved.clone()
            };
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "homework_ladder_settings",
                Some(&target),
                Some(&if saved.is_empty() {
                    "restore-default".to_string()
                } else {
                    serde_json::to_string(&saved).unwrap_or_default()
                }),
                "更新作業提醒階梯設定",
            );
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "account_id": target,
                    "stages": effective,
                    "use_default": saved.is_empty(),
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("save homework ladder failed: {e}") })),
        )
            .into_response(),
    }
}


// ───────────────────────────────────────────────────────────────────────────────
// NOTIFY_TEST_ALL_V1：通知頁「把所有 Discord 會寄的訊息各發一遍」。
//
// 為什麼要排程、不能一次全送：Discord DM 端點的速率限制大約是每 5 秒 5 則／每個頻道，
// 超過就 429（serenity 會自動等待，但一整串 15 則會把 bot 卡住數十秒，期間真正的點名
// 通知也被排在後面）。因此這裡回應「已排程 N 則」後在背景**逐則**送，每則之間固定
// 等 TEST_ALL_GAP 1.5 秒（比 discord.rs 的 350 ms 全域節流再保守 4 倍），整批約 20 秒，
// 遠低於 5 則／5 秒的門檻。整批同時只允許一個在跑（全域鎖），節流 key 另計
// （test_all 每帳號每分鐘 3 次上限與其他測試端點一致，但實際上一批就要 20 秒）。
//
// 走 messenger.push_to_account_or_admin（與真實通知同一條路），所以：
// - 使用者在通知頁關掉的類別不會送（可藉此驗證開關真的有效）；
// - 若同時啟用 LINE，也會一併收到 LINE 版本（同一份 OutboundMessage）；
// - Dashboard 副本（send_dashboard_copy）照常，rollcall_id 用 9_990_000+n 避免和真點名撞去重。
// 所有測試訊息標題／內容含「測試」字樣，不含表情符號。
// ───────────────────────────────────────────────────────────────────────────────

/// 兩則測試訊息間隔。
const TEST_ALL_GAP: Duration = Duration::from_millis(1500);

static TEST_ALL_RUNNING: once_cell::sync::Lazy<tokio::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(()));

/// 依序建構所有會由系統主動寄出的 Discord 訊息（不含指令回覆 Status／控制訊息）。
fn build_test_all_messages(
    account_id: &str,
    user_name: &str,
    base_url: &str,
) -> Vec<(&'static str, crate::adapters::events::OutboundMessage)> {
    use crate::adapters::events::*;
    let base = base_url.trim().trim_end_matches('/');
    let url = |path: &str| {
        if base.is_empty() {
            String::new()
        } else {
            format!("{base}{path}")
        }
    };
    let now = chrono::Local::now();
    let course = "測試課程（非真實）";
    let teacher = "測試教師";
    let mut out: Vec<(&'static str, OutboundMessage)> = Vec::new();
    out.push((
        "system_started",
        OutboundMessage::SystemStarted(SystemStartedEvent {
            account: account_id.to_string(),
            user_name: user_name.to_string(),
            poll_interval_secs: 10,
            adapter_name: "discord（測試）".to_string(),
        }),
    ));
    out.push((
        "rollcall_detected",
        OutboundMessage::RollcallDetected(RollcallEvent {
            rollcall_id: 9_990_001,
            account: account_id.to_string(),
            course_name: course.to_string(),
            teacher_name: teacher.to_string(),
            attendance_type: "數字簽到（測試）".to_string(),
        }),
    ));
    out.push((
        "number_code",
        OutboundMessage::NumberCodeRequested(NumberCodeRequest {
            rollcall_id: 9_990_002,
            account: account_id.to_string(),
            course_name: course.to_string(),
            teacher_name: teacher.to_string(),
            input_url: url("/admin/dashboard#rollcall"),
            alt_input_url: String::new(),
            timeout_secs: 300,
        }),
    ));
    // QrCodeRequested 的 scan_url 會被做成連結按鈕，Discord 要求必須是合法 URL；
    // 沒設定 public_base_url 時退回 dashboard 網域說明頁避免 400。
    let scan_url = if base.is_empty() {
        "https://dashboard.jasoncloudserver.me/scanner".to_string()
    } else {
        url("/scanner")
    };
    out.push((
        "qr_code",
        OutboundMessage::QrCodeRequested(QrCodeRequest {
            rollcall_id: 9_990_003,
            account: account_id.to_string(),
            course_name: course.to_string(),
            teacher_name: teacher.to_string(),
            scan_url,
            alt_scan_url: String::new(),
            timeout_secs: 300,
        }),
    ));
    out.push((
        "number_code",
        OutboundMessage::RollcallInputReminder(RollcallInputReminderEvent {
            rollcall_id: 9_990_002,
            account: account_id.to_string(),
            course_name: course.to_string(),
            attendance_type: "數字簽到（測試）".to_string(),
            input_url: url("/admin/dashboard#rollcall"),
            waited_secs: 60,
            remaining_secs: 240,
        }),
    ));
    out.push((
        "rollcall_finished",
        OutboundMessage::RollcallFinished(RollcallResultEvent {
            rollcall_id: 9_990_001,
            account: account_id.to_string(),
            course_name: course.to_string(),
            attendance_type: "數字簽到（測試）".to_string(),
            success: true,
            result: "測試：簽到成功（此為測試訊息，非真實點名）".to_string(),
            elapsed_ms: 1234,
        }),
    ));
    out.push((
        "homework_deadline",
        OutboundMessage::HomeworkDeadline(HomeworkDeadlineEvent {
            account: account_id.to_string(),
            course_name: course.to_string(),
            title: "測試作業（非真實）".to_string(),
            stage_badge: "最後 1 小時".to_string(),
            deadline: (now + chrono::Duration::minutes(45)).format("%Y-%m-%d %H:%M").to_string(),
            time_remaining: "45 分鐘".to_string(),
        }),
    ));
    out.push((
        "bulletin",
        OutboundMessage::Bulletin(BulletinEvent {
            account: account_id.to_string(),
            course_name: course.to_string(),
            title: "測試公告（非真實）".to_string(),
            summary: "這是一則測試公告摘要，用來確認公告通知的排版與送達。".to_string(),
        }),
    ));
    out.push((
        "other",
        OutboundMessage::ClassReminder(ClassReminderEvent {
            account: account_id.to_string(),
            course_name: course.to_string(),
            time_text: "08:10 - 09:00（第 1 節，測試）".to_string(),
            classroom: "測試教室".to_string(),
            teacher: teacher.to_string(),
            minutes_until: 15,
            configured_minutes: 15,
        }),
    ));
    out.push((
        "other",
        OutboundMessage::TimetableChanged(TimetableChangedEvent {
            account: account_id.to_string(),
            added: vec!["週一 第 1 節 測試課程（非真實）".to_string()],
            added_total: 1,
            removed: vec!["週五 第 8 節 測試課程（已移除，非真實）".to_string()],
            removed_total: 1,
        }),
    ));
    out.push((
        "other",
        OutboundMessage::MonitorEvent(MonitorEventNotice {
            kind: MonitorEventKind::ReauthSucceeded,
            account: account_id.to_string(),
            error: String::new(),
            diagnosis: None,
        }),
    ));
    out.push((
        "other",
        OutboundMessage::MonitorEvent(MonitorEventNotice {
            kind: MonitorEventKind::AutoReauthFailed,
            account: account_id.to_string(),
            error: "測試：登入失敗（模擬，非真實錯誤）".to_string(),
            diagnosis: Some(crate::monitor_alerts::diagnose(
                "測試：登入失敗（模擬，非真實錯誤）",
                true,
            )),
        }),
    ));
    out.push((
        "other",
        OutboundMessage::AdminAlert(AdminAlertNotice {
            account: account_id.to_string(),
            grade: "警告".to_string(),
            is_recovery: false,
            title: "測試：管理員警示（非真實）".to_string(),
            body: "這是一則測試用的管理員警示，用來確認排版與送達。".to_string(),
        }),
    ));
    out.push((
        "other",
        OutboundMessage::PushSubscriptionExpired(PushSubscriptionExpiredEvent {
            account: account_id.to_string(),
            removed_devices: 1,
            remaining_devices: 1,
            reason: "測試：HTTP 410（模擬）".to_string(),
            dashboard_url: url("/admin/push-status"),
        }),
    ));
    out.push((
        "security_alert",
        OutboundMessage::SecurityAlert(SecurityAlertEvent {
            username: account_id.to_string(),
            ip: "203.0.113.10".to_string(),
            user_failures: 3,
            ip_failures: 3,
            timestamp: now.format("%Y-%m-%d %H:%M:%S").to_string(),
            ip_is_blacklisted: false,
            account_id: account_id.to_string(),
            allow_owner_unblock: false,
            login_locked: false,
        }),
    ));
    out.push((
        "other",
        OutboundMessage::Text(
            "測試結束：以上為 Discord 所有通知類型的測試訊息，皆非真實事件。".to_string(),
        ),
    ));
    out
}

/// POST /admin/api/notifications/test-all
/// 只能對自己的帳號（管理員亦然：避免拿別人的 DM 當測試目標）。
pub(super) async fn admin_notification_test_all_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    let account_id = session.account_id.clone();
    if let Err(response) = check_test_notification_rate(&state, "test_all", &account_id).await {
        return response;
    }
    let Some(messenger) = state.messenger.as_ref().map(Arc::clone) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "伺服器未啟用任何 Bot（Discord／LINE），無法發送測試" })),
        )
            .into_response();
    };
    let Ok(Some(account)) = state.accounts_db.get(&account_id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "找不到帳號資料" })),
        )
            .into_response();
    };
    if account.discord_user_id.trim().is_empty() && account.line_user_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "此帳號尚未綁定 Discord 或 LINE，請先在上方儲存綁定" })),
        )
            .into_response();
    }
    if TEST_ALL_RUNNING.try_lock().is_err() {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "已有一批測試訊息正在發送中，請等它送完（約 25 秒）再試" })),
        )
            .into_response();
    }

    let base_url = state
        .scanner
        .as_ref()
        .map(|s| s.public_base_url().to_string())
        .filter(|b| !b.trim().is_empty())
        .or_else(|| {
            crate::config::try_get_config()
                .map(|c| c.adapters.discord.public_base_url.trim().to_string())
                .filter(|b| !b.is_empty())
        })
        .unwrap_or_default();
    let user_name = state
        .account_by_id(&account_id)
        .map(|a| a.username.clone())
        .unwrap_or_else(|| account.username.clone());
    let messages = build_test_all_messages(&account_id, &user_name, &base_url);
    let scheduled = messages.len();
    let estimated_secs = (scheduled as u64 * TEST_ALL_GAP.as_millis() as u64) / 1000;
    let target = crate::adapters::events::AdapterAccountTarget::new(
        account_id.clone(),
        account.line_user_id.clone(),
        account.discord_user_id.clone(),
    );
    crate::logs::record_audit(
        "web",
        &account_id,
        "notification_test_all",
        Some(account_id.as_str()),
        Some(&format!("scheduled={scheduled}")),
        "Web 通知頁：發送所有 Discord 通知類型測試",
    );
    info!(account = %account_id, scheduled, "NOTIFY_TEST_ALL_V1: 排程測試訊息");

    let task_account = account_id.clone();
    tokio::spawn(async move {
        // 整批期間持有鎖：第二個人按下去會拿到 409，而不是把兩批交錯塞進 Discord。
        let _guard = TEST_ALL_RUNNING.lock().await;
        let mut sent = 0usize;
        let mut failed = 0usize;
        for (index, (key, message)) in messages.into_iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(TEST_ALL_GAP).await;
            }
            match messenger.push_to_account_or_admin(&target, &message).await {
                Ok(()) => sent += 1,
                Err(error) => {
                    failed += 1;
                    warn!(account = %task_account, key, index, %error, "NOTIFY_TEST_ALL_V1: 測試訊息發送失敗");
                }
            }
        }
        info!(account = %task_account, sent, failed, "NOTIFY_TEST_ALL_V1: 測試訊息批次完成");
    });

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "scheduled": scheduled,
            "gap_ms": TEST_ALL_GAP.as_millis() as u64,
            "estimated_secs": estimated_secs,
            "account_id": account_id,
            "message": format!(
                "已排程 {scheduled} 則測試訊息，為避開 Discord 速率限制會每 {:.1} 秒送一則，約 {estimated_secs} 秒內送完；你在通知頁關閉的類別不會送出。",
                TEST_ALL_GAP.as_secs_f64()
            ),
        })),
    )
        .into_response()
}
