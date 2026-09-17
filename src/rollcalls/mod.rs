//! 簽到主邏輯模組
//!
//! 作為各種簽到類型的調度中心：
//! - 接收 rollcall 列表
//! - 根據 `is_number` / `is_radar` 判斷類型
//! - 分派到對應的子模組（number / radar / qrcode）執行
//! - 統一處理結果與錯誤回報

pub mod group_submit;
pub mod number;
pub mod qrcode;
pub mod radar;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{debug, error, info, instrument, warn};

use crate::account::AccountConfig;
use crate::adapters::events::{
    AdapterAccountTarget, AdapterMessenger, MonitorStatus, NumberCodeRequest, OutboundMessage,
    QrCodeRequest, RollcallEvent, RollcallInputReminderEvent, RollcallResultEvent,
};
use crate::adapters::requests::{
    NumberCodeInput, NumberCodeReceiver, NumberCodeSender, QrCodeReceiver, QrCodeSender,
    RadarPointReceiver, RadarPointSender,
};
use crate::adapters::scanner::QrScannerRegistry;
use crate::api::{
    is_auth_error,
    rollcall::{AttendanceType, Rollcall, RollcallProgress, AUTO_SUBMIT_PERCENT},
    ApiClient,
};

use self::group_submit::{wait_group_submit_turn, GroupSubmitKind};
use self::qrcode::{attempt_qrcode_rollcall, QrCodeResult};
use self::radar::{attempt_radar_rollcall, RadarResult};

// ─── 簽到執行結果 ─────────────────────────────────────────────────────────────

/// 單次 rollcall 的最終執行結果
#[derive(Debug, Clone)]
pub struct RollcallOutcome {
    /// 對應的 rollcall
    pub rollcall: Rollcall,

    /// 簽到類型
    pub attendance_type: AttendanceType,

    /// 執行結果
    pub result: RollcallResult,

    /// 本次簽到花費的時間（毫秒）
    pub elapsed_ms: u64,
}

impl RollcallOutcome {
    pub fn is_success(&self) -> bool {
        self.result.is_success()
    }
}

impl std::fmt::Display for RollcallOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {} → {} ({}ms)",
            self.rollcall.rollcall_id, self.attendance_type, self.result, self.elapsed_ms,
        )
    }
}

/// 簽到結果枚舉
#[derive(Debug, Clone)]
pub enum RollcallResult {
    /// 簽到成功
    Success { detail: String },

    /// 簽到失敗（非致命，下次仍可重試）
    Failed { reason: String },

    /// 等待外部輸入（QR code 需要使用者掃碼）
    WaitingForInput { prompt: String },

    /// 跳過（已簽到、已過期，或不需處理）
    Skipped { reason: String },

    /// 致命錯誤（session 過期，需重新認證）
    FatalError { reason: String },
}

impl RollcallResult {
    pub fn is_success(&self) -> bool {
        matches!(self, RollcallResult::Success { .. })
    }

    pub fn is_fatal(&self) -> bool {
        matches!(self, RollcallResult::FatalError { .. })
    }

    pub fn is_skipped(&self) -> bool {
        matches!(self, RollcallResult::Skipped { .. })
    }
}

impl std::fmt::Display for RollcallResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RollcallResult::Success { detail } => write!(f, "✅ 成功：{detail}"),
            RollcallResult::Failed { reason } => write!(f, "❌ 失敗：{reason}"),
            RollcallResult::WaitingForInput { prompt } => write!(f, "⏳ 等待輸入：{prompt}"),
            RollcallResult::Skipped { reason } => write!(f, "⏭️  跳過：{reason}"),
            RollcallResult::FatalError { reason } => write!(f, "🔴 致命錯誤：{reason}"),
        }
    }
}

// ─── 主調度函式 ───────────────────────────────────────────────────────────────

/// 處理單個 rollcall 簽到
///
/// 根據 rollcall 的類型（number / radar / qrcode）分派到對應的處理函式。
///
/// # 參數
/// - `api`：已認證的 API 客戶端（Arc 包裹，可跨任務共享）
/// - `rollcall`：要處理的 rollcall
/// - `config`：應用程式設定
/// - `messenger`：Adapter 訊息發送介面（用於發送通知）
/// - `qr_rx`：QR code 輸入通道接收端（等待使用者透過 adapter 傳送 QR code）
#[instrument(
    skip(
        api,
        config,
        messenger,
        number_rx,
        qr_rx,
        qr_tx,
        radar_rx,
        radar_tx,
        scanner_registry
    ),
    fields(
        rollcall_id = rollcall.rollcall_id,
        course = %rollcall.course_title,
        r#type = %rollcall.attendance_type(),
    )
)]
pub async fn process_rollcall(
    api: Arc<ApiClient>,
    rollcall: Rollcall,
    config: &AccountConfig,
    account_label: &str,
    messenger: Option<&dyn AdapterMessenger>,
    adapter_target: Arc<Mutex<AdapterAccountTarget>>,
    status: Arc<Mutex<MonitorStatus>>,
    number_rx: Option<NumberCodeReceiver>,
    number_tx: Option<NumberCodeSender>,
    qr_rx: Option<QrCodeReceiver>,
    qr_tx: Option<QrCodeSender>,
    radar_rx: Option<RadarPointReceiver>,
    radar_tx: Option<RadarPointSender>,
    scanner_registry: Option<Arc<QrScannerRegistry>>,
) -> RollcallOutcome {
    let start = std::time::Instant::now();
    let attendance_type = rollcall.attendance_type();

    // ── 前置檢查 ──────────────────────────────────────────────────────────────

    // 已簽到，跳過
    if rollcall.is_attended() {
        debug!(rollcall_id = rollcall.rollcall_id, "已簽到，跳過");
        return RollcallOutcome {
            attendance_type,
            result: RollcallResult::Skipped {
                reason: "已簽到（on_call_fine）".to_string(),
            },
            elapsed_ms: start.elapsed().as_millis() as u64,
            rollcall,
        };
    }

    // 已過期，跳過
    if rollcall.is_expired {
        debug!(rollcall_id = rollcall.rollcall_id, "已過期，跳過");
        return RollcallOutcome {
            attendance_type,
            result: RollcallResult::Skipped {
                reason: "簽到已過期".to_string(),
            },
            elapsed_ms: start.elapsed().as_millis() as u64,
            rollcall,
        };
    }

    // 狀態不是 absent，跳過
    if !rollcall.needs_attendance() {
        debug!(
            rollcall_id = rollcall.rollcall_id,
            status = %rollcall.status,
            "狀態非 absent，跳過"
        );
        return RollcallOutcome {
            attendance_type,
            result: RollcallResult::Skipped {
                reason: format!("狀態為 `{}`，不需要簽到", rollcall.status),
            },
            elapsed_ms: start.elapsed().as_millis() as u64,
            rollcall,
        };
    }

    info!(
        rollcall_id = rollcall.rollcall_id,
        course = %rollcall.course_title,
        teacher = %rollcall.created_by_name,
        r#type = %attendance_type,
        "開始處理簽到"
    );

    // Web Push 綁定當前 account_label，伺服器不接受前端指定其他帳號，
    // 所以只會通知這次偵測所屬帳號的擁有者。
    // PUSH_DEEP_LINK_V1：數字／QR 點名改在對應 handler 註冊好輸入頁後才推播
    // （payload 會帶專屬輸入頁網址）；這裡只處理雷達／自助簽到。
    if !matches!(
        attendance_type,
        AttendanceType::Number | AttendanceType::QrCode
    ) {
        crate::web_push_notifications::notify_rollcall(
            account_label,
            rollcall.rollcall_id,
            &rollcall.course_title,
            &attendance_type.to_string(),
        );
    }

    // ── 發送開始通知 ──────────────────────────────────────────────────────────
    // 數字 / QR Code 會在對應 handler 先註冊到網頁狀態，再發出專用通知。
    // 這裡略過通用 RollcallDetected，避免 15 人時 Discord 通知節流卡住，導致網頁很久才看到所有人。
    if matches!(
        attendance_type,
        AttendanceType::Radar | AttendanceType::SelfRegistration
    ) {
        if let Some(bot) = messenger {
            let msg = OutboundMessage::RollcallDetected(RollcallEvent {
                rollcall_id: rollcall.rollcall_id,
                account: account_label.to_string(),
                course_name: rollcall.course_title.clone(),
                teacher_name: rollcall.created_by_name.clone(),
                attendance_type: attendance_type.to_string(),
            });
            let target = adapter_target.lock().await.clone();
            if let Err(e) = bot.push_to_account_or_admin(&target, &msg).await {
                warn!(error = %e, "發送開始通知失敗");
            }
        }
    }

    // ── 根據類型分派 ──────────────────────────────────────────────────────────
    let result = match attendance_type {
        AttendanceType::Number => {
            handle_number_rollcall(
                Arc::clone(&api),
                &rollcall,
                config,
                account_label,
                messenger,
                Arc::clone(&adapter_target),
                Arc::clone(&status),
                number_rx,
                number_tx,
                scanner_registry.clone(),
            )
            .await
        }
        AttendanceType::Radar => {
            handle_radar_rollcall(
                Arc::clone(&api),
                &rollcall,
                config,
                account_label,
                messenger,
                Arc::clone(&adapter_target),
                Arc::clone(&status),
                radar_rx,
                radar_tx,
                scanner_registry.clone(),
            )
            .await
        }
        AttendanceType::SelfRegistration => {
            handle_self_registration_rollcall(Arc::clone(&api), &rollcall).await
        }
        AttendanceType::QrCode => {
            handle_qrcode_rollcall(
                Arc::clone(&api),
                &rollcall,
                config,
                account_label,
                messenger,
                Arc::clone(&adapter_target),
                Arc::clone(&status),
                qr_rx,
                qr_tx,
                scanner_registry,
            )
            .await
        }
    };

    // ROLLCALL_SUBMITTED_NOTICE_V1（fix32）：送出成功≠立即有結論——二次確認
    // 最多約 9 秒。先推一則「已送出、確認中」的即時通知，讓使用者知道點名
    // 已經送出、不是卡住；Web Push 與最終結果同 tag，結果會直接替換這則。
    if result.is_success() {
        if let Some(bot) = messenger {
            let target = adapter_target.lock().await.clone();
            let msg = OutboundMessage::Text(format!(
                "📨 {course}（{kind}）點名已送出，正在二次確認…",
                course = rollcall.course_title,
                kind = attendance_type
            ));
            if let Err(e) = bot.push_to_account_or_admin(&target, &msg).await {
                warn!(error = %e, "發送「點名已送出」即時通知失敗");
            }
        }
        crate::web_push_notifications::notify_rollcall_submitted(
            account_label,
            rollcall.rollcall_id,
            &rollcall.course_title,
            &attendance_type.to_string(),
        );
    }

    // ROLLCALL_REVERIFY_V1：簽到 API 回應成功≠真的簽到了。向 Tronclass 重查，
    // 確認該點名狀態真的脫離 absent 才標記為成功；明確仍 absent 則降級為
    // 失敗（狀態未變，monitor 下輪會自然重試）。
    let result = if result.is_success() {
        reverify_rollcall(&api, &rollcall, result).await
    } else {
        result
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;

    // ── 發送結果通知 ──────────────────────────────────────────────────────────
    if let Some(bot) = messenger {
        let msg = OutboundMessage::RollcallFinished(RollcallResultEvent {
            rollcall_id: rollcall.rollcall_id,
            account: account_label.to_string(),
            course_name: rollcall.course_title.clone(),
            attendance_type: attendance_type.to_string(),
            success: result.is_success(),
            result: result.to_string(),
            elapsed_ms,
        });
        let target = adapter_target.lock().await.clone();
        if let Err(e) = bot.push_to_account_or_admin(&target, &msg).await {
            warn!(error = %e, "發送 Line 結果通知失敗");
        }
    }
    // ROLLCALL_FINISH_PUSH_V1：點名結果同步推到手機 Web Push
    //（偏好鍵 rollcall_finished，可在通知設定頁關閉）。
    crate::web_push_notifications::notify_rollcall_finished(
        account_label,
        rollcall.rollcall_id,
        &rollcall.course_title,
        &attendance_type.to_string(),
        result.is_success(),
        &result.to_string(),
    );

    let outcome = RollcallOutcome {
        rollcall,
        attendance_type,
        result,
        elapsed_ms,
    };

    if outcome.is_success() {
        info!(outcome = %outcome, "簽到完成");
    } else {
        warn!(outcome = %outcome, "簽到未成功");
    }

    outcome
}

// ─── 簽到二次確認（ROLLCALL_REVERIFY_V1）────────────────────────────────────

/// 重查間隔：Tronclass 寫入狀態有延遲，太快查會誤判。
const REVERIFY_DELAY_SECS: u64 = 3;
/// 重查次數上限（共 3 次 × 3 秒 ≈ 9 秒內完成確認）。
const REVERIFY_ATTEMPTS: u32 = 3;

/// 單次重查的判定結果（純邏輯，可單元測試）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReverifyVerdict {
    /// 狀態已脫離 absent（on_call_fine、late 等）：真的簽到了。
    Confirmed { status: String },
    /// 仍在清單上且狀態仍為 absent：簽到沒有生效（過期與否都算沒點到）。
    StillAbsent { status: String },
    /// 清單已查無此點名：無法證實也無法否定，保守保留原判。
    Missing,
}

