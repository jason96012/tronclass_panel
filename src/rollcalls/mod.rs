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
pub(crate) mod qr_watchdog;
pub mod radar;

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Mutex;
use tracing::{debug, error, info, instrument, warn};

use crate::account::AccountConfig;
use crate::adapters::events::{
    AdapterAccountTarget, AdapterMessenger, MonitorStatus, NumberCodeRequest, OutboundMessage,
    QrCodeRequest, RollcallEvent, RollcallInputReminderEvent, RollcallResultEvent,
};
use crate::adapters::requests::{
    NumberCodeInput, NumberCodeReceiver, NumberCodeSender, QrCodeReceiver, QrCodeSender,
    create_number_code_channel, create_qrcode_channel, create_radar_point_channel,
    register_rollcall_input, RadarPointReceiver, RadarPointSender, RollcallInputSender,
};
use crate::adapters::scanner::QrScannerRegistry;
use crate::api::{
    is_auth_error,
    rollcall::{AttendanceType, Rollcall, RollcallProgress, AUTO_SUBMIT_PERCENT},
    ApiClient,
};

use self::group_submit::{wait_group_submit_turn, GroupSubmitKind};
use self::qrcode::{attempt_qrcode_rollcall, QrCodeResult};
use self::qr_watchdog::{QrPhase, QR_MAX_RECOVERIES};
use self::radar::{attempt_radar_rollcall, radar_channel_has_point, RadarResult};

// ─── 簽到執行結果 ─────────────────────────────────────────────────────────────

// NOTIFY_LATENCY_GUARD_V1：下面「發送開始通知」那段 Discord 推播是在
// `process_rollcall`──也就是實際判讀／送出簽到──的流程正中間同步 `.await`。
// Discord 官方 API 偶爾會因為單一 bot 短時間內要推播給很多人（例如一堂課
// 十幾個帳號幾乎同時偵測到同一場雷達／自助簽到）觸發速率限制，serenity 的
// HTTP 層會自動等 `Retry-After` 到期再重送——如果卡住個幾秒，簽到視窗本來
// 就短的雷達／自助簽到，會被這通「通知還沒發完」的等待硬生生吃掉寶貴的
// 簽到時間。這裡設一個保底逾時：通知最多等這麼久，逾時就放棄那一次通知、
// 繼續往下走正式簽到流程——通知是錦上添花，簽到本身才是這個專案的核心任務，
// 優先序不能顛倒。
const DETECTED_NOTIFY_TIMEOUT: Duration = Duration::from_secs(3);

