//! Adapter-neutral outbound events.
//!
//! Core flows produce semantic outbound messages here. Concrete adapters decide
//! whether those messages become plain text, buttons, templates, embeds, or
//! other platform-specific UI.

use async_trait::async_trait;
use miette::Result;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Per-account adapter delivery targets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterAccountTarget {
    pub account_id: String,
    pub line_user_id: String,
    pub discord_user_id: String,
}

impl AdapterAccountTarget {
    pub fn new(
        account_id: impl Into<String>,
        line_user_id: impl Into<String>,
        discord_user_id: impl Into<String>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            line_user_id: line_user_id.into(),
            discord_user_id: discord_user_id.into(),
        }
    }
}

/// Common messaging capability implemented by concrete adapters.
#[async_trait]
pub trait AdapterMessenger: Send + Sync {
    fn adapter_name(&self) -> &'static str;

    fn admin_user_id(&self) -> &str;

    async fn reply(&self, reply_token: &str, message: &OutboundMessage) -> Result<()>;

    async fn push(&self, to: &str, message: &OutboundMessage) -> Result<()>;

    async fn push_to_user_or_admin(&self, user_id: &str, message: &OutboundMessage) -> Result<()> {
        let to = if !user_id.is_empty() {
            user_id
        } else if !self.admin_user_id().is_empty() {
            self.admin_user_id()
        } else {
            return Err(miette::miette!(
                "user_id 與 admin_user_id 均未設定，無法推送通知"
            ));
        };

        self.push(to, message).await
    }

    async fn push_to_account_or_admin(
        &self,
        target: &AdapterAccountTarget,
        message: &OutboundMessage,
    ) -> Result<()> {
        self.push_to_user_or_admin(&target.line_user_id, message)
            .await
    }
}

/// Fan-out messenger used by monitor/rollcall flows when multiple adapters are enabled.
pub struct AdapterHub {
    messengers: Vec<std::sync::Arc<dyn AdapterMessenger>>,
}

impl AdapterHub {
    pub fn new(messengers: Vec<std::sync::Arc<dyn AdapterMessenger>>) -> Self {
        Self { messengers }
    }
}

#[async_trait]
impl AdapterMessenger for AdapterHub {
    fn adapter_name(&self) -> &'static str {
        "hub"
    }

    fn admin_user_id(&self) -> &str {
        ""
    }

    async fn reply(&self, _reply_token: &str, _message: &OutboundMessage) -> Result<()> {
        Err(miette::miette!(
            "AdapterHub 不支援 reply；請使用具體 adapter"
        ))
    }

    async fn push(&self, to: &str, message: &OutboundMessage) -> Result<()> {
        for messenger in &self.messengers {
            if let Err(e) = messenger.push(to, message).await {
                warn!(
                    adapter = messenger.adapter_name(),
                    error = %e,
                    "adapter hub push 失敗"
                );
            }
        }
        Ok(())
    }

    async fn push_to_account_or_admin(
        &self,
        target: &AdapterAccountTarget,
        message: &OutboundMessage,
    ) -> Result<()> {
        for messenger in &self.messengers {
            if let Err(e) = messenger.push_to_account_or_admin(target, message).await {
                warn!(
                    adapter = messenger.adapter_name(),
                    account = %target.account_id,
                    error = %e,
                    "adapter hub account push 失敗"
                );
            }
        }
        Ok(())
    }
}

/// Adapter-neutral outbound content.
///
/// Core logic produces these semantic messages. Each adapter decides whether to
/// render them as plain text, buttons, templates, embeds, or platform-specific
/// interactive UI.
#[derive(Debug, Clone, PartialEq)]
pub enum OutboundMessage {
    Text(String),
    SecurityAlert(SecurityAlertEvent),
    SystemStarted(SystemStartedEvent),
    RollcallDetected(RollcallEvent),
    NumberCodeRequested(NumberCodeRequest),
    QrCodeRequested(QrCodeRequest),
    RollcallFinished(RollcallResultEvent),
    /// 作業截止倒數提醒。獨立成一個 variant，讓它能被通知偏好開關控管。
    HomeworkDeadline(HomeworkDeadlineEvent),
    /// 新課程公告（含 AI 重點摘要）。供 Web Push 與 Discord 等 adapter 使用。
    Bulletin(BulletinEvent),
    /// PUSH_STALE_CLEANUP_V1：手機 Web Push 訂閱失效、已被系統移除，提醒使用者重新啟用。
    PushSubscriptionExpired(PushSubscriptionExpiredEvent),
    /// ROLLCALL_INPUT_REMINDER_V1：數字／QR 點名等了 N 秒仍沒有任何輸入，再催一次。
    RollcallInputReminder(RollcallInputReminderEvent),
    /// DISCORD_NOTIFY_UNIFY_V1：監控循環自身的事件（重登成功／失敗、循環異常結束）。
    MonitorEvent(MonitorEventNotice),
    /// DISCORD_NOTIFY_UNIFY_V1：監控警示／恢復（給管理員）。
    AdminAlert(AdminAlertNotice),
    /// DISCORD_NOTIFY_UNIFY_V1：上課前提醒。
    ClassReminder(ClassReminderEvent),
    /// DISCORD_NOTIFY_UNIFY_V1：課表自動同步後有差異。
    TimetableChanged(TimetableChangedEvent),
    Help,
    Welcome,
    UnsupportedMedia,
    LocationReceived { latitude: f64, longitude: f64 },
    Status(StatusMessage),
    NotAuthorized,
    MonitorPaused,
    MonitorResumed,
    ForcePollTriggered,
    ReauthTriggered,
    QrAccepted,
    QrAmbiguousTarget,
    QrNoBoundAccount,
    QrNoPendingRequest,
    UnknownCommand { text: String },
}