fn reverify_verdict(list: &[Rollcall], rollcall_id: u64) -> ReverifyVerdict {
    match list.iter().find(|r| r.rollcall_id == rollcall_id) {
        Some(current) => {
            if current.status == "absent" {
                ReverifyVerdict::StillAbsent { status: current.status.clone() }
            } else {
                ReverifyVerdict::Confirmed { status: current.status.clone() }
            }
        }
        None => ReverifyVerdict::Missing,
    }
}

/// 送出成功後的二次確認：重查 rollcall 清單直到狀態脫離 absent 或嘗試用盡。
/// - 確認通過 → Success（detail 加註「已二次確認」）
/// - 明確仍 absent → 降級為 Failed（monitor 下輪重試）
/// - 查無此點名／查詢連續失敗 → 保留原判（不誤降級）
async fn reverify_rollcall(
    api: &crate::api::ApiClient,
    rollcall: &Rollcall,
    result: RollcallResult,
) -> RollcallResult {
    for attempt in 1..=REVERIFY_ATTEMPTS {
        tokio::time::sleep(Duration::from_secs(REVERIFY_DELAY_SECS)).await;
        let fetched = api.get_rollcalls().await;
        match fetched {
            Ok(list) => match reverify_verdict(&list, rollcall.rollcall_id) {
                ReverifyVerdict::Confirmed { status } => {
                    info!(attempt, status = %status, "簽到二次確認通過");
                    if let RollcallResult::Success { detail } = result {
                        return RollcallResult::Success {
                            detail: format!("{detail}（已二次確認）"),
                        };
                    }
                    return result;
                }
                ReverifyVerdict::StillAbsent { status } => {
                    debug!(attempt, status = %status, "二次確認：狀態仍為 absent，稍後重查");
                }
                ReverifyVerdict::Missing => {
                    warn!(attempt, "二次確認：點名已不在清單中，保留原結果");
                    return result;
                }
            },
            Err(e) => {
                warn!(attempt, error = %e, "二次確認：查詢失敗，重試");
            }
        }
    }
    warn!("簽到二次確認未通過：送出後 Tronclass 狀態仍為未簽到，降級為失敗");
    RollcallResult::Failed {
        reason: format!("二次確認未通過：送出後狀態仍為未簽到（原始回應：{result}）"),
    }
}

// ─── 各類型處理函式 ───────────────────────────────────────────────────────────

