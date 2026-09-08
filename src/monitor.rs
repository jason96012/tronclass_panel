//! 主監控循環模組
//!
//! 實現程式的核心邏輯：
//! - 定期輪詢 `/api/radar/rollcalls`
//! - 偵測到新的待簽到項目時，分派到對應的簽到模組
//! - Session 過期時自動重新認證
//! - 透過 request state 接收 adapter 的控制指令（/stop、/start、/force）
//! - 更新 `MonitorStatus` 供 `/status` 指令查詢
//!
//! # 執行流程
//! ```
//! main()
//!  ├── init_config()
//!  ├── AuthClient::new()           ← CAS 登錄
//!  ├── ApiClient::from_config()    ← 建立 API 客戶端
//!  ├── LineBotClient::new()        ← 建立 Line Bot 客戶端
//!  ├── start_webhook_server()      ← 背景啟動 Webhook 伺服器
//!  └── run_monitor_loop()          ← 進入主監控循環（阻塞）
//!       ├── sleep(startup_delay)
//!       └── loop {
//!             wait_for_trigger()   ← 等待定時、強制觸發或重新認證
//!             if !is_running { continue }
//!             get_rollcalls()
//!             process_rollcall_batch()
//!             update_status()
//!             on_failure: maybe_reauth()
//!           }
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Local, NaiveTime, Timelike, Weekday};
use miette::{Result, WrapErr};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, instrument, warn};

use crate::account::{AccountConfig, AccountsFile};
use crate::adapters::events::{
    AdapterAccountTarget, AdapterMessenger, MonitorStatus, OutboundMessage, RollcallHistoryEntry,
    SystemStartedEvent,
};
use crate::adapters::requests::{
    create_number_code_channel, create_qrcode_channel, AccountRegistry, NumberCodeReceiver,
    NumberCodeSender, QrCodeReceiver, QrCodeSender, RequestAccountState,
};
use crate::adapters::scanner::QrScannerRegistry;
use crate::api::{is_auth_error_report, ApiClient};
use crate::auth::AuthClient;
use crate::config::{parse_schedule_period, AppConfig, PollingScheduleConfig};
use crate::db::AccountsDb;
use crate::rollcalls::{process_rollcall_batch, BatchSummary};

// ─── 監控器設定 ───────────────────────────────────────────────────────────────

const FORCE_CASCADE_DEDUPE_TTL: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct ForceCascade {
    // ACCOUNT_HOT_ADD_V1：peers 改為可在執行期追加（熱新增帳號時 add_peer）。
    // 只在同步區段短暫持鎖，不跨 await。
    peers: Arc<std::sync::RwLock<Vec<ForceCascadePeer>>>,
    recent_rollcalls: Arc<Mutex<HashMap<String, Instant>>>,
}

#[derive(Clone)]
struct ForceCascadePeer {
    account_id: String,
    notify: Arc<Notify>,
}

