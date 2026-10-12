//! Adapter inbound requests and request channels.
//!
//! Platform adapters convert user messages into `AdapterRequest` values and
//! delegate them here. This module only translates those requests into shared
//! state reads, monitor control signals, or input-channel delivery.

use std::collections::HashMap;
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

/// RADAR_INPUT_PAGE_V1（fix71）：雷達點名輸入頁送出的座標。
///
/// 雷達簽到需要的不是密碼而是「人在哪裡」——這個值只有使用者知道，所以比照數字點名
/// 做一條回傳通道：輸入頁把座標送進來，監控流程就拿它當下一個要嘗試的座標。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadarPointInput {
    pub latitude: f64,
    pub longitude: f64,
    /// 瀏覽器回報的定位精確度（公尺）；手動輸入時為 `None`。
    pub accuracy: Option<f64>,
    /// 是否由裝置定位取得（學生頁現在固定用系統演算送出；此欄位只用於訊息與日誌）。
    pub from_device: bool,
}

impl RadarPointInput {
    /// 合法經緯度才收（避免把打錯的座標送去學校）。
    pub fn is_valid(&self) -> bool {
        self.latitude.is_finite()
            && self.longitude.is_finite()
            && (-90.0..=90.0).contains(&self.latitude)
            && (-180.0..=180.0).contains(&self.longitude)
            && !(self.latitude == 0.0 && self.longitude == 0.0)
    }

    /// 顯示用來源說明。
    pub fn source_label(&self) -> &'static str {
        if self.from_device {
            "裝置定位"
        } else {
            "手動輸入"
        }
    }
}

pub type RadarPointSender = mpsc::Sender<RadarPointInput>;
pub type RadarPointReceiver = Arc<Mutex<mpsc::Receiver<RadarPointInput>>>;

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
    /// RADAR_INPUT_PAGE_V1（fix71）：雷達點名頁送出的座標通道。
    pub radar_tx: RadarPointSender,
}