/// NOTIFY_LATENCY_GUARD_V1：本檔案所有「簽到流程中途插播」的 Discord／LINE
/// 通知統一經由這裡發送——原本每個呼叫點各自寫一次
/// `bot.push_to_account_or_admin(...).await`（有些甚至用 `let _ =` 整個吞掉
/// 錯誤），逾時／錯誤處理不統一，也完全沒有逾時保護。集中成一個函式：
/// ① 套用 [`DETECTED_NOTIFY_TIMEOUT`] 逾時保底；② 統一記錄錯誤／逾時 log
/// （`context_label` 只給 log 用，方便事後分辨是哪一種通知）；③ 呼叫端不再需要
/// 自己判斷要不要印警告，少了「有的分支忘記印、有的吞掉」這種不一致。
async fn send_rollcall_notice(
    bot: &dyn AdapterMessenger,
    target: &AdapterAccountTarget,
    msg: &OutboundMessage,
    context_label: &str,
) {
    match tokio::time::timeout(DETECTED_NOTIFY_TIMEOUT, bot.push_to_account_or_admin(target, msg))
        .await
    {
        Ok(Err(e)) => warn!(error = %e, "{}失敗", context_label),
        Err(_) => warn!(
            "{}逾時（{:?}），放棄這次通知、繼續處理簽到",
            context_label, DETECTED_NOTIFY_TIMEOUT
        ),
        Ok(Ok(())) => {}
    }
}

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

    /// ATT_STATS_AUTO_MANUAL_V1：這次是不是「大家都沒按按鈕，系統因為班上
    /// 超過 `AUTO_SUBMIT_PERCENT`（15%）人點名成功而自動送出」
    /// （`AUTO_SUBMIT_BY_CLASS_PROGRESS_V1`／fix72）。
    ///
    /// 只有「使用者完全沒有提供任何輸入（沒打代碼、沒按『自動讀取』／
    /// 『系統演算』）、純粹因為等待逾時 + 班上進度超過門檻，系統自己讀碼/
    /// 用座標送出」才算 `true`。使用者自己打代碼、或自己按下「自動讀取」
    /// （number.html）／「系統演算」（radar.html）按鈕，即使底層一樣是「自動
    /// 讀取代碼」或「用系統座標送出」，也算「手動」——因為是人觸發的。
    /// 目前只有數字點名與雷達點名有這個機制（QR Code／自主報到沒有，一律
    /// `false`）；跳過（已簽到／已過期／狀態非 absent）也一律 `false`。
    pub auto_submitted: bool,
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
            auto_submitted: false,
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
            auto_submitted: false,
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
            auto_submitted: false,
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
            send_rollcall_notice(bot, &target, &msg, "發送開始通知").await;
        }
    }

    // ── 根據類型分派 ──────────────────────────────────────────────────────────
    // ATT_STATS_AUTO_MANUAL_V1：只有數字／雷達點名有「15% 自動送出」機制，
    // 用這個 out 參數讓對應 handler 回報這次最終結果是不是純系統自動送出
    // （沒有任何人動手）；其餘類型不會去動它，維持預設 `false`（手動）。
    let mut auto_submitted = false;
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
                &mut auto_submitted,
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
                &mut auto_submitted,
            )
            .await
        }
        AttendanceType::SelfRegistration => {
            // SELF_REGISTRATION_RELAY_V1：要 provider／帳號 id 才能排隊，所以把 config 傳進去。
            handle_self_registration_rollcall(Arc::clone(&api), &rollcall, config).await
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
    // QR_RECOVERY_V1：QR 已在自己的輸入迴圈內完成確認／發送中通知。
    if result.is_success() && attendance_type != AttendanceType::QrCode {
        if let Some(bot) = messenger {
            let target = adapter_target.lock().await.clone();
            let msg = OutboundMessage::Text(format!(
                "📨 {course}（{kind}）點名已送出，正在二次確認…",
                course = rollcall.course_title,
                kind = attendance_type
            ));
            send_rollcall_notice(bot, &target, &msg, "發送「點名已送出」即時通知").await;
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
    let result = if result.is_success() && attendance_type != AttendanceType::QrCode {
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
        send_rollcall_notice(bot, &target, &msg, "發送 Line 結果通知").await;
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
        auto_submitted,
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

// QR_RECOVERY_V1：QR 必須在移出待點名名單之前確認；讀不到不能冒充已到。
const QR_NETWORK_TIMEOUT: Duration = Duration::from_secs(15);
const QR_PROGRESS_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, PartialEq, Eq)]
enum QrAttendanceVerification {
    Confirmed,
    Retry(String),
    Fatal(String),
}

fn qr_attendance_confirmed(list: &[Rollcall], rollcall_id: u64) -> bool {
    list.iter().any(|current| {
        current.rollcall_id == rollcall_id
            && (current.is_attended() || current.status == "late")
    })
}

// QR 的週期查詢與二次確認使用同一個成功判定；登入失效交回既有重登流程。
async fn check_qr_server_status(
    api: &ApiClient,
    rollcall_id: u64,
) -> std::result::Result<ServerRollcallStatus, String> {
    match tokio::time::timeout(QR_NETWORK_TIMEOUT, api.get_rollcalls()).await {
        // QR_STALL_WATCHDOG_V1：查無資料／未知狀態先保留等待，否則 worker
        // 會在監督器來得及觀察前退出，掃碼連結留下「未點名但收不到碼」。
        Ok(Ok(list)) => Ok(qr_watchdog_server_status(&list, rollcall_id)),
        Ok(Err(e)) => {
            let reason = e.to_string();
            if is_auth_error(&reason) {
                return Err(reason);
            }
            warn!(rollcall_id, error = %reason, "QR 學校狀態查詢失敗，維持等待");
            Ok(ServerRollcallStatus::UnknownError)
        }
        Err(_) => {
            warn!(rollcall_id, "QR 學校狀態查詢逾時，維持等待");
            Ok(ServerRollcallStatus::UnknownError)
        }
    }
}

async fn verify_qr_attendance(api: &ApiClient, rollcall_id: u64) -> QrAttendanceVerification {
    for attempt in 1..=REVERIFY_ATTEMPTS {
        tokio::time::sleep(Duration::from_secs(REVERIFY_DELAY_SECS)).await;
        match tokio::time::timeout(QR_NETWORK_TIMEOUT, api.get_rollcalls()).await {
            Ok(Ok(list)) if qr_attendance_confirmed(&list, rollcall_id) => {
                return QrAttendanceVerification::Confirmed;
            }
            Ok(Ok(_)) => debug!(rollcall_id, attempt, "QR 尚未取得學校確認，保留待點名狀態"),
            Ok(Err(e)) => {
                let reason = e.to_string();
                if is_auth_error(&reason) {
                    return QrAttendanceVerification::Fatal(reason);
                }
                warn!(rollcall_id, attempt, error = %reason, "QR 二次確認查詢失敗");
            }
            Err(_) => warn!(rollcall_id, attempt, "QR 二次確認查詢逾時"),
        }
    }
    QrAttendanceVerification::Retry(
        "尚未取得學校端簽到確認，已恢復等待 QR，請繼續掃描最新的碼（不需重啟監控）".to_string(),
    )
}

// ─── 各類型處理函式 ───────────────────────────────────────────────────────────

/// 處理數字簽到（爆破已關閉，等待 adapter 手動輸入 4 位數字）
/// TronClass 伺服器上的點名即時狀態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerRollcallStatus {
    /// 點名仍在進行中，且本人尚未簽到（需繼續等待）
    ActivePending,
    /// 伺服器確認本人已經簽到成功（on_call_fine）
    AlreadyAttended,
    /// 點名已由老師關閉、已過期、或已不在伺服器清單中
    ClosedOrExpired,
    /// 暫時性網路錯誤（無法連線）
    UnknownError,
}

/// 檢查點名在 TronClass 伺服器上的即時狀態。
async fn check_server_rollcall_status(api: &ApiClient, rollcall_id: u64) -> ServerRollcallStatus {
    match api.get_rollcalls().await {
        Ok(list) => {
            if let Some(rc) = list.iter().find(|r| r.rollcall_id == rollcall_id) {
                if rc.is_attended() || rc.status == "on_call_fine" {
                    ServerRollcallStatus::AlreadyAttended
                } else if rc.is_expired || rc.status != "absent" {
                    ServerRollcallStatus::ClosedOrExpired
                } else {
                    ServerRollcallStatus::ActivePending
                }
            } else {
                ServerRollcallStatus::ClosedOrExpired
            }
        }
        Err(e) => {
            warn!(rollcall_id = rollcall_id, error = %e, "查詢伺服器點名狀態失敗，暫時維持等待");
            ServerRollcallStatus::UnknownError
        }
    }
}

/// 處理數字簽到（跟隨老師點名時長，老師開多久就等多久；全員點到則立刻停止請求）
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
    auto_submitted: &mut bool,
) -> RollcallResult {
    let max_wait_duration = Duration::from_secs(10800); // 3 小時安全硬上限，老師開多久等多久

    info!(
        rollcall_id = rollcall.rollcall_id,
        "數字簽到流程啟動：持續等待至老師關閉點名為止，預設等待手動輸入或自動讀取"
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
                        max_wait_duration,
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
            timeout_secs: 2700,
        });
        send_rollcall_notice(bot, &target, &msg, "發送數字點名碼請求通知").await;
    }

    let started_waiting = tokio::time::Instant::now();
    let max_deadline = started_waiting + max_wait_duration;
    let reminder_at = started_waiting + Duration::from_secs(INPUT_REMINDER_SECS);
    let mut reminder_sent = false;
    let mut last_error: Option<String> = None;
    let mut auto_detect_tried = false;
    let _ = class_progress_snapshot(
        &api,
        scanner_registry.as_ref(),
        &config.provider,
        rollcall.rollcall_id,
    )
    .await;
    let mut next_progress_check = started_waiting + Duration::from_secs(AUTO_SUBMIT_POLL_SECS);
    let mut next_server_active_check = started_waiting + Duration::from_secs(15);
    loop {
        let now = tokio::time::Instant::now();
        if now >= max_deadline {
            if let Some((registry, _)) = number_registered.as_ref() {
                registry
                    .unregister_number_pending(&config.provider, rollcall.rollcall_id, &config.id)
                    .await;
            }
            return RollcallResult::Failed {
                reason: last_error
                    .unwrap_or_else(|| "數字點名等待逾時（已達安全上限 3 小時）".to_string()),
            };
        }

        // 節流與提早結束：數字群組已沒有等待中的帳號時，不再發送任何伺服器確認請求。
        // H2：只有本帳號確實簽到成功才回報 Success，否則回報 Failed（附原因）。
        if let Some((registry, _)) = number_registered.as_ref() {
            if registry.is_number_group_completed(&config.provider, rollcall.rollcall_id).await {
                if registry
                    .is_number_account_completed(&config.provider, rollcall.rollcall_id, &config.id)
                    .await
                {
                    info!(
                        rollcall_id = rollcall.rollcall_id,
                        "本帳號已完成數字簽到，數字群組已無等待中的帳號，結束等待"
                    );
                    return RollcallResult::Success {
                        detail: "本帳號已簽到成功（群組已無等待中的帳號）".to_string(),
                    };
                }
                warn!(
                    rollcall_id = rollcall.rollcall_id,
                    "數字群組已無等待中的帳號，但本帳號未簽到成功，結束等待"
                );
                return RollcallResult::Failed {
                    reason: last_error
                        .clone()
                        .unwrap_or_else(|| "數字點名已結束，本帳號未取得簽到成功".to_string()),
                };
            }
        }

        // 定期向 TronClass 伺服器同步點名活躍狀態
        if now >= next_server_active_check {
            next_server_active_check = now + Duration::from_secs(15);
            match check_server_rollcall_status(&api, rollcall.rollcall_id).await {
                ServerRollcallStatus::AlreadyAttended => {
                    info!(rollcall_id = rollcall.rollcall_id, "學校伺服器確認已完成簽到，停止請求並結束等待");
                    if let Some((registry, _)) = number_registered.as_ref() {
                        registry
                            .mark_number_completed(&config.provider, rollcall.rollcall_id, &config.id)
                            .await;
                    }
                    return RollcallResult::Success {
                        detail: "學校伺服器確認已完成簽到（on_call_fine）".to_string(),
                    };
                }
                ServerRollcallStatus::ClosedOrExpired => {
                    info!(rollcall_id = rollcall.rollcall_id, "學校伺服器顯示數字點名已結束或已過期，停止請求並結束等待");
                    if let Some((registry, _)) = number_registered.as_ref() {
                        registry
                            .unregister_number_pending(&config.provider, rollcall.rollcall_id, &config.id)
                            .await;
                    }
                    return RollcallResult::Failed {
                        reason: last_error.unwrap_or_else(|| "點名已結束（老師已關閉或過期）".to_string()),
                    };
                }
                ServerRollcallStatus::ActivePending | ServerRollcallStatus::UnknownError => {
                    // 點名仍在進行中且尚未簽到，繼續等待
                }
            }
        }

        let wait_until = if !reminder_sent && last_error.is_none() {
            reminder_at.max(now)
        } else {
            next_server_active_check.min(max_deadline)
        };
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
                    if auto_detect_tried {
                        debug!("後台自動讀碼已試過，繼續等手動輸入");
                    } else {
                        auto_detect_tried = true;
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
                            Ok(result) => {
                                if result.is_success() {
                                    if let Some(bot) = messenger {
                                        let target = adapter_target.lock().await.clone();
                                        let msg = OutboundMessage::Text(format!(
                                            "🌐 班上已有 {}/{} 人點名成功（{}%，超過 {}%），系統自動讀取數字點名碼並送出。",
                                            progress.completed,
                                            progress.total,
                                            progress.percent_rounded(),
                                            AUTO_SUBMIT_PERCENT
                                        ));
                                        send_rollcall_notice(bot, &target, &msg, "發送自動讀碼送出通知").await;
                                    }
                                }
                                // ATT_STATS_AUTO_MANUAL_V1：這條路徑是「等不到手動
                                // 輸入、純粹因逾時輪詢發現班上超過 15% 門檻」才觸發
                                // （見上面 `class_progress_over_threshold`），全程沒有
                                // 任何人按過按鈕，是真正的「系統自動送出」。
                                *auto_submitted = true;
                                return result;
                            }
                            Err(AutoLookupFailure::NoCode { status }) => {
                                last_error =
                                    Some("自動偵測不到數字點名碼".to_string());
                                notify_number_auto_miss(
                                    messenger,
                                    &adapter_target,
                                    &status,
                                )
                                .await;
                            }
                            Err(AutoLookupFailure::Error { reason }) => {
                                last_error = Some(reason.clone());
                                notify_number_auto_miss(
                                    messenger,
                                    &adapter_target,
                                    &reason,
                                )
                                .await;
                            }
                        }
                    }
                }
                continue;
            }
            Err(_) if wait_until <= reminder_at && tokio::time::Instant::now() >= reminder_at => {
                reminder_sent = true;
                if last_error.is_none() {
                    let remaining = max_deadline
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
            Err(_) => {
                continue;
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
                if auto_detect_tried {
                    info!("自動讀碼已試過，忽略再次自動讀取");
                    continue;
                }
                auto_detect_tried = true;
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
                        last_error = Some("自動偵測不到數字點名碼".to_string());
                        notify_number_auto_miss(messenger, &adapter_target, &status).await;
                    }
                    Err(AutoLookupFailure::Error { reason }) => {
                        last_error = Some(reason.clone());
                        notify_number_auto_miss(messenger, &adapter_target, &reason).await;
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

/// SELF_REGISTRATION_RELAY_V1：自主報到排隊的等待上限。
/// 與雷達同一個精神（見 `radar::RADAR_RELAY_MAX_WAIT`）：寧可提早送出，
/// 也不要等到學校的點名視窗關了。
const SELF_REG_RELAY_MAX_WAIT: Duration = Duration::from_secs(30);

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

/// NUMBER_AUTO_DETECT_ONCE_V1：自動偵測數字碼失敗時通知一次，之後改等手動輸入。
async fn notify_number_auto_miss(
    messenger: Option<&dyn AdapterMessenger>,
    adapter_target: &Arc<Mutex<AdapterAccountTarget>>,
    detail: &str,
) {
    warn!(detail = %detail, "自動偵測數字碼失敗，不再重試，改通知使用者手動輸入");
    if let Some(bot) = messenger {
        let target = adapter_target.lock().await.clone();
        let msg = OutboundMessage::Text(format!(
            "⚠️ 自動偵測不到數字點名碼（{detail}），不再重試。請在輸入頁輸入 4 位數字，或用 `/num code:1234`。"
        ));
        send_rollcall_notice(bot, &target, &msg, "發送數字碼自動偵測失敗通知").await;
    }
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
        send_rollcall_notice(bot, &target, &msg, "發送點名催促通知").await;
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
    auto_submitted: &mut bool,
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
    // M4：雷達逾時使用雷達自己的 `radar.scan_timeout_secs`，不再共用 QR 的設定。
    let timeout_secs = radar_config.scan_timeout_secs;
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
             開這一頁按「🧮 系統演算」就會用教室座標送出點名；\
             如果一直被回報距離不足，請到面板「雷達」頁把教室座標改成正確的。{}",
            rollcall.course_title,
            account_label,
            rollcall.created_by_name,
            rollcall.rollcall_id,
            page
        ));
        send_rollcall_notice(bot, &target, &msg, "發送雷達點名頁通知").await;
    }

    // CLASS_COUNT_HEADER_V1 + AUTO_SUBMIT 15% 閘門：
    // 自動送出必須等班上已點人數嚴格超過 15%；頁面「系統演算」仍可立刻送。
    // 一進場就讀進度並寫進頁面狀態，上方人數條才不會卡在「讀取中」。
    let api_for_retry = Arc::clone(&api);
    let wait_deadline = tokio::time::Instant::now() + timeout;
    loop {
        let progress = class_progress_snapshot(
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
                    progress.clone(),
                )
                .await;
        }
        let passed = progress
            .as_ref()
            .map(|p| p.passed(AUTO_SUBMIT_PERCENT))
            .unwrap_or(false);
        let manual = radar_channel_has_point(radar_rx.as_ref()).await;
        if passed {
            info!(
                rollcall_id = rollcall.rollcall_id,
                completed = progress.as_ref().map(|p| p.completed).unwrap_or(0),
                total = progress.as_ref().map(|p| p.total).unwrap_or(0),
                "班上已點人數超過 15%，開始自動送出雷達點名"
            );
            // ATT_STATS_AUTO_MANUAL_V1：沿用上面既有的判斷優先序（`passed` 比
            // `manual` 先檢查）——這裡走到的是「班上超過門檻」這條路，即使使用者
            // 「剛好同一輪」也按了系統演算（極少見的巧合），仍歸類為自動送出，
            // 與上面既有的 log 訊息（只提 15% 沒提使用者是否也按了）口徑一致。
            *auto_submitted = true;
            break;
        }
        if manual {
            info!(
                rollcall_id = rollcall.rollcall_id,
                "使用者在頁面按下系統演算，不等人數到 15% 立刻送出"
            );
            break;
        }
        let now = tokio::time::Instant::now();
        if now >= wait_deadline {
            return RollcallResult::Failed {
                reason: format!(
                    "班上已點人數未超過 {AUTO_SUBMIT_PERCENT}%，未自動送出雷達點名"
                ),
            };
        }
        let slice = Duration::from_secs(AUTO_SUBMIT_POLL_SECS)
            .min(wait_deadline.saturating_duration_since(now));
        tokio::time::sleep(slice).await;
    }

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
                // ATT_STATS_AUTO_MANUAL_V1：這次成功是「系統自己發現班上超過
                // 15% 門檻才主動重試」送出的（不管前一次失敗的嘗試原本是手動
                // 還是自動觸發），最終算數的這一次是自動送出。
                *auto_submitted = true;
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
                    format!("\n🎯 打開頁面按「🧮 系統演算」再送一次：{input_url}")
                };
                let target = adapter_target.lock().await.clone();
                let msg = OutboundMessage::Text(format!(
                    "🌐 班上已有 {}/{} 人點名成功（{}%，超過 {}%），但雷達簽到還沒過{last_distance}。{tail}",
                    progress.completed,
                    progress.total,
                    progress.percent_rounded(),
                    AUTO_SUBMIT_PERCENT
                ));
                send_rollcall_notice(bot, &target, &msg, "發送雷達重試未過通知").await;
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
    config: &AccountConfig,
) -> RollcallResult {
    info!(rollcall_id = rollcall.rollcall_id, "開始自主報到");

    // SELF_REGISTRATION_RELAY_V1：自主報到是「空的 PUT、立刻會成功」，但多帳號同時打學校
    // 一樣會擠在同一毫秒。所以它也要排隊：第一位立即送出，之後每位間隔 base + U(0, jitter)
    // （面板「數字點名」那組節奏）。等待有上限，不會因為帳號多就拖過點名視窗。
    let _ = wait_group_submit_turn(
        &config.provider,
        rollcall.rollcall_id,
        GroupSubmitKind::SelfRegistration,
        &config.id,
        Some(SELF_REG_RELAY_MAX_WAIT),
    )
    .await;

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
    let max_wait_duration = Duration::from_secs(10800); // 3 小時安全硬上限，老師開多久等多久

    info!(rollcall_id = rollcall.rollcall_id, "開始 QR Code 簽到流程（持續至老師關閉點名為止）");

    // 構建掃碼頁面 URL（讓使用者點擊）
    let legacy_scan_url = format!(
        "{}?rollcall_id={}",
        config.provider_config.qrcode.scanner_base_url, rollcall.rollcall_id
    );
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
                    max_wait_duration,
                )
                .await
            {
                Ok(link) => {
                    registry.remember_qr(&config.provider, &config.id, rollcall).await;
                    Some((Arc::clone(registry), link.scan_url, link.already_submitted))
                },
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
            scan_url: scan_url.clone(),
            alt_scan_url,
            timeout_secs: 2700,
        });

        if !scanner_already_submitted {
            let target = adapter_target.lock().await.clone();
            send_rollcall_notice(bot, &target, &msg, "發送 QR Code 請求通知").await;
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
        "等待 QR code 輸入（將持續等待至老師關閉點名為止）..."
    );

    let started_waiting = tokio::time::Instant::now();
    let max_deadline = started_waiting + max_wait_duration;
    let reminder_at = started_waiting + Duration::from_secs(INPUT_REMINDER_SECS);
    let mut reminder_sent = reminder_at >= max_deadline || scanner_already_submitted;
    let mut last_error: Option<String> = None;
    let mut last_qr_attempt: Option<(String, tokio::time::Instant)> = None;
    let mut next_progress_check = started_waiting;
    let mut next_server_active_check = started_waiting + Duration::from_secs(15);
    let mut progress_nudged = false;

    loop {
        let now = tokio::time::Instant::now();
        if now >= max_deadline {
            warn!(
                rollcall_id = rollcall.rollcall_id,
                "QR code 等待達到安全上限（3小時）"
            );
            if let Some((registry, _, _)) = scanner_registered.as_ref() {
                registry
                    .unregister_pending(&config.provider, rollcall.rollcall_id, &config.id)
                    .await;
            }
            return RollcallResult::Failed {
                reason: last_error.unwrap_or_else(|| "QR code 輸入逾時（已達安全上限 3 小時）".to_string()),
            };
        }

        if let Some(registry) = scanner_registry.as_ref() {
            // QR_MANUAL_SAFE_WINDOW_V1: a manual request never cancels an active PUT/verify.
            // At the next loop boundary hand control back to the immediate supervisor.
            if registry.qr_manual_requested(&config.provider, rollcall.rollcall_id, &config.id).await {
                registry.qr_phase(&config.provider, rollcall.rollcall_id, &config.id, QrPhase::WaitingInput).await;
                tokio::time::sleep(Duration::from_secs(6)).await;
            }
            registry.qr_phase(&config.provider, rollcall.rollcall_id, &config.id, QrPhase::Checking).await;
        }

        // 節流與提早結束：掃碼群組已沒有等待中的帳號時，不再發送任何伺服器確認請求。
        // H2：「沒有人在等」不等於「本帳號成功」——只有本帳號確實簽到成功才回報 Success。
        if let Some((registry, _, _)) = scanner_registered.as_ref() {
            if registry.is_qr_group_completed(&config.provider, rollcall.rollcall_id).await {
                if registry
                    .is_qr_account_completed(&config.provider, rollcall.rollcall_id, &config.id)
                    .await
                {
                    info!(
                        rollcall_id = rollcall.rollcall_id,
                        "本帳號已完成 QR 簽到，掃碼群組已無等待中的帳號，結束等待"
                    );
                    return RollcallResult::Success {
                        detail: "本帳號已簽到成功（掃碼群組已無等待中的帳號）".to_string(),
                    };
                }
                warn!(
                    rollcall_id = rollcall.rollcall_id,
                    "掃碼群組已無等待中的帳號，但本帳號未簽到成功，結束等待"
                );
                return RollcallResult::Failed {
                    reason: last_error
                        .clone()
                        .unwrap_or_else(|| "掃碼點名已結束，本帳號未取得簽到成功".to_string()),
                };
            }
        }

        // 定期向 TronClass 伺服器同步點名活躍狀態：若老師已在伺服器上關閉點名或本人已簽到，立即停止請求
        if now >= next_server_active_check {
            let server_status = match check_qr_server_status(&api, rollcall.rollcall_id).await {
                Ok(state) => state,
                Err(reason) => {
                    if let Some((registry, _, _)) = scanner_registered.as_ref() {
                        registry.unregister_pending(&config.provider, rollcall.rollcall_id, &config.id).await;
                    }
                    return RollcallResult::FatalError { reason };
                }
            };
            next_server_active_check = tokio::time::Instant::now() + Duration::from_secs(15);
            match server_status {
                ServerRollcallStatus::AlreadyAttended => {
                    info!(rollcall_id = rollcall.rollcall_id, "學校伺服器確認已完成簽到，停止請求並結束等待");
                    if let Some((registry, _, _)) = scanner_registered.as_ref() {
                        registry
                            .mark_completed(&config.provider, rollcall.rollcall_id, &config.id)
                            .await;
                    }
                    return RollcallResult::Success {
                        detail: "學校伺服器確認已完成簽到".to_string(),
                    };
                }
                ServerRollcallStatus::ClosedOrExpired => {
                    info!(rollcall_id = rollcall.rollcall_id, "學校伺服器顯示 QR 點名已結束或已過期，停止請求並結束等待");
                    if let Some((registry, _, _)) = scanner_registered.as_ref() {
                        registry
                            .unregister_pending(&config.provider, rollcall.rollcall_id, &config.id)
                            .await;
                    }
                    return RollcallResult::Failed {
                        reason: last_error.unwrap_or_else(|| "點名已結束（老師已關閉或過期）".to_string()),
                    };
                }
                ServerRollcallStatus::ActivePending | ServerRollcallStatus::UnknownError => {
                    // 點名仍在進行中且尚未簽到，繼續等待
                }
            }
        }

        if let Some(registry) = scanner_registry.as_ref() {
            registry.qr_phase(&config.provider, rollcall.rollcall_id, &config.id, QrPhase::WaitingInput).await;
        }

        let now = tokio::time::Instant::now();
        // ROLLCALL_INPUT_REMINDER_V1：第一段只等到催促時間點。
        let wait_until = if !reminder_sent && last_error.is_none() {
            reminder_at.max(now)
        } else {
            next_server_active_check.min(max_deadline)
        };
        // AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：這一段最多只等到「下一次班上進度檢查時間」。
        let progress_at = (next_progress_check < wait_until).then_some(next_progress_check);
        let segment_end = progress_at.unwrap_or(wait_until);
        let remaining = segment_end.saturating_duration_since(now);
        // None 代表「催促時間點到了但還沒有輸入」（rx_guard 已在 block 結束時釋放）。
        let qr_input: Option<String> = {
            let mut rx_guard = rx.lock().await;
            match tokio::time::timeout(remaining, rx_guard.recv()).await {
                Ok(Some(input)) => {
                    if let Some(registry) = scanner_registry.as_ref() {
                        registry.qr_phase(&config.provider, rollcall.rollcall_id, &config.id, QrPhase::WaitingTurn).await;
                    }
                    info!(input_len = input.len(), "收到 QR code 輸入，嘗試簽到");
                    Some(input)
                }
                Err(_) if progress_at.is_some() => {
                    if let Some(registry) = scanner_registry.as_ref() {
                        registry.qr_phase(&config.provider, rollcall.rollcall_id, &config.id, QrPhase::Checking).await;
                    }
                    let progress = tokio::time::timeout(
                        QR_PROGRESS_TIMEOUT,
                        class_progress_over_threshold(
                            &api,
                            scanner_registry.as_ref(),
                            &config.provider,
                            rollcall.rollcall_id,
                            account_label,
                        ),
                    ).await.unwrap_or(None);
                    next_progress_check =
                        tokio::time::Instant::now() + Duration::from_secs(AUTO_SUBMIT_POLL_SECS);
                    if !progress_nudged {
                        if let Some(progress) = progress {
                            progress_nudged = true;
                            warn!(
                                rollcall_id = rollcall.rollcall_id,
                                completed = progress.completed,
                                total = progress.total,
                                "班上已超過門檻，但 QR payload 只能靠掃碼取得，改為催使用者"
                            );
                            if let Some(bot) = messenger {
                                let target = adapter_target.lock().await.clone();
                                let msg = OutboundMessage::Text(format!(
                                    "🔥 班上已有 {}/{} 人點名成功（{}%），但 QR 內容只能靠掃碼取得——請立刻開掃碼頁：{scan_url}",
                                    progress.completed,
                                    progress.total,
                                    progress.percent_rounded()
                                ));
                                send_rollcall_notice(bot, &target, &msg, "發送 QR 催促通知").await;
                            }
                        }
                    }
                    continue;
                }
                Err(_) if wait_until <= reminder_at && tokio::time::Instant::now() >= reminder_at => None,
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
                let remaining_secs = max_deadline
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
        let _ = wait_group_submit_turn(
            &config.provider,
            rollcall.rollcall_id,
            GroupSubmitKind::QrCode,
            &config.id,
            relay_budget(max_deadline),
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
        if let Some(registry) = scanner_registry.as_ref() {
            registry.qr_phase(&config.provider, rollcall.rollcall_id, &config.id, QrPhase::WaitingTurn).await;
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

        if let Some((registry, _, _)) = scanner_registered.as_ref() {
            registry.set_qr_error(&config.provider, rollcall.rollcall_id, &config.id, None).await;
        }
        if let Some(registry) = scanner_registry.as_ref() {
            registry.qr_phase(&config.provider, rollcall.rollcall_id, &config.id, QrPhase::Submitting).await;
        }
        // QR_RECOVERY_V1：單次送出卡住只放棄該次等待，不解除整場的 QR 輸入登記。
        // timeout 不代表學校沒收到；下一輪仍會查詢實際狀態，不自行宣稱失敗／成功。
        let (result, attempt_timed_out) = match tokio::time::timeout(
            QR_NETWORK_TIMEOUT,
            attempt_qrcode_rollcall(Arc::clone(&api), rollcall.rollcall_id, &fresh_input, false),
        ).await {
            Ok(result) => (result, false),
            Err(_) => (QrCodeResult::Failed {
                reason: "QR 送出等待超過15秒，結果尚未確認；已恢復等待，請持續掃描".to_string(),
            }, true),
        };

        match result {
            QrCodeResult::Success { .. } => {
                if let Some(registry) = scanner_registry.as_ref() {
                    registry.qr_phase(&config.provider, rollcall.rollcall_id, &config.id, QrPhase::Verifying).await;
                }

                if let Some((registry, _, _)) = scanner_registered.as_ref() {
                    registry.set_qr_error(
                        &config.provider, rollcall.rollcall_id, &config.id,
                        Some("QR 已送出，正在等待學校端二次確認；請保持掃描"),
                    ).await;
                }
                if let Some(bot) = messenger {
                    let target = adapter_target.lock().await.clone();
                    let msg = OutboundMessage::Text(format!(
                        "📨 {}（QR Code）點名已送出，正在二次確認…", rollcall.course_title
                    ));
                    send_rollcall_notice(bot, &target, &msg, "發送 QR 已送出通知").await;
                }
                crate::web_push_notifications::notify_rollcall_submitted(
                    account_label, rollcall.rollcall_id, &rollcall.course_title,
                    &AttendanceType::QrCode.to_string(),
                );
                match verify_qr_attendance(&api, rollcall.rollcall_id).await {
                    QrAttendanceVerification::Confirmed => {
                        if let Some((registry, _, _)) = scanner_registered.as_ref() {
                            registry.mark_completed(&config.provider, rollcall.rollcall_id, &config.id).await;
                        }
                        return RollcallResult::Success {
                            detail: "QR 簽到成功（學校端已二次確認）".to_string(),
                        };
                    }
                    QrAttendanceVerification::Retry(reason) => {
                        if let Some(registry) = scanner_registry.as_ref() {
                            if registry.qr_attempt_feedback(&config.provider, rollcall.rollcall_id, &config.id, true).await {
                                // QR_WATCHDOG_SAFE_HANDOFF_V1：連續掃描也要留下檢查窗口，
                                // 不在 PUT 或確認尚未結束時為了重啟而中斷它。
                                tokio::time::sleep(Duration::from_secs(6)).await;
                            }
                        }
                        // 不返回外層／不移出登記：同一流程直接接下一個新碼。
                        last_error = Some(reason);
                    }
                    QrAttendanceVerification::Fatal(reason) => {
                        if let Some((registry, _, _)) = scanner_registered.as_ref() {
                            registry.unregister_pending(&config.provider, rollcall.rollcall_id, &config.id).await;
                        }
                        return RollcallResult::FatalError { reason };
                    }
                }
            }
            QrCodeResult::Failed { reason } => {
                if let Some(registry) = scanner_registry.as_ref() {
                    if registry.qr_attempt_feedback(&config.provider, rollcall.rollcall_id, &config.id, attempt_timed_out).await {
                        // QR_WATCHDOG_SAFE_HANDOFF_V1：連續掃描也要留下檢查窗口。
                        tokio::time::sleep(Duration::from_secs(6)).await;
                    }
                }
                warn!(reason = %reason, "QR Code 簽到失敗，持續等待下一個 QR payload");
                last_error = Some(reason);
            }
            QrCodeResult::ParseError { reason } => {
                if let Some(registry) = scanner_registry.as_ref() {
                    registry.qr_attempt_feedback(&config.provider, rollcall.rollcall_id, &config.id, false).await;
                }
                let reason = format!("QR code 解析失敗：{reason}");
                warn!(reason = %reason, "QR Code 解析失敗，持續等待下一個 QR payload");
                last_error = Some(reason);
            }
            QrCodeResult::Error(e) => {
                if let Some(registry) = scanner_registry.as_ref() {
                    registry.qr_attempt_feedback(&config.provider, rollcall.rollcall_id, &config.id, false).await;
                }
                // H1（QR_ERROR_KEEPS_REGISTRATION_V1）：只有致命錯誤才移出掃碼群組。
                // 舊寫法先移出再判斷，非致命錯誤（例如學校端暫時封鎖）會讓帳號留在迴圈裡，
                // 卻已收不到之後的 payload，直到 3 小時上限才結束。
                if is_auth_error(&e) {
                    if let Some((registry, _, _)) = scanner_registered.as_ref() {
                        registry
                            .unregister_pending(&config.provider, rollcall.rollcall_id, &config.id)
                            .await;
                    }
                    return RollcallResult::FatalError { reason: e };
                }
                warn!(reason = %e, "QR Code 簽到錯誤（非致命），保留在掃碼群組中等待下一筆 payload");
                last_error = Some(e);
            }
        }
        if let Some((registry, _, _)) = scanner_registered.as_ref() {
            registry.set_qr_error(
                &config.provider, rollcall.rollcall_id, &config.id, last_error.as_deref(),
            ).await;
        }
    }
}

// QR_STALL_WATCHDOG_V1：查不到點名不能當作已完成，也不能憑模糊狀態重建。
fn qr_watchdog_server_status(list: &[Rollcall], id: u64) -> ServerRollcallStatus {
    if qr_attendance_confirmed(list, id) { return ServerRollcallStatus::AlreadyAttended; }
    match list.iter().find(|rc| rc.rollcall_id == id) {
        Some(rc) if rc.needs_attendance() => ServerRollcallStatus::ActivePending,
        Some(rc) if rc.is_expired => ServerRollcallStatus::ClosedOrExpired,
        _ => ServerRollcallStatus::UnknownError,
    }
}

async fn probe_qr_watchdog(api: &ApiClient, id: u64) -> std::result::Result<ServerRollcallStatus, String> {
    match tokio::time::timeout(QR_NETWORK_TIMEOUT, api.get_rollcalls()).await {
        Ok(Ok(list)) => Ok(qr_watchdog_server_status(&list, id)),
        Ok(Err(e)) if is_auth_error(&e.to_string()) => Err(e.to_string()),
        _ => Ok(ServerRollcallStatus::UnknownError),
    }
}

// QR_MANUAL_SCHOOL_PROBE_V1: only a fresh exact-ID school result resolves rechecking.
async fn probe_qr_manual(
    api: &ApiClient, registry: &QrScannerRegistry, config: &AccountConfig,
    id: u64, deadline: tokio::time::Instant,
) -> std::result::Result<ServerRollcallStatus, String> {
    let result = tokio::time::timeout_at(deadline, probe_qr_watchdog(api, id)).await
        .unwrap_or(Ok(ServerRollcallStatus::UnknownError));
    let error = match &result {
        Ok(ServerRollcallStatus::UnknownError) => Some("學校狀態暫時無法確認；鏡頭保持開啟，請稍後再按開啟相機重查"),
        Ok(ServerRollcallStatus::ClosedOrExpired) => Some("學校顯示本場已過期；無法用舊場次補點，請開啟目前點名連結"),
        Err(_) => Some("登入狀態失效，等待原監控重新登入；尚未確認簽到"),
        _ => None,
    };
    registry.finish_qr_manual(&config.provider, id, &config.id,
        matches!(result, Ok(ServerRollcallStatus::ActivePending)), error).await;
    if matches!(result, Ok(ServerRollcallStatus::AlreadyAttended)) {
        registry.mark_completed(&config.provider, id, &config.id).await;
    }
    info!(provider = %config.provider, account = %config.id, rollcall_id = id,
        active_pending = matches!(result, Ok(ServerRollcallStatus::ActivePending)),
        confirmed = matches!(result, Ok(ServerRollcallStatus::AlreadyAttended)),
        "QR鏡頭重新核對完成；只有明確未點到才恢復");
    result
}

fn qr_manual_result(result: std::result::Result<ServerRollcallStatus, String>) -> RollcallResult {
    match result {
        Ok(ServerRollcallStatus::AlreadyAttended) => RollcallResult::Success {
            detail: "重新向學校確認已點到（未重送QR）".to_string(),
        },
        Err(reason) => RollcallResult::FatalError { reason },
        Ok(ServerRollcallStatus::ClosedOrExpired) => RollcallResult::Skipped { reason: "重查：本場點名已過期，未送出QR".to_string() },
        _ => RollcallResult::Skipped { reason: "重查無法確認學校狀態；保留既有出席紀錄，請稍後重查".to_string() },
    }
}

enum QrSupervisorAction {
    ManualRestart,
    Restart(String),
    Finish(RollcallResult),
}

/// QR_STALL_WATCHDOG_V1：每個(account, provider, rollcall)獨立監督。
/// 不 spawn 點名 worker，不呼叫全域 reset；取消 batch 會同時取消此 supervisor。
async fn process_qr_with_watchdog(
    api: Arc<ApiClient>,
    rollcall: Rollcall,
    config: &AccountConfig,
    account_label: &str,
    messenger: Option<&dyn AdapterMessenger>,
    adapter_target: Arc<Mutex<AdapterAccountTarget>>,
    status: Arc<Mutex<MonitorStatus>>,
    registry: Arc<QrScannerRegistry>,
) -> RollcallOutcome {
    let started = tokio::time::Instant::now();
    let id = rollcall.rollcall_id;
    let remaining = registry.qr_remaining(&config.provider, id).await.unwrap_or(Duration::from_secs(10800));
    let deadline = started + remaining.min(Duration::from_secs(10800));
    let mut manual_rx = registry.qr_resume_subscribe();
    let result = loop {
        if tokio::time::Instant::now() >= deadline {
            break RollcallResult::Failed { reason: "QR原等待期限已到；重新開鏡不延長連結".to_string() };
        }
        // Completed jobs have no receiver. Recheck BEFORE registering or replaying any QR.
        if registry.qr_manual_requested(&config.provider, id, &config.id).await {
            let result = probe_qr_manual(&api, &registry, config, id, deadline).await;
            if !matches!(result, Ok(ServerRollcallStatus::ActivePending)) { break qr_manual_result(result); }
        }
        // 每輪全新 receiver/lane。這個 scope 結束時，舊 future 與 guard 先被 drop。
        let action = {
            let (tx, rx) = create_qrcode_channel(4);
            let _input_guard = register_rollcall_input(&config.id, id, RollcallInputSender::Qr(tx.clone()));
            let worker = process_rollcall(
                Arc::clone(&api), rollcall.clone(), config, account_label, messenger,
                Arc::clone(&adapter_target), Arc::clone(&status),
                None, None, Some(rx), Some(tx), None, None, Some(Arc::clone(&registry)),
            );
            tokio::pin!(worker);
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut next_probe = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(deadline) => {
                        break QrSupervisorAction::Finish(RollcallResult::Failed {
                            reason: "QR 等待已達整場3小時上限（自動恢復不延長期限）".to_string(),
                        });
                    }
                    outcome = &mut worker => {
                        if registry.qr_manual_requested(&config.provider, id, &config.id).await {
                            let result = probe_qr_manual(&api, &registry, config, id, deadline).await;
                            if matches!(result, Ok(ServerRollcallStatus::ActivePending)) {
                                break QrSupervisorAction::ManualRestart;
                            }
                            if matches!(result, Ok(ServerRollcallStatus::UnknownError | ServerRollcallStatus::ClosedOrExpired)) {
                                return outcome; // preserve the real attempt's result; a read failure is not a new failed attendance
                            }
                            break QrSupervisorAction::Finish(qr_manual_result(result));
                        }
                        return outcome;
                    }
                    _ = async {
                        tokio::select! { _ = tick.tick() => {}, _ = manual_rx.changed() => {} }
                    } => {
                        if registry.qr_manual_requested(&config.provider, id, &config.id).await {
                            if !registry.qr_manual_safe(&config.provider, id, &config.id).await { continue; }
                            let result = probe_qr_manual(&api, &registry, config, id, deadline).await;
                            match result {
                                Ok(ServerRollcallStatus::ActivePending) => break QrSupervisorAction::ManualRestart,
                                // Unknown must not cancel a still-live worker or mark success.
                                Ok(ServerRollcallStatus::UnknownError) => continue,
                                other => break QrSupervisorAction::Finish(qr_manual_result(other)),
                            }
                        }
                        if tokio::time::Instant::now() < next_probe { continue; }
                        let Some((reason, allowed, recoveries)) = registry.qr_stall(&config.provider, id, &config.id).await else { continue; };
                        next_probe = tokio::time::Instant::now() + Duration::from_secs(30);
                        if !allowed {
                            let notice = if recoveries >= QR_MAX_RECOVERIES {
                                format!("{reason}；已達每場每帳號{QR_MAX_RECOVERIES}次自動恢復上限，請檢查學校狀態、登入及目前QR；系統不會無限重啟")
                            } else {
                                format!("{reason}；目前在120秒恢復冷卻期，系統會自動再檢查")
                            };
                            registry.qr_watchdog_notice(&config.provider, id, &config.id, &notice).await;
                            continue;
                        }
                        registry.qr_watchdog_notice(&config.provider, id, &config.id,
                            &format!("偵測到疑似停滯：{reason}；正在核對學校狀態")
                        ).await;
                        warn!(provider = %config.provider, account = %config.id, rollcall_id = id,
                            reason = %reason, "QR watchdog 偵測到疑似停滯");
                        // 舊 worker 此時仍存在但暫停輪詢；確認未知時保留，不盲目重送。
                        let probe = tokio::time::timeout_at(deadline, probe_qr_watchdog(&api, id)).await;
                        next_probe = tokio::time::Instant::now() + Duration::from_secs(30);
                        match probe {
                            Ok(Ok(ServerRollcallStatus::ActivePending)) => break QrSupervisorAction::Restart(reason),
                            Ok(Ok(ServerRollcallStatus::AlreadyAttended)) => {
                                break QrSupervisorAction::Finish(RollcallResult::Success {
                                    detail: "QR watchdog 重查學校已簽到，已同步完成狀態（沒有重送）".to_string(),
                                });
                            }
                            Ok(Ok(ServerRollcallStatus::ClosedOrExpired)) => {
                                break QrSupervisorAction::Finish(RollcallResult::Failed {
                                    reason: "學校顯示本場點名已過期，停止局部恢復；未冒充簽到成功".to_string(),
                                });
                            }
                            Ok(Ok(ServerRollcallStatus::UnknownError)) => {
                                registry.qr_watchdog_notice(&config.provider, id, &config.id,
                                    "偵測到QR停滯，但學校狀態暫時無法確認；保留原流程，30秒後自動再查，避免重複送出"
                                ).await;
                            }
                            Ok(Err(reason)) => break QrSupervisorAction::Finish(RollcallResult::FatalError { reason }),
                            Err(_) => break QrSupervisorAction::Finish(RollcallResult::Failed {
                                reason: "QR 等待已達整場3小時上限".to_string(),
                            }),
                        }
                    }
                }
            }
        }; // DROP_OLD_QR_BEFORE_REBIND_V1：worker/receiver/input guard 全部先銷毀。
        match action {
            QrSupervisorAction::ManualRestart => {
                // Old future/input guard already dropped; no global reset or automatic budget reset.
                info!(account = %config.id, rollcall_id = id, "手動開鏡：局部恢復QR接收，等待最新碼");
            }
            QrSupervisorAction::Restart(reason) => {
                if !registry.begin_qr_recovery(&config.provider, id, &config.id).await {
                    break RollcallResult::Failed { reason: "QR恢復條件已改變，停止本次重建".to_string() };
                }
                warn!(provider = %config.provider, account = %config.id, rollcall_id = id,
                    reason = %reason, "QR watchdog 僅重建這一場的輸入通道與工作者");
                tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_secs(5))).await;
            }
            QrSupervisorAction::Finish(result) => break result,
        }
    };

    // 正常 worker 已有自己的結果通知，只有 supervisor 提早收尾才走這裡。
    if result.is_success() {
        registry.mark_completed(&config.provider, id, &config.id).await;
    } else {
        registry.unregister_pending(&config.provider, id, &config.id).await;
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if result.is_skipped() {
        // A passive failed/expired recheck must not overwrite attendance history or emit a failed-PUT notice.
        return RollcallOutcome { rollcall, attendance_type: AttendanceType::QrCode, result, elapsed_ms, auto_submitted: false };
    }
    if let Some(bot) = messenger {
        let target = adapter_target.lock().await.clone();
        let msg = OutboundMessage::RollcallFinished(RollcallResultEvent {
            rollcall_id: id, account: account_label.to_string(),
            course_name: rollcall.course_title.clone(), attendance_type: AttendanceType::QrCode.to_string(),
            success: result.is_success(), result: result.to_string(), elapsed_ms,
        });
        send_rollcall_notice(bot, &target, &msg, "QR watchdog 結果通知").await;
    }
    crate::web_push_notifications::notify_rollcall_finished(
        account_label, id, &rollcall.course_title, &AttendanceType::QrCode.to_string(),
        result.is_success(), &result.to_string(),
    );
    RollcallOutcome {
        rollcall, attendance_type: AttendanceType::QrCode, result, elapsed_ms, auto_submitted: false,
    }
}