/// DISCORD_NOTIFY_UNIFY_V1：監控循環事件種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorEventKind {
    /// 手動或自動重新認證成功。
    ReauthSucceeded,
    /// 手動 /reauth 失敗。
    ReauthFailed,
    /// 輪詢出錯，正在自動重新認證。
    AutoReauthStarted,
    /// 自動重新認證失敗（會依退避重試）。
    AutoReauthFailed,
    /// 監控循環異常結束，需要重啟。
    LoopCrashed,
}

/// DISCORD_NOTIFY_UNIFY_V1：監控循環事件（取代原本手寫的 ✅❌⚠️ 純文字）。
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorEventNotice {
    pub kind: MonitorEventKind,
    pub account: String,
    /// 錯誤訊息（成功事件為空）。
    pub error: String,
    /// FAILURE_DIAGNOSIS_V1：對 error 的分類與建議；成功事件為 None。
    pub diagnosis: Option<crate::monitor_alerts::FailureDiagnosis>,
}

impl MonitorEventNotice {
    pub fn title(&self) -> &'static str {
        match self.kind {
            MonitorEventKind::ReauthSucceeded => "重新認證成功",
            MonitorEventKind::ReauthFailed => "重新認證失敗",
            MonitorEventKind::AutoReauthStarted => "簽到監控遇到錯誤，正在自動重新認證",
            MonitorEventKind::AutoReauthFailed => "自動重新認證失敗",
            MonitorEventKind::LoopCrashed => "監控循環異常結束",
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self.kind, MonitorEventKind::ReauthSucceeded)
    }

    /// 各 adapter 共用的純文字（不含表情符號）。
    pub fn render(&self) -> String {
        let mut lines = vec![format!("{}：{}", self.title(), self.account)];
        if !self.error.trim().is_empty() {
            lines.push(format!("錯誤：{}", self.error.trim()));
        }
        if let Some(d) = &self.diagnosis {
            lines.push(format!("原因：{}", d.label));
            lines.push(format!("建議：{}", d.advice));
        }
        match self.kind {
            MonitorEventKind::LoopCrashed => lines.push("監控看門狗會在 30 秒內自動重新登入並重啟此帳號的監控（結果另行通知）；若反覆發生，請檢查系統 Log。".to_string()),
            MonitorEventKind::AutoReauthFailed => {
                lines.push("系統會以逐步拉長的間隔自動重試；也可輸入 /reauth 立即再試。".to_string())
            }
            MonitorEventKind::AutoReauthStarted => lines.push("不需要操作，成功後會再通知。".to_string()),
            _ => {}
        }
        lines.join("\n")
    }
}

/// DISCORD_NOTIFY_UNIFY_V1：管理員監控警示／恢復。
#[derive(Debug, Clone, PartialEq)]
pub struct AdminAlertNotice {
    pub account: String,
    /// "警告" / "嚴重" / "已恢復"。
    pub grade: String,
    pub is_recovery: bool,
    pub title: String,
    pub body: String,
}

impl AdminAlertNotice {
    pub fn render(&self) -> String {
        format!("{}\n\n{}", self.title, self.body)
    }
}

/// DISCORD_NOTIFY_UNIFY_V1：上課前提醒事件。
#[derive(Debug, Clone, PartialEq)]
pub struct ClassReminderEvent {
    pub account: String,
    pub course_name: String,
    /// 例如 "08:10 - 09:00（第 1-2 節）"。
    pub time_text: String,
    pub classroom: String,
    pub teacher: String,
    pub minutes_until: i64,
    pub configured_minutes: i64,
}

