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
}

impl HomeworkDeadlineEvent {
    /// 組出各 adapter 共用的訊息文字。
    pub fn render(&self) -> String {
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
}
