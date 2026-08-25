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

use crate::account::AccountConfig;
use crate::adapters::events::{
    AdapterAccountTarget, AdapterMessenger, MonitorStatus, OutboundMessage, RollcallHistoryEntry,
    SystemStartedEvent,
};
use crate::adapters::requests::{
    create_number_code_channel, create_qrcode_channel, NumberCodeReceiver, NumberCodeSender,
    QrCodeReceiver, QrCodeSender, RequestAccountState,
};
use crate::adapters::scanner::QrScannerRegistry;
use crate::api::{is_auth_error, ApiClient};
use crate::auth::AuthClient;
use crate::config::{parse_schedule_period, AppConfig, PollingScheduleConfig};
use crate::db::AccountsDb;
use crate::rollcalls::{process_rollcall_batch, BatchSummary};

// ─── 監控器設定 ───────────────────────────────────────────────────────────────

const FORCE_CASCADE_DEDUPE_TTL: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct ForceCascade {
    peers: Arc<Vec<ForceCascadePeer>>,
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
            peers: Arc::new(
                peers
                    .into_iter()
                    .map(|(account_id, notify)| ForceCascadePeer { account_id, notify })
                    .collect(),
            ),
            recent_rollcalls: Arc::new(Mutex::new(HashMap::new())),
        }
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
        for peer in self.peers.iter() {
            if peer.account_id == source_account_id {
                continue;
            }
            peer.notify.notify_one();
            notified += 1;
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

    // PERFORMANCE_MONITOR_TARGET_CACHE_V1:
    // Cache adapter_target at the start of each iteration to avoid 5+ lock
    // acquisitions per loop cycle. The target rarely changes (only on
    // Discord/LINE rebind), so one read per iteration is sufficient.
    let mut cached_target = ctx.adapter_target.lock().await.clone();

    loop {
        // ── 等待觸發（定時、強制 or 重新認證）──────────────────────────────────
        let poll_interval =
            crate::runtime_settings::poll_interval(provider_poll_interval_secs).await;
        let trigger = wait_for_trigger(
            poll_interval,
            force_poll_notify.as_deref(),
            reauth_notify.as_deref(),
            &schedule,
        )
        .await;

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
            if !is_within_poll_window(&schedule, now.weekday(), now.time()) {
                debug!("目前不在課堂輪詢時段內，略過本次定時觸發");
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

        match do_poll_and_attend(&mut ctx, trigger).await {
            Ok(summary) => {
                // 成功輪詢
                consecutive_failures = 0;

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

                // 更新狀態
                {
                    let mut s = ctx.status.lock().await;
                    s.consecutive_failures = consecutive_failures;
                }

                // 判斷是否需要重新認證
                if is_auth_error(&e.to_string()) || consecutive_failures >= max_failures {
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

                            // 重新認證失敗後等待更長的時間再重試
                            warn!(
                                retry_secs = config.monitor.retry_interval_secs,
                                "重新認證失敗，等待 {} 秒後重試",
                                config.monitor.retry_interval_secs
                            );
                            tokio::time::sleep(retry_interval).await;
                        }
                    }
                } else {
                    // 普通失敗，等待 retry_interval 後繼續
                    warn!(
                        retry_secs = config.monitor.retry_interval_secs,
                        "等待 {} 秒後重試...", config.monitor.retry_interval_secs
                    );
                    tokio::time::sleep(retry_interval).await;
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
        .map(|period| parse_schedule_period(period).expect("validated schedule period"))
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

    let idx = WEEK
        .iter()
        .position(|day| *day == weekday)
        .expect("known weekday");
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
    use super::*;
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

    // ── is_auth_error ─────────────────────────────────────────────────────────

    #[test]
    fn test_is_auth_error_true_cases() {
        assert!(is_auth_error("Unauthorized access"));
        assert!(is_auth_error("HTTP 401 error"));
        assert!(is_auth_error("Response status: 403 Forbidden"));
        assert!(is_auth_error("Session expired, please re-login"));
        assert!(is_auth_error("session cookie invalid"));
        assert!(is_auth_error("session 可能已過期"));
        assert!(is_auth_error("批次簽到中發生致命錯誤"));
    }

    #[test]
    fn test_is_auth_error_false_cases() {
        assert!(!is_auth_error("Network timeout"));
        assert!(!is_auth_error("HTTP 500 Internal Server Error"));
        assert!(!is_auth_error("JSON parse error"));
        assert!(!is_auth_error("距離不足 50 公尺"));
        assert!(!is_auth_error("爆破失敗：無正確代碼"));
        assert!(!is_auth_error(""));
    }

    #[test]
    fn test_is_auth_error_edge_cases() {
        // 包含 "4" 和 "01" 但不是 "401"
        assert!(!is_auth_error("Error code 4: timeout"));
        // 包含 "Session" 的正常訊息
        assert!(is_auth_error("Session management failed"));
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
