//! Adapter inbound requests and request channels.
//!
//! Platform adapters convert user messages into `AdapterRequest` values and
//! delegate them here. This module only translates those requests into shared
//! state reads, monitor control signals, or input-channel delivery.

use std::sync::Arc;

use miette::Result;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, instrument, warn};

use crate::adapters::events::{
    AccountStatusMessage, AdapterAccountTarget, AdapterMessenger, MonitorStatus, OutboundMessage,
    StatusMessage,
};

/// QR Code input sent by an adapter and consumed by the QR Code rollcall flow.
pub type QrCodeSender = mpsc::Sender<String>;
pub type QrCodeReceiver = Arc<Mutex<mpsc::Receiver<String>>>;

/// Manual number-code action sent by an adapter and consumed by the number rollcall flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NumberCodeInput {
    /// User manually entered a 4-digit code.
    Code(String),
    /// User explicitly chose to auto-read `number_code` from the Tronclass API.
    AutoLookup,
}

pub type NumberCodeSender = mpsc::Sender<NumberCodeInput>;
pub type NumberCodeReceiver = Arc<Mutex<mpsc::Receiver<NumberCodeInput>>>;

/// Which per-account platform binding a `RequestState` should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterBindingKind {
    Line,
    Discord,
}

/// Per-account request channels exposed to adapter event handlers.
#[derive(Clone)]
pub struct RequestChannels {
    pub qr_tx: QrCodeSender,
    pub number_tx: NumberCodeSender,
}

impl RequestChannels {
    pub fn new(qr_tx: QrCodeSender, number_tx: NumberCodeSender) -> Self {
        Self { qr_tx, number_tx }
    }
}

/// Build a QR Code input channel.
pub fn create_qrcode_channel(buffer: usize) -> (QrCodeSender, QrCodeReceiver) {
    let (tx, rx) = mpsc::channel(buffer);
    (tx, Arc::new(Mutex::new(rx)))
}

/// Build a manual number-code input channel.
pub fn create_number_code_channel(buffer: usize) -> (NumberCodeSender, NumberCodeReceiver) {
    let (tx, rx) = mpsc::channel(buffer);
    (tx, Arc::new(Mutex::new(rx)))
}

/// Adapter message/request converted from a platform webhook event.
#[derive(Debug, Clone)]
pub struct AdapterRequest {
    pub user_id: String,
    pub reply_token: Option<String>,
    pub is_direct_user: bool,
    pub content: RequestContent,
}

impl AdapterRequest {
    pub fn new(
        user_id: impl Into<String>,
        reply_token: Option<String>,
        is_direct_user: bool,
        content: RequestContent,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            reply_token,
            is_direct_user,
            content,
        }
    }
}

/// Adapter-neutral inbound request content.
#[derive(Debug, Clone)]
pub enum RequestContent {
    Text(String),
    Sticker,
    Media,
    Location { latitude: f64, longitude: f64 },
    Follow,
    Unfollow,
    Join,
    Leave,
    Unknown,
}

/// User command parsed from inbound text or postback data.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestCommand {
    GetStatus,
    ForceAttend,
    Stop,
    Start,
    ReAuth,
    QrCode(String),
    NumberCode(String),
    NumberAutoLookup { account_id: Option<String> },
    Help,
    Unknown(String),
}

impl RequestCommand {
    pub fn parse(text: &str) -> Self {
        let text = text.trim();

        if Self::looks_like_qr_code(text) {
            return RequestCommand::QrCode(text.to_string());
        }

        if let Some(code) = Self::parse_number_code(text) {
            return RequestCommand::NumberCode(code);
        }

        match text.to_lowercase().as_str() {
            "/status" | "status" | "狀態" | "查詢" => RequestCommand::GetStatus,
            "/force" | "force" | "強制簽到" | "手動簽到" => RequestCommand::ForceAttend,
            "/stop" | "stop" | "停止" | "暫停" => RequestCommand::Stop,
            "/start" | "start" | "啟動" | "開始" => RequestCommand::Start,
            "/reauth" | "reauth" | "重新登錄" | "重新認證" => RequestCommand::ReAuth,
            "/help" | "help" | "幫助" | "說明" | "?" | "？" => RequestCommand::Help,
            _ => RequestCommand::Unknown(text.to_string()),
        }
    }