impl ClassReminderEvent {
    pub fn headline(&self) -> String {
        if self.minutes_until <= 0 {
            "現在上課".to_string()
        } else {
            format!("{} 分鐘後上課", self.minutes_until)
        }
    }

    pub fn render(&self) -> String {
        format!(
            "上課提醒｜{}\n\n課程：{}\n時間：{}\n教室：{}\n老師：{}\n帳號：{}\n\n你設定的是上課前 {} 分鐘提醒。",
            self.headline(),
            self.course_name,
            self.time_text,
            self.classroom,
            self.teacher,
            self.account,
            self.configured_minutes
        )
    }
}

/// DISCORD_NOTIFY_UNIFY_V1：課表自動更新事件。
#[derive(Debug, Clone, PartialEq)]
pub struct TimetableChangedEvent {
    pub account: String,
    /// 已 humanize 的新增項目（最多 5 筆）。
    pub added: Vec<String>,
    pub added_total: usize,
    pub removed: Vec<String>,
    pub removed_total: usize,
}

impl TimetableChangedEvent {
    pub fn render(&self) -> String {
        let mut lines = vec!["課表已自動更新".to_string()];
        if self.added_total > 0 {
            lines.push(format!("新增 {} 堂：", self.added_total));
            for item in &self.added {
                lines.push(format!("  ＋ {item}"));
            }
            if self.added_total > self.added.len() {
                lines.push(format!("  …等 {} 堂", self.added_total));
            }
        }
        if self.removed_total > 0 {
            lines.push(format!("移除 {} 堂：", self.removed_total));
            for item in &self.removed {
                lines.push(format!("  － {item}"));
            }
            if self.removed_total > self.removed.len() {
                lines.push(format!("  …等 {} 堂", self.removed_total));
            }
        }
        lines.push(format!("帳號：{}", self.account));
        lines.join("\n")
    }
}

/// 作業截止倒數提醒事件。
#[derive(Debug, Clone, PartialEq)]
pub struct HomeworkDeadlineEvent {
    pub account: String,
    pub course_name: String,
    pub title: String,
    /// 階段徽章，例如「最後 1 小時」。
    pub stage_badge: String,
    /// 截止時間（本地時間字串）。
    pub deadline: String,
    /// 剩餘時間描述，例如「約 5 小時」。
    pub time_remaining: String,
    /// HOMEWORK_NEW_NOTIFY_V1：`true`＝「新上架作業」通知（不是倒數提醒）。
    ///
    /// 兩者共用同一個事件型別，是因為它們走**完全相同的發送路徑與通知偏好開關**
    /// （`homework_deadline`），差別只有訊息文案。
    pub is_new: bool,
}

impl HomeworkDeadlineEvent {
    /// 組出各 adapter 共用的訊息文字。
    pub fn render(&self) -> String {
        // HOMEWORK_NEW_NOTIFY_V1：新作業通知的文案與倒數提醒不同——
        // 標題不能寫「截止提醒」（距離截止還很久），並引導到作業頁。
        if self.is_new {
            return format!(
                "🆕 新作業通知｜{}\n\n\
                 課程：{}\n\
                 標題：{}\n\
                 截止：{}\n\
                 剩餘：{}\n\
                 帳號：{}\n\n\
                 可至面板查看作業內容與附件，或直接線上繳交。",
                self.stage_badge,
                self.course_name,
                self.title,
                self.deadline,
                self.time_remaining,
                self.account
            );
        }

        format!(
            "📝 作業截止提醒｜{}\n\n\
             課程：{}\n\
             標題：{}\n\
             截止：{}\n\
             剩餘：{}\n\
             帳號：{}\n\n\
             請儘速至 Tronclass 確認並繳交。",
            self.stage_badge,
            self.course_name,
            self.title,
            self.deadline,
            self.time_remaining,
            self.account
        )
    }
}

/// PUSH_STALE_CLEANUP_V1：手機推播訂閱失效事件。
#[derive(Debug, Clone, PartialEq)]
pub struct PushSubscriptionExpiredEvent {
    pub account: String,
    /// 該帳號本次被移除的裝置數。
    pub removed_devices: usize,
    /// 移除後仍有效的裝置數（0 代表這個帳號已完全收不到手機推播）。
    pub remaining_devices: usize,
    /// 最後一次失敗的原因（已截短），例如「HTTP 403」「連續 5 次失敗」。
    pub reason: String,
    /// 重新啟用推播的頁面網址；未設定 public_base_url 時為空。
    pub dashboard_url: String,
}