impl ForceCascade {
    pub fn new(peers: Vec<(String, Arc<Notify>)>) -> Self {
        Self {
            peers: Arc::new(std::sync::RwLock::new(
                peers
                    .into_iter()
                    .map(|(account_id, notify)| ForceCascadePeer { account_id, notify })
                    .collect(),
            )),
            recent_rollcalls: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// ACCOUNT_HOT_ADD_V1：執行期加入新的 cascade 對象（同 id 已存在則略過）。
    pub fn add_peer(&self, account_id: String, notify: Arc<Notify>) {
        let mut peers = self
            .peers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if peers.iter().any(|peer| peer.account_id == account_id) {
            return;
        }
        peers.push(ForceCascadePeer { account_id, notify });
    }

    async fn notify_peers_once(
        &self,
        source_account_id: &str,
        provider: &str,
        rollcall_ids: &[u64],
    ) {
        if rollcall_ids.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut should_notify = false;
        {
            let mut recent = self.recent_rollcalls.lock().await;
            recent.retain(|_, last_seen| now.duration_since(*last_seen) < FORCE_CASCADE_DEDUPE_TTL);

            for rollcall_id in rollcall_ids {
                let key = format!("{provider}:{rollcall_id}");
                if let std::collections::hash_map::Entry::Vacant(entry) = recent.entry(key) {
                    entry.insert(now);
                    should_notify = true;
                }
            }
        }

        if !should_notify {
            debug!(
                account = %source_account_id,
                provider = %provider,
                rollcall_ids = ?rollcall_ids,
                "force cascade 已在近期觸發過，略過重複廣播"
            );
            return;
        }

        let mut notified = 0usize;
        {
            let peers = self
                .peers
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for peer in peers.iter() {
                if peer.account_id == source_account_id {
                    continue;
                }
                peer.notify.notify_one();
                notified += 1;
            }
        }

        info!(
            account = %source_account_id,
            provider = %provider,
            rollcall_ids = ?rollcall_ids,
            notified = notified,
            "使用者 force 偵測到點名，已觸發其他帳號一起 force 偵測"
        );
    }
}

// BACKGROUND_NOTIFICATION_HELPER_PHASE_M7B
// Low-priority adapter messages must own cloned data so they never hold a
// monitor borrow or delay the next poll / rollcall handling step.
fn notify_background(
    messenger: Option<Arc<dyn AdapterMessenger>>,
    target: AdapterAccountTarget,
    message: OutboundMessage,
) {
    if let Some(bot) = messenger {
        tokio::spawn(async move {
            if let Err(error) = bot.push_to_account_or_admin(&target, &message).await {
                warn!(%error, "背景通知發送失敗");
            }
        });
    }
}

/// 監控器執行時的完整上下文
pub struct MonitorContext {
    /// 全域設定
    pub global_config: Arc<AppConfig>,

    /// 單一帳號設定
    pub account: Arc<AccountConfig>,

    /// 已認證的 HTTP 客戶端（持有 session cookie）
    pub auth_client: AuthClient,

    /// 帳號資料庫，用來保存最近一次成功登入的 session cookie。
    pub accounts_db: AccountsDb,

    /// Tronclass API 客戶端
    pub api_client: Arc<ApiClient>,

    /// Adapter messenger（可選）
    pub messenger: Option<Arc<dyn AdapterMessenger>>,

    /// 此帳號可被 adapter 使用者請求操作的共享狀態
    pub request_account: Option<RequestAccountState>,

    /// 此帳號在各 adapter 的綁定目標（可由 Discord 控制台即時更新）
    pub adapter_target: Arc<Mutex<AdapterAccountTarget>>,

    /// QR code 輸入通道接收端
    pub qr_rx: QrCodeReceiver,

    /// QR code 輸入通道傳送端（可複製給 Webhook handler）
    pub qr_tx: QrCodeSender,

    /// 數字點名手動輸入通道接收端
    pub number_rx: NumberCodeReceiver,

    /// 數字點名手動輸入通道傳送端（給 adapter 指令使用）
    pub number_tx: NumberCodeSender,

    /// 共享 QR scanner registry（可選）
    pub scanner_registry: Option<Arc<QrScannerRegistry>>,

    /// 任一帳號手動 force 偵測到點名後，用來通知其他帳號立即檢查。
    pub force_cascade: Option<Arc<ForceCascade>>,

    /// 共享的監控狀態
    pub status: Arc<Mutex<MonitorStatus>>,
}

impl MonitorContext {
    /// 建立監控器上下文
    ///
    /// 此函式會：
    /// 1. 執行 CAS 登錄（或注入手動 cookie）
    /// 2. 建立 API 客戶端
    /// 3. （可選）建立 Line Bot 客戶端
    /// 4. 建立 QR code 通道
    /// 5. 初始化監控狀態
    pub async fn new(
        global_config: Arc<AppConfig>,
        account: Arc<AccountConfig>,
        accounts_db: AccountsDb,
        messenger: Option<Arc<dyn AdapterMessenger>>,
        interactive_adapter: bool,
        scanner_registry: Option<Arc<QrScannerRegistry>>,
    ) -> Result<Self> {
        // ── 登錄 ──────────────────────────────────────────────────────────────
        info!(account = %account.id, "正在登錄 Tronclass...");

        let (auth_client, session) = AuthClient::new(&account)
            .await
            .wrap_err("登錄失敗，請檢查帳號密碼或 cookie 設定")?;

        let safe_user_name = display_session_user_name(&session.user_name, &account);
        info!(account = %account.id, user_name = %safe_user_name, "登錄成功！");
        persist_session_cookie(&accounts_db, &account.id, &session.cookie_string).await;

        // ── API 客戶端 ────────────────────────────────────────────────────────
        // 使用 auth_client 內的 client（帶 session cookie）
        // 注意：重新建立一個共享的 client（透過 Arc）
        let api_client = Arc::new(ApiClient::new(
            auth_client.client.clone(),
            account.base_url().to_string(),
        ));

        // ── QR code / 數字點名手動輸入通道 ───────────────────────────────────
        let (qr_tx, qr_rx) = create_qrcode_channel(4);
        let (number_tx, number_rx) = create_number_code_channel(8);

        // ── 監控狀態 ──────────────────────────────────────────────────────────
        let started_at = current_unix_secs();
        let status = Arc::new(Mutex::new(MonitorStatus {
            is_running: true,
            user_name: safe_user_name,
            last_poll_timestamp: None,
            last_success_course: None,
            last_success_rollcall_id: None,
            last_detected_timestamp: None,
            last_detected_course: None,
            last_detected_rollcall_id: None,
            last_detected_url: None,
            rollcall_history: vec![],
            consecutive_failures: 0,
            started_at,
            last_failure: None,
        }));

        let adapter_target = Arc::new(Mutex::new(AdapterAccountTarget::new(
            account.id.clone(),
            account.line_user_id.clone(),
            account.discord_user_id.clone(),
        )));

        // ── Adapter request 帳號狀態 ──────────────────────────────────────────
        let request_account = if interactive_adapter {
            Some(RequestAccountState::new_with_target(
                account.id.clone(),
                account.username.clone(),
                Arc::clone(&adapter_target),
                qr_tx.clone(),
                number_tx.clone(),
                Arc::clone(&status),
            ))
        } else {
            None
        };

        Ok(Self {
            global_config,
            account,
            auth_client,
            accounts_db,
            api_client,
            messenger,
            request_account,
            adapter_target,
            qr_rx,
            qr_tx,
            number_rx,
            number_tx,
            scanner_registry,
            force_cascade: None,
            status,
        })
    }
}

// ─── 執行期熱新增帳號 ─────────────────────────────────────────────────────────

// ACCOUNT_HOT_ADD_V1
// 啟動時 main.rs 建好一份 `MonitorSpawner`，交給 Web 管理頁與 Discord bot；
// 新增帳號寫入 accounts.db 後直接呼叫 `spawn_account`，不必重啟服務：
//   DB 讀回 → AccountsFile::resolve（沿用啟動時同一套驗證）→ MonitorContext::new（真的登入）
//   → 註冊到共享 AccountRegistry + ForceCascade → tokio::spawn(run_monitor_loop)。
// 熱啟動的循環不掛在 main 的 JoinSet 上：它意外結束時只記錄／通知，不會拖垮整個服務。

/// 熱啟動成功後回傳給呼叫端的資訊。
#[derive(Debug, Clone)]
pub struct HotAddedMonitor {
    pub account_id: String,
    /// 登入後取得的顯示名稱（可能為空字串）。
    pub user_name: String,
}

#[derive(Clone)]
pub struct MonitorSpawner {
    global_config: Arc<AppConfig>,
    accounts_db: AccountsDb,
    messenger: Option<Arc<dyn AdapterMessenger>>,
    interactive_adapter: bool,
    scanner_registry: Option<Arc<QrScannerRegistry>>,
    force_cascade: Arc<ForceCascade>,
    registry: AccountRegistry,
    /// ACCOUNT_HOT_ADD_FEEDBACK_V1：每個帳號最近一次熱啟動失敗的原因，
    /// 讓 Web 狀態頁／健康檢查能持續顯示「為什麼還沒在監控」，而不是只閃一次 toast。
    last_errors: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

impl MonitorSpawner {
    pub fn new(
        global_config: Arc<AppConfig>,
        accounts_db: AccountsDb,
        messenger: Option<Arc<dyn AdapterMessenger>>,
        interactive_adapter: bool,
        scanner_registry: Option<Arc<QrScannerRegistry>>,
        force_cascade: Arc<ForceCascade>,
        registry: AccountRegistry,
    ) -> Self {
        Self {
            global_config,
            accounts_db,
            messenger,
            interactive_adapter,
            scanner_registry,
            force_cascade,
            registry,
            last_errors: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// ACCOUNT_HOT_ADD_FEEDBACK_V1：該帳號是否已有監控循環在跑。
    pub fn is_active(&self, account_id: &str) -> bool {
        self.registry.find(account_id).is_some()
    }

    /// ACCOUNT_HOT_ADD_FEEDBACK_V1：該帳號最近一次熱啟動失敗的原因（成功後會清除）。
    pub fn last_error(&self, account_id: &str) -> Option<String> {
        self.last_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account_id)
            .cloned()
    }

    fn record_result(&self, account_id: &str, result: &std::result::Result<HotAddedMonitor, String>) {
        let mut errors = self
            .last_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match result {
            Ok(_) => {
                errors.remove(account_id);
            }
            Err(reason) => {
                errors.insert(account_id.to_string(), reason.clone());
            }
        }
    }

    /// 以 accounts.db 內既有的帳號 id 熱啟動監控循環。
    ///
    /// 會實際登入一次（與服務啟動時相同），因此可能耗時數秒到數十秒；
    /// Discord 這類有回覆時限的呼叫端請放到背景任務執行。
    pub async fn spawn_account(&self, account_id: &str) -> std::result::Result<HotAddedMonitor, String> {
        let account_id = account_id.trim().to_string();
        let result = self.spawn_account_inner(&account_id).await;
        if !account_id.is_empty() {
            self.record_result(&account_id, &result);
        }
        result
    }

    async fn spawn_account_inner(&self, account_id: &str) -> std::result::Result<HotAddedMonitor, String> {
        if account_id.is_empty() {
            return Err("帳號 id 不可為空".to_string());
        }
        if self.registry.find(account_id).is_some() {
            return Err(format!("帳號 `{account_id}` 已在監控中"));
        }

        let raw = match self.accounts_db.get(account_id).await {
            Ok(Some(raw)) => raw,
            Ok(None) => return Err(format!("資料庫中找不到帳號 `{account_id}`")),
            Err(e) => return Err(format!("讀取帳號 `{account_id}` 失敗：{}", report_chain_text(&e))),
        };
        if !raw.enabled {
            return Err(format!("帳號 `{account_id}` 已停用（enabled = false）"));
        }
        // VIEWER_NO_SCHOOL_API_V1：檢視者不登入學校系統，也不建監控循環。
        if raw.is_viewer {
            return Err(format!("帳號 `{account_id}` 為檢視者（唯讀），不會啟動監控"));
        }

        // 沿用啟動時同一套驗證（provider 存在、kind 有登入流程、欄位不為空）。
        let mut resolved = AccountsFile {
            accounts: vec![raw],
        }
        .resolve(&self.global_config)
        .map_err(|e| format!("帳號設定無效：{}", report_chain_text(&e)))?;
        let Some(account) = resolved.pop() else {
            return Err(format!("帳號 `{account_id}` 無法解析為可監控的設定"));
        };
        let account = Arc::new(account);

        let mut ctx = MonitorContext::new(
            Arc::clone(&self.global_config),
            Arc::clone(&account),
            self.accounts_db.clone(),
            self.messenger.as_ref().map(Arc::clone),
            self.interactive_adapter,
            self.scanner_registry.as_ref().map(Arc::clone),
        )
        .await
        .map_err(|e| format!("登入失敗：{}", report_chain_text(&e)))?;
        ctx.force_cascade = Some(Arc::clone(&self.force_cascade));

        let user_name = ctx.status.lock().await.user_name.clone();
        if let Some(request_account) = ctx.request_account.clone() {
            // 先進登錄簿（同 id 併發新增時只有一個會成功），再加入 cascade。
            if !self.registry.push(request_account.clone()) {
                return Err(format!("帳號 `{account_id}` 已在監控中"));
            }
            self.force_cascade
                .add_peer(account.id.clone(), Arc::clone(&request_account.force_poll_tx));
        }

        let task_account_id = account.id.clone();
        let task_messenger = self.messenger.as_ref().map(Arc::clone);
        let task_target = ctx.adapter_target.lock().await.clone();
        tokio::spawn(async move {
            match run_monitor_loop(ctx).await {
                Ok(()) => warn!(account = %task_account_id, "熱啟動的監控循環提前正常結束"),
                Err(e) => {
                    error!(account = %task_account_id, error = %e, "熱啟動的監控循環異常結束");
                    notify_background(
                        task_messenger,
                        task_target,
                        OutboundMessage::Text(format!(
                            "[{task_account_id}] 監控循環異常結束：{e}\n請重啟服務以恢復此帳號的監控。"
                        )),
                    );
                }
            }
        });

        info!(
            account = %account.id,
            user_name = %user_name,
            "已於執行期熱啟動監控循環（未重啟服務）"
        );
        Ok(HotAddedMonitor {
            account_id: account.id.clone(),
            user_name,
        })
    }
}

/// 把 miette 錯誤鏈攤平成一行給使用者看（外層 wrap_err 訊息 + 內層真正原因）。
fn report_chain_text(report: &miette::Report) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in report.chain() {
        let text = cause.to_string();
        let text = text.trim();
        if text.is_empty() || parts.iter().any(|existing| existing == text) {
            continue;
        }
        parts.push(text.to_string());
    }
    if parts.is_empty() {
        report.to_string()
    } else {
        parts.join("；")
    }
}

// ─── 主監控循環 ───────────────────────────────────────────────────────────────

/// 執行主監控循環（阻塞直到程式終止）
///
/// # 流程
/// 1. 等待 `startup_delay_secs` 後開始第一次輪詢
/// 2. 每隔 `poll_interval_secs` 輪詢一次
/// 3. 若收到強制觸發（`force_poll_tx`），立即執行一次
/// 4. 若監控暫停（`is_running = false`），跳過輪詢但繼續等待
/// 5. 若連續失敗超過 `max_failures_before_reauth`，嘗試重新認證
/// 6. 收到 `reauth_tx` 通知時，立即重新認證
///
/// # 參數
/// - `ctx`：監控器上下文（mut，因為重新認證時需要更新 auth_client）
// DETERMINISTIC_POLL_JITTER_PHASE_M5B
// A stable per-account offset prevents same-provider monitor tasks from polling
// in the same millisecond after a shared startup delay. Force/Reauth bypass this.
fn startup_poll_jitter(account_id: &str) -> Duration {
    const MAX_JITTER_MS: u64 = 400;
    let hash = account_id.bytes().fold(0u64, |value, byte| {
        value.wrapping_mul(31).wrapping_add(byte as u64)
    });
    Duration::from_millis(hash % (MAX_JITTER_MS + 1))
}

pub async fn run_monitor_loop(mut ctx: MonitorContext) -> Result<()> {
    let config = Arc::clone(&ctx.global_config);
    let account_label = ctx.account.display_name().to_string();
    let provider_poll_interval_secs = ctx.account.provider_config.api.poll_interval_secs;
    let startup_delay = Duration::from_secs(config.monitor.startup_delay_secs);
    let poll_jitter = startup_poll_jitter(&ctx.account.id);
    let initial_poll_delay = startup_delay + poll_jitter;

    // ── 啟動延遲與帳號相位錯開 ────────────────────────────────────────────────
    if initial_poll_delay > Duration::ZERO {
        info!(
            delay_secs = config.monitor.startup_delay_secs,
            jitter_ms = poll_jitter.as_millis(),
            "等待啟動延遲與帳號輪詢錯開後開始第一次輪詢"
        );
        tokio::time::sleep(initial_poll_delay).await;
    }

    // 取得控制訊號的參考（從 request_account）
    let force_poll_notify = ctx
        .request_account
        .as_ref()
        .map(|s| Arc::clone(&s.force_poll_tx));
    let reauth_notify = ctx
        .request_account
        .as_ref()
        .map(|s| Arc::clone(&s.reauth_tx));
    let is_running_lock = ctx
        .request_account
        .as_ref()
        .map(|s| Arc::clone(&s.is_running));
    let schedule = ctx.account.provider_config.schedule.clone();

    info!(account = %account_label, "🚀 開始主監控循環");

    // 傳送啟動通知給該帳號綁定使用者（若未綁定則退回管理員）
    if let Some(bot) = &ctx.messenger {
        let user_name = {
            let s = ctx.status.lock().await;
            s.user_name.clone()
        };
        let startup_msg = OutboundMessage::SystemStarted(SystemStartedEvent {
            account: account_label.clone(),
            user_name,
            poll_interval_secs: ctx.account.provider_config.api.poll_interval_secs,
            adapter_name: bot.adapter_name().to_string(),
        });
        let target = ctx.adapter_target.lock().await.clone();
        // BACKGROUND_STARTUP_NOTIFICATION_PHASE_M7B2
        notify_background(Some(Arc::clone(bot)), target, startup_msg);
    }

    let mut consecutive_failures: u32 = 0;
    // REAUTH_REQUEST_DEDUPE_PHASE_M2: coalesce repeated adapter/session triggers.
    let mut last_reauth_request: Option<Instant> = None;
    let reauth_dedupe_window = Duration::from_secs(5);
    let max_failures = config.monitor.max_failures_before_reauth;
    let retry_interval = Duration::from_secs(config.monitor.retry_interval_secs);
    // POLL_BACKOFF_V1：失敗後的等待不再固定 retry_interval，而是依連續失敗次數
    // 指數拉長並加抖動；成功一次就歸零（consecutive_failures = 0）。
    // 學校 API 大當機時所有帳號不會同步狂打，也不會同時醒來。
    let backoff_for = |failures: u32| -> Duration { poll_backoff(retry_interval, failures) };

    // PERFORMANCE_MONITOR_TARGET_CACHE_V1:
    // Cache adapter_target at the start of each iteration to avoid 5+ lock
    // acquisitions per loop cycle. The target rarely changes (only on
    // Discord/LINE rebind), so one read per iteration is sufficient.
    let mut cached_target;
    let mut last_timetable_window_state: Option<(bool, bool)> = None;
    // REST_DAY_NO_POLL_V1：同一個休息日只記一次 log。
    let mut last_rest_day_logged: Option<chrono::NaiveDate> = None;

    loop {
        // ── 等待觸發（定時、強制 or 重新認證）──────────────────────────────────
        let normal_poll_interval =
            crate::runtime_settings::poll_interval(provider_poll_interval_secs).await;
        // REST_DAY_NO_POLL_V1：週六、週日與國定假日整天不做自動輪詢。
        // 休息日只等「下一個上課日 00:00」或最多 30 分鐘（讓 config/holidays.json
        // 的修改半小時內生效）；期間仍可被「立即檢查」／重新認證喚醒，
        // 這兩種觸發走下面同一套流程（Timer 觸發才會被休息日擋掉）。
        let rest_today = Local::now().date_naive();
        let rest_day_reason = crate::holidays::rest_day_reason(rest_today);
        if let Some(reason) = rest_day_reason {
            if last_rest_day_logged != Some(rest_today) {
                info!(
                    account = %ctx.account.id,
                    reason = %reason,
                    next_working_day = ?crate::holidays::next_working_day_after(rest_today),
                    "今天是{reason}，暫停自動輪詢（手動「立即檢查」仍可用）"
                );
                last_rest_day_logged = Some(rest_today);
            }
        }
        let timetable_policy = if rest_day_reason.is_some() {
            TimetablePollPolicy {
                interval: rest_day_wait(Local::now()),
                controls_schedule: true,
                inside_window: false,
                polling_allowed: false,
            }
        } else {
            timetable_poll_policy(&ctx, normal_poll_interval).await
        };
        let timetable_state = (
            timetable_policy.inside_window,
            timetable_policy.polling_allowed,
        );
        if rest_day_reason.is_none()
            && timetable_policy.controls_schedule
            && last_timetable_window_state != Some(timetable_state)
        {
            info!(
                account = %ctx.account.id,
                inside_window = timetable_policy.inside_window,
                polling_allowed = timetable_policy.polling_allowed,
                wait_secs = timetable_policy.interval.as_secs(),
                "課表偵測排程狀態已切換"
            );
            last_timetable_window_state = Some(timetable_state);
        }
        let trigger = if timetable_policy.controls_schedule {
            wait_for_duration(
                timetable_policy.interval,
                force_poll_notify.as_deref(),
                reauth_notify.as_deref(),
            )
            .await
        } else {
            wait_for_trigger(
                timetable_policy.interval,
                force_poll_notify.as_deref(),
                reauth_notify.as_deref(),
                &schedule,
            )
            .await
        };

        // Refresh cached target once per iteration
        cached_target = ctx.adapter_target.lock().await.clone();

        match trigger {
            MonitorTrigger::ForcePoll => debug!("由強制觸發啟動本次輪詢"),
            MonitorTrigger::Timer => debug!("由定時觸發啟動本次輪詢"),
            MonitorTrigger::Reauth => {
                if last_reauth_request.is_some_and(|last| last.elapsed() < reauth_dedupe_window) {
                    debug!(account = %ctx.account.id, "忽略短時間內重複的重新認證請求");
                    continue;
                }
                last_reauth_request = Some(Instant::now());
                info!("收到重新認證請求");
                if let Err(e) = do_reauth(&mut ctx).await {
                    error!(error = %e, "重新認證失敗");
                    // PERFORMANCE_MONITOR_TARGET_CACHE_V1: use cached target
                    notify_background(
                        ctx.messenger.clone(),
                        cached_target.clone(),
                        OutboundMessage::Text(format!("❌ [{}] 重新認證失敗：{e}", account_label)),
                    );
                } else {
                    consecutive_failures = 0;
                    // PERFORMANCE_MONITOR_TARGET_CACHE_V1: use cached target
                    notify_background(
                        ctx.messenger.clone(),
                        cached_target.clone(),
                        OutboundMessage::Text(format!("✅ [{}] 重新認證成功！", account_label)),
                    );
                }
                continue;
            }
        }

        if trigger == MonitorTrigger::Timer {
            let now = Local::now();
            // REST_DAY_NO_POLL_V1：等待結束時再確認一次（可能已跨日到上課日，
            // 也可能 holidays.json 剛被改成放假）。
            if crate::holidays::is_rest_day(now.date_naive()) {
                debug!(account = %ctx.account.id, "休息日（週末／國定假日），略過本次定時輪詢");
                continue;
            }
            if timetable_policy.controls_schedule {
                // Re-evaluate after every wait so boundaries are applied exactly:
                // 07:00 resumes immediately, while 19:00 suppresses the poll that
                // would otherwise run as the night quiet period begins.
                let current_policy = timetable_poll_policy(&ctx, normal_poll_interval).await;
                if current_policy.controls_schedule && !current_policy.polling_allowed {
                    debug!(account = %ctx.account.id, "目前為 19:00–07:00 夜間停止時段，略過簽到列表輪詢");
                    continue;
                }
            } else if !is_within_poll_window(&schedule, now.weekday(), now.time()) {
                debug!("目前不在 provider 課堂輪詢時段內，略過本次定時觸發");
                continue;
            }
            if crate::runtime_settings::is_rest_time(now).await {
                debug!("目前在 Web 管理頁設定的休息時間內，略過本次定時觸發");
                continue;
            }
        }

        // ── 自動停止時間 ──────────────────────────────────────────────────────
        if crate::runtime_settings::auto_stop_reached(Local::now().time()).await {
            if let Some(ref lock) = is_running_lock {
                *lock.lock().await = false;
            }
            {
                let mut s = ctx.status.lock().await;
                s.is_running = false;
            }
            debug!("已達 Web 管理頁設定的自動停止時間，暫停本帳號輪詢");
            continue;
        }

        // ── 檢查是否暫停 ──────────────────────────────────────────────────────
        if let Some(ref lock) = is_running_lock {
            let is_running = *lock.lock().await;
            if !is_running {
                debug!("監控已暫停，跳過本次輪詢");
                continue;
            }
        }

        // ── 更新輪詢時間 ──────────────────────────────────────────────────────
        let poll_timestamp = current_unix_secs();
        {
            let mut s = ctx.status.lock().await;
            s.last_poll_timestamp = Some(poll_timestamp);
        }

        // ── 執行輪詢 ──────────────────────────────────────────────────────────
        info!("🔄 開始輪詢簽到列表...");

        // METRICS_HISTORY_V1：記錄本次輪詢耗時與成敗（維護頁延遲圖）。
        let poll_started = Instant::now();
        let poll_result = do_poll_and_attend(&mut ctx, trigger).await;
        crate::metrics_history::record_poll_latency(poll_started.elapsed(), poll_result.is_ok());
        match poll_result {
            Ok(summary) => {
                // 成功輪詢
                consecutive_failures = 0;
                // FAILURE_DIAGNOSIS_V1：成功即清除最近失敗診斷。
                {
                    let mut s = ctx.status.lock().await;
                    s.last_failure = None;
                }
                // MONITOR_ALERT_GRADING_V1：先前若送過管理員警示，這裡送「已恢復」。
                crate::monitor_alerts::report_recovery(
                    &ctx.accounts_db,
                    ctx.messenger.as_deref(),
                    &ctx.account.id,
                )
                .await;

                if summary.total > 0 {
                    info!(summary = %summary, "本次輪詢完成");

                    // 更新最後成功簽到課程
                    // （實際上 summary 沒有包含課程名，這裡只是示意）
                }

                // 更新狀態中的連續失敗計數
                {
                    let mut s = ctx.status.lock().await;
                    s.consecutive_failures = 0;
                    s.is_running = true;
                }
            }

            Err(e) => {
                consecutive_failures += 1;
                error!(
                    error = %e,
                    consecutive_failures = consecutive_failures,
                    "輪詢失敗（第 {} 次連續失敗）",
                    consecutive_failures
                );

                // MONITOR_ALERT_GRADING_V1：錯誤分級。輕微只寫 log；警告／嚴重通知管理員。
                let auth_error = is_auth_error_report(&e);
                let error_text = format!("{e:#}");

                // 更新狀態（FAILURE_DIAGNOSIS_V1：同時寫入分類與自助建議，儀表板帳號列會顯示）
                {
                    let mut s = ctx.status.lock().await;
                    s.consecutive_failures = consecutive_failures;
                    s.last_failure = Some(crate::monitor_alerts::diagnose(&error_text, auth_error));
                }
                let grade = crate::monitor_alerts::classify(
                    auth_error,
                    consecutive_failures,
                    max_failures,
                    false,
                );
                crate::monitor_alerts::report_failure(
                    &ctx.accounts_db,
                    ctx.messenger.as_deref(),
                    &ctx.account.id,
                    grade,
                    consecutive_failures,
                    auth_error,
                    &error_text,
                )
                .await;

                // 判斷是否需要重新認證
                // AUTH_ERROR_TYPED_V1：改用 report 版走完整條錯誤鏈，
                // 避免 wrap_err 外層訊息（如「取得簽到列表失敗」）掩蓋
                // 內層真正的 ApiError::Unauthorized。
                if auth_error || consecutive_failures >= max_failures {
                    if consecutive_failures >= max_failures {
                        warn!(
                            consecutive_failures = consecutive_failures,
                            max = max_failures,
                            "連續失敗次數達到上限，嘗試重新認證"
                        );
                    } else {
                        warn!("偵測到認證錯誤，嘗試重新認證");
                    }

                    if let Some(bot) = &ctx.messenger {
                        // PERFORMANCE_MONITOR_TARGET_CACHE_V1: use cached target
                        let _ = bot
                            .push_to_account_or_admin(
                                &cached_target,
                                &OutboundMessage::Text(format!(
                                    "⚠️ [{}] 簽到監控遇到錯誤，嘗試重新認證...\n錯誤：{e}",
                                    account_label
                                )),
                            )
                            .await;
                    }

                    match do_reauth(&mut ctx).await {
                        Ok(()) => {
                            info!("重新認證成功，重置失敗計數");
                            consecutive_failures = 0;
                            {
                                let mut s = ctx.status.lock().await;
                                s.consecutive_failures = 0;
                                s.last_failure = None;
                            }
                            if let Some(bot) = &ctx.messenger {
                                // PERFORMANCE_MONITOR_TARGET_CACHE_V1: use cached target
                                let _ = bot
                                    .push_to_account_or_admin(
                                        &cached_target,
                                        &OutboundMessage::Text(format!(
                                            "✅ [{}] 自動重新認證成功！",
                                            account_label
                                        )),
                                    )
                                    .await;
                            }
                        }
                        Err(reauth_err) => {
                            error!(error = %reauth_err, "重新認證失敗！");
                            // FAILURE_DIAGNOSIS_V1：重登失敗的原因比原本輪詢錯誤更關鍵，覆蓋之。
                            {
                                let reauth_text = format!("{reauth_err:#}");
                                let mut s = ctx.status.lock().await;
                                s.last_failure = Some(crate::monitor_alerts::diagnose(
                                    &reauth_text,
                                    is_auth_error_report(&reauth_err),
                                ));
                            }
                            // MONITOR_ALERT_GRADING_V1：自動重新認證失敗 → 嚴重。
                            crate::monitor_alerts::report_failure(
                                &ctx.accounts_db,
                                ctx.messenger.as_deref(),
                                &ctx.account.id,
                                crate::monitor_alerts::ErrorGrade::Critical,
                                consecutive_failures,
                                true,
                                &format!("{reauth_err:#}"),
                            )
                            .await;
                            if let Some(bot) = &ctx.messenger {
                                // PERFORMANCE_MONITOR_TARGET_CACHE_V1: use cached target
                                let _ = bot
                                    .push_to_account_or_admin(
                                        &cached_target,
                                        &OutboundMessage::Text(format!(
                                            "❌ [{}] 自動重新認證失敗：{reauth_err}\n\n\
                                         請手動更新 accounts.toml 中對應帳號的 manual_cookie，\n\
                                         或輸入 /reauth 再次嘗試。",
                                            account_label
                                        )),
                                    )
                                    .await;
                            }

                            // 重新認證失敗後等待更長的時間再重試（POLL_BACKOFF_V1：指數退避）
                            let wait = backoff_for(consecutive_failures);
                            warn!(
                                retry_secs = wait.as_secs(),
                                consecutive_failures = consecutive_failures,
                                "重新認證失敗，等待 {} 秒後重試",
                                wait.as_secs()
                            );
                            tokio::time::sleep(wait).await;
                        }
                    }
                } else {
                    // 普通失敗（POLL_BACKOFF_V1：依連續失敗次數指數退避 + 抖動）
                    let wait = backoff_for(consecutive_failures);
                    warn!(
                        retry_secs = wait.as_secs(),
                        consecutive_failures = consecutive_failures,
                        "等待 {} 秒後重試...",
                        wait.as_secs()
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

// ─── 單次輪詢與簽到 ───────────────────────────────────────────────────────────

/// 執行一次完整的輪詢與簽到流程
///
/// 1. 呼叫 `GET /api/radar/rollcalls`
/// 2. 篩選出需要簽到的項目
/// 3. 分派到 `process_rollcall_batch`
/// 4. 返回批次統計摘要
#[instrument(skip(ctx))]
async fn do_poll_and_attend(
    ctx: &mut MonitorContext,
    trigger: MonitorTrigger,
) -> Result<BatchSummary> {
    // ── 取得簽到列表 ──────────────────────────────────────────────────────────
    let rollcalls = ctx
        .api_client
        .get_rollcalls()
        .await
        .wrap_err("取得簽到列表失敗")?;

    let total = rollcalls.len();
    let pending_count = rollcalls.iter().filter(|rc| rc.needs_attendance()).count();

    debug!(
        total = total,
        pending = pending_count,
        "取得簽到列表：共 {total} 個，{pending_count} 個需要簽到"
    );

    if pending_count == 0 {
        if total > 0 {
            debug!("所有 {} 個簽到均已完成或已過期", total);
        } else {
            debug!("目前沒有任何簽到");
        }
        clear_detected_rollcall_if_expired(&ctx.status, 30 * 60).await;
        return Ok(BatchSummary::default());
    }

    let pending_rollcalls = rollcalls
        .iter()
        .filter(|rc| rc.needs_attendance())
        .collect::<Vec<_>>();
    let pending_rollcall_ids = pending_rollcalls
        .iter()
        .map(|rc| rc.rollcall_id)
        .collect::<Vec<_>>();

    if let Some(first_pending) = pending_rollcalls.first() {
        let mut status = ctx.status.lock().await;
        status.last_detected_timestamp = Some(current_unix_secs());
        status.last_detected_course = Some(first_pending.course_title.clone());
        status.last_detected_rollcall_id = Some(first_pending.rollcall_id);
        status.last_detected_url = None;
    }

    if trigger == MonitorTrigger::ForcePoll {
        if let Some(force_cascade) = ctx.force_cascade.as_ref() {
            force_cascade
                .notify_peers_once(
                    &ctx.account.id,
                    &ctx.account.provider,
                    &pending_rollcall_ids,
                )
                .await;
        }
    }

    info!(
        pending = pending_count,
        "偵測到 {} 個待簽到項目，開始自動簽到...", pending_count
    );

    // 記錄每個待簽到項目
    for rc in rollcalls.iter().filter(|rc| rc.needs_attendance()) {
        info!(
            rollcall_id = rc.rollcall_id,
            course = %rc.course_title,
            teacher = %rc.created_by_name,
            r#type = %rc.attendance_type(),
            "待簽到：{}",
            rc.display()
        );
    }

    // ── 執行批次簽到 ──────────────────────────────────────────────────────────
    let messenger_ref = ctx.messenger.as_ref().map(|b| b.as_ref());

    let outcomes = process_rollcall_batch(
        Arc::clone(&ctx.api_client),
        rollcalls,
        &ctx.account,
        ctx.account.display_name(),
        messenger_ref,
        Arc::clone(&ctx.adapter_target),
        Arc::clone(&ctx.status),
        ctx.request_account
            .as_ref()
            .map(|_| Arc::clone(&ctx.number_rx)),
        ctx.request_account.as_ref().map(|_| ctx.number_tx.clone()),
        ctx.request_account.as_ref().map(|_| Arc::clone(&ctx.qr_rx)),
        ctx.request_account.as_ref().map(|_| ctx.qr_tx.clone()),
        ctx.scanner_registry.as_ref().map(Arc::clone),
    )
    .await;

    let summary = BatchSummary::from_outcomes(&outcomes);

    // ── 記錄結果 ──────────────────────────────────────────────────────────────
    info!(summary = %summary, "批次簽到完成");

    // 記錄每個結果
    for outcome in &outcomes {
        if !outcome.result.is_skipped() {
            push_rollcall_history(&ctx.status, outcome).await;
        }

        if outcome.is_success() {
            info!(
                rollcall_id = outcome.rollcall.rollcall_id,
                course = %outcome.rollcall.course_title,
                result = %outcome.result,
                elapsed_ms = outcome.elapsed_ms,
                "✅ 簽到成功"
            );

            // 更新最後成功課程，並清除管理頁上的「偵測到點名」提醒。
            {
                let mut s = ctx.status.lock().await;
                s.last_success_course = Some(outcome.rollcall.course_title.clone());
                s.last_success_rollcall_id = Some(outcome.rollcall.rollcall_id);
                if s.last_detected_rollcall_id == Some(outcome.rollcall.rollcall_id) {
                    s.last_detected_timestamp = None;
                    s.last_detected_course = None;
                    s.last_detected_rollcall_id = None;
                    s.last_detected_url = None;
                }
            }
        } else if outcome.result.is_skipped() {
            debug!(
                rollcall_id = outcome.rollcall.rollcall_id,
                result = %outcome.result,
                "⏭️ 跳過"
            );
        } else {
            warn!(
                rollcall_id = outcome.rollcall.rollcall_id,
                course = %outcome.rollcall.course_title,
                result = %outcome.result,
                elapsed_ms = outcome.elapsed_ms,
                "❌ 簽到失敗"
            );
        }
    }

    // 若批次中有致命錯誤，傳播為 Err
    if summary.has_fatal() {
        return Err(miette::miette!(
            "批次簽到中發生致命錯誤（session 可能已過期）"
        ));
    }

    Ok(summary)
}

// ─── 重新認證 ─────────────────────────────────────────────────────────────────

/// 執行重新認證流程
///
/// 1. 呼叫 `auth_client.re_authenticate()`
/// 2. 用新的 client 重建 `api_client`
/// 3. 更新 `ctx.status` 中的 user_name
#[instrument(skip(ctx))]
async fn do_reauth(ctx: &mut MonitorContext) -> Result<()> {
    info!("開始重新認證...");

    // MONITOR_ACCOUNT_RELOAD_V1：重新認證前自資料庫重讀帳號。
    // ctx.account 是「啟動時」的快照——dashboard/CLI 更新帳號密碼後，
    // 若不重讀，舊密碼會一直被拿去登入（「改回正確密碼仍登入失敗」的根因）。
    // 手動 reauth 與連續失敗後的自動 reauth 都走這裡，因此兩條路徑同時修復。
    match ctx.accounts_db.get(&ctx.account.id).await {
        Ok(Some(raw)) => {
            if let Some(provider) = ctx.global_config.providers.get(&raw.provider).cloned() {
                let fresh = AccountConfig {
                    request_timeout_secs: provider.api.request_timeout_secs,
                    provider_config: provider,
                    id: raw.id,
                    provider: raw.provider,
                    username: raw.username.clone(),
                    password: raw.password.clone(),
                    enabled: raw.enabled,
                    line_user_id: raw.line_user_id,
                    discord_user_id: raw.discord_user_id,
                    session_cookie: raw.session_cookie,
                    is_admin: raw.is_admin,
                };
                if fresh.username != ctx.account.username || fresh.password != ctx.account.password
                {
                    info!(
                        account = %fresh.id,
                        "偵測到帳號資料變更（帳號／密碼），重新認證改用最新設定"
                    );
                }
                ctx.account = Arc::new(fresh);
            } else {
                warn!(
                    provider = %raw.provider,
                    "帳號的 provider 已不存在於設定檔，沿用啟動時快照"
                );
            }
        }
        Ok(None) => {
            warn!(
                account = %ctx.account.id,
                "帳號已自資料庫刪除；沿用啟動時快照嘗試重新認證"
            );
        }
        Err(e) => {
            warn!(error = %e, "重讀帳號失敗；沿用啟動時快照重新認證");
        }
    }

    let session = ctx
        .auth_client
        .re_authenticate(&ctx.account)
        .await
        .wrap_err("重新認證失敗")?;

    let safe_user_name = display_session_user_name(&session.user_name, &ctx.account);
    info!(user_name = %safe_user_name, "重新認證成功");
    persist_session_cookie(&ctx.accounts_db, &ctx.account.id, &session.cookie_string).await;

    // 重建 API 客戶端（使用新的 session cookie）
    ctx.api_client = Arc::new(ApiClient::new(
        ctx.auth_client.client.clone(),
        ctx.account.base_url().to_string(),
    ));

    // 更新狀態中的 user_name
    {
        let mut s = ctx.status.lock().await;
        s.user_name = safe_user_name;
        s.consecutive_failures = 0;
    }

    Ok(())
}

// ─── 觸發等待 ─────────────────────────────────────────────────────────────────

/// 等待輪詢觸發
///
/// 等待以下其中之一：
/// 1. 定時器到期（`poll_interval`）
/// 2. 強制觸發通知（`force_poll_notify`）
///
/// # 返回
/// 觸發來源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorTrigger {
    Timer,
    ForcePoll,
    Reauth,
}

#[derive(Debug, Clone, Copy)]
struct TimetablePollPolicy {
    interval: Duration,
    controls_schedule: bool,
    inside_window: bool,
    polling_allowed: bool,
}

/// Resolve the interval for one monitor iteration using local server time.
/// From 07:00 through 18:59, class windows use the normal interval and all
/// other times use the configured outside interval. From 19:00 through 06:59,
/// timer-triggered provider polling is paused and the wake is aligned to 07:00.
async fn timetable_poll_policy(
    ctx: &MonitorContext,
    normal_interval: Duration,
) -> TimetablePollPolicy {
    let fallback = TimetablePollPolicy {
        interval: normal_interval,
        controls_schedule: false,
        inside_window: false,
        polling_allowed: true,
    };
    let preference = match ctx
        .accounts_db
        .get_detection_schedule_preference(&ctx.account.id)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            warn!(account = %ctx.account.id, error = %error, "讀取課表偵測設定失敗，沿用 provider 排程");
            return fallback;
        }
    };
    if !preference.enabled {
        return fallback;
    }

    let now = Local::now();
    if let Some(until_daytime) = crate::detection_schedule::seconds_until_daytime(now.time()) {
        return TimetablePollPolicy {
            interval: Duration::from_secs(until_daytime),
            controls_schedule: true,
            inside_window: false,
            polling_allowed: false,
        };
    }

    let entries = match ctx
        .accounts_db
        .list_timetable_entries_for_account(&ctx.account.id)
        .await
    {
        Ok(value) => value,
        Err(error) => {
            warn!(account = %ctx.account.id, error = %error, "讀取課表失敗，沿用 provider 排程");
            return fallback;
        }
    };
    let evaluation = crate::detection_schedule::evaluate(
        &entries,
        now.weekday(),
        now.time(),
        &preference,
    );
    let outside_interval = Duration::from_secs(
        u64::try_from(preference.outside_interval_secs.clamp(60, 3600)).unwrap_or(300),
    );
    let mut interval = if evaluation.active {
        normal_interval
    } else {
        outside_interval
    };
    if !evaluation.active {
        if let Some(until_window) = evaluation.next_window_in_secs.map(Duration::from_secs) {
            interval = interval.min(until_window.max(Duration::from_secs(1)));
        }
    }
    if let Some(until_night) =
        crate::detection_schedule::seconds_until_night_pause(now.time()).map(Duration::from_secs)
    {
        interval = interval.min(until_night.max(Duration::from_secs(1)));
    }

    TimetablePollPolicy {
        interval,
        controls_schedule: true,
        inside_window: evaluation.active,
        polling_allowed: true,
    }
}

/// REST_DAY_NO_POLL_V1：休息日要睡多久——到隔天 00:00:05 為止，但最多 30 分鐘
/// （這樣 holidays.json 的 workdays 覆寫、或跨日到上課日都能在半小時內反應）。
fn rest_day_wait(now: chrono::DateTime<Local>) -> Duration {
    let secs_today = u64::from(now.hour()) * 3600 + u64::from(now.minute()) * 60 + u64::from(now.second());
    let until_midnight = (24u64 * 3600).saturating_sub(secs_today) + 5;
    Duration::from_secs(until_midnight.clamp(5, 30 * 60))
}

async fn wait_for_trigger(
    poll_interval: Duration,
    force_poll_notify: Option<&tokio::sync::Notify>,
    reauth_notify: Option<&tokio::sync::Notify>,
    schedule: &PollingScheduleConfig,
) -> MonitorTrigger {
    if let Some(next_window) =
        next_poll_window(schedule, Local::now().weekday(), Local::now().time())
    {
        info!(
            next_label = %next_window.label,
            wait_secs = next_window.wait.as_secs(),
            "目前不在課堂時段內，等待下一個輪詢時段"
        );

        return wait_for_duration(next_window.wait, force_poll_notify, reauth_notify).await;
    }

    wait_for_duration(poll_interval, force_poll_notify, reauth_notify).await
}

/// POLL_BACKOFF_V1：失敗後的重試等待時間。
///
/// `base × 2^(failures-1)`，上限 `base × 16`（預設 30 秒 → 最長 8 分鐘），
/// 再加 0 到 25 % 的隨機抖動，避免多個帳號在同一秒同時重試。
/// `failures` 為 0 或 1 時等於 base（含抖動）。
pub(crate) fn poll_backoff(base: Duration, failures: u32) -> Duration {
    const MAX_MULTIPLIER: u32 = 16;
    let exponent = failures.saturating_sub(1).min(4); // 2^4 = 16
    let multiplier = (1u32 << exponent).min(MAX_MULTIPLIER);
    let base_secs = base.as_secs().max(1);
    let wait_secs = base_secs.saturating_mul(u64::from(multiplier));
    // 抖動：0..=25 %
    let jitter_pct = u64::from(rand::random::<u8>() % 26);
    let jitter = wait_secs.saturating_mul(jitter_pct) / 100;
    Duration::from_secs(wait_secs.saturating_add(jitter))
}

async fn wait_for_duration(
    duration: Duration,
    force_poll_notify: Option<&tokio::sync::Notify>,
    reauth_notify: Option<&tokio::sync::Notify>,
) -> MonitorTrigger {
    match (force_poll_notify, reauth_notify) {
        (Some(force_notify), Some(reauth_notify)) => {
            tokio::select! {
                _ = tokio::time::sleep(duration) => MonitorTrigger::Timer,
                _ = force_notify.notified() => MonitorTrigger::ForcePoll,
                _ = reauth_notify.notified() => MonitorTrigger::Reauth,
            }
        }
        (Some(force_notify), None) => {
            tokio::select! {
                _ = tokio::time::sleep(duration) => MonitorTrigger::Timer,
                _ = force_notify.notified() => MonitorTrigger::ForcePoll,
            }
        }
        (None, Some(reauth_notify)) => {
            tokio::select! {
                _ = tokio::time::sleep(duration) => MonitorTrigger::Timer,
                _ = reauth_notify.notified() => MonitorTrigger::Reauth,
            }
        }
        (None, None) => {
            tokio::time::sleep(duration).await;
            MonitorTrigger::Timer
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NextPollWindow {
    label: String,
    wait: Duration,
}

fn is_within_poll_window(
    schedule: &PollingScheduleConfig,
    weekday: Weekday,
    now: NaiveTime,
) -> bool {
    if !schedule.is_configured() {
        return true;
    }
    if schedule.is_rest_day(weekday) {
        return false;
    }

    parsed_schedule_periods(schedule)
        .into_iter()
        .any(|(start, end)| now >= start && now < end)
}

fn next_poll_window(
    schedule: &PollingScheduleConfig,
    weekday: Weekday,
    now: NaiveTime,
) -> Option<NextPollWindow> {
    if !schedule.is_configured() || is_within_poll_window(schedule, weekday, now) {
        return None;
    }

    let periods = parsed_schedule_periods(schedule);
    if periods.is_empty() {
        return None;
    }

    let now_mins = minutes_since_midnight(now);
    for day_offset in 0..7u64 {
        let day = add_days(weekday, day_offset as usize);
        if schedule.is_rest_day(day) {
            continue;
        }

        for (start, end) in &periods {
            let start_mins = minutes_since_midnight(*start);
            if day_offset == 0 && start_mins <= now_mins {
                continue;
            }

            let wait_mins = day_offset * 24 * 60 + u64::from(start_mins) - u64::from(now_mins);
            return Some(NextPollWindow {
                label: format!("{}~{}", start.format("%H:%M"), end.format("%H:%M")),
                wait: Duration::from_secs(wait_mins * 60),
            });
        }
    }

    None
}

fn parsed_schedule_periods(schedule: &PollingScheduleConfig) -> Vec<(NaiveTime, NaiveTime)> {
    let mut periods = schedule
        .periods
        .iter()
        // UNWRAP_SWEEP_V1：config.validate() 已擋掉壞格式，但 runtime 熱重載或
        // 測試建構的 config 不一定走過 validate；壞項目直接略過並 warn，不 panic。
        .filter_map(|period| match parse_schedule_period(period) {
            Ok(parsed) => Some(parsed),
            Err(error) => {
                warn!(period = %period, error = %error, "輪詢時段格式無效，已略過");
                None
            }
        })
        .collect::<Vec<_>>();
    periods.sort_by_key(|(start, _)| *start);
    periods
}

fn minutes_since_midnight(time: NaiveTime) -> u32 {
    time.hour() * 60 + time.minute()
}

fn add_days(weekday: Weekday, days: usize) -> Weekday {
    const WEEK: [Weekday; 7] = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];

    // UNWRAP_SWEEP_V1：WEEK 涵蓋全部 7 個 Weekday，position 不可能為 None；
    // 仍以 unwrap_or(0) 收斂，讓此函式在任何情況下都不會 panic。
    let idx = WEEK
        .iter()
        .position(|day| *day == weekday)
        .unwrap_or(0);
    WEEK[(idx + days) % WEEK.len()]
}

// ─── 輔助函式 ─────────────────────────────────────────────────────────────────

fn display_session_user_name(raw_user_name: &str, account: &AccountConfig) -> String {
    let sanitized = redact_sensitive_display_text(raw_user_name)
        .trim()
        .to_string();
    let display = if sanitized.is_empty() || looks_like_sensitive_token(&sanitized) {
        account.username.clone()
    } else {
        sanitized
    };

    // 使用者名稱欄位只顯示 Tronclass profile name / fallback username。
    // 帳號 ID 已經在另一欄顯示，這裡不要再加「(帳號 ID)」，避免有些人有括號、有些人沒有。
    display
}

fn redact_sensitive_display_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '(' {
            let mut inner = String::new();
            let mut closed = false;
            while let Some(next) = chars.next() {
                if next == ')' {
                    closed = true;
                    break;
                }
                inner.push(next);
            }

            if closed {
                if looks_like_sensitive_token(&inner) {
                    continue;
                }
                output.push('(');
                output.push_str(&redact_sensitive_tokens(&inner));
                output.push(')');
            } else {
                output.push('(');
                output.push_str(&redact_sensitive_tokens(&inner));
            }
        } else {
            output.push(ch);
        }
    }

    redact_sensitive_tokens(&output)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_sensitive_tokens(input: &str) -> String {
    // PERFORMANCE_REDACT_V1: avoid intermediate Vec allocation by writing
    // directly to the output buffer.
    let mut output = String::with_capacity(input.len());
    let mut first = true;
    for part in input.split_whitespace() {
        if !first {
            output.push(' ');
        }
        first = false;
        let trimmed = part
            .trim_matches(|c: char| matches!(c, ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}'));
        if looks_like_sensitive_token(trimmed) {
            output.push_str("[隱藏]");
        } else {
            output.push_str(part);
        }
    }
    output
}

fn looks_like_sensitive_token(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    lower.contains("session=")
        || lower.contains("x-session")
        || lower.starts_with("v2-")
        || lower.contains(".ma.")
        || (value.len() >= 48 && value.contains('-') && value.contains('.'))
}

async fn push_rollcall_history(
    status: &Arc<Mutex<MonitorStatus>>,
    outcome: &crate::rollcalls::RollcallOutcome,
) {
    const MAX_HISTORY: usize = 50;

    let mut status = status.lock().await;
    status.rollcall_history.insert(
        0,
        RollcallHistoryEntry {
            timestamp: current_unix_secs(),
            rollcall_id: outcome.rollcall.rollcall_id,
            course_name: outcome.rollcall.course_title.clone(),
            attendance_type: outcome.attendance_type.to_string(),
            success: outcome.is_success(),
            result: outcome.result.to_string(),
        },
    );
    if status.rollcall_history.len() > MAX_HISTORY {
        status.rollcall_history.truncate(MAX_HISTORY);
    }
}

async fn clear_detected_rollcall(status: &Arc<Mutex<MonitorStatus>>) {
    let mut status = status.lock().await;
    status.last_detected_timestamp = None;
    status.last_detected_course = None;
    status.last_detected_rollcall_id = None;
    status.last_detected_url = None;
}

async fn clear_detected_rollcall_if_expired(status: &Arc<Mutex<MonitorStatus>>, ttl_secs: i64) {
    let mut status = status.lock().await;
    if let Some(detected_at) = status.last_detected_timestamp {
        let age_secs = current_unix_secs().saturating_sub(detected_at);
        if age_secs < ttl_secs {
            // Tronclass 有時候會短暫回傳沒有待點名，不能因此把 Web 上的點名提示清掉。
            // 只有超過 TTL 還沒有新的偵測/成功結果時才清除，避免提示永久殘留。
            return;
        }
    }

    status.last_detected_timestamp = None;
    status.last_detected_course = None;
    status.last_detected_rollcall_id = None;
    status.last_detected_url = None;
}

async fn persist_session_cookie(accounts_db: &AccountsDb, account_id: &str, cookie_string: &str) {
    if cookie_string.trim().is_empty() {
        return;
    }

    match accounts_db
        .set_session_cookie(account_id, cookie_string)
        .await
    {
        Ok(true) => debug!(account = %account_id, "已保存最新 session cookie"),
        Ok(false) => warn!(account = %account_id, "保存 session cookie 失敗：找不到帳號"),
        Err(e) => warn!(account = %account_id, error = %e, "保存 session cookie 失敗"),
    }
}

/// 取得當前 Unix 時間戳（秒）
pub fn current_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// POLL_BACKOFF_V1
    #[test]
    fn poll_backoff_grows_and_caps() {
        use std::time::Duration;
        let base = Duration::from_secs(30);
        let within = |failures: u32, expect_base: u64| {
            let got = super::poll_backoff(base, failures).as_secs();
            assert!(
                got >= expect_base && got <= expect_base + expect_base / 4,
                "failures={failures} got={got} expected {expect_base}..={}",
                expect_base + expect_base / 4
            );
        };
        within(0, 30);
        within(1, 30);
        within(2, 60);
        within(3, 120);
        within(4, 240);
        within(5, 480);
        within(6, 480); // capped at 16x
        within(100, 480);
    }

    use super::*;
    use crate::api::is_auth_error;
    use crate::config::ScheduleWeekday;
    use std::sync::atomic::{AtomicBool, Ordering};

    // ── current_unix_secs ─────────────────────────────────────────────────────

    #[test]
    fn test_current_unix_secs_positive() {
        let ts = current_unix_secs();
        assert!(ts > 0, "Unix 時間戳應大於 0");
    }

    #[test]
    fn test_current_unix_secs_reasonable() {
        let ts = current_unix_secs();
        // 2024-01-01 00:00:00 UTC = 1704067200
        assert!(ts > 1_704_067_200, "時間戳應在 2024 年之後");
        // 2100-01-01 00:00:00 UTC = 4102444800
        assert!(ts < 4_102_444_800, "時間戳應在 2100 年之前");
    }

    #[test]
    fn test_current_unix_secs_monotonic() {
        let t1 = current_unix_secs();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let t2 = current_unix_secs();
        assert!(t2 >= t1, "時間戳應單調遞增");
    }

    // ── is_auth_error（AUTH_ERROR_TYPED_V1：精確標記比對）────────────────────

    #[test]
    fn test_is_auth_error_true_cases() {
        // ApiError::Unauthorized 的標準 Display
        assert!(is_auth_error("Session 已過期（HTTP 401/403），需要重新登錄"));
        // monitor 的致命錯誤包裹訊息
        assert!(is_auth_error("批次簽到中發生致命錯誤（session 可能已過期）"));
        assert!(is_auth_error("🔴 致命錯誤：簽到 session 過期"));
        // 明確帶前綴的 HTTP 狀態字串（本專案自己 format 的格式）
        assert!(is_auth_error("HTTP 401 Unauthorized"));
        assert!(is_auth_error("HTTP 403: Forbidden"));
        assert!(is_auth_error("API 返回錯誤狀態 401：token 無效"));
        // 明確的英文字眼
        assert!(is_auth_error("Unauthorized access"));
        assert!(is_auth_error("Session expired, please re-login"));
    }

    #[test]
    fn test_is_auth_error_false_cases() {
        assert!(!is_auth_error("Network timeout"));
        assert!(!is_auth_error("HTTP 500 Internal Server Error"));
        assert!(!is_auth_error("JSON parse error"));
        assert!(!is_auth_error("距離不足 50 公尺"));
        assert!(!is_auth_error("爆破失敗：無正確代碼"));
        assert!(!is_auth_error(""));
        assert!(!is_auth_error("Error code 4: timeout"));
    }

    /// AUTH_ERROR_NUMERIC_FP_V1：舊版裸 `contains("401")` / `contains("403")`
    /// 會把這些含數字片段的無關訊息誤判成認證錯誤，觸發不必要的重新登入。
    #[test]
    fn test_is_auth_error_rejects_bare_numbers() {
        assert!(!is_auth_error("取得課程 40123 失敗"));
        assert!(!is_auth_error("錯誤代碼 4012"));
        assert!(!is_auth_error("最後距離 403.52 公尺"));
        assert!(!is_auth_error("rollcall 1740341 已過期"));
    }

    /// 舊版裸「session／登入／登錄／認證」會把這些無關訊息誤判成認證錯誤。
    #[test]
    fn test_is_auth_error_rejects_bare_login_words() {
        assert!(!is_auth_error("Session management failed"));
        assert!(!is_auth_error("session cookie invalid"));
        assert!(!is_auth_error("session 可能已過期")); // 只出現在「致命錯誤」訊息內，單獨出現不代表認證錯誤
        assert!(!is_auth_error("使用者尚未登入 LINE"));
        assert!(!is_auth_error("登入頁面載入超時"));
    }

    /// AUTH_ERROR_TYPED_V1：monitor 主循環改用 report 版——
    /// wrap_err 的外層訊息不再掩蓋內層的 ApiError::Unauthorized。
    #[test]
    fn test_is_auth_error_report_finds_wrapped_cause() {
        use crate::api::ApiError;
        let report = miette::miette!(ApiError::Unauthorized).wrap_err("取得簽到列表失敗");
        // 外層訊息本身沒有任何認證標記…
        assert!(!is_auth_error("取得簽到列表失敗"));
        // …但 report 版會走完整條錯誤鏈找到內層原因。
        assert!(is_auth_error_report(&report));
        // 一般暫時性錯誤即使被包住也不誤判。
        let transient = miette::miette!("Network timeout").wrap_err("取得簽到列表失敗");
        assert!(!is_auth_error_report(&transient));
    }

    // ── wait_for_trigger ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_wait_for_trigger_timeout() {
        let interval = Duration::from_millis(50);
        let result =
            wait_for_trigger(interval, None, None, &PollingScheduleConfig::default()).await;
        assert_eq!(result, MonitorTrigger::Timer, "應為定時觸發");
    }

    #[tokio::test]
    async fn test_wait_for_trigger_force() {
        let interval = Duration::from_secs(60); // 很長，確保不會先到期
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = Arc::clone(&notify);

        // 在背景觸發通知
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            notify_clone.notify_one();
        });

        let result = wait_for_trigger(
            interval,
            Some(&notify),
            None,
            &PollingScheduleConfig::default(),
        )
        .await;
        assert_eq!(result, MonitorTrigger::ForcePoll, "應為強制觸發");
    }

    #[tokio::test]
    async fn test_wait_for_trigger_prefers_force_over_timer() {
        // 設定很短的 interval，但強制通知更早到達
        let interval = Duration::from_millis(100);
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = Arc::clone(&notify);

        // 立即觸發通知
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            notify_clone.notify_one();
        });

        let start = std::time::Instant::now();
        let result = wait_for_trigger(
            interval,
            Some(&notify),
            None,
            &PollingScheduleConfig::default(),
        )
        .await;
        let elapsed = start.elapsed();

        // 應該在 50ms 內返回（因為通知在 1ms 後觸發）
        assert_eq!(result, MonitorTrigger::ForcePoll, "應為強制觸發");
        assert!(
            elapsed < Duration::from_millis(50),
            "應在 50ms 內返回，實際：{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_wait_for_trigger_reauth_interrupts_timer() {
        let interval = Duration::from_secs(60); // 很長，確保不會先到期
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = Arc::clone(&notify);

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            notify_clone.notify_one();
        });

        let start = std::time::Instant::now();
        let result = wait_for_trigger(
            interval,
            None,
            Some(&notify),
            &PollingScheduleConfig::default(),
        )
        .await;
        let elapsed = start.elapsed();

        assert_eq!(result, MonitorTrigger::Reauth, "應為重新認證觸發");
        assert!(
            elapsed < Duration::from_millis(50),
            "應在 50ms 內返回，實際：{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_wait_for_trigger_no_force_respects_interval() {
        let interval = Duration::from_millis(100);
        let start = std::time::Instant::now();
        let result =
            wait_for_trigger(interval, None, None, &PollingScheduleConfig::default()).await;
        let elapsed = start.elapsed();

        assert_eq!(result, MonitorTrigger::Timer, "應為定時觸發");
        assert!(
            elapsed >= Duration::from_millis(90),
            "應等待至少 90ms，實際：{elapsed:?}"
        );
    }

    #[test]
    fn test_is_within_poll_window_true_during_period() {
        let schedule = make_test_schedule();
        assert!(is_within_poll_window(
            &schedule,
            Weekday::Mon,
            NaiveTime::from_hms_opt(7, 30, 0).unwrap()
        ));
    }

    #[test]
    fn test_is_within_poll_window_false_on_rest_day() {
        let schedule = make_test_schedule();
        assert!(!is_within_poll_window(
            &schedule,
            Weekday::Sun,
            NaiveTime::from_hms_opt(7, 30, 0).unwrap()
        ));
    }

    #[test]
    fn test_next_poll_window_between_periods_same_day() {
        let schedule = make_test_schedule();
        let next = next_poll_window(
            &schedule,
            Weekday::Mon,
            NaiveTime::from_hms_opt(8, 5, 0).unwrap(),
        )
        .unwrap();

        assert_eq!(next.label, "08:10~09:00");
        assert_eq!(next.wait, Duration::from_secs(5 * 60));
    }

    #[test]
    fn test_next_poll_window_skips_sunday_rest_day() {
        let schedule = make_test_schedule();
        let next = next_poll_window(
            &schedule,
            Weekday::Sun,
            NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        )
        .unwrap();

        assert_eq!(next.label, "07:10~08:00");
        assert_eq!(
            next.wait,
            Duration::from_secs((24 * 60 + 7 * 60 + 10 - 9 * 60) * 60)
        );
    }

    // ── MonitorStatus 相關 ────────────────────────────────────────────────────

    #[test]
    fn test_monitor_status_payload_fields() {
        let status = MonitorStatus {
            is_running: true,
            user_name: "test_user".to_string(),
            last_poll_timestamp: Some(1_700_000_000),
            last_success_course: Some("計算機概論".to_string()),
            last_success_rollcall_id: Some(1),
            last_detected_timestamp: None,
            last_detected_course: None,
            last_detected_rollcall_id: None,
            last_detected_url: None,
            rollcall_history: vec![],
            consecutive_failures: 2,
            started_at: 1_699_900_000,
            last_failure: None,
        };

        assert!(status.is_running);
        assert_eq!(status.user_name, "test_user");
        assert_eq!(status.last_success_course.as_deref(), Some("計算機概論"));
        assert_eq!(status.consecutive_failures, 2);
    }

    #[test]
    fn test_monitor_status_no_poll_yet() {
        let status = MonitorStatus {
            is_running: true,
            user_name: "user".to_string(),
            last_poll_timestamp: None,
            last_success_course: None,
            last_success_rollcall_id: None,
            last_detected_timestamp: None,
            last_detected_course: None,
            last_detected_rollcall_id: None,
            last_detected_url: None,
            rollcall_history: vec![],
            consecutive_failures: 0,
            started_at: 0,
            last_failure: None,
        };

        assert_eq!(status.last_poll_timestamp, None);
        assert_eq!(status.last_success_course, None);
    }

    // ── BatchSummary 整合 ─────────────────────────────────────────────────────

    #[test]
    fn test_batch_summary_default_is_empty() {
        let summary = BatchSummary::default();
        assert_eq!(summary.total, 0);
        assert_eq!(summary.success, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.fatal, 0);
        assert!(!summary.has_fatal());
        assert!(summary.all_success()); // 0 == 0
    }

    // ── 並發安全性測試 ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_status_concurrent_access() {
        let status = Arc::new(Mutex::new(MonitorStatus {
            is_running: true,
            user_name: "initial".to_string(),
            last_poll_timestamp: None,
            last_success_course: None,
            last_success_rollcall_id: None,
            last_detected_timestamp: None,
            last_detected_course: None,
            last_detected_rollcall_id: None,
            last_detected_url: None,
            rollcall_history: vec![],
            consecutive_failures: 0,
            started_at: 0,
            last_failure: None,
        }));

        let mut handles = Vec::new();

        // 多個並發任務同時更新狀態
        for i in 0..10 {
            let status_clone = Arc::clone(&status);
            let handle = tokio::spawn(async move {
                let mut s = status_clone.lock().await;
                s.consecutive_failures = i;
                s.last_poll_timestamp = Some(i as i64);
                drop(s);

                // 小延遲讓其他任務有機會執行
                tokio::time::sleep(Duration::from_millis(1)).await;

                let s = status_clone.lock().await;
                // 確認狀態可以正常讀取
                assert!(s.consecutive_failures <= 10);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // 最終狀態應該是某個任務設定的值
        let final_status = status.lock().await;
        assert!(final_status.consecutive_failures <= 9);
    }

    #[tokio::test]
    async fn test_notify_multiple_waiters() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let ready = Arc::new(AtomicBool::new(false));

        let notify_clone = Arc::clone(&notify);
        let ready_clone = Arc::clone(&ready);

        // 等待通知的任務
        let waiter = tokio::spawn(async move {
            notify_clone.notified().await;
            ready_clone.store(true, Ordering::SeqCst);
        });

        // 發送通知
        tokio::time::sleep(Duration::from_millis(10)).await;
        notify.notify_one();

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(ready.load(Ordering::SeqCst), "等待者應在通知後被喚醒");

        waiter.await.unwrap();
    }

    // ── QR code 通道整合測試 ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_qrcode_channel_integration() {
        use crate::adapters::requests::create_qrcode_channel;

        let (tx, rx) = create_qrcode_channel(2);

        // 傳送兩個 QR code
        tx.send("qr_1".to_string()).await.unwrap();
        tx.send("qr_2".to_string()).await.unwrap();

        let mut rx_guard = rx.lock().await;
        assert_eq!(rx_guard.recv().await, Some("qr_1".to_string()));
        assert_eq!(rx_guard.recv().await, Some("qr_2".to_string()));
    }

    #[tokio::test]
    async fn test_qrcode_channel_overflow() {
        use crate::adapters::requests::create_qrcode_channel;

        let (tx, _rx) = create_qrcode_channel(1); // 容量只有 1

        // 第一個應該成功
        tx.send("qr_1".to_string()).await.unwrap();

        // 第二個在 channel 滿時應該失敗（因為 _rx 不讀取）
        let result = tx.try_send("qr_2".to_string());
        assert!(result.is_err(), "Channel 滿時 try_send 應失敗");
    }

    // ── 時間相關邊界測試 ──────────────────────────────────────────────────────

    #[test]
    fn test_duration_from_secs_zero() {
        let d = Duration::from_secs(0);
        assert_eq!(d, Duration::ZERO);
        // startup_delay = 0 時不應等待
        assert!(d.is_zero());
    }

    #[test]
    fn test_duration_comparison() {
        let short = Duration::from_millis(100);
        let long = Duration::from_secs(60);
        assert!(short < long);
        assert!(long > short);
    }

    fn make_test_schedule() -> PollingScheduleConfig {
        PollingScheduleConfig {
            periods: vec!["07:10~08:00".to_string(), "08:10~09:00".to_string()],
            rest_weekdays: vec![ScheduleWeekday::Sun],
        }
    }
}