    fn looks_like_qr_code(text: &str) -> bool {
        if text.contains("elearn2.fju.edu.tw")
            || text.contains("/scanner-jumper?p=")
            || text.contains("/j?p=")
        {
            return true;
        }

        if text.contains('~') && text.contains('!') {
            let looks_like_segments = text.split('!').take(2).all(|s| s.contains('~'));
            if looks_like_segments {
                return true;
            }
        }

        false
    }

    fn parse_number_code(text: &str) -> Option<String> {
        let lower = text.trim().to_ascii_lowercase();
        let candidate = lower
            .strip_prefix("/num ")
            .or_else(|| lower.strip_prefix("num "))
            .or_else(|| lower.strip_prefix("/number "))
            .or_else(|| lower.strip_prefix("number "))
            .or_else(|| lower.strip_prefix("/code "))
            .or_else(|| lower.strip_prefix("code "))
            .unwrap_or(lower.as_str())
            .trim();
        let candidate = candidate
            .strip_prefix("code:")
            .or_else(|| candidate.strip_prefix("code="))
            .or_else(|| candidate.strip_prefix("code "))
            .unwrap_or(candidate)
            .trim();

        if candidate.len() == 4 && candidate.chars().all(|c| c.is_ascii_digit()) {
            Some(candidate.to_string())
        } else {
            None
        }
    }
}

impl std::fmt::Display for RequestCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestCommand::GetStatus => write!(f, "GetStatus"),
            RequestCommand::ForceAttend => write!(f, "ForceAttend"),
            RequestCommand::Stop => write!(f, "Stop"),
            RequestCommand::Start => write!(f, "Start"),
            RequestCommand::ReAuth => write!(f, "ReAuth"),
            RequestCommand::QrCode(data) => {
                // REDIRECT_UTF8_SLICE_V1：QR data 可能含任意 UTF-8，改用字元截斷。
                write!(f, "QrCode({}...)", crate::text_utils::truncate_chars(data, 20))
            }
            RequestCommand::NumberCode(code) => write!(f, "NumberCode({code})"),
            RequestCommand::NumberAutoLookup { account_id } => {
                write!(
                    f,
                    "NumberAutoLookup({})",
                    account_id.as_deref().unwrap_or("*")
                )
            }
            RequestCommand::Help => write!(f, "Help"),
            RequestCommand::Unknown(text) => write!(f, "Unknown({text})"),
        }
    }
}

/// Single account exposed to adapter request handling.
#[derive(Clone)]
pub struct RequestAccountState {
    pub account_id: String,
    pub username: String,
    pub target: Arc<Mutex<AdapterAccountTarget>>,
    pub requests: RequestChannels,
    pub monitor_status: Arc<Mutex<MonitorStatus>>,
    pub is_running: Arc<Mutex<bool>>,
    pub force_poll_tx: Arc<tokio::sync::Notify>,
    pub reauth_tx: Arc<tokio::sync::Notify>,
}

impl RequestAccountState {
    pub fn new(
        account_id: impl Into<String>,
        user_id: impl Into<String>,
        qr_tx: QrCodeSender,
        number_tx: NumberCodeSender,
        monitor_status: Arc<Mutex<MonitorStatus>>,
    ) -> Self {
        let account_id = account_id.into();
        let target = AdapterAccountTarget::new(account_id.clone(), user_id, "");
        Self::new_with_target(
            account_id,
            "",
            Arc::new(Mutex::new(target)),
            qr_tx,
            number_tx,
            monitor_status,
        )
    }