impl PushSubscriptionExpiredEvent {
    /// 組出各 adapter 共用的訊息文字（不含表情符號）。
    pub fn render(&self) -> String {
        let scope = if self.remaining_devices == 0 {
            "這個帳號目前已完全收不到手機背景推播（點名、作業、公告、晨間摘要）。".to_string()
        } else {
            format!(
                "仍有 {} 台裝置有效，但失效的那台不會再收到手機推播。",
                self.remaining_devices
            )
        };
        let how = if self.dashboard_url.trim().is_empty() {
            "請用手機開啟 Dashboard，到「手機推播狀態」重新啟用背景推播。".to_string()
        } else {
            format!(
                "請用手機開啟 {} 重新啟用背景推播。",
                self.dashboard_url.trim()
            )
        };
        format!(
            "手機推播訂閱已失效\n\n\
             帳號：{}\n\
             已移除裝置：{} 台\n\
             原因：{}\n\n\
             {}\n{}",
            self.account, self.removed_devices, self.reason, scope, how
        )
    }
}

/// ROLLCALL_INPUT_REMINDER_V1：點名輸入催促事件。
#[derive(Debug, Clone, PartialEq)]
pub struct RollcallInputReminderEvent {
    pub rollcall_id: u64,
    pub account: String,
    pub course_name: String,
    /// 「數字簽到」或「QR Code 簽到」。
    pub attendance_type: String,
    /// 該帳號專屬的輸入頁／掃碼頁；未設定 public_base_url 時為空。
    pub input_url: String,
    /// 已等待秒數。
    pub waited_secs: u64,
    /// 距離逾時還剩幾秒。
    pub remaining_secs: u64,
}

impl RollcallInputReminderEvent {
    pub fn is_number(&self) -> bool {
        self.attendance_type.contains("數字")
    }

    /// 各 adapter 共用的催促文字（不含表情符號）。
    pub fn render(&self) -> String {
        let what = if self.is_number() {
            "還沒收到數字點名碼"
        } else {
            "還沒收到 QR Code"
        };
        let page = if self.input_url.trim().is_empty() {
            String::new()
        } else if self.is_number() {
            format!("\n數字點名頁：{}", self.input_url.trim())
        } else {
            format!("\n掃碼頁：{}", self.input_url.trim())
        };
        format!(
            "點名提醒：{what}\n\n\
             課程：{}\n\
             帳號：{}\n\
             點名 ID：{}\n\
             已等待 {} 秒，還剩 {} 秒就會逾時。{}",
            self.course_name,
            self.account,
            self.rollcall_id,
            self.waited_secs,
            self.remaining_secs,
            page
        )
    }
}

/// 新課程公告事件（含 AI 重點摘要）。
#[derive(Debug, Clone, PartialEq)]
pub struct BulletinEvent {
    pub account: String,
    pub course_name: String,
    pub title: String,
    /// AI 生成的重點摘要（精簡、保留關鍵資訊）；可能為空字串（摘要失敗時退回標題）。
    pub summary: String,
}

impl BulletinEvent {
    /// 組出各 adapter 共用的通知文字：短、保留重點。
    pub fn render(&self) -> String {
        let head = format!("📢 新公告｜{}", self.course_name);
        let body = if self.summary.trim().is_empty() {
            format!("【{}】", self.title)
        } else {
            format!("【{}】\n{}", self.title, self.summary)
        };
        format!("{head}\n{body}\n\n帳號：{}", self.account)
    }
}

/// Security alert emitted after repeated failed Dashboard authentication.
///
/// The IP is normalized by the server before this event is created and is
/// rendered as a Discord component ID only for administrator recipients.
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityAlertEvent {
    pub username: String,
    pub ip: String,
    pub user_failures: u32,
    pub ip_failures: u32,
    pub timestamp: String,
    /// True when this alert is emitted after the server has already blocked
    /// the IP automatically. Adapters can then offer an unblock action rather
    /// than a redundant block action.
    pub ip_is_blacklisted: bool,
    /// Account that caused an automatic block. Used only for self-service
    /// Discord unblock authorization; never rendered as a credential.
    pub account_id: String,
    pub allow_owner_unblock: bool,
    /// LOGIN_TIERED_LOCKOUT_V1：第一層鎖定（帳號密碼登入暫時鎖住、IP 尚未封鎖）。
    /// Discord 端據此改渲染「取消這次鎖定」＋「封鎖此 IP」兩顆按鈕給帳號持有人。
    pub login_locked: bool,
}

/// Outbound event emitted when a monitored account starts.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemStartedEvent {
    pub account: String,
    pub user_name: String,
    pub poll_interval_secs: u64,
    pub adapter_name: String,
}