/// 處理數字簽到（爆破已關閉，等待 adapter 手動輸入 4 位數字）
async fn handle_number_rollcall(
    api: Arc<ApiClient>,
    rollcall: &Rollcall,
    config: &AccountConfig,
    account_label: &str,
    messenger: Option<&dyn AdapterMessenger>,
    adapter_target: Arc<Mutex<AdapterAccountTarget>>,
    status: Arc<Mutex<MonitorStatus>>,
    number_rx: Option<NumberCodeReceiver>,
    number_tx: Option<NumberCodeSender>,
    scanner_registry: Option<Arc<QrScannerRegistry>>,
) -> RollcallResult {
    let timeout_secs = config.provider_config.qrcode.scan_timeout_secs;
    let timeout = Duration::from_secs(timeout_secs);

    info!(
        rollcall_id = rollcall.rollcall_id,
        timeout_secs = timeout_secs,
        "數字爆破已關閉；預設等待手動輸入，使用者可在介面選擇自動讀取"
    );

    let Some(rx) = number_rx else {
        return RollcallResult::Failed {
            reason: "數字爆破已關閉，但目前沒有啟用手動數字碼回傳通道".to_string(),
        };
    };

    let number_registered =
        if let (Some(registry), Some(tx)) = (scanner_registry.as_ref(), number_tx) {
            if registry.public_base_url().trim().is_empty() {
                None
            } else {
                match registry
                    .register_number_pending(
                        &config.provider,
                        rollcall.rollcall_id,
                        &config.id,
                        tx,
                        timeout,
                    )
                    .await
                {
                    Ok(link) => Some((Arc::clone(registry), link.input_url)),
                    Err(e) => {
                        warn!(error = %e, "註冊數字點名頁失敗，改用 Discord 指令輸入");
                        None
                    }
                }
            }
        } else {
            None
        };
    let input_url = number_registered
        .as_ref()
        .map(|(_, input_url)| input_url.clone())
        .unwrap_or_default();
    update_detected_url(&status, &input_url).await;
    // PUSH_DEEP_LINK_V1：推播直接帶輸入頁網址（未註冊成功則退回儀表板）。
    crate::web_push_notifications::notify_rollcall_with_url(
        account_label,
        rollcall.rollcall_id,
        &rollcall.course_title,
        &AttendanceType::Number.to_string(),
        Some(input_url.as_str()),
    );

    if let Some(bot) = messenger {
        let target = adapter_target.lock().await.clone();
        let msg = OutboundMessage::NumberCodeRequested(NumberCodeRequest {
            rollcall_id: rollcall.rollcall_id,
            account: account_label.to_string(),
            course_name: rollcall.course_title.clone(),
            teacher_name: rollcall.created_by_name.clone(),
            input_url: input_url.clone(),
            alt_input_url: crate::adapters::scanner::alternate_public_url(&input_url)
                .unwrap_or_default(),
            timeout_secs,
        });
        if let Err(e) = bot.push_to_account_or_admin(&target, &msg).await {
            warn!(error = %e, "發送數字點名碼請求通知失敗");
        }
    }

    let started_waiting = tokio::time::Instant::now();
    let deadline = started_waiting + timeout;
    // ROLLCALL_INPUT_REMINDER_V1：20 秒後若仍無任何輸入就催一次；逾時本身不到 20 秒則不催。
    let reminder_at = started_waiting + Duration::from_secs(INPUT_REMINDER_SECS);
    let mut reminder_sent = reminder_at >= deadline;
    let mut last_error: Option<String> = None;
    // AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：第一次檢查班上進度的時間點。
    let mut next_progress_check = started_waiting + Duration::from_secs(AUTO_SUBMIT_POLL_SECS);
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            if let Some((registry, _)) = number_registered.as_ref() {
                registry
                    .unregister_number_pending(&config.provider, rollcall.rollcall_id, &config.id)
                    .await;
            }
            return RollcallResult::Failed {
                reason: last_error
                    .unwrap_or_else(|| format!("數字點名碼輸入逾時（{}秒）", timeout.as_secs())),
            };
        }

        // ROLLCALL_INPUT_REMINDER_V1：第一段只等到催促時間點，醒來後判斷是否要催。
        let wait_until = if !reminder_sent && last_error.is_none() {
            reminder_at.max(now)
        } else {
            deadline
        };
        // AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：等待期間也要定期看班上進度，
        // 所以這一段最多只等到「下一次檢查時間」（不是 deadline）。
        let progress_at = (next_progress_check < wait_until).then_some(next_progress_check);
        let segment_end = progress_at.unwrap_or(wait_until);
        let input = match wait_for_number_input(rx.clone(), segment_end.saturating_duration_since(now))
            .await
        {
            Ok(input) => input,
            Err(_) if progress_at.is_some() => {
                next_progress_check =
                    tokio::time::Instant::now() + Duration::from_secs(AUTO_SUBMIT_POLL_SECS);
                if let Some(progress) = class_progress_over_threshold(
                    &api,
                    scanner_registry.as_ref(),
                    &config.provider,
                    rollcall.rollcall_id,
                    account_label,
                )
                .await
                {
                    if let Some(bot) = messenger {
                        let target = adapter_target.lock().await.clone();
                        let _ = bot
                            .push_to_account_or_admin(
                                &target,
                                &OutboundMessage::Text(format!(
                                    "🌐 班上已有 {}/{} 人點名成功（{}%，超過 {}%），系統自動讀取數字點名碼並送出。",
                                    progress.completed,
                                    progress.total,
                                    progress.percent_rounded(),
                                    AUTO_SUBMIT_PERCENT
                                )),
                            )
                            .await;
                    }
                    match auto_lookup_and_submit_number(
                        &api,
                        rollcall,
                        config,
                        number_registered
                            .as_ref()
                            .map(|(registry, _)| Arc::clone(registry)),
                    )
                    .await
                    {
                        Ok(result) => return result,
                        Err(AutoLookupFailure::NoCode { .. }) => {
                            last_error =
                                Some("班上已超過門檻，但自動讀不到數字碼".to_string());
                        }
                        Err(AutoLookupFailure::Error { reason }) => {
                            last_error = Some(reason);
                        }
                    }
                }
                continue;
            }
            Err(_) if wait_until < deadline && tokio::time::Instant::now() >= reminder_at => {
                reminder_sent = true;
                if last_error.is_none() {
                    let remaining = deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .as_secs();
                    send_input_reminder(
                        messenger,
                        &adapter_target,
                        account_label,
                        rollcall,
                        AttendanceType::Number,
                        &input_url,
                        INPUT_REMINDER_SECS,
                        remaining,
                    )
                    .await;
                }
                continue;
            }
            Err(reason) => {
                if let Some((registry, _)) = number_registered.as_ref() {
                    registry
                        .unregister_number_pending(
                            &config.provider,
                            rollcall.rollcall_id,
                            &config.id,
                        )
                        .await;
                }
                return RollcallResult::Failed {
                    reason: last_error.unwrap_or(reason),
                };
            }
        };

        match input {
            NumberCodeInput::Code(code) => {
                let result = submit_number_code(
                    Arc::clone(&api),
                    rollcall.rollcall_id,
                    NumberCodeSource::Manual(code.clone()),
                    config,
                    number_registered
                        .as_ref()
                        .map(|(registry, _)| Arc::clone(registry)),
                )
                .await;

                match result {
                    RollcallResult::Success { .. } | RollcallResult::FatalError { .. } => {
                        return result
                    }
                    RollcallResult::Failed { reason } => {
                        warn!(code = %code, reason = %reason, "手動數字碼簽到失敗，繼續等待下一次輸入");
                        last_error = Some(reason);
                    }
                    other => return other,
                }
            }
            NumberCodeInput::AutoLookup => {
                match auto_lookup_and_submit_number(
                    &api,
                    rollcall,
                    config,
                    number_registered
                        .as_ref()
                        .map(|(registry, _)| Arc::clone(registry)),
                )
                .await
                {
                    Ok(result) => return result,
                    Err(AutoLookupFailure::NoCode { status }) => {
                        warn!(
                            rollcall_id = rollcall.rollcall_id,
                            status = %status,
                            "自動讀取未取得 number_code，繼續等待手動輸入"
                        );
                        if let Some(bot) = messenger {
                            let target = adapter_target.lock().await.clone();
                            let _ = bot
                                .push_to_account_or_admin(
                                    &target,
                                    &OutboundMessage::Text(
                                        "⚠️ 自動讀取未取得數字點名碼，請改用 `/num code:1234` 或直接傳 4 位數字。".to_string(),
                                    ),
                                )
                                .await;
                        }
                    }
                    Err(AutoLookupFailure::Error { reason }) => {
                        warn!(
                            rollcall_id = rollcall.rollcall_id,
                            error = %reason,
                            "自動讀取數字點名碼失敗，繼續等待手動輸入"
                        );
                        last_error = Some(reason.clone());
                        if let Some(bot) = messenger {
                            let target = adapter_target.lock().await.clone();
                            let _ = bot
                                .push_to_account_or_admin(
                                    &target,
                                    &OutboundMessage::Text(format!(
                                        "⚠️ 自動讀取數字點名碼失敗：{reason}\n請改用 `/num code:1234` 或直接傳 4 位數字。"
                                    )),
                                )
                                .await;
                        }
                    }
                }
            }
        }
    }
}

async fn submit_number_code(
    api: Arc<ApiClient>,
    rollcall_id: u64,
    source: NumberCodeSource,
    config: &AccountConfig,
    scanner_registry: Option<Arc<QrScannerRegistry>>,
) -> RollcallResult {
    let is_auto = matches!(source, NumberCodeSource::Auto(_));
    let code = match source {
        NumberCodeSource::Manual(code) | NumberCodeSource::Auto(code) => code,
    };

    // 等到輪次再送出。fix69'：iClass 的數字碼整場不變，所以等待期間**不重抓**
    // （13 個帳號時那會是十幾次白打的請求）；真的被拒就照原流程重試／等下一次輸入。
    let _ = wait_group_submit_turn(
        &config.provider,
        rollcall_id,
        GroupSubmitKind::Number,
        &config.id,
        None,
    )
    .await;

    let success_detail = if is_auto {
        format!("自動取得數字代碼 `{code}` 簽到成功")
    } else {
        format!("手動數字代碼 `{code}` 簽到成功")
    };

    match api.answer_number_rollcall(rollcall_id, &code).await {
        Ok(crate::api::rollcall::AttendanceResult::Success) => {
            if let Some(registry) = scanner_registry.as_ref() {
                registry
                    .mark_number_completed(&config.provider, rollcall_id, &config.id)
                    .await;
            }
            RollcallResult::Success {
                detail: success_detail,
            }
        }
        Ok(crate::api::rollcall::AttendanceResult::Failed { reason })
        | Ok(crate::api::rollcall::AttendanceResult::TransientFailure { reason }) => {
            if let Some(registry) = scanner_registry.as_ref() {
                registry
                    .mark_number_failed(&config.provider, rollcall_id, &config.id, &reason)
                    .await;
            }
            RollcallResult::Failed { reason }
        }
        Ok(crate::api::rollcall::AttendanceResult::RadarTooFar { distance }) => {
            let reason = format!("非預期的回應（RadarTooFar: {distance:.2}m）");
            if let Some(registry) = scanner_registry.as_ref() {
                registry
                    .mark_number_failed(&config.provider, rollcall_id, &config.id, &reason)
                    .await;
            }
            RollcallResult::Failed { reason }
        }
        Err(e) => {
            let reason = e.to_string();
            if is_auth_error(&reason) {
                RollcallResult::FatalError { reason }
            } else {
                if let Some(registry) = scanner_registry.as_ref() {
                    registry
                        .mark_number_failed(&config.provider, rollcall_id, &config.id, &reason)
                        .await;
                }
                RollcallResult::Failed { reason }
            }
        }
    }
}

async fn wait_for_number_input(
    rx: NumberCodeReceiver,
    timeout: Duration,
) -> std::result::Result<NumberCodeInput, String> {
    let mut rx_guard = rx.lock().await;

    loop {
        match tokio::time::timeout(timeout, rx_guard.recv()).await {
            Ok(Some(NumberCodeInput::Code(code))) => {
                let code = code.trim().to_string();
                if is_valid_number_code(&code) {
                    info!(code = %code, "收到手動數字點名碼");
                    return Ok(NumberCodeInput::Code(code));
                }
                warn!(code = %code, "收到無效數字點名碼，必須是 4 位數字，繼續等待");
            }
            Ok(Some(NumberCodeInput::AutoLookup)) => {
                info!("收到自動讀取數字點名碼請求");
                return Ok(NumberCodeInput::AutoLookup);
            }
            Ok(None) => return Err("數字點名碼輸入通道已關閉".to_string()),
            Err(_) => return Err(format!("數字點名碼輸入逾時（{}秒）", timeout.as_secs())),
        }
    }
}

