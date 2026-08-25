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
    QrCodeRequest, RollcallEvent, RollcallResultEvent,
};
use crate::adapters::requests::{
    NumberCodeInput, NumberCodeReceiver, NumberCodeSender, QrCodeReceiver, QrCodeSender,
};
use crate::adapters::scanner::QrScannerRegistry;
use crate::api::{
    is_auth_error,
    rollcall::{AttendanceType, Rollcall},
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
    skip(api, config, messenger, number_rx, qr_rx, qr_tx, scanner_registry),
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
        AttendanceType::Radar => handle_radar_rollcall(Arc::clone(&api), &rollcall, config).await,
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

    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_error: Option<String> = None;
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

        let input = match wait_for_number_input(rx.clone(), deadline.saturating_duration_since(now))
            .await
        {
            Ok(input) => input,
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
                    code.clone(),
                    config,
                    number_registered
                        .as_ref()
                        .map(|(registry, _)| Arc::clone(registry)),
                    format!("手動數字代碼 `{code}` 簽到成功"),
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
                match api.get_number_rollcall_code(rollcall.rollcall_id).await {
                    Ok(lookup) => {
                        if let Some(code) = lookup.code {
                            info!(
                                rollcall_id = rollcall.rollcall_id,
                                code = %code,
                                source = %lookup.source,
                                "使用者選擇自動讀取，已取得數字點名碼"
                            );
                            let result = submit_number_code(
                                Arc::clone(&api),
                                rollcall.rollcall_id,
                                code.clone(),
                                config,
                                number_registered
                                    .as_ref()
                                    .map(|(registry, _)| Arc::clone(registry)),
                                format!("自動取得數字代碼 `{code}` 簽到成功"),
                            )
                            .await;

                            match result {
                                RollcallResult::Success { .. }
                                | RollcallResult::FatalError { .. } => return result,
                                RollcallResult::Failed { reason } => {
                                    warn!(code = %code, reason = %reason, "自動讀取數字碼簽到失敗，繼續等待下一次輸入");
                                    last_error = Some(reason);
                                }
                                other => return other,
                            }
                            continue;
                        }
                        warn!(
                            rollcall_id = rollcall.rollcall_id,
                            status = %lookup.status,
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
                    Err(e) => {
                        let reason = e.to_string();
                        if is_auth_error(&reason) {
                            return RollcallResult::FatalError { reason };
                        }
                        warn!(
                            rollcall_id = rollcall.rollcall_id,
                            error = %reason,
                            "自動讀取數字點名碼失敗，繼續等待手動輸入"
                        );
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
    code: String,
    config: &AccountConfig,
    scanner_registry: Option<Arc<QrScannerRegistry>>,
    success_detail: String,
) -> RollcallResult {
    wait_group_submit_turn(
        &config.provider,
        rollcall_id,
        GroupSubmitKind::Number,
        &config.id,
    )
    .await;

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
async fn handle_radar_rollcall(
    api: Arc<ApiClient>,
    rollcall: &Rollcall,
    config: &AccountConfig,
) -> RollcallResult {
    info!(
        rollcall_id = rollcall.rollcall_id,
        default_coords = config.provider_config.radar.default_coords.len(),
        "開始雷達簽到"
    );

    let result =
        attempt_radar_rollcall(api, rollcall.rollcall_id, &config.provider_config.radar).await;

    match result {
        RadarResult::Success { coord } => RollcallResult::Success {
            detail: format!("座標 ({:.6}, {:.6})", coord.latitude, coord.longitude),
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

    // 發送 Line 通知，要求使用者提供 QR code
    if let Some(bot) = messenger {
        let alt_scan_url =
            crate::adapters::scanner::alternate_public_url(&scan_url).unwrap_or_default();
        let msg = OutboundMessage::QrCodeRequested(QrCodeRequest {
            rollcall_id: rollcall.rollcall_id,
            account: account_label.to_string(),
            course_name: rollcall.course_title.clone(),
            teacher_name: rollcall.created_by_name.clone(),
            scan_url,
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

    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_error: Option<String> = None;

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

        let remaining = deadline.saturating_duration_since(now);
        let qr_input = {
            let mut rx_guard = rx.lock().await;
            match tokio::time::timeout(remaining, rx_guard.recv()).await {
                Ok(Some(input)) => {
                    info!(input_len = input.len(), "收到 QR code 輸入，嘗試簽到");
                    input
                }
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

        // 執行 QR Code 簽到。scanner 會持續掃描並送出更新後的 QR payload；失敗時繼續等下一筆。
        wait_group_submit_turn(
            &config.provider,
            rollcall.rollcall_id,
            GroupSubmitKind::QrCode,
            &config.id,
        )
        .await;
        let result =
            attempt_qrcode_rollcall(Arc::clone(&api), rollcall.rollcall_id, &qr_input, false).await;

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
        assert!(is_auth_error("403 Forbidden"));
        assert!(is_auth_error("session cookie invalid"));
        assert!(is_auth_error("需要重新登錄"));
        assert!(is_auth_error("認證失敗"));
    }

    #[test]
    fn test_is_auth_error_false() {
        assert!(!is_auth_error("Network timeout"));
        assert!(!is_auth_error("距離不足 42m"));
        assert!(!is_auth_error("Code 0042 incorrect"));
        assert!(!is_auth_error("JSON parse error"));
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