/// Outbound event emitted when a pending rollcall is found.
#[derive(Debug, Clone, PartialEq)]
pub struct RollcallEvent {
    pub rollcall_id: u64,
    pub account: String,
    pub course_name: String,
    pub teacher_name: String,
    pub attendance_type: String,
}

/// Outbound event asking the user to provide a number rollcall code or choose auto lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct NumberCodeRequest {
    pub rollcall_id: u64,
    pub account: String,
    pub course_name: String,
    pub teacher_name: String,
    pub input_url: String,
    pub alt_input_url: String,
    pub timeout_secs: u64,
}

/// Outbound event asking the user to provide QR Code input.
#[derive(Debug, Clone, PartialEq)]
pub struct QrCodeRequest {
    pub rollcall_id: u64,
    pub account: String,
    pub course_name: String,
    pub teacher_name: String,
    pub scan_url: String,
    pub alt_scan_url: String,
    pub timeout_secs: u64,
}

/// Outbound event emitted when a rollcall attempt finishes.
#[derive(Debug, Clone, PartialEq)]
pub struct RollcallResultEvent {
    pub rollcall_id: u64,
    pub account: String,
    pub course_name: String,
    pub attendance_type: String,
    pub success: bool,
    pub result: String,
    pub elapsed_ms: u64,
}

/// Status payload to be rendered by an adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusMessage {
    NoAccounts,
    Single(MonitorStatus),
    UserAccount {
        account_id: String,
        status: MonitorStatus,
    },
    AdminAccounts(Vec<AccountStatusMessage>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountStatusMessage {
    pub account_id: String,
    pub status: MonitorStatus,
}

/// Recent rollcall result shown in Web admin history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollcallHistoryEntry {
    /// Unix timestamp in seconds.
    pub timestamp: i64,
    /// Tronclass rollcall ID.
    pub rollcall_id: u64,
    /// Course name/title.
    pub course_name: String,
    /// Attendance type, e.g. 數字簽到 / QR Code 簽到 / 雷達簽到.
    pub attendance_type: String,
    /// Whether this attempt succeeded.
    pub success: bool,
    /// Rendered result detail.
    pub result: String,
}

/// Shared monitor status used by adapter status replies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorStatus {
    pub is_running: bool,
    pub user_name: String,
    pub last_poll_timestamp: Option<i64>,
    pub last_success_course: Option<String>,
    /// 最近一次成功點名的 rollcall ID。
    #[serde(default)]
    pub last_success_rollcall_id: Option<u64>,
    /// 最近一次偵測到待簽到點名的時間戳。
    #[serde(default)]
    pub last_detected_timestamp: Option<i64>,
    /// 最近一次偵測到待簽到點名的課程名稱。
    #[serde(default)]
    pub last_detected_course: Option<String>,
    /// 最近一次偵測到待簽到點名的 rollcall ID。
    #[serde(default)]
    pub last_detected_rollcall_id: Option<u64>,
    /// 最近一次點名對應的操作頁 URL（例如 /number 或 /scanner）。
    #[serde(default)]
    pub last_detected_url: Option<String>,
    /// 最近點名處理紀錄，新的在前面。
    #[serde(default)]
    pub rollcall_history: Vec<RollcallHistoryEntry>,
    pub consecutive_failures: u32,
    pub started_at: i64,
    /// FAILURE_DIAGNOSIS_V1：最近一次輪詢／重登失敗的分類與自助建議；成功後清空。
    #[serde(default)]
    pub last_failure: Option<crate::monitor_alerts::FailureDiagnosis>,
    /// REAUTH_VISIBLE_V1：最近一次重新認證（手動 /reauth 或連續失敗後自動）的時間與結果，
    /// 讓儀表板帳號列看得到「自動重登有沒有在動、上次成不成功」。
    #[serde(default)]
    pub last_reauth: Option<ReauthRecord>,
}

/// REAUTH_VISIBLE_V1：單次重新認證的紀錄。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReauthRecord {
    /// 發生時間（Unix 秒）。
    pub at: i64,
    /// 是否成功。
    pub ok: bool,
    /// 觸發來源：`manual`（/reauth、儀表板「重登」）或 `auto`（連續失敗／認證錯誤）。
    pub trigger: String,
    /// 失敗時的截短錯誤訊息；成功為空字串。
    pub message: String,
}

impl ReauthRecord {
    pub fn new(ok: bool, trigger: &str, message: &str) -> Self {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self {
            at,
            ok,
            trigger: trigger.to_string(),
            message: crate::text_utils::truncate_chars(message.trim(), 200).to_string(),
        }
    }
}