/// ROLLCALL_INPUT_REMINDER_V1：數字／QR 點名送出請求後，等這麼多秒仍沒有任何輸入就再催一次。
const INPUT_REMINDER_SECS: u64 = 20;
/// QR_FRESH_PAYLOAD_V1（fix68）：同一個 QR payload 的最短重送間隔。
/// 與掃碼頁的 `RESUBMIT_SAME_QR_MS`（3 秒）一致：同一個碼在 3 秒內不重複送，
/// 避免失敗後把佇列裡同一個過期 payload 連續打出去。
const QR_SAME_PAYLOAD_COOLDOWN: Duration = Duration::from_secs(3);
/// RELAY_DEADLINE_BUDGET_V1（fix69）：接力等待最多只用到「等待窗結束前」這麼多秒，
/// 留一點時間讓送出與回應真的跑完；超過就不再等（寧可提早送，也不要送出時視窗已關）。
const RELAY_DEADLINE_MARGIN: Duration = Duration::from_secs(3);
/// RELAY_DEADLINE_BUDGET_V1（fix69）：把本次流程的等待窗換算成接力等待的預算。
///
/// `deadline` 是 `handle_*_rollcall` 收到輸入後「最多等多久」的截止時間；
/// 扣掉 [`RELAY_DEADLINE_MARGIN`] 讓請求有時間送出去。剩餘時間不足時回傳 0
/// （＝不再等待、立刻送出）。
fn relay_budget(deadline: tokio::time::Instant) -> Option<Duration> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    Some(remaining.saturating_sub(RELAY_DEADLINE_MARGIN))
}

/// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：檢查班上進度的間隔（秒）。
/// 多帳號共用 registry 裡的同一份快取（見 `QrScannerRegistry::class_progress`），
/// 所以不管有幾個帳號，同一個點名最多就是每 10 秒讀一次。
const AUTO_SUBMIT_POLL_SECS: u64 = 10;

/// 讀一次班上進度。
///
/// 有 registry 時走共用快取（同一個點名每 [`AUTO_SUBMIT_POLL_SECS`] 秒最多真的讀一次），
/// 沒有 registry（測試路徑）時就直接讀一次 API。
async fn class_progress_snapshot(
    api: &ApiClient,
    registry: Option<&Arc<QrScannerRegistry>>,
    provider: &str,
    rollcall_id: u64,
) -> Option<RollcallProgress> {
    match registry {
        Some(registry) => registry.class_progress(api, provider, rollcall_id).await,
        None => api.get_rollcall_progress(rollcall_id).await.ok().flatten(),
    }
}

/// 讀一次班上進度，並回報「是否已超過門檻」。
///
/// 讀不到（欄位長得不一樣、只有自己那一筆、網路錯誤…）一律回 `None`：
/// 這時候行為與 fix72 之前完全相同（繼續等使用者輸入），不會亂觸發。
async fn class_progress_over_threshold(
    api: &ApiClient,
    registry: Option<&Arc<QrScannerRegistry>>,
    provider: &str,
    rollcall_id: u64,
    account_label: &str,
) -> Option<RollcallProgress> {
    match class_progress_snapshot(api, registry, provider, rollcall_id).await {
        Some(progress) if progress.passed(AUTO_SUBMIT_PERCENT) => {
            info!(
                rollcall_id = rollcall_id,
                account = %account_label,
                completed = progress.completed,
                total = progress.total,
                percent = progress.percent_rounded(),
                source = %progress.source,
                "班上點名進度超過門檻，改成自動送出"
            );
            Some(progress)
        }
        Some(progress) => {
            debug!(
                rollcall_id = rollcall_id,
                completed = progress.completed,
                total = progress.total,
                "班上點名進度還沒超過門檻，繼續等"
            );
            None
        }
        _ => None,
    }
}

/// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1：自動讀取為什麼沒成功（只用來決定發哪一則提示）。
enum AutoLookupFailure {
    /// payload 裡沒有數字碼
    NoCode { status: String },
    /// 讀取時發生錯誤（非認證問題）
    Error { reason: String },
}

/// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1：讀數字碼並立刻送出（自動讀取）。
///
/// 回傳 `Ok(result)`＝流程結束（成功／致命錯誤／其他要回傳的結果）；
/// `Err(_)`＝這一輪沒成，呼叫端照原樣繼續等使用者輸入。
///
/// 注意 fix69' 的守線：`submit_number_code` 內**不得**重抓數字碼（那是「送出前換碼」）。
/// 這裡是流程層「先讀一次、立刻送出」，與使用者按「自動讀取」走的是同一條路。
async fn auto_lookup_and_submit_number(
    api: &Arc<ApiClient>,
    rollcall: &Rollcall,
    config: &AccountConfig,
    scanner_registry: Option<Arc<QrScannerRegistry>>,
) -> std::result::Result<RollcallResult, AutoLookupFailure> {
    match api.get_number_rollcall_code(rollcall.rollcall_id).await {
        Ok(lookup) => match lookup.code {
            Some(code) => {
                info!(
                    rollcall_id = rollcall.rollcall_id,
                    code = %code,
                    source = %lookup.source,
                    "自動讀取數字點名碼成功，立刻送出"
                );
                let result = submit_number_code(
                    Arc::clone(api),
                    rollcall.rollcall_id,
                    NumberCodeSource::Auto(code.clone()),
                    config,
                    scanner_registry,
                )
                .await;
                match result {
                    RollcallResult::Failed { ref reason } => {
                        warn!(
                            code = %code,
                            reason = %reason,
                            "自動讀取數字碼簽到失敗，繼續等待下一次輸入"
                        );
                        Err(AutoLookupFailure::Error {
                            reason: reason.clone(),
                        })
                    }
                    other => Ok(other),
                }
            }
            None => {
                warn!(
                    rollcall_id = rollcall.rollcall_id,
                    status = %lookup.status,
                    "自動讀取未取得 number_code"
                );
                Err(AutoLookupFailure::NoCode {
                    status: lookup.status.clone(),
                })
            }
        },
        Err(e) => {
            let reason = e.to_string();
            if is_auth_error(&reason) {
                return Ok(RollcallResult::FatalError { reason });
            }
            warn!(
                rollcall_id = rollcall.rollcall_id,
                error = %reason,
                "自動讀取數字點名碼失敗"
            );
            Err(AutoLookupFailure::Error { reason })
        }
    }
}

/// 數字點名碼的來源：只用來區分完成訊息（「自動取得」／「手動」）。
/// fix69'：iClass 的數字碼整場不變，送出的碼一律照用、不重抓。
enum NumberCodeSource {
    /// 使用者輸入的碼。
    Manual(String),
    /// 自動讀取取得的碼。
    Auto(String),
}

/// ROLLCALL_INPUT_REMINDER_V1：對綁定使用者（或管理員）與該帳號的手機推播各催一次。
/// 每筆點名最多送一次；有任何輸入（即使簽到失敗）就不催。
#[allow(clippy::too_many_arguments)]
async fn send_input_reminder(
    messenger: Option<&dyn AdapterMessenger>,
    adapter_target: &Arc<Mutex<AdapterAccountTarget>>,
    account_label: &str,
    rollcall: &Rollcall,
    attendance_type: AttendanceType,
    input_url: &str,
    waited_secs: u64,
    remaining_secs: u64,
) {
    info!(
        rollcall_id = rollcall.rollcall_id,
        account = %account_label,
        waited_secs,
        remaining_secs,
        "等待輸入超過 {INPUT_REMINDER_SECS} 秒仍無回應，發送催促通知"
    );
    let attendance_label = attendance_type.to_string();
    crate::web_push_notifications::notify_rollcall_reminder(
        account_label,
        rollcall.rollcall_id,
        &rollcall.course_title,
        &attendance_label,
        Some(input_url),
        remaining_secs,
    );
    if let Some(bot) = messenger {
        let msg = OutboundMessage::RollcallInputReminder(RollcallInputReminderEvent {
            rollcall_id: rollcall.rollcall_id,
            account: account_label.to_string(),
            course_name: rollcall.course_title.clone(),
            attendance_type: attendance_label,
            input_url: input_url.to_string(),
            waited_secs,
            remaining_secs,
        });
        let target = adapter_target.lock().await.clone();
        if let Err(e) = bot.push_to_account_or_admin(&target, &msg).await {
            warn!(error = %e, "發送點名催促通知失敗");
        }
    }
}

async fn update_detected_url(status: &Arc<Mutex<MonitorStatus>>, url: &str) {
    if url.trim().is_empty() {
        return;
    }
    let mut status = status.lock().await;
    status.last_detected_url = Some(url.to_string());
}

fn is_valid_number_code(code: &str) -> bool {
    code.len() == 4 && code.chars().all(|c| c.is_ascii_digit())
}