// ─── 批次處理 ─────────────────────────────────────────────────────────────────

// ROLLCALL_FORCE_RESET_V1：管理員按「強制結束並重新偵測」時，所有進行中的批次都會收到這個訊號，
// 丟棄尚未完成的點名等待（QR／數字／雷達），清掉它們的登記，然後由監控循環立即重新偵測一次。
// 用 watch 而不是 Notify：每個批次開始時 subscribe，只會看到「開始之後」的重設，且不會遺失。
static ROLLCALL_RESET: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> =
    std::sync::OnceLock::new();

fn rollcall_reset_sender() -> &'static tokio::sync::watch::Sender<u64> {
    ROLLCALL_RESET.get_or_init(|| tokio::sync::watch::channel(0u64).0)
}

/// 通知所有進行中的批次結束目前的點名等待（不會登出、不會重新登入）。
/// 呼叫端應另外對各帳號送出 force_poll，讓循環立刻重新偵測。
pub fn request_rollcall_reset() {
    rollcall_reset_sender().send_modify(|generation| *generation = generation.wrapping_add(1));
}

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

    // ROLLCALL_PARALLEL_V1：每個點名各自一組輸入通道（數字／QR／雷達），並登記到路由表，
    // 讓 Discord／LINE 指令（依點名 ID 指定）與網頁輸入都送到正確的點名；點名之間平行處理。
    // 舊行為是 for 迴圈依序處理：數字／QR 最長 3 小時的等待會阻塞同帳號其他點名。
    let isolate_inputs = number_tx.is_some() || qr_tx.is_some() || radar_tx.is_some();
    let account_id = config.id.clone();
    let mut all_rollcall_ids: Vec<u64> = pending.iter().map(|rc| rc.rollcall_id).collect();
    let mut active_ids: std::collections::HashSet<u64> = all_rollcall_ids.iter().copied().collect();
    let mut next_index = pending.len();
    let mut manual_rx = scanner_registry.as_ref().map(|registry| registry.qr_resume_subscribe());
    let mut reset_rx = rollcall_reset_sender().subscribe();
    let mut running = FuturesUnordered::new();
    let make_job = |index: usize, rollcall: Rollcall| {
        let rollcall_id = rollcall.rollcall_id;
        let attendance = rollcall.attendance_type();
        debug!(
            idx = index + 1,
            rollcall_id = rollcall_id,
            "加入平行處理的 rollcall"
        );

        let (rc_number_tx, rc_number_rx, rc_qr_tx, rc_qr_rx, rc_radar_tx, rc_radar_rx) =
            if isolate_inputs {
                let (n_tx, n_rx) = create_number_code_channel(8);
                let (q_tx, q_rx) = create_qrcode_channel(4);
                let (r_tx, r_rx) = create_radar_point_channel(8);
                (Some(n_tx), Some(n_rx), Some(q_tx), Some(q_rx), Some(r_tx), Some(r_rx))
            } else {
                (
                    number_tx.clone(),
                    number_rx.clone(),
                    qr_tx.clone(),
                    qr_rx.clone(),
                    radar_tx.clone(),
                    radar_rx.clone(),
                )
            };

        let supervise_qr = isolate_inputs && attendance == AttendanceType::QrCode
            && scanner_registry.as_ref().is_some_and(|registry| !registry.public_base_url().trim().is_empty());
        // 只登記與點名類型相符的輸入；點名結束（包含被取消）時 guard 會自動移除登記。
        let input_guard = match attendance {
            AttendanceType::Number => rc_number_tx.clone().map(|tx| {
                register_rollcall_input(&account_id, rollcall_id, RollcallInputSender::Number(tx))
            }),
            AttendanceType::QrCode if !supervise_qr => rc_qr_tx.clone().map(|tx| {
                register_rollcall_input(&account_id, rollcall_id, RollcallInputSender::Qr(tx))
            }),
            _ => None,
        };

        let api = Arc::clone(&api);
        let adapter_target = Arc::clone(&adapter_target);
        let status = Arc::clone(&status);
        let scanner_registry = scanner_registry.clone();
        async move {
            let _input_guard = input_guard;
            let outcome = if let Some(registry) = scanner_registry.as_ref().filter(|_| supervise_qr) {
                process_qr_with_watchdog(
                    api, rollcall, config, account_label, messenger, adapter_target, status, Arc::clone(registry),
                ).await
            } else {
                process_rollcall(
                    api,
                    rollcall,
                    config,
                    account_label,
                    messenger,
                    adapter_target,
                    status,
                    rc_number_rx,
                    rc_number_tx,
                    rc_qr_rx,
                    rc_qr_tx,
                    rc_radar_rx,
                    rc_radar_tx,
                    scanner_registry,
                )
                .await
            };
            (index, rollcall_id, outcome)
        }
    };
    for (index, rollcall) in pending.into_iter().enumerate() { running.push(make_job(index, rollcall)); }

    let mut results: Vec<(usize, u64, RollcallOutcome)> = Vec::new();
    let mut reset_requested = false;
    loop {
        // QR_MANUAL_BATCH_V1: finished QR jobs can rejoin while unrelated number/radar jobs wait.
        if let Some(registry) = scanner_registry.as_ref().filter(|_| isolate_inputs) {
            for rollcall in registry.qr_manual_rollcalls(&config.provider, &config.id).await {
                if active_ids.insert(rollcall.rollcall_id) {
                    results.retain(|(_, id, _)| *id != rollcall.rollcall_id);
                    if !all_rollcall_ids.contains(&rollcall.rollcall_id) { all_rollcall_ids.push(rollcall.rollcall_id); }
                    running.push(make_job(next_index, rollcall));
                    next_index += 1;
                }
            }
        }
        if running.is_empty() { break; }
        tokio::select! {
            _ = async {
                match manual_rx.as_mut() {
                    Some(rx) => { let _ = rx.changed().await; },
                    None => std::future::pending::<()>().await,
                }
            } => {},
            next = running.next() => {
                let Some((index, rollcall_id, outcome)) = next else { break };
                active_ids.remove(&rollcall_id);
                let fatal = outcome.result.is_fatal();
                results.push((index, rollcall_id, outcome));
                // 致命錯誤（session 過期）是整個帳號的問題：取消其餘仍在等待的點名。
                if fatal {
                    error!(
                        rollcall_id = rollcall_id,
                        "批次處理遇到致命錯誤，取消其餘 rollcall"
                    );
                    break;
                }
            }
            Ok(()) = reset_rx.changed() => {
                reset_requested = true;
                break;
            }
        }
    }
    // 取消尚未完成的點名（它們的輸入登記會隨 guard 一起移除）。
    drop(running);
    if reset_requested {
        let finished: std::collections::HashSet<u64> =
            results.iter().map(|(_, rollcall_id, _)| *rollcall_id).collect();
        let mut dropped = 0usize;
        if let Some(registry) = scanner_registry.as_ref() {
            for rollcall_id in all_rollcall_ids.iter().filter(|id| !finished.contains(id)) {
                registry
                    .reset_account_pending(&config.provider, *rollcall_id, &config.id)
                    .await;
                dropped += 1;
            }
        }
        info!(
            account = %account_id,
            dropped = dropped,
            "管理員強制結束點名：已取消進行中的等待，準備重新偵測"
        );
    }
    results.sort_by_key(|(index, _, _)| *index);
    outcomes = results
        .into_iter()
        .map(|(_, _, outcome)| outcome)
        .collect();

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
    fn watchdog_only_restarts_explicit_active_pending() {
        assert_eq!(qr_watchdog_server_status(&[rc(42, "absent")], 42), ServerRollcallStatus::ActivePending);
        assert_eq!(qr_watchdog_server_status(&[rc(42, "on_call_fine")], 42), ServerRollcallStatus::AlreadyAttended);
        assert_eq!(qr_watchdog_server_status(&[rc(42, "late")], 42), ServerRollcallStatus::AlreadyAttended);
        assert_eq!(qr_watchdog_server_status(&[], 42), ServerRollcallStatus::UnknownError);
        assert_eq!(qr_watchdog_server_status(&[rc(42, "unknown")], 42), ServerRollcallStatus::UnknownError);
        assert_eq!(qr_watchdog_server_status(&[rc(99, "absent")], 42), ServerRollcallStatus::UnknownError);
        let mut expired = rc(42, "absent");
        expired.is_expired = true;
        assert_eq!(qr_watchdog_server_status(&[expired], 42), ServerRollcallStatus::ClosedOrExpired);
    }

    // QR_RECOVERY_V1：只有明確已到／遲到證據才能解除 QR 等待。
    #[test]
    fn qr_confirmation_requires_matching_positive_school_status() {
        assert!(qr_attendance_confirmed(&[rc(42, "on_call_fine")], 42));
        assert!(qr_attendance_confirmed(&[rc(42, "late")], 42));
        for status in ["absent", "", "unknown", "closed", "leave"] {
            assert!(!qr_attendance_confirmed(&[rc(42, status)], 42));
        }
        assert!(!qr_attendance_confirmed(&[], 42));
        assert!(!qr_attendance_confirmed(&[rc(99, "on_call_fine")], 42));
        let mut expired = rc(42, "absent");
        expired.is_expired = true;
        assert!(!qr_attendance_confirmed(&[expired], 42));
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
            auto_submitted: false,
        }
    }
}