    pub fn new_with_target(
        account_id: impl Into<String>,
        username: impl Into<String>,
        target: Arc<Mutex<AdapterAccountTarget>>,
        qr_tx: QrCodeSender,
        number_tx: NumberCodeSender,
        monitor_status: Arc<Mutex<MonitorStatus>>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            username: username.into(),
            target,
            requests: RequestChannels::new(qr_tx, number_tx),
            monitor_status,
            is_running: Arc::new(Mutex::new(true)),
            force_poll_tx: Arc::new(tokio::sync::Notify::new()),
            reauth_tx: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

/// Shared state used by adapter request handlers.
#[derive(Clone)]
pub struct RequestState {
    pub messenger: Arc<dyn AdapterMessenger>,
    pub accounts: Arc<Vec<RequestAccountState>>,
    pub binding_kind: AdapterBindingKind,
}

impl RequestState {
    pub fn new(messenger: Arc<dyn AdapterMessenger>, accounts: Vec<RequestAccountState>) -> Self {
        Self::new_with_binding(messenger, accounts, AdapterBindingKind::Line)
    }

    pub fn new_with_binding(
        messenger: Arc<dyn AdapterMessenger>,
        accounts: Vec<RequestAccountState>,
        binding_kind: AdapterBindingKind,
    ) -> Self {
        Self {
            messenger,
            accounts: Arc::new(accounts),
            binding_kind,
        }
    }

    fn first_account(&self) -> Option<&RequestAccountState> {
        self.accounts.first()
    }

    async fn account_for_user(&self, user_id: &str) -> Option<&RequestAccountState> {
        if user_id.is_empty() {
            return None;
        }

        for account in self.accounts.iter() {
            let target = account.target.lock().await;
            let bound_user_id = match self.binding_kind {
                AdapterBindingKind::Line => target.line_user_id.as_str(),
                AdapterBindingKind::Discord => target.discord_user_id.as_str(),
            };
            if !bound_user_id.is_empty() && bound_user_id == user_id {
                return Some(account);
            }
        }

        None
    }

    fn is_admin_user(&self, user_id: &str) -> bool {
        !self.messenger.admin_user_id().is_empty() && user_id == self.messenger.admin_user_id()
    }

    fn admin_is_unrestricted(&self) -> bool {
        self.messenger.admin_user_id().is_empty()
    }

    pub async fn set_discord_user_id(&self, account_id: &str, discord_user_id: &str) -> bool {
        for account in self.accounts.iter() {
            if account.account_id == account_id {
                let mut target = account.target.lock().await;
                target.discord_user_id = discord_user_id.to_string();
                return true;
            }
        }

        false
    }

    pub async fn discord_bindings(&self) -> Vec<(String, String)> {
        let mut bindings = Vec::with_capacity(self.accounts.len());
        for account in self.accounts.iter() {
            let target = account.target.lock().await;
            bindings.push((account.username.clone(), target.discord_user_id.clone()));
        }
        bindings
    }
}

#[instrument(skip(request, state), fields(adapter = state.messenger.adapter_name()))]
pub async fn handle_request(request: AdapterRequest, state: &RequestState) -> Result<()> {
    match request.content {
        RequestContent::Text(text) => {
            if !request.is_direct_user {
                debug!("忽略非個人對話來源的文字請求");
                return Ok(());
            }

            let cmd = RequestCommand::parse(&text);
            info!(command = %cmd, "解析請求指令");
            execute_command(
                cmd,
                request.reply_token.as_deref().unwrap_or(""),
                state,
                &request.user_id,
            )
            .await?;
        }
        RequestContent::Sticker => {
            if request.is_direct_user {
                reply_current_status(
                    request.reply_token.as_deref().unwrap_or(""),
                    state,
                    &request.user_id,
                )
                .await?;
            }
        }
        RequestContent::Media => {
            if request.is_direct_user {
                reply_if_possible(
                    request.reply_token.as_deref().unwrap_or(""),
                    state,
                    OutboundMessage::UnsupportedMedia,
                )
                .await?;
            }
        }
        RequestContent::Location {
            latitude,
            longitude,
        } => {
            if request.is_direct_user {
                reply_if_possible(
                    request.reply_token.as_deref().unwrap_or(""),
                    state,
                    OutboundMessage::LocationReceived {
                        latitude,
                        longitude,
                    },
                )
                .await?;
            }
        }
        RequestContent::Follow => {
            info!(user_id = %request.user_id, "新使用者追蹤 adapter");
            reply_if_possible(
                request.reply_token.as_deref().unwrap_or(""),
                state,
                OutboundMessage::Welcome,
            )
            .await?;
        }
        RequestContent::Unfollow => {
            info!(user_id = %request.user_id, "使用者取消追蹤 adapter");
        }
        RequestContent::Join => info!("adapter 被加入群組或多人聊天室"),
        RequestContent::Leave => info!("adapter 離開群組或多人聊天室"),
        RequestContent::Unknown => debug!("收到未知或未處理的 adapter 請求，忽略"),
    }

    Ok(())
}

#[instrument(skip(reply_token, state), fields(command = %cmd))]
pub async fn execute_command(
    cmd: RequestCommand,
    reply_token: &str,
    state: &RequestState,
    user_id: &str,
) -> Result<()> {
    let is_admin = state.admin_is_unrestricted() || state.is_admin_user(user_id);

    match cmd {
        RequestCommand::GetStatus => reply_current_status(reply_token, state, user_id).await?,
        RequestCommand::Stop => {
            if !is_admin {
                reply_not_authorized(reply_token, state).await?;
                return Ok(());
            }

            set_all_running(state, false).await;
            info!("收到 /stop 指令，暫停監控");
            reply_if_possible(reply_token, state, OutboundMessage::MonitorPaused).await?;
        }
        RequestCommand::Start => {
            if !is_admin {
                reply_not_authorized(reply_token, state).await?;
                return Ok(());
            }

            set_all_running(state, true).await;
            notify_all_force_poll(state);
            info!("收到 /start 指令，恢復監控");
            reply_if_possible(reply_token, state, OutboundMessage::MonitorResumed).await?;
        }
        RequestCommand::ForceAttend => {
            if is_admin {
                notify_all_force_poll(state);
                info!("收到管理員 /force 指令，強制觸發所有帳號簽到檢查");
            } else if let Some(account) = state.account_for_user(user_id).await {
                // 為了避免等某個帳號先偵測到點名才 cascade，任何已綁定使用者 /force
                // 都會立即喚醒所有帳號檢查；未綁定者仍不能觸發。
                notify_all_force_poll(state);
                info!(
                    account = %account.account_id,
                    user_id = %user_id,
                    "收到使用者 /force 指令，立即強制觸發所有帳號簽到檢查"
                );
            } else {
                reply_if_possible(reply_token, state, OutboundMessage::QrNoBoundAccount).await?;
                return Ok(());
            }

            reply_if_possible(reply_token, state, OutboundMessage::ForcePollTriggered).await?;
        }
        RequestCommand::ReAuth => {
            if is_admin {
                notify_all_reauth(state);
                info!("收到管理員 /reauth 指令，觸發所有帳號重新認證");
            } else if let Some(account) = state.account_for_user(user_id).await {
                account.reauth_tx.notify_one();
                info!(
                    account = %account.account_id,
                    user_id = %user_id,
                    "收到使用者 /reauth 指令，觸發綁定帳號重新認證"
                );
            } else {
                reply_if_possible(reply_token, state, OutboundMessage::QrNoBoundAccount).await?;
                return Ok(());
            }

            reply_if_possible(reply_token, state, OutboundMessage::ReauthTriggered).await?;
        }
        RequestCommand::QrCode(qr_data) => {
            info!(data_len = qr_data.len(), "收到 QR code 資料");
            let account = if is_admin {
                if state.accounts.len() == 1 {
                    state.first_account()
                } else {
                    None
                }
            } else {
                state.account_for_user(user_id).await
            };

            let Some(account) = account else {
                let message = if is_admin {
                    OutboundMessage::QrAmbiguousTarget
                } else {
                    OutboundMessage::QrNoBoundAccount
                };
                reply_if_possible(reply_token, state, message).await?;
                return Ok(());
            };

            match account.requests.qr_tx.send(qr_data.clone()).await {
                Ok(_) => {
                    debug!("QR code 資料已傳送到簽到模組");
                    reply_if_possible(reply_token, state, OutboundMessage::QrAccepted).await?;
                }
                Err(e) => {
                    warn!(error = %e, "QR code 傳送失敗（通道可能已關閉或沒有等待中的簽到）");
                    reply_if_possible(reply_token, state, OutboundMessage::QrNoPendingRequest)
                        .await?;
                }
            }
        }
        RequestCommand::NumberCode(code) => {
            if !is_valid_number_code(&code) {
                reply_if_possible(
                    reply_token,
                    state,
                    OutboundMessage::Text(
                        "數字點名碼必須是 4 位數字，例如 /number code:1234。".to_string(),
                    ),
                )
                .await?;
                return Ok(());
            }

            let targets: Vec<&RequestAccountState> = if is_admin {
                state.accounts.iter().collect()
            } else {
                match state.account_for_user(user_id).await {
                    Some(account) => vec![account],
                    None => {
                        reply_if_possible(reply_token, state, OutboundMessage::QrNoBoundAccount)
                            .await?;
                        return Ok(());
                    }
                }
            };

            let mut sent = 0usize;
            for account in targets {
                match account
                    .requests
                    .number_tx
                    .send(NumberCodeInput::Code(code.clone()))
                    .await
                {
                    Ok(_) => sent += 1,
                    Err(e) => warn!(
                        account = %account.account_id,
                        error = %e,
                        "數字點名碼傳送失敗（通道可能已關閉）"
                    ),
                }
            }

            if sent == 0 {
                reply_if_possible(
                    reply_token,
                    state,
                    OutboundMessage::Text("目前沒有可接收數字點名碼的帳號。".to_string()),
                )
                .await?;
            } else {
                info!(code = %code, targets = sent, "數字點名碼已傳送到等待中的帳號");
                reply_if_possible(
                    reply_token,
                    state,
                    OutboundMessage::Text(format!(
                        "已收到數字點名碼 `{code}`，送出給 {sent} 個帳號等待流程。"
                    )),
                )
                .await?;
            }
        }
        RequestCommand::NumberAutoLookup { account_id } => {
            let targets: Vec<&RequestAccountState> = if let Some(account_id) = account_id {
                let Some(account) = state
                    .accounts
                    .iter()
                    .find(|account| account.account_id == account_id)
                else {
                    reply_if_possible(
                        reply_token,
                        state,
                        OutboundMessage::Text(format!("找不到帳號 `{account_id}`。")),
                    )
                    .await?;
                    return Ok(());
                };

                if !is_admin {
                    let bound_account = state.account_for_user(user_id).await;
                    if bound_account.map(|bound| bound.account_id.as_str())
                        != Some(account.account_id.as_str())
                    {
                        reply_not_authorized(reply_token, state).await?;
                        return Ok(());
                    }
                }
                vec![account]
            } else if is_admin {
                state.accounts.iter().collect()
            } else {
                match state.account_for_user(user_id).await {
                    Some(account) => vec![account],
                    None => {
                        reply_if_possible(reply_token, state, OutboundMessage::QrNoBoundAccount)
                            .await?;
                        return Ok(());
                    }
                }
            };

            let mut sent = 0usize;
            for account in targets {
                match account
                    .requests
                    .number_tx
                    .send(NumberCodeInput::AutoLookup)
                    .await
                {
                    Ok(_) => sent += 1,
                    Err(e) => warn!(
                        account = %account.account_id,
                        error = %e,
                        "自動讀取數字點名碼請求傳送失敗（通道可能已關閉）"
                    ),
                }
            }

            reply_if_possible(
                reply_token,
                state,
                OutboundMessage::Text(format!(
                    "已選擇自動讀取數字點名碼，送出給 {sent} 個帳號等待流程。"
                )),
            )
            .await?;
        }
        RequestCommand::Help => {
            reply_if_possible(reply_token, state, OutboundMessage::Help).await?;
        }
        RequestCommand::Unknown(text) => {
            debug!(text = %text, "收到未知指令或純文字");
            reply_if_possible(reply_token, state, OutboundMessage::UnknownCommand { text }).await?;
        }
    }

    Ok(())
}

async fn reply_if_possible(
    reply_token: &str,
    state: &RequestState,
    message: OutboundMessage,
) -> Result<()> {
    if reply_token.is_empty() {
        return Ok(());
    }

    state.messenger.reply(reply_token, &message).await
}

pub async fn reply_current_status(
    reply_token: &str,
    state: &RequestState,
    user_id: &str,
) -> Result<()> {
    if reply_token.is_empty() {
        return Ok(());
    }

    let is_admin = state.admin_is_unrestricted() || state.is_admin_user(user_id);
    let message = status_message_for_user(state, user_id, is_admin).await;
    state.messenger.reply(reply_token, &message).await?;
    Ok(())
}

pub async fn status_message_for_user(
    state: &RequestState,
    user_id: &str,
    is_admin: bool,
) -> OutboundMessage {
    if is_admin {
        return OutboundMessage::Status(admin_status_message(state).await);
    }

    match state.account_for_user(user_id).await {
        Some(account) => {
            let status = account.monitor_status.lock().await;
            OutboundMessage::Status(StatusMessage::UserAccount {
                account_id: account.account_id.clone(),
                status: status.clone(),
            })
        }
        None => OutboundMessage::QrNoBoundAccount,
    }
}

async fn admin_status_message(state: &RequestState) -> StatusMessage {
    if state.accounts.is_empty() {
        return StatusMessage::NoAccounts;
    }

    if state.accounts.len() == 1 {
        let account = &state.accounts[0];
        let status = account.monitor_status.lock().await;
        return StatusMessage::Single(status.clone());
    }

    let mut accounts = Vec::with_capacity(state.accounts.len());
    for account in state.accounts.iter() {
        let status = account.monitor_status.lock().await;
        accounts.push(AccountStatusMessage {
            account_id: account.account_id.clone(),
            status: status.clone(),
        });
    }

    StatusMessage::AdminAccounts(accounts)
}

async fn set_all_running(state: &RequestState, is_running: bool) {
    for account in state.accounts.iter() {
        let mut running = account.is_running.lock().await;
        *running = is_running;

        let mut status = account.monitor_status.lock().await;
        status.is_running = is_running;
    }
}

fn notify_all_force_poll(state: &RequestState) {
    for account in state.accounts.iter() {
        account.force_poll_tx.notify_one();
    }
}

fn notify_all_reauth(state: &RequestState) {
    for account in state.accounts.iter() {
        account.reauth_tx.notify_one();
    }
}

fn is_valid_number_code(code: &str) -> bool {
    code.len() == 4 && code.chars().all(|c| c.is_ascii_digit())
}

async fn reply_not_authorized(reply_token: &str, state: &RequestState) -> Result<()> {
    reply_if_possible(reply_token, state, OutboundMessage::NotAuthorized).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    #[derive(Debug)]
    struct TestMessenger {
        admin: String,
        replies: Arc<Mutex<Vec<OutboundMessage>>>,
        pushes: Arc<Mutex<Vec<(String, OutboundMessage)>>>,
    }

    impl TestMessenger {
        fn new(admin: &str) -> Self {
            Self {
                admin: admin.to_string(),
                replies: Arc::new(Mutex::new(vec![])),
                pushes: Arc::new(Mutex::new(vec![])),
            }
        }
    }

    #[async_trait]
    impl AdapterMessenger for TestMessenger {
        fn adapter_name(&self) -> &'static str {
            "test"
        }

        fn admin_user_id(&self) -> &str {
            &self.admin
        }

        async fn reply(&self, _reply_token: &str, message: &OutboundMessage) -> Result<()> {
            self.replies.lock().await.push(message.clone());
            Ok(())
        }

        async fn push(&self, to: &str, message: &OutboundMessage) -> Result<()> {
            self.pushes
                .lock()
                .await
                .push((to.to_string(), message.clone()));
            Ok(())
        }
    }

    fn make_test_status(user_name: &str, is_running: bool) -> Arc<Mutex<MonitorStatus>> {
        Arc::new(Mutex::new(MonitorStatus {
            is_running,
            user_name: user_name.to_string(),
            last_poll_timestamp: None,
            last_success_course: None,
            last_success_rollcall_id: None,
            last_detected_timestamp: None,
            last_detected_course: None,
            last_detected_rollcall_id: None,
            last_detected_url: None,
            rollcall_history: Vec::new(),
            consecutive_failures: 0,
            started_at: 0,
        }))
    }

    fn make_state(accounts: Vec<RequestAccountState>) -> RequestState {
        RequestState::new(Arc::new(TestMessenger::new("Uadmin123")), accounts)
    }

    fn make_account(
        account_id: &str,
        user_id: &str,
    ) -> (RequestAccountState, tokio::sync::mpsc::Receiver<String>) {
        let (qr_tx, qr_rx) = tokio::sync::mpsc::channel(10);
        let (number_tx, _number_rx) = tokio::sync::mpsc::channel(10);
        (
            RequestAccountState::new(
                account_id,
                user_id,
                qr_tx,
                number_tx,
                make_test_status(account_id, true),
            ),
            qr_rx,
        )
    }

    #[test]
    fn test_parse_command_status() {
        assert_eq!(RequestCommand::parse("/status"), RequestCommand::GetStatus);
        assert_eq!(RequestCommand::parse("status"), RequestCommand::GetStatus);
        assert_eq!(RequestCommand::parse("狀態"), RequestCommand::GetStatus);
        assert_eq!(RequestCommand::parse("查詢"), RequestCommand::GetStatus);
    }

    #[test]
    fn test_parse_command_qr_code_p_param() {
        assert!(matches!(
            RequestCommand::parse("0~12345!3~mydata!4~67890"),
            RequestCommand::QrCode(_)
        ));
    }

    #[test]
    fn test_parse_command_number_code() {
        assert_eq!(
            RequestCommand::parse("/num 1234"),
            RequestCommand::NumberCode("1234".to_string())
        );
        assert_eq!(
            RequestCommand::parse("/num code:1234"),
            RequestCommand::NumberCode("1234".to_string())
        );
        assert_eq!(
            RequestCommand::parse("5678"),
            RequestCommand::NumberCode("5678".to_string())
        );
        assert!(matches!(
            RequestCommand::parse("12345"),
            RequestCommand::Unknown(_)
        ));
    }

    #[test]
    fn test_monitor_status_payload_is_plain_data() {
        let status = MonitorStatus {
            is_running: true,
            user_name: "張三".to_string(),
            last_poll_timestamp: Some(1_700_000_000),
            last_success_course: Some("計算機網路".to_string()),
            last_success_rollcall_id: Some(1),
            last_detected_timestamp: None,
            last_detected_course: None,
            last_detected_rollcall_id: None,
            last_detected_url: None,
            rollcall_history: Vec::new(),
            consecutive_failures: 0,
            started_at: 1_699_900_000,
        };
        let msg = OutboundMessage::Status(StatusMessage::Single(status.clone()));
        assert_eq!(msg, OutboundMessage::Status(StatusMessage::Single(status)));
    }

    #[tokio::test]
    async fn test_qrcode_command_sends_to_channel() {
        let (account, mut qr_rx) = make_account("acc1", "Uuser1");
        let state = make_state(vec![account]);

        execute_command(
            RequestCommand::QrCode("0~100!3~secret_data!4~42".to_string()),
            "",
            &state,
            "Uadmin123",
        )
        .await
        .unwrap();

        assert_eq!(
            qr_rx.recv().await,
            Some("0~100!3~secret_data!4~42".to_string())
        );
    }

    #[tokio::test]
    async fn test_number_code_command_sends_to_bound_account() {
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let (number_tx, mut number_rx) = tokio::sync::mpsc::channel(10);
        let account = RequestAccountState::new(
            "acc1",
            "Uuser1",
            qr_tx,
            number_tx,
            make_test_status("acc1", true),
        );
        let state = make_state(vec![account]);

        execute_command(
            RequestCommand::NumberCode("1234".to_string()),
            "",
            &state,
            "Uuser1",
        )
        .await
        .unwrap();

        assert_eq!(
            number_rx.recv().await,
            Some(NumberCodeInput::Code("1234".to_string()))
        );
    }

    #[tokio::test]
    async fn test_force_poll_command_notifies() {
        let (account, _rx) = make_account("acc1", "Uuser1");
        let notify = Arc::clone(&account.force_poll_tx);
        let state = make_state(vec![account]);

        let handle = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_millis(500), notify.notified())
                .await
                .is_ok()
        });

        execute_command(RequestCommand::ForceAttend, "", &state, "Uadmin123")
            .await
            .unwrap();

        assert!(handle.await.unwrap(), "ForceAttend 應觸發 notify");
    }

    #[tokio::test]
    async fn test_non_admin_control_command_is_ignored() {
        let (account, _rx) = make_account("acc-a", "Ualice");
        let state = make_state(vec![account]);

        execute_command(RequestCommand::Stop, "", &state, "Ualice")
            .await
            .unwrap();

        assert!(
            *state.accounts[0].is_running.lock().await,
            "非管理員不應能暫停監控"
        );
    }

    #[tokio::test]
    async fn test_non_admin_can_read_own_account_status() {
        let (account_a, _rx_a) = make_account("acc-a", "Ualice");
        let (account_b, _rx_b) = make_account("acc-b", "Ubob");
        let state = make_state(vec![account_a, account_b]);

        let msg = status_message_for_user(&state, "Ualice", false).await;

        match msg {
            OutboundMessage::Status(StatusMessage::UserAccount { account_id, .. }) => {
                assert_eq!(account_id, "acc-a");
            }
            other => panic!("unexpected status message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_discord_binding_updates_runtime_status_lookup() {
        let (account, _rx) = make_account("acc-a", "");
        let state = RequestState::new_with_binding(
            Arc::new(TestMessenger::new("999")),
            vec![account],
            AdapterBindingKind::Discord,
        );

        let before = status_message_for_user(&state, "123", false).await;
        assert_eq!(before, OutboundMessage::QrNoBoundAccount);

        assert!(state.set_discord_user_id("acc-a", "123").await);
        let after = status_message_for_user(&state, "123", false).await;
        match after {
            OutboundMessage::Status(StatusMessage::UserAccount { account_id, .. }) => {
                assert_eq!(account_id, "acc-a");
            }
            other => panic!("unexpected status message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_qrcode_channel_send_recv() {
        let (tx, rx) = create_qrcode_channel(1);
        tx.send("test_qr_data".to_string()).await.unwrap();

        let mut receiver = rx.lock().await;
        let received = receiver.recv().await;
        assert_eq!(received, Some("test_qr_data".to_string()));
    }

    #[tokio::test]
    async fn test_qrcode_channel_buffer_limit() {
        let (tx, _rx) = create_qrcode_channel(1);

        tx.send("qr_1".to_string()).await.unwrap();
        let result = tx.try_send("qr_2".to_string());
        assert!(result.is_err(), "Channel 滿時 try_send 應失敗");
    }
}