/// 處理雷達簽到（地理位置）
#[allow(clippy::too_many_arguments)]
async fn handle_radar_rollcall(
    api: Arc<ApiClient>,
    rollcall: &Rollcall,
    config: &AccountConfig,
    account_label: &str,
    messenger: Option<&dyn AdapterMessenger>,
    adapter_target: Arc<Mutex<AdapterAccountTarget>>,
    status: Arc<Mutex<MonitorStatus>>,
    radar_rx: Option<RadarPointReceiver>,
    radar_tx: Option<RadarPointSender>,
    scanner_registry: Option<Arc<QrScannerRegistry>>,
) -> RollcallResult {
    info!(
        rollcall_id = rollcall.rollcall_id,
        default_coords = config.provider_config.radar.default_coords.len(),
        "開始雷達簽到"
    );

    // RADAR_PANEL_V1（fix70）：改讀「執行期有效設定」——面板（資料庫）的覆寫優先於設定檔，
    // 且會把面板上按按鈕取得的「我的位置」接在座標清單最後一起嘗試。
    // 取不到有效設定時（例如沒有啟動登記基準值的測試路徑）退回設定檔的值，行為與 fix69 前相同。
    let radar_config = match crate::radar_settings::effective(&config.provider).await {
        Some(effective) => effective,
        None => config.provider_config.radar.clone(),
    };
    if config.provider_config.radar.default_coords != radar_config.default_coords
        || config.provider_config.radar.accuracy != radar_config.accuracy
        || config.provider_config.radar.altitude != radar_config.altitude
    {
        debug!(
            rollcall_id = rollcall.rollcall_id,
            provider = %config.provider,
            coords = radar_config.default_coords.len(),
            accuracy = radar_config.accuracy,
            altitude = radar_config.altitude,
            "雷達設定採用了面板的值（與設定檔不同）"
        );
    }

    // RADAR_INPUT_PAGE_V1（fix71）：公告「這次點名也可以輸入座標」——
    // 流程與數字點名頁完全相同（註冊 → 通知帶網址 → 頁面輪詢狀態 → 送出座標 → 監控端取用）。
    // 注意：雷達不像數字點名需要「等」輸入（監控每一輪都會重試），所以這裡不阻塞，
    // 只負責把網址送到使用者手上並讓頁面看得到進度。
    let timeout_secs = config.provider_config.qrcode.scan_timeout_secs;
    let timeout = Duration::from_secs(timeout_secs);
    let radar_registered =
        if let (Some(registry), Some(tx)) = (scanner_registry.as_ref(), radar_tx) {
            if registry.public_base_url().trim().is_empty() {
                None
            } else {
                match registry
                    .register_radar_pending(
                        &config.provider,
                        rollcall.rollcall_id,
                        &config.id,
                        tx,
                        timeout,
                        &radar_config.default_coords,
                        &rollcall.course_title,
                    )
                    .await
                {
                    Ok(link) => Some(link.input_url),
                    Err(e) => {
                        warn!(error = %e, "註冊雷達點名頁失敗，改用設定檔的座標自動嘗試");
                        None
                    }
                }
            }
        } else {
            None
        };
    let input_url = radar_registered.clone().unwrap_or_default();
    update_detected_url(&status, &input_url).await;
    crate::web_push_notifications::notify_rollcall_with_url(
        account_label,
        rollcall.rollcall_id,
        &rollcall.course_title,
        &AttendanceType::Radar.to_string(),
        Some(input_url.as_str()),
    );
    if let Some(bot) = messenger {
        let target = adapter_target.lock().await.clone();
        let page = match radar_registered.as_deref() {
            Some(url) => format!("\n雷達點名頁（輸入座標）：{url}"),
            None => "（這次沒有可用的輸入頁，先用設定檔／面板的座標自動嘗試）".to_string(),
        };
        let msg = OutboundMessage::Text(format!(
            "🎯 需要雷達座標\n\n課程：{}\n帳號：{}\n教師：{}\n點名 ID：{}\n\n\
             不知道教室在哪、或設定的座標一直被回報距離不足時，\
             開這一頁輸入你的座標（或按「用我目前位置」）就會用那組座標簽到。{}",
            rollcall.course_title,
            account_label,
            rollcall.created_by_name,
            rollcall.rollcall_id,
            page
        ));
        if let Err(e) = bot.push_to_account_or_admin(&target, &msg).await {
            warn!(error = %e, "發送雷達點名頁通知失敗");
        }
    }

    // §70c：有效設定（面板覆寫優先）＋輸入頁座標通道一起交給雷達流程。
    let api_for_retry = Arc::clone(&api);
    let mut result = attempt_radar_rollcall(
        api,
        rollcall.rollcall_id,
        &config.provider,
        &config.id,
        &radar_config,
        radar_rx.clone(),
    )
    .await;

    // AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：雷達頁面會顯示「班上點名進度」，
    // 所以每一輪都讀一次（走共用快取，10 秒內不會重複讀），讀到就記在這場點名的頁面狀態上。
    let class_progress = class_progress_snapshot(
        &api_for_retry,
        scanner_registry.as_ref(),
        &config.provider,
        rollcall.rollcall_id,
    )
    .await;
    if let Some(registry) = scanner_registry.as_ref() {
        registry
            .set_radar_class_progress(
                &config.provider,
                rollcall.rollcall_id,
                &config.id,
                class_progress.clone(),
            )
            .await;
    }

    // 班上超過 15% 的人已成功、我們卻還失敗時，立刻用「最新的座標」再試一次
    // （輸入頁剛送來的座標會由 Phase 0 先取用），不必等下一輪輪詢——
    // 班上都在成功，就代表點名還開著、值得馬上再送一次。
    if !result.is_success() {
        if let Some(progress) = class_progress
            .clone()
            .filter(|progress| progress.passed(AUTO_SUBMIT_PERCENT))
        {
            info!(
                rollcall_id = rollcall.rollcall_id,
                completed = progress.completed,
                total = progress.total,
                "班上點名進度超過門檻，立刻用最新座標再試一次雷達簽到"
            );
            let retry = attempt_radar_rollcall(
                api_for_retry,
                rollcall.rollcall_id,
                &config.provider,
                &config.id,
                &radar_config,
                radar_rx,
            )
            .await;
            if retry.is_success() {
                result = retry;
            } else if let Some(bot) = messenger {
                // 班上都在成功、我們卻還失敗 → 給使用者一個明確的下一步：
                // 開頁面用「現在的位置」再送一次（教室座標若已用過，再送也不會過）。
                let last_distance = match &retry {
                    RadarResult::Failed { last_distance, .. } => last_distance
                        .map(|d| format!("，最後距離 {d:.2} 公尺"))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let tail = if input_url.is_empty() {
                    String::new()
                } else {
                    format!("\n🎯 打開頁面用目前位置再送一次：{input_url}")
                };
                let target = adapter_target.lock().await.clone();
                let _ = bot
                    .push_to_account_or_admin(
                        &target,
                        &OutboundMessage::Text(format!(
                            "🌐 班上已有 {}/{} 人點名成功（{}%，超過 {}%），但雷達簽到還沒過{last_distance}。{tail}",
                            progress.completed,
                            progress.total,
                            progress.percent_rounded(),
                            AUTO_SUBMIT_PERCENT
                        )),
                    )
                    .await;
            }
        }
    }

    // 輸入頁的狀態同步：成功 → 已點到；失敗 → 顯示原因（多半是「距離不足 X 公尺」）。
    if let Some(registry) = scanner_registry.as_ref() {
        match &result {
            RadarResult::Success { .. } | RadarResult::SuccessEmpty => {
                registry
                    .mark_radar_completed(&config.provider, rollcall.rollcall_id, &config.id)
                    .await;
            }
            RadarResult::Failed { .. } | RadarResult::Error(_) => {
                let reason = match &result {
                    RadarResult::Failed { last_distance, .. } => last_distance
                        .map(|d| format!("距離不足，最後距離 {d:.2} 公尺"))
                        .unwrap_or_else(|| "這一輪沒有可用的距離資訊".to_string()),
                    RadarResult::Error(e) => e.clone(),
                    RadarResult::Success { .. } | RadarResult::SuccessEmpty => String::new(),
                };
                registry
                    .mark_radar_failed(&config.provider, rollcall.rollcall_id, &config.id, &reason)
                    .await;
            }
        }
    }

    match result {
        RadarResult::Success { coord } => RollcallResult::Success {
            detail: format!("座標 ({:.6}, {:.6})", coord.latitude, coord.longitude),
        },
        RadarResult::SuccessEmpty => RollcallResult::Success {
            detail: "空答案（未帶座標，RADAR_EMPTY_ANSWER_V1）".to_string(),
        },
        RadarResult::Failed {
            last_distance,
            tried_coords,
        } => {
            let dist_msg = last_distance
                .map(|d| format!("，最後距離 {d:.2}m"))
                .unwrap_or_default();
            RollcallResult::Failed {
                reason: format!(
                    "所有座標均失敗（嘗試 {} 個座標{}）",
                    tried_coords.len(),
                    dist_msg
                ),
            }
        }
        RadarResult::Error(e) => {
            if is_auth_error(&e) {
                RollcallResult::FatalError { reason: e }
            } else {
                RollcallResult::Failed { reason: e }
            }
        }
    }
}

async fn handle_self_registration_rollcall(
    api: Arc<ApiClient>,
    rollcall: &Rollcall,
) -> RollcallResult {
    info!(rollcall_id = rollcall.rollcall_id, "開始自主報到");

    match api
        .answer_self_registration_rollcall(rollcall.rollcall_id)
        .await
    {
        Ok(crate::api::rollcall::AttendanceResult::Success) => RollcallResult::Success {
            detail: "自主報到成功".to_string(),
        },
        Ok(crate::api::rollcall::AttendanceResult::Failed { reason })
        | Ok(crate::api::rollcall::AttendanceResult::TransientFailure { reason }) => {
            RollcallResult::Failed { reason }
        }
        Ok(crate::api::rollcall::AttendanceResult::RadarTooFar { distance }) => {
            RollcallResult::Failed {
                reason: format!("非預期的回應（RadarTooFar: {distance:.2}m）"),
            }
        }
        Err(e) => {
            let reason = e.to_string();
            if is_auth_error(&reason) {
                RollcallResult::FatalError { reason }
            } else {
                RollcallResult::Failed { reason }
            }
        }
    }
}

/// 處理 QR Code 簽到
///
/// 流程：
/// 1. 透過 Line Bot 發送掃碼請求給該帳號綁定使用者（若未綁定則退回管理員）
/// 2. 等待使用者透過 Line Bot 回傳 QR code URL（或 p 參數）
/// 3. 解析並呼叫 API 完成簽到
/// 4. 若超時（`config.provider_config.qrcode.scan_timeout_secs`），返回失敗
async fn handle_qrcode_rollcall(
    api: Arc<ApiClient>,
    rollcall: &Rollcall,
    config: &AccountConfig,
    account_label: &str,
    messenger: Option<&dyn AdapterMessenger>,
    adapter_target: Arc<Mutex<AdapterAccountTarget>>,
    status: Arc<Mutex<MonitorStatus>>,
    qr_rx: Option<QrCodeReceiver>,
    qr_tx: Option<QrCodeSender>,
    scanner_registry: Option<Arc<QrScannerRegistry>>,
) -> RollcallResult {
    info!(rollcall_id = rollcall.rollcall_id, "開始 QR Code 簽到流程");

    // 構建掃碼頁面 URL（讓使用者點擊）
    let legacy_scan_url = format!(
        "{}?rollcall_id={}",
        config.provider_config.qrcode.scanner_base_url, rollcall.rollcall_id
    );
    let timeout = Duration::from_secs(config.provider_config.qrcode.scan_timeout_secs);
    let scanner_registered = if let (Some(registry), Some(tx)) = (scanner_registry.as_ref(), qr_tx)
    {
        if registry.public_base_url().trim().is_empty() {
            None
        } else {
            match registry
                .register_pending(
                    &config.provider,
                    rollcall.rollcall_id,
                    &config.id,
                    tx,
                    timeout,
                )
                .await
            {
                Ok(link) => Some((Arc::clone(registry), link.scan_url, link.already_submitted)),
                Err(e) => {
                    warn!(error = %e, "註冊 QR scanner 失敗，改用 legacy scanner URL");
                    None
                }
            }
        }
    } else {
        None
    };
    let scan_url = scanner_registered
        .as_ref()
        .map(|(_, scan_url, _)| scan_url.clone())
        .unwrap_or(legacy_scan_url);
    let scanner_already_submitted = scanner_registered
        .as_ref()
        .map(|(_, _, already_submitted)| *already_submitted)
        .unwrap_or(false);
    update_detected_url(&status, &scan_url).await;
    // PUSH_DEEP_LINK_V1：推播直接帶掃碼頁網址；已由別人代掃完成時不再吵使用者。
    if !scanner_already_submitted {
        crate::web_push_notifications::notify_rollcall_with_url(
            account_label,
            rollcall.rollcall_id,
            &rollcall.course_title,
            &AttendanceType::QrCode.to_string(),
            Some(scan_url.as_str()),
        );
    }

    // 發送 Line 通知，要求使用者提供 QR code
    if let Some(bot) = messenger {
        let alt_scan_url =
            crate::adapters::scanner::alternate_public_url(&scan_url).unwrap_or_default();
        let msg = OutboundMessage::QrCodeRequested(QrCodeRequest {
            rollcall_id: rollcall.rollcall_id,
            account: account_label.to_string(),
            course_name: rollcall.course_title.clone(),
            teacher_name: rollcall.created_by_name.clone(),
            // ROLLCALL_INPUT_REMINDER_V1：後面催促還要用，這裡改 clone。
            scan_url: scan_url.clone(),
            alt_scan_url,
            timeout_secs: config.provider_config.qrcode.scan_timeout_secs,
        });

        if !scanner_already_submitted {
            let target = adapter_target.lock().await.clone();
            if let Err(e) = bot.push_to_account_or_admin(&target, &msg).await {
                warn!(error = %e, "發送 QR Code 請求通知失敗");
            }
        }
    } else {
        // 沒有 Line Bot，只能記錄日誌
        warn!(
            rollcall_id = rollcall.rollcall_id,
            scan_url = %scan_url,
            "QR Code 簽到需要 Line Bot 支援，但 Line Bot 未啟用！"
        );
        if let Some((registry, _, _)) = scanner_registered.as_ref() {
            registry
                .unregister_pending(&config.provider, rollcall.rollcall_id, &config.id)
                .await;
        }
        return RollcallResult::Failed {
            reason: "QR Code 簽到需要 Line Bot，請在 config.toml 中啟用 line_bot".to_string(),
        };
    }

    // 等待 QR code 輸入（帶逾時）
    let Some(rx) = qr_rx else {
        error!("QR code 接收通道未初始化");
        if let Some((registry, _, _)) = scanner_registered.as_ref() {
            registry
                .unregister_pending(&config.provider, rollcall.rollcall_id, &config.id)
                .await;
        }
        return RollcallResult::Failed {
            reason: "此部署模式未啟用 QR Code 回傳通道".to_string(),
        };
    };

    info!(
        rollcall_id = rollcall.rollcall_id,
        timeout_secs = config.provider_config.qrcode.scan_timeout_secs,
        "等待 QR code 輸入..."
    );

    let started_waiting = tokio::time::Instant::now();
    let deadline = started_waiting + timeout;
    // ROLLCALL_INPUT_REMINDER_V1：20 秒後若仍沒有任何 QR 輸入就催一次。
    let reminder_at = started_waiting + Duration::from_secs(INPUT_REMINDER_SECS);
    let mut reminder_sent = reminder_at >= deadline || scanner_already_submitted;
    let mut last_error: Option<String> = None;
    // QR_FRESH_PAYLOAD_V1（fix68）：上一次實際送出的 payload 與時間（避免短時間重複送同一個碼）。
    let mut last_qr_attempt: Option<(String, tokio::time::Instant)> = None;
    // AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：班上進度檢查時間點／是否已催過。
    let mut next_progress_check = started_waiting + Duration::from_secs(AUTO_SUBMIT_POLL_SECS);
    let mut progress_nudged = false;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            warn!(
                rollcall_id = rollcall.rollcall_id,
                "QR code 等待逾時（{}秒）", config.provider_config.qrcode.scan_timeout_secs
            );
            if let Some((registry, _, _)) = scanner_registered.as_ref() {
                registry
                    .unregister_pending(&config.provider, rollcall.rollcall_id, &config.id)
                    .await;
            }
            return RollcallResult::Failed {
                reason: last_error.unwrap_or_else(|| {
                    format!(
                        "QR code 輸入逾時（{}秒）",
                        config.provider_config.qrcode.scan_timeout_secs
                    )
                }),
            };
        }

        // ROLLCALL_INPUT_REMINDER_V1：第一段只等到催促時間點。
        let wait_until = if !reminder_sent && last_error.is_none() {
            reminder_at.max(now)
        } else {
            deadline
        };
        // AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：這一段最多只等到「下一次班上進度檢查時間」。
        // QR 的 payload 只能靠掃碼取得（學校 API 沒有），所以過門檻時只催人、不假造。
        let progress_at = (next_progress_check < wait_until).then_some(next_progress_check);
        let segment_end = progress_at.unwrap_or(wait_until);
        let remaining = segment_end.saturating_duration_since(now);
        // None 代表「催促時間點到了但還沒有輸入」（rx_guard 已在 block 結束時釋放）。
        let qr_input: Option<String> = {
            let mut rx_guard = rx.lock().await;
            match tokio::time::timeout(remaining, rx_guard.recv()).await {
                Ok(Some(input)) => {
                    info!(input_len = input.len(), "收到 QR code 輸入，嘗試簽到");
                    Some(input)
                }
                Err(_) if progress_at.is_some() => {
                    next_progress_check =
                        tokio::time::Instant::now() + Duration::from_secs(AUTO_SUBMIT_POLL_SECS);
                    if !progress_nudged {
                        if let Some(progress) = class_progress_over_threshold(
                            &api,
                            scanner_registry.as_ref(),
                            &config.provider,
                            rollcall.rollcall_id,
                            account_label,
                        )
                        .await
                        {
                            progress_nudged = true;
                            warn!(
                                rollcall_id = rollcall.rollcall_id,
                                completed = progress.completed,
                                total = progress.total,
                                "班上已超過門檻，但 QR payload 只能靠掃碼取得，改為催使用者"
                            );
                            if let Some(bot) = messenger {
                                let target = adapter_target.lock().await.clone();
                                let _ = bot
                                    .push_to_account_or_admin(
                                        &target,
                                        &OutboundMessage::Text(format!(
                                            "🔥 班上已有 {}/{} 人點名成功（{}%），但 QR 內容只能靠掃碼取得——請立刻開掃碼頁：{scan_url}",
                                            progress.completed,
                                            progress.total,
                                            progress.percent_rounded()
                                        )),
                                    )
                                    .await;
                            }
                        }
                    }
                    continue;
                }
                Err(_) if wait_until < deadline => None,
                Ok(None) => {
                    if let Some((registry, _, _)) = scanner_registered.as_ref() {
                        registry
                            .unregister_pending(&config.provider, rollcall.rollcall_id, &config.id)
                            .await;
                    }
                    return RollcallResult::FatalError {
                        reason: "QR code 輸入通道已關閉".to_string(),
                    };
                }
                Err(_) => continue,
            }
        };
        let Some(qr_input) = qr_input else {
            reminder_sent = true;
            if last_error.is_none() {
                let remaining_secs = deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_secs();
                send_input_reminder(
                    messenger,
                    &adapter_target,
                    account_label,
                    rollcall,
                    AttendanceType::QrCode,
                    &scan_url,
                    INPUT_REMINDER_SECS,
                    remaining_secs,
                )
                .await;
            }
            continue;
        };

        // 執行 QR Code 簽到。scanner 會持續掃描並送出更新後的 QR payload；失敗時繼續等下一筆。
        // RELAY_DEADLINE_BUDGET_V1（fix69）：等待最多只用到「等待窗結束前幾秒」，
        // 避免等太久才送出、結果學校的點名視窗已經關了。數字點名不需要這個上限
        // （fix69'：數字碼整場不變，等久一點沒有壞處），所以只有 QR 帶預算。
        let _ = wait_group_submit_turn(
            &config.provider,
            rollcall.rollcall_id,
            GroupSubmitKind::QrCode,
            &config.id,
            relay_budget(deadline),
        )
        .await;

        // QR_FRESH_PAYLOAD_V1（fix68）：輪到自己之後、真正送出之前，先把佇列裡**最新**的
        // QR payload 取出來。等待期間課堂 QR 可能已經輪換，抱著舊 payload 送出只是白跑一趟。
        let mut fresh_input = qr_input;
        let mut refreshed = 0usize;
        {
            let mut rx_guard = rx.lock().await;
            while let Ok(newer) = rx_guard.try_recv() {
                fresh_input = newer;
                refreshed += 1;
            }
        }
        if refreshed > 0 {
            info!(
                rollcall_id = rollcall.rollcall_id,
                skipped_older = refreshed,
                "送出前改用較新的 QR payload（略過佇列中較舊的 {refreshed} 筆）"
            );
        }

        // 同一個 payload 短時間內不重複送出（與掃碼頁 3 秒規則一致）。
        let submit_at = tokio::time::Instant::now();
        if let Some((last_payload, last_at)) = last_qr_attempt.as_ref() {
            if last_payload == &fresh_input
                && submit_at.saturating_duration_since(*last_at) < QR_SAME_PAYLOAD_COOLDOWN
            {
                debug!(
                    rollcall_id = rollcall.rollcall_id,
                    "同一個 QR payload 冷卻中，等下一筆輸入"
                );
                continue;
            }
        }
        last_qr_attempt = Some((fresh_input.clone(), submit_at));

        let result =
            attempt_qrcode_rollcall(Arc::clone(&api), rollcall.rollcall_id, &fresh_input, false)
                .await;

        match result {
            QrCodeResult::Success { data } => {
                if let Some((registry, _, _)) = scanner_registered.as_ref() {
                    registry
                        .mark_completed(&config.provider, rollcall.rollcall_id, &config.id)
                        .await;
                }
                return RollcallResult::Success {
                    detail: format!("QR code data 簽到成功（data 長度：{}）", data.len()),
                };
            }
            QrCodeResult::Failed { reason } => {
                warn!(reason = %reason, "QR Code 簽到失敗，持續等待下一個 QR payload");
                last_error = Some(reason);
            }
            QrCodeResult::ParseError { reason } => {
                let reason = format!("QR code 解析失敗：{reason}");
                warn!(reason = %reason, "QR Code 解析失敗，持續等待下一個 QR payload");
                last_error = Some(reason);
            }
            QrCodeResult::Error(e) => {
                if let Some((registry, _, _)) = scanner_registered.as_ref() {
                    registry
                        .unregister_pending(&config.provider, rollcall.rollcall_id, &config.id)
                        .await;
                }
                if is_auth_error(&e) {
                    return RollcallResult::FatalError { reason: e };
                }
                last_error = Some(e);
            }
        }
    }
}