impl RequestChannels {
    pub fn new(
        qr_tx: QrCodeSender,
        number_tx: NumberCodeSender,
        radar_tx: RadarPointSender,
    ) -> Self {
        Self {
            qr_tx,
            number_tx,
            radar_tx,
        }
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

/// RADAR_INPUT_PAGE_V1（fix71）：建立雷達點名頁的座標輸入通道。
pub fn create_radar_point_channel(buffer: usize) -> (RadarPointSender, RadarPointReceiver) {
    let (tx, rx) = mpsc::channel(buffer);
    (tx, Arc::new(Mutex::new(rx)))
}

// ─── ROLLCALL_INPUT_ROUTING_V1：每個點名各自的輸入通道 ─────────────────────
//
// 每個進行中的點名（帳號 + 點名 ID）在自己的 mpsc 通道上等待輸入；這裡登記它的送出端。
// Discord／LINE 指令依點名 ID 選目標；網頁掃碼頁本來就帶 rollcall_id，直接用該點名自己的 sender。
// 規則（不猜）：
// - 指令指定 ID → 送到該點名（同 ID 的所有帳號都送）；該點名沒有在等待 → 回覆錯誤。
// - 沒指定 ID → 只有一個點名在等待時送它；有多個時回覆請使用者指定，不送。

/// 一個點名正在等待的輸入通道（數字碼或 QR）。
#[derive(Clone, Debug)]
pub enum RollcallInputSender {
    Number(NumberCodeSender),
    Qr(QrCodeSender),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RollcallInputKind {
    Number,
    Qr,
}

impl RollcallInputSender {
    fn kind(&self) -> RollcallInputKind {
        match self {
            RollcallInputSender::Number(_) => RollcallInputKind::Number,
            RollcallInputSender::Qr(_) => RollcallInputKind::Qr,
        }
    }
}

type RollcallInputKey = (String, u64);

static ROLLCALL_INPUTS: once_cell::sync::Lazy<
    std::sync::Mutex<HashMap<RollcallInputKey, RollcallInputSender>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

/// 點名存活期間持有；離開（正常結束或被取消）時自動移除登記。
#[must_use = "dropping the guard unregisters the rollcall input"]
pub struct RollcallInputGuard {
    key: RollcallInputKey,
}

impl Drop for RollcallInputGuard {
    fn drop(&mut self) {
        let mut map = ROLLCALL_INPUTS.lock().unwrap_or_else(|p| p.into_inner());
        map.remove(&self.key);
    }
}

/// 登記一個點名的輸入送出端。回傳的 guard 必須跟著點名的生命週期持有。
pub fn register_rollcall_input(
    account_id: &str,
    rollcall_id: u64,
    sender: RollcallInputSender,
) -> RollcallInputGuard {
    let key: RollcallInputKey = (account_id.to_string(), rollcall_id);
    let mut map = ROLLCALL_INPUTS.lock().unwrap_or_else(|p| p.into_inner());
    map.insert(key.clone(), sender);
    RollcallInputGuard { key }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RollcallRouteError {
    /// 沒有任何點名在等待這種輸入。
    NoneWaiting,
    /// 指定的點名目前沒有在等待這種輸入。
    NotWaiting(u64),
    /// 多個點名同時等待，且沒有指定 ID（列出等待中的 ID）。
    Ambiguous(Vec<u64>),
}

/// 依指令選出要送入的點名。回傳 (點名 ID, sender) 清單；同 ID 的多個帳號會全部列出。
pub fn route_rollcall_input(
    account_ids: &[String],
    kind: RollcallInputKind,
    explicit: Option<u64>,
) -> std::result::Result<Vec<(u64, RollcallInputSender)>, RollcallRouteError> {
    let mut candidates: Vec<(u64, RollcallInputSender)> = {
        let map = ROLLCALL_INPUTS.lock().unwrap_or_else(|p| p.into_inner());
        map.iter()
            .filter(|((account, _), sender)| {
                sender.kind() == kind && account_ids.iter().any(|id| id == account)
            })
            .map(|((_, rollcall_id), sender)| (*rollcall_id, sender.clone()))
            .collect()
    };
    candidates.sort_by_key(|(rollcall_id, _)| *rollcall_id);

    if let Some(id) = explicit {
        let picked: Vec<(u64, RollcallInputSender)> = candidates
            .into_iter()
            .filter(|(rollcall_id, _)| *rollcall_id == id)
            .collect();
        return if picked.is_empty() {
            Err(RollcallRouteError::NotWaiting(id))
        } else {
            Ok(picked)
        };
    }

    let mut ids: Vec<u64> = candidates.iter().map(|(rollcall_id, _)| *rollcall_id).collect();
    ids.dedup();
    // 明確指定原則（使用者決定）：未指定 ID 一律不路由，即使只有一個點名在等待。
    match ids.len() {
        0 => Err(RollcallRouteError::NoneWaiting),
        _ => Err(RollcallRouteError::Ambiguous(ids)),
    }
}

/// 路由失敗時給使用者看的訊息。`what` 例如「數字點名碼」。
pub fn route_error_text(err: &RollcallRouteError, what: &str) -> String {
    match err {
        RollcallRouteError::NoneWaiting => format!("目前沒有等待{what}的點名。"),
        RollcallRouteError::NotWaiting(id) => format!("點名 #{id} 目前沒有在等待{what}。"),
        RollcallRouteError::Ambiguous(ids) if ids.len() == 1 => format!(
            "目前有一個點名在等待{what}：#{}。請指定點名 ID，例如在指令後面加上 #{}。",
            ids[0], ids[0]
        ),
        RollcallRouteError::Ambiguous(ids) => {
            let list = ids
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join("、");
            let example = ids.first().copied().unwrap_or_default();
            format!(
                "目前有多個點名在等待{what}：{list}。請指定要送到哪一個，例如在指令後面加上 #{example}。"
            )
        }
    }
}

/// 解析點名 ID：`#123`、`id:123` 或純數字。
pub fn parse_rollcall_id(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    let digits = trimmed
        .strip_prefix('#')
        .or_else(|| trimmed.strip_prefix("id:"))
        .or_else(|| trimmed.strip_prefix("ID:"))
        .unwrap_or(trimmed);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// 指令可在最前或最後帶一個點名 ID（`#123` 或 `id:123`），先拆掉再解析內容。
fn split_rollcall_target(text: &str) -> (Option<u64>, &str) {
    let trimmed = text.trim();
    if let Some((first, rest)) = trimmed.split_once(char::is_whitespace) {
        if let Some(id) = parse_rollcall_id_token(first) {
            return (Some(id), rest.trim());
        }
    }
    if let Some((rest, last)) = trimmed.rsplit_once(char::is_whitespace) {
        if let Some(id) = parse_rollcall_id_token(last) {
            return (Some(id), rest.trim());
        }
    }
    (None, trimmed)
}

fn parse_rollcall_id_token(token: &str) -> Option<u64> {
    if token.starts_with('#') || token.starts_with("id:") || token.starts_with("ID:") {
        parse_rollcall_id(token)
    } else {
        None
    }
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
    /// `rollcall_id` 為 None 時，只有唯一等待中的點名才會收到。
    QrCode { data: String, rollcall_id: Option<u64> },
    NumberCode { code: String, rollcall_id: Option<u64> },
    NumberAutoLookup { account_id: Option<String>, rollcall_id: Option<u64> },
    Help,
    Unknown(String),
}

impl RequestCommand {
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        let (rollcall_id, text) = split_rollcall_target(text);

        if Self::looks_like_qr_code(text) {
            return RequestCommand::QrCode {
                data: text.to_string(),
                rollcall_id,
            };
        }

        if let Some(code) = Self::parse_number_code(text) {
            return RequestCommand::NumberCode { code, rollcall_id };
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
            RequestCommand::QrCode { data, .. } => {
                // REDIRECT_UTF8_SLICE_V1：QR data 可能含任意 UTF-8，改用字元截斷。
                write!(f, "QrCode({}...)", crate::text_utils::truncate_chars(data, 20))
            }
            RequestCommand::NumberCode { code, .. } => write!(f, "NumberCode({code})"),
            RequestCommand::NumberAutoLookup { account_id, .. } => {
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
    /// MONITOR_WATCHDOG_V1：監控循環的心跳（Unix 秒）。循環每次迭代、每段等待／退避
    /// 睡眠中至少每 60 秒更新一次；看門狗與 /healthz 據此判斷循環是否卡死。
    /// 0 代表循環尚未開始（啟動延遲中）。
    pub heartbeat: Arc<std::sync::atomic::AtomicI64>,
    /// MONITOR_WATCHDOG_V1：循環任務的中止把手；看門狗判定卡死時用它殺掉舊任務再重生。
    /// 用 std Mutex（只在同步區段短暫持鎖）。
    pub loop_abort: Arc<std::sync::Mutex<Option<tokio::task::AbortHandle>>>,
}

/// MONITOR_WATCHDOG_V1：心跳哨兵值——循環已異常結束（非 0、且一定早於任何門檻）。
pub const HEARTBEAT_CRASHED: i64 = 1;

impl RequestAccountState {
    /// MONITOR_WATCHDOG_V1：更新心跳為現在時間。
    pub fn beat(&self) {
        let now = chrono::Local::now().timestamp();
        self.heartbeat
            .store(now, std::sync::atomic::Ordering::Relaxed);
    }

    /// MONITOR_WATCHDOG_V1：最近一次心跳（Unix 秒；0 = 尚未開始；1 = 循環已異常結束）。
    pub fn last_heartbeat(&self) -> i64 {
        self.heartbeat.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// MONITOR_WATCHDOG_V1：循環以 Err 結束時呼叫。把心跳設成哨兵值 `HEARTBEAT_CRASHED`，
    /// 看門狗下一輪掃描（30 秒內）就會把它當成卡死立刻重生，不必等心跳逾時。
    pub fn mark_loop_crashed(&self) {
        self.heartbeat
            .store(HEARTBEAT_CRASHED, std::sync::atomic::Ordering::Relaxed);
    }

    /// MONITOR_WATCHDOG_V1：記錄目前跑這個帳號循環的任務把手（重生時覆蓋）。
    pub fn set_loop_abort(&self, handle: tokio::task::AbortHandle) {
        let mut guard = self
            .loop_abort
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(handle);
    }

    /// MONITOR_WATCHDOG_V1：取走任務把手（取走後看門狗才能 abort，避免重複中止）。
    pub fn take_loop_abort(&self) -> Option<tokio::task::AbortHandle> {
        let mut guard = self
            .loop_abort
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.take()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        account_id: impl Into<String>,
        user_id: impl Into<String>,
        qr_tx: QrCodeSender,
        number_tx: NumberCodeSender,
        radar_tx: RadarPointSender,
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
            radar_tx,
            monitor_status,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_target(
        account_id: impl Into<String>,
        username: impl Into<String>,
        target: Arc<Mutex<AdapterAccountTarget>>,
        qr_tx: QrCodeSender,
        number_tx: NumberCodeSender,
        radar_tx: RadarPointSender,
        monitor_status: Arc<Mutex<MonitorStatus>>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            username: username.into(),
            target,
            requests: RequestChannels::new(qr_tx, number_tx, radar_tx),
            monitor_status,
            is_running: Arc::new(Mutex::new(true)),
            force_poll_tx: Arc::new(tokio::sync::Notify::new()),
            reauth_tx: Arc::new(tokio::sync::Notify::new()),
            heartbeat: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            loop_abort: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

// ACCOUNT_HOT_ADD_V1：runtime 帳號清單改成可在執行期追加的共享登錄簿。
//
// 啟動時 main.rs 建立一份，Web 管理頁、LINE、Discord 三個 adapter 共用同一個
// handle；執行期新增帳號時 `MonitorSpawner` push 進來，所有 adapter 立刻看得到。
// 內部採 copy-on-write：讀取端拿到 `Arc<Vec<_>>` 快照後就不再持鎖，
// 因此可以安全地跨 `.await` 使用；寫入（很少發生）才會重建一份 Vec。
#[derive(Clone, Default)]
pub struct AccountRegistry {
    inner: Arc<std::sync::RwLock<AccountRegistrySnapshot>>,
}

#[derive(Default)]
struct AccountRegistrySnapshot {
    list: Arc<Vec<RequestAccountState>>,
    // PERFORMANCE_ACCOUNT_LOOKUP_V1 延續：account_id -> index，避免每次線性掃描。
    by_id: HashMap<String, usize>,
}

impl AccountRegistry {
    pub fn new(accounts: Vec<RequestAccountState>) -> Self {
        let mut list: Vec<RequestAccountState> = Vec::with_capacity(accounts.len());
        let mut by_id = HashMap::with_capacity(accounts.len());
        for account in accounts {
            if by_id.contains_key(&account.account_id) {
                continue;
            }
            by_id.insert(account.account_id.clone(), list.len());
            list.push(account);
        }
        Self {
            inner: Arc::new(std::sync::RwLock::new(AccountRegistrySnapshot {
                list: Arc::new(list),
                by_id,
            })),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, AccountRegistrySnapshot> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 目前所有 runtime 帳號的快照（便宜的 Arc clone，不持鎖）。
    pub fn snapshot(&self) -> Arc<Vec<RequestAccountState>> {
        Arc::clone(&self.read().list)
    }

    /// 目前所有 runtime 帳號的 owned 複本（每個欄位都是 Arc，clone 成本很低）。
    pub fn all(&self) -> Vec<RequestAccountState> {
        self.read().list.iter().cloned().collect()
    }

    pub fn find(&self, account_id: &str) -> Option<RequestAccountState> {
        let guard = self.read();
        guard
            .by_id
            .get(account_id)
            .and_then(|index| guard.list.get(*index))
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.read().list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().list.is_empty()
    }

    /// MONITOR_WATCHDOG_V1：移除一個 runtime 帳號（看門狗重生前把舊的登錄拿掉，
    /// 否則 `MonitorSpawner::spawn_account` 會因「已在監控中」拒絕）。
    /// 回傳被移除的狀態（沒有則 None）。
    pub fn remove(&self, account_id: &str) -> Option<RequestAccountState> {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = *guard.by_id.get(account_id)?;
        let mut list: Vec<RequestAccountState> = guard.list.iter().cloned().collect();
        if index >= list.len() {
            return None;
        }
        let removed = list.remove(index);
        let by_id = list
            .iter()
            .enumerate()
            .map(|(i, account)| (account.account_id.clone(), i))
            .collect();
        guard.by_id = by_id;
        guard.list = Arc::new(list);
        Some(removed)
    }

    /// 追加一個 runtime 帳號；account_id 已存在時回傳 false 且不變更。
    pub fn push(&self, account: RequestAccountState) -> bool {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.by_id.contains_key(&account.account_id) {
            return false;
        }
        let mut list = Vec::with_capacity(guard.list.len() + 1);
        list.extend(guard.list.iter().cloned());
        guard.by_id.insert(account.account_id.clone(), list.len());
        list.push(account);
        guard.list = Arc::new(list);
        true
    }
}

impl From<Vec<RequestAccountState>> for AccountRegistry {
    fn from(accounts: Vec<RequestAccountState>) -> Self {
        Self::new(accounts)
    }
}

/// Shared state used by adapter request handlers.
#[derive(Clone)]
pub struct RequestState {
    pub messenger: Arc<dyn AdapterMessenger>,
    pub accounts: AccountRegistry,
    pub binding_kind: AdapterBindingKind,
}

impl RequestState {
    pub fn new(
        messenger: Arc<dyn AdapterMessenger>,
        accounts: impl Into<AccountRegistry>,
    ) -> Self {
        Self::new_with_binding(messenger, accounts, AdapterBindingKind::Line)
    }

    pub fn new_with_binding(
        messenger: Arc<dyn AdapterMessenger>,
        accounts: impl Into<AccountRegistry>,
        binding_kind: AdapterBindingKind,
    ) -> Self {
        Self {
            messenger,
            accounts: accounts.into(),
            binding_kind,
        }
    }

    fn first_account(&self) -> Option<RequestAccountState> {
        self.accounts.snapshot().first().cloned()
    }

    async fn account_for_user(&self, user_id: &str) -> Option<RequestAccountState> {
        if user_id.is_empty() {
            return None;
        }

        let accounts = self.accounts.snapshot();
        for account in accounts.iter() {
            let target = account.target.lock().await;
            let bound_user_id = match self.binding_kind {
                AdapterBindingKind::Line => target.line_user_id.as_str(),
                AdapterBindingKind::Discord => target.discord_user_id.as_str(),
            };
            if !bound_user_id.is_empty() && bound_user_id == user_id {
                return Some(account.clone());
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
        if let Some(account) = self.accounts.find(account_id) {
            let mut target = account.target.lock().await;
            target.discord_user_id = discord_user_id.to_string();
            return true;
        }

        false
    }

    pub async fn discord_bindings(&self) -> Vec<(String, String)> {
        let accounts = self.accounts.snapshot();
        let mut bindings = Vec::with_capacity(accounts.len());
        for account in accounts.iter() {
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
        RequestCommand::QrCode { data: qr_data, rollcall_id } => {
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

            let picked = match route_rollcall_input(
                std::slice::from_ref(&account.account_id),
                RollcallInputKind::Qr,
                rollcall_id,
            ) {
                Ok(picked) => picked,
                Err(err) => {
                    reply_if_possible(
                        reply_token,
                        state,
                        OutboundMessage::Text(route_error_text(&err, "QR Code 簽到")),
                    )
                    .await?;
                    return Ok(());
                }
            };
            let mut accepted = false;
            for (picked_id, sender) in picked {
                if let RollcallInputSender::Qr(tx) = sender {
                    match tx.send(qr_data.clone()).await {
                        Ok(_) => {
                            accepted = true;
                            debug!(rollcall_id = picked_id, "QR code 資料已傳送到簽到模組");
                        }
                        Err(e) => warn!(
                            rollcall_id = picked_id,
                            error = %e,
                            "QR code 傳送失敗（通道可能已關閉或沒有等待中的簽到）"
                        ),
                    }
                }
            }
            if accepted {
                reply_if_possible(reply_token, state, OutboundMessage::QrAccepted).await?;
            } else {
                reply_if_possible(reply_token, state, OutboundMessage::QrNoPendingRequest).await?;
            }
        }
        RequestCommand::NumberCode { code, rollcall_id } => {
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

            let targets: Vec<RequestAccountState> = if is_admin {
                state.accounts.all()
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

            let account_ids: Vec<String> = targets.iter().map(|a| a.account_id.clone()).collect();
            let picked = match route_rollcall_input(&account_ids, RollcallInputKind::Number, rollcall_id) {
                Ok(picked) => picked,
                Err(err) => {
                    reply_if_possible(
                        reply_token,
                        state,
                        OutboundMessage::Text(route_error_text(&err, "數字點名碼")),
                    )
                    .await?;
                    return Ok(());
                }
            };
            let mut sent = 0usize;
            for (picked_id, sender) in picked {
                if let RollcallInputSender::Number(tx) = sender {
                    match tx.send(NumberCodeInput::Code(code.clone())).await {
                        Ok(_) => sent += 1,
                        Err(e) => warn!(
                            rollcall_id = picked_id,
                            error = %e,
                            "數字點名碼傳送失敗（通道可能已關閉）"
                        ),
                    }
                }
            }
            if sent == 0 {
                reply_if_possible(
                    reply_token,
                    state,
                    OutboundMessage::Text("目前沒有可接收數字點名碼的點名。".to_string()),
                )
                .await?;
            } else {
                info!(code = %code, rollcall_id = ?rollcall_id, targets = sent, "數字點名碼已傳送到等待中的點名");
                reply_if_possible(
                    reply_token,
                    state,
                    OutboundMessage::Text(format!(
                        "已收到數字點名碼 `{code}`，送出給 {sent} 個等待中的點名。"
                    )),
                )
                .await?;
            }
        }
        RequestCommand::NumberAutoLookup { account_id, rollcall_id } => {
            let targets: Vec<RequestAccountState> = if let Some(account_id) = account_id {
                let Some(account) = state.accounts.find(&account_id) else {
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
                    if bound_account.as_ref().map(|bound| bound.account_id.as_str())
                        != Some(account.account_id.as_str())
                    {
                        reply_not_authorized(reply_token, state).await?;
                        return Ok(());
                    }
                }
                vec![account]
            } else if is_admin {
                state.accounts.all()
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

            let account_ids: Vec<String> = targets.iter().map(|a| a.account_id.clone()).collect();
            let picked = match route_rollcall_input(&account_ids, RollcallInputKind::Number, rollcall_id) {
                Ok(picked) => picked,
                Err(err) => {
                    reply_if_possible(
                        reply_token,
                        state,
                        OutboundMessage::Text(route_error_text(&err, "數字點名碼")),
                    )
                    .await?;
                    return Ok(());
                }
            };
            let mut sent = 0usize;
            for (picked_id, sender) in picked {
                if let RollcallInputSender::Number(tx) = sender {
                    match tx.send(NumberCodeInput::AutoLookup).await {
                        Ok(_) => sent += 1,
                        Err(e) => warn!(
                            rollcall_id = picked_id,
                            error = %e,
                            "自動讀取數字點名碼請求傳送失敗（通道可能已關閉）"
                        ),
                    }
                }
            }

            reply_if_possible(
                reply_token,
                state,
                OutboundMessage::Text(format!(
                    "已選擇自動讀取數字點名碼，送出給 {sent} 個等待中的點名。"
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
    let runtime_accounts = state.accounts.snapshot();

    if runtime_accounts.len() == 1 {
        let account = &runtime_accounts[0];
        let status = account.monitor_status.lock().await;
        return StatusMessage::Single(status.clone());
    }

    let mut accounts = Vec::with_capacity(runtime_accounts.len());
    for account in runtime_accounts.iter() {
        let status = account.monitor_status.lock().await;
        accounts.push(AccountStatusMessage {
            account_id: account.account_id.clone(),
            status: status.clone(),
        });
    }

    StatusMessage::AdminAccounts(accounts)
}

async fn set_all_running(state: &RequestState, is_running: bool) {
    for account in state.accounts.snapshot().iter() {
        let mut running = account.is_running.lock().await;
        *running = is_running;

        let mut status = account.monitor_status.lock().await;
        status.is_running = is_running;
    }
}

fn notify_all_force_poll(state: &RequestState) {
    for account in state.accounts.snapshot().iter() {
        account.force_poll_tx.notify_one();
    }
}

fn notify_all_reauth(state: &RequestState) {
    for account in state.accounts.snapshot().iter() {
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
            last_failure: None,
            last_reauth: None,
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
        let (radar_tx, _radar_rx) = create_radar_point_channel(10);
        (
            RequestAccountState::new(
                account_id,
                user_id,
                qr_tx,
                number_tx,
                radar_tx,
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
            RequestCommand::QrCode { .. }
        ));
    }

    #[test]
    fn test_parse_command_number_code() {
        assert_eq!(
            RequestCommand::parse("/num 1234"),
            RequestCommand::NumberCode { code: "1234".to_string(), rollcall_id: None }
        );
        assert_eq!(
            RequestCommand::parse("/num code:1234"),
            RequestCommand::NumberCode { code: "1234".to_string(), rollcall_id: None }
        );
        assert_eq!(
            RequestCommand::parse("5678"),
            RequestCommand::NumberCode { code: "5678".to_string(), rollcall_id: None }
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
            last_failure: None,
            last_reauth: None,
        };
        let msg = OutboundMessage::Status(StatusMessage::Single(status.clone()));
        assert_eq!(msg, OutboundMessage::Status(StatusMessage::Single(status)));
    }

    #[tokio::test]
    async fn test_qrcode_command_sends_to_channel() {
        // 明確指定原則：QR 必須帶點名 ID，且該點名已登記為等待中，才會送達。
        let (account, _unused_qr_rx) = make_account("acc-qr-test", "Uuser1");
        let state = make_state(vec![account]);
        let (qr_tx, mut qr_rx) = tokio::sync::mpsc::channel(10);
        let _guard = register_rollcall_input("acc-qr-test", 77, RollcallInputSender::Qr(qr_tx));

        // 未指定 ID：即使只有一個等待中的點名也不送達。
        execute_command(
            RequestCommand::QrCode { data: "0~100!3~secret_data!4~42".to_string(), rollcall_id: None },
            "",
            &state,
            "Uadmin123",
        )
        .await
        .unwrap();
        assert!(qr_rx.try_recv().is_err(), "未指定點名 ID 不應送達");

        execute_command(
            RequestCommand::QrCode { data: "0~100!3~secret_data!4~42".to_string(), rollcall_id: Some(77) },
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
        let (radar_tx, _radar_rx) = create_radar_point_channel(10);
        let account = RequestAccountState::new(
            "acc-num-test",
            "Uuser1",
            qr_tx,
            number_tx.clone(),
            radar_tx,
            make_test_status("acc-num-test", true),
        );
        let state = make_state(vec![account]);
        // 明確指定原則：數字點名碼必須指定點名 ID，且該點名已登記為等待中。
        let _guard = register_rollcall_input(
            "acc-num-test",
            78,
            RollcallInputSender::Number(number_tx),
        );

        // 未指定 ID：不送達。
        execute_command(
            RequestCommand::NumberCode { code: "1234".to_string(), rollcall_id: None },
            "",
            &state,
            "Uuser1",
        )
        .await
        .unwrap();
        assert!(number_rx.try_recv().is_err(), "未指定點名 ID 不應送達");

        execute_command(
            RequestCommand::NumberCode { code: "1234".to_string(), rollcall_id: Some(78) },
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
            *state.accounts.snapshot()[0].is_running.lock().await,
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

#[cfg(test)]
mod rollcall_routing_tests {
    use super::*;

    #[test]
    fn parse_accepts_rollcall_target_before_or_after() {
        assert_eq!(
            RequestCommand::parse("#123 1234"),
            RequestCommand::NumberCode {
                code: "1234".to_string(),
                rollcall_id: Some(123)
            }
        );
        assert_eq!(
            RequestCommand::parse("/number code:1234 id:9"),
            RequestCommand::NumberCode {
                code: "1234".to_string(),
                rollcall_id: Some(9)
            }
        );
        assert_eq!(
            RequestCommand::parse("1234"),
            RequestCommand::NumberCode {
                code: "1234".to_string(),
                rollcall_id: None
            }
        );
    }

    #[test]
    fn parse_rollcall_id_accepts_only_digits() {
        assert_eq!(parse_rollcall_id("#42"), Some(42));
        assert_eq!(parse_rollcall_id("id:7"), Some(7));
        assert_eq!(parse_rollcall_id("#x"), None);
        assert_eq!(parse_rollcall_id(""), None);
    }

    #[tokio::test]
    async fn routing_requires_id_when_ambiguous_and_unregisters_on_drop() {
        let (tx_a, _rx_a) = create_number_code_channel(2);
        let (tx_b, _rx_b) = create_number_code_channel(2);
        let guard_a = register_rollcall_input("acc-route-test", 100, RollcallInputSender::Number(tx_a));
        let _guard_b = register_rollcall_input("acc-route-test", 200, RollcallInputSender::Number(tx_b));
        let ids = vec!["acc-route-test".to_string()];

        assert_eq!(
            route_rollcall_input(&ids, RollcallInputKind::Number, None).err(),
            Some(RollcallRouteError::Ambiguous(vec![100, 200]))
        );
        let picked = route_rollcall_input(&ids, RollcallInputKind::Number, Some(200));
        assert_eq!(picked.map(|p| p.len()).ok(), Some(1));
        assert_eq!(
            route_rollcall_input(&ids, RollcallInputKind::Number, Some(999)).err(),
            Some(RollcallRouteError::NotWaiting(999))
        );
        assert_eq!(
            route_rollcall_input(&ids, RollcallInputKind::Qr, None).err(),
            Some(RollcallRouteError::NoneWaiting)
        );

        drop(guard_a);
        // 明確指定：即使只剩一個等待中的點名，未指定 ID 仍必須拒絕。
        assert_eq!(
            route_rollcall_input(&ids, RollcallInputKind::Number, None).err(),
            Some(RollcallRouteError::Ambiguous(vec![200]))
        );
        assert_eq!(
            route_rollcall_input(&ids, RollcallInputKind::Number, Some(200)).map(|p| p.len()).ok(),
            Some(1)
        );
    }
}