// ─── 批次處理 ─────────────────────────────────────────────────────────────────

/// 批次處理多個 rollcall
///
/// 對傳入的 rollcall 列表，依序（或並發）執行簽到。
/// 返回每個 rollcall 的執行結果。
///
/// # 注意
/// QR code 與數字點名需要等待使用者輸入，因此整體以串行模式執行。
/// 雷達簽到的內部嘗試在各自的子模組中處理。
pub async fn process_rollcall_batch(
    api: Arc<ApiClient>,
    rollcalls: Vec<Rollcall>,
    config: &AccountConfig,
    account_label: &str,
    messenger: Option<&dyn AdapterMessenger>,
    adapter_target: Arc<Mutex<AdapterAccountTarget>>,
    status: Arc<Mutex<MonitorStatus>>,
    number_rx: Option<NumberCodeReceiver>,
    number_tx: Option<NumberCodeSender>,
    qr_rx: Option<QrCodeReceiver>,
    qr_tx: Option<QrCodeSender>,
    radar_rx: Option<RadarPointReceiver>,
    radar_tx: Option<RadarPointSender>,
    scanner_registry: Option<Arc<QrScannerRegistry>>,
) -> Vec<RollcallOutcome> {
    let total = rollcalls.len();
    let mut outcomes = Vec::with_capacity(total);

    if total == 0 {
        debug!("沒有需要處理的 rollcall");
        return outcomes;
    }

    // 篩選出需要簽到的 rollcall
    let pending: Vec<Rollcall> = rollcalls
        .into_iter()
        .filter(|rc| rc.needs_attendance())
        .collect();

    if pending.is_empty() {
        debug!(
            total = total,
            "全部 rollcall 均不需要簽到（已簽到或已過期）"
        );
        return outcomes;
    }

    info!(
        pending = pending.len(),
        total = total,
        "開始批次處理 {} 個待簽到 rollcall",
        pending.len()
    );

    for (i, rollcall) in pending.into_iter().enumerate() {
        let rollcall_id = rollcall.rollcall_id;
        debug!(
            idx = i + 1,
            rollcall_id = rollcall_id,
            "處理第 {} 個 rollcall",
            i + 1
        );

        let outcome = process_rollcall(
            Arc::clone(&api),
            rollcall,
            config,
            account_label,
            messenger,
            Arc::clone(&adapter_target),
            Arc::clone(&status),
            number_rx.clone(),
            number_tx.clone(),
            qr_rx.clone(),
            qr_tx.clone(),
            radar_rx.clone(),
            radar_tx.clone(),
            scanner_registry.clone(),
        )
        .await;

        // 若發生致命錯誤（session 過期），立即停止批次
        if outcome.result.is_fatal() {
            error!(
                rollcall_id = rollcall_id,
                "批次處理遇到致命錯誤，停止處理剩餘 rollcall"
            );
            outcomes.push(outcome);
            break;
        }

        outcomes.push(outcome);
    }

    // 統計結果
    let success_count = outcomes.iter().filter(|o| o.is_success()).count();
    let failed_count = outcomes
        .iter()
        .filter(|o| matches!(o.result, RollcallResult::Failed { .. }))
        .count();
    let skipped_count = outcomes.iter().filter(|o| o.result.is_skipped()).count();

    info!(
        total = outcomes.len(),
        success = success_count,
        failed = failed_count,
        skipped = skipped_count,
        "批次處理完成"
    );

    outcomes
}

// ─── 統計摘要 ─────────────────────────────────────────────────────────────────

/// 批次處理結果的統計摘要
#[derive(Debug, Clone, Default)]
pub struct BatchSummary {
    pub total: usize,
    pub success: usize,
    pub failed: usize,
    pub skipped: usize,
    pub fatal: usize,
    pub waiting: usize,
}

impl BatchSummary {
    pub fn from_outcomes(outcomes: &[RollcallOutcome]) -> Self {
        let mut s = BatchSummary::default();
        s.total = outcomes.len();
        for o in outcomes {
            match &o.result {
                RollcallResult::Success { .. } => s.success += 1,
                RollcallResult::Failed { .. } => s.failed += 1,
                RollcallResult::Skipped { .. } => s.skipped += 1,
                RollcallResult::FatalError { .. } => s.fatal += 1,
                RollcallResult::WaitingForInput { .. } => s.waiting += 1,
            }
        }
        s
    }

    pub fn has_fatal(&self) -> bool {
        self.fatal > 0
    }

    pub fn all_success(&self) -> bool {
        self.success == self.total
    }
}

impl std::fmt::Display for BatchSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "共 {} 個 | ✅ {} 成功 | ❌ {} 失敗 | ⏭️ {} 跳過 | 🔴 {} 致命",
            self.total, self.success, self.failed, self.skipped, self.fatal,
        )
    }
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rollcall::Rollcall;

    // ── 二次確認判定（ROLLCALL_REVERIFY_V1）──────────────────────────────────

    fn rc(id: u64, status: &str) -> Rollcall {
        Rollcall {
            rollcall_id: id,
            status: status.into(),
            ..Default::default()
        }
    }

    #[test]
    fn reverify_verdict_confirmed_on_non_absent() {
        let list = vec![rc(42, "on_call_fine"), rc(43, "late")];
        assert_eq!(
            reverify_verdict(&list, 42),
            ReverifyVerdict::Confirmed { status: "on_call_fine".into() }
        );
        assert_eq!(
            reverify_verdict(&list, 43),
            ReverifyVerdict::Confirmed { status: "late".into() }
        );
    }

    #[test]
    fn reverify_verdict_still_absent_even_if_expired() {
        // 過期且仍 absent＝來不及簽到，同樣算「沒點到」。
        let mut item = rc(42, "absent");
        item.is_expired = true;
        assert_eq!(
            reverify_verdict(&[item], 42),
            ReverifyVerdict::StillAbsent { status: "absent".into() }
        );
    }

    #[test]
    fn reverify_verdict_missing_when_not_in_list() {
        assert_eq!(reverify_verdict(&[], 42), ReverifyVerdict::Missing);
        assert_eq!(
            reverify_verdict(&[rc(99, "on_call_fine")], 42),
            ReverifyVerdict::Missing
        );
    }

    #[test]
    fn reverify_success_detail_gets_annotation() {
        // 確認通過時 detail 會加上「（已二次確認）」標記。
        let r = RollcallResult::Success { detail: "QR data 簽到成功".into() };
        let annotated = match r {
            RollcallResult::Success { detail } => RollcallResult::Success {
                detail: format!("{detail}（已二次確認）"),
            },
            other => other,
        };
        assert_eq!(
            annotated.to_string(),
            "✅ 成功：QR data 簽到成功（已二次確認）"
        );
    }

    // ── RollcallResult ────────────────────────────────────────────────────────

    #[test]
    fn test_rollcall_result_is_success() {
        assert!(RollcallResult::Success {
            detail: "ok".into()
        }
        .is_success());
        assert!(!RollcallResult::Failed {
            reason: "err".into()
        }
        .is_success());
        assert!(!RollcallResult::Skipped {
            reason: "skip".into()
        }
        .is_success());
        assert!(!RollcallResult::FatalError {
            reason: "fatal".into()
        }
        .is_success());
        assert!(!RollcallResult::WaitingForInput {
            prompt: "wait".into()
        }
        .is_success());
    }

    #[test]
    fn test_rollcall_result_is_fatal() {
        assert!(RollcallResult::FatalError {
            reason: "fatal".into()
        }
        .is_fatal());
        assert!(!RollcallResult::Success {
            detail: "ok".into()
        }
        .is_fatal());
        assert!(!RollcallResult::Failed {
            reason: "err".into()
        }
        .is_fatal());
    }

    #[test]
    fn test_rollcall_result_is_skipped() {
        assert!(RollcallResult::Skipped {
            reason: "skip".into()
        }
        .is_skipped());
        assert!(!RollcallResult::Success {
            detail: "ok".into()
        }
        .is_skipped());
    }

    #[test]
    fn test_rollcall_result_display() {
        let s = RollcallResult::Success {
            detail: "code=0042".into(),
        };
        assert!(s.to_string().contains("成功") && s.to_string().contains("0042"));

        let f = RollcallResult::Failed {
            reason: "距離不足".into(),
        };
        assert!(f.to_string().contains("失敗") && f.to_string().contains("距離不足"));

        let sk = RollcallResult::Skipped {
            reason: "已簽到".into(),
        };
        assert!(sk.to_string().contains("跳過") && sk.to_string().contains("已簽到"));

        let fe = RollcallResult::FatalError {
            reason: "session 過期".into(),
        };
        assert!(fe.to_string().contains("致命") && fe.to_string().contains("session"));

        let w = RollcallResult::WaitingForInput {
            prompt: "請掃碼".into(),
        };
        assert!(w.to_string().contains("等待") && w.to_string().contains("掃碼"));
    }

    // ── RollcallOutcome ───────────────────────────────────────────────────────

    #[test]
    fn test_rollcall_outcome_is_success() {
        let outcome = make_outcome(RollcallResult::Success {
            detail: "ok".into(),
        });
        assert!(outcome.is_success());
    }

    #[test]
    fn test_rollcall_outcome_display() {
        let outcome = make_outcome(RollcallResult::Success {
            detail: "ok".into(),
        });
        let s = outcome.to_string();
        assert!(s.contains("✅"));
        assert!(s.contains("ms"));
    }

    // ── BatchSummary ──────────────────────────────────────────────────────────

    #[test]
    fn test_batch_summary_from_outcomes() {
        let outcomes = vec![
            make_outcome(RollcallResult::Success {
                detail: "ok".into(),
            }),
            make_outcome(RollcallResult::Failed {
                reason: "err".into(),
            }),
            make_outcome(RollcallResult::Skipped {
                reason: "skip".into(),
            }),
            make_outcome(RollcallResult::FatalError {
                reason: "fatal".into(),
            }),
            make_outcome(RollcallResult::WaitingForInput {
                prompt: "wait".into(),
            }),
        ];

        let summary = BatchSummary::from_outcomes(&outcomes);
        assert_eq!(summary.total, 5);
        assert_eq!(summary.success, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.fatal, 1);
        assert_eq!(summary.waiting, 1);
    }

    #[test]
    fn test_batch_summary_has_fatal() {
        let outcomes = vec![make_outcome(RollcallResult::FatalError {
            reason: "fatal".into(),
        })];
        let summary = BatchSummary::from_outcomes(&outcomes);
        assert!(summary.has_fatal());
    }

    #[test]
    fn test_batch_summary_all_success() {
        let outcomes = vec![
            make_outcome(RollcallResult::Success {
                detail: "ok1".into(),
            }),
            make_outcome(RollcallResult::Success {
                detail: "ok2".into(),
            }),
        ];
        let summary = BatchSummary::from_outcomes(&outcomes);
        assert!(summary.all_success());
    }

    #[test]
    fn test_batch_summary_not_all_success() {
        let outcomes = vec![
            make_outcome(RollcallResult::Success {
                detail: "ok".into(),
            }),
            make_outcome(RollcallResult::Failed {
                reason: "err".into(),
            }),
        ];
        let summary = BatchSummary::from_outcomes(&outcomes);
        assert!(!summary.all_success());
    }

    #[test]
    fn test_batch_summary_display() {
        let outcomes: Vec<RollcallOutcome> = vec![];
        let summary = BatchSummary::from_outcomes(&outcomes);
        let s = summary.to_string();
        assert!(s.contains("共"));
        assert!(s.contains("成功"));
        assert!(s.contains("失敗"));
    }

    #[test]
    fn test_batch_summary_empty() {
        let summary = BatchSummary::from_outcomes(&[]);
        assert_eq!(summary.total, 0);
        assert!(!summary.has_fatal());
        assert!(summary.all_success()); // 0 == 0，視為全成功
    }

    // ── is_auth_error ─────────────────────────────────────────────────────────

    #[test]
    fn test_is_auth_error_true() {
        assert!(is_auth_error("Session expired"));
        assert!(is_auth_error("HTTP 401 Unauthorized"));
        assert!(is_auth_error("API 返回錯誤狀態 403：Forbidden"));
        assert!(is_auth_error("Session 已過期（HTTP 401/403），需要重新登錄"));
        assert!(is_auth_error("需要重新登錄"));
        assert!(is_auth_error("🔴 致命錯誤：session 過期"));
    }

    #[test]
    fn test_is_auth_error_false() {
        assert!(!is_auth_error("Network timeout"));
        assert!(!is_auth_error("距離不足 42m"));
        assert!(!is_auth_error("Code 0042 incorrect"));
        assert!(!is_auth_error("JSON parse error"));
        // AUTH_ERROR_NUMERIC_FP_V1：裸數字 / 裸登入字不再是認證錯誤
        assert!(!is_auth_error("403 Forbidden"));
        assert!(!is_auth_error("課程 40123 不存在"));
        assert!(!is_auth_error("認證失敗"));
        assert!(!is_auth_error("session cookie invalid"));
    }

    // ── QR code channel ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_qrcode_channel_send_recv() {
        use crate::adapters::requests::create_qrcode_channel;

        let (tx, rx) = create_qrcode_channel(1);
        tx.send("test_qr_data".to_string()).await.unwrap();

        let mut rx_guard = rx.lock().await;
        let received = rx_guard.recv().await;
        assert_eq!(received, Some("test_qr_data".to_string()));
    }

    #[tokio::test]
    async fn test_qrcode_channel_timeout() {
        use crate::adapters::requests::create_qrcode_channel;

        let (_tx, rx) = create_qrcode_channel(1);
        let mut rx_guard = rx.lock().await;
        let result = tokio::time::timeout(Duration::from_millis(50), rx_guard.recv()).await;
        assert!(result.is_err(), "應該逾時");
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn make_test_rollcall() -> Rollcall {
        Rollcall {
            rollcall_id: 1,
            course_title: "測試課程".to_string(),
            created_by_name: "測試教授".to_string(),
            department_name: "測試系".to_string(),
            is_expired: false,
            is_number: false,
            is_radar: false,
            status: "absent".to_string(),
            rollcall_status: "ongoing".to_string(),
            scored: false,
            rollcall_type_raw: String::new(),
            rollcall_type: String::new(),
            name: String::new(),
        }
    }

    fn make_outcome(result: RollcallResult) -> RollcallOutcome {
        RollcallOutcome {
            rollcall: make_test_rollcall(),
            attendance_type: AttendanceType::QrCode,
            result,
            elapsed_ms: 100,
        }
    }
}
