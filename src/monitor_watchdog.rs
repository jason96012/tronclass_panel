//! MONITOR_WATCHDOG_V1：監控循環看門狗。
//!
//! 每個帳號的監控循環在每次迭代、每段等待／退避睡眠中至少每 60 秒更新一次心跳
//! （`RequestAccountState::beat`）。這裡每 30 秒掃一次登錄簿：
//!
//! - 心跳距今超過 `stale_after(interval)`（輪詢間隔 × 6，最少 5 分鐘、最多 30 分鐘）
//!   → 視為卡死：寫 log、記錄到系統日誌、中止該任務、透過 `MonitorSpawner::respawn_account`
//!   重新登入並起一條新循環，並以 Discord／手機推播通知管理員（成功與失敗都通知）。
//! - 心跳為 0（循環還在啟動延遲中）或帳號已暫停（`is_running = false`）→ 不判定。
//! - 同一帳號兩次重生之間至少隔 `RESPAWN_COOLDOWN`，避免學校網站長時間無回應時無限重登。
//!
//! /healthz 也用同一套 `stale_after` 規則判定帳號是否失聯（見 admin_web/health.rs）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};

use crate::adapters::events::{AdapterMessenger, AdminAlertNotice, OutboundMessage};
use crate::monitor::MonitorSpawner;

/// 掃描週期。
pub const SCAN_INTERVAL: Duration = Duration::from_secs(30);
/// 同一帳號兩次重生的最短間隔。
const RESPAWN_COOLDOWN: Duration = Duration::from_secs(10 * 60);
/// 心跳容忍倍數與上下限。
const STALE_MULTIPLIER: u64 = 6;
const STALE_MIN_SECS: u64 = 5 * 60;
const STALE_MAX_SECS: u64 = 30 * 60;

/// 依輪詢間隔算出「多久沒心跳算卡死」（秒）。
pub fn stale_after_secs(poll_interval_secs: u64) -> u64 {
    poll_interval_secs
        .saturating_mul(STALE_MULTIPLIER)
        .clamp(STALE_MIN_SECS, STALE_MAX_SECS)
}

/// 判定單一帳號心跳是否過期。`heartbeat == 0` 代表尚未開始，不算過期；
/// `heartbeat == HEARTBEAT_CRASHED`（1）代表循環已異常結束，一律視為過期。
pub fn is_stale(heartbeat: i64, now: i64, poll_interval_secs: u64) -> bool {
    if heartbeat <= 0 {
        return false;
    }
    if heartbeat == crate::adapters::requests::HEARTBEAT_CRASHED {
        return true;
    }
    now.saturating_sub(heartbeat) > stale_after_secs(poll_interval_secs) as i64
}

/// 啟動看門狗背景任務（main 呼叫一次）。
pub fn spawn(spawner: MonitorSpawner, default_poll_interval_secs: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_respawn: HashMap<String, Instant> = HashMap::new();
        let mut ticker = tokio::time::interval(SCAN_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            scan_once(&spawner, default_poll_interval_secs, &mut last_respawn).await;
        }
    })
}

async fn scan_once(
    spawner: &MonitorSpawner,
    default_poll_interval_secs: u64,
    last_respawn: &mut HashMap<String, Instant>,
) {
    let poll_interval_secs = crate::runtime_settings::poll_interval(default_poll_interval_secs)
        .await
        .as_secs()
        .max(1);
    let now = chrono::Local::now().timestamp();
    let accounts = spawner.registry().snapshot();
    let mut stale_ids: Vec<(String, i64, bool)> = Vec::new();
    for account in accounts.iter() {
        let heartbeat = account.last_heartbeat();
        if !is_stale(heartbeat, now, poll_interval_secs) {
            continue;
        }
        // 已暫停的帳號：循環仍在跑（等待觸發）所以心跳不會停；這裡多一層保險，
        // 暫停中就算心跳過期也不重生（重生會把它變成 running）。
        if !*account.is_running.lock().await {
            debug!(account = %account.account_id, "看門狗：帳號已暫停，略過心跳判定");
            continue;
        }
        let crashed = heartbeat == crate::adapters::requests::HEARTBEAT_CRASHED;
        stale_ids.push((
            account.account_id.clone(),
            if crashed { 0 } else { now.saturating_sub(heartbeat) },
            crashed,
        ));
    }
    if stale_ids.is_empty() {
        return;
    }
    let cooldown_gate = Instant::now();
    last_respawn.retain(|_, at| cooldown_gate.duration_since(*at) < RESPAWN_COOLDOWN * 3);
    for (account_id, silent_secs, crashed) in stale_ids {
        let situation = if crashed {
            "監控循環異常結束（已回報錯誤）".to_string()
        } else {
            format!(
                "監控循環 {silent_secs} 秒沒有心跳（門檻 {} 秒）",
                stale_after_secs(poll_interval_secs)
            )
        };
        if let Some(at) = last_respawn.get(&account_id) {
            if cooldown_gate.duration_since(*at) < RESPAWN_COOLDOWN {
                debug!(account = %account_id, "看門狗：距上次重生未滿冷卻時間，略過");
                continue;
            }
        }
        last_respawn.insert(account_id.clone(), Instant::now());
        warn!(
            account = %account_id,
            silent_secs,
            crashed,
            threshold_secs = stale_after_secs(poll_interval_secs),
            "看門狗：監控循環停止（卡死或異常結束），準備中止並重生"
        );
        crate::logs::record_system(
            "WARN",
            "monitor_watchdog",
            format!("帳號 {account_id}：{situation}，看門狗中止並重新啟動"),
            None,
        );
        let result = spawner.respawn_account(&account_id).await;
        let (title, body) = match &result {
            Ok(added) => {
                info!(account = %account_id, user_name = %added.user_name, "看門狗：監控循環已重生");
                (
                    format!("監控看門狗：已重啟 {account_id}"),
                    format!(
                        "帳號：{account_id}\n狀況：{situation}，已自動中止並重新登入。\n結果：重生成功，監控已恢復。"
                    ),
                )
            }
            Err(reason) => {
                error!(account = %account_id, error = %reason, "看門狗：監控循環重生失敗");
                crate::logs::record_system(
                    "ERROR",
                    "monitor_watchdog",
                    format!("帳號 {account_id} 看門狗重生失敗：{reason}"),
                    None,
                );
                (
                    format!("監控看門狗：重啟 {account_id} 失敗"),
                    format!(
                        "帳號：{account_id}\n狀況：{situation}，已中止舊循環。\n結果：重新登入失敗：{reason}\n看門狗 10 分鐘後會再試一次；也可到儀表板「未監控帳號」按啟動監控。"
                    ),
                )
            }
        };
        notify_admins(spawner, &account_id, result.is_ok(), title, body).await;
    }
}

/// 通知管理員（Discord admin DM + is_admin 帳號 + 手機推播）。
async fn notify_admins(
    spawner: &MonitorSpawner,
    account_id: &str,
    ok: bool,
    title: String,
    body: String,
) {
    let messenger: Option<Arc<dyn AdapterMessenger>> = spawner.messenger();
    // (Discord user id, 對應帳號 id)：DC_CHANNEL_PREFS_V1 逐位管理員檢查偏好。
    let mut discord_recipients: Vec<(String, Option<String>)> = Vec::new();
    let mut admin_account_ids: Vec<String> = Vec::new();
    if let Some(messenger) = messenger.as_ref() {
        let admin = messenger.admin_user_id().trim();
        if !admin.is_empty() {
            discord_recipients.push((admin.to_string(), None));
        }
    }
    match spawner.accounts_db().list().await {
        Ok(accounts) => {
            for account in accounts {
                if !account.is_admin {
                    continue;
                }
                admin_account_ids.push(account.id.clone());
                let id = account.discord_user_id.trim();
                if !id.is_empty()
                    && !discord_recipients
                        .iter()
                        .any(|(existing, _)| existing == id)
                {
                    discord_recipients.push((id.to_string(), Some(account.id.clone())));
                }
            }
        }
        Err(error) => warn!(error = %error, "看門狗：讀取管理員清單失敗"),
    }
    if let Some(messenger) = messenger.as_ref() {
        let message = OutboundMessage::AdminAlert(AdminAlertNotice {
            account: account_id.to_string(),
            grade: if ok { "警告".to_string() } else { "嚴重".to_string() },
            is_recovery: ok,
            title: title.clone(),
            body: body.clone(),
        });
        for (to, pref_account) in &discord_recipients {
            // DC_CHANNEL_PREFS_V1：管理員可在通知設定頁關閉監控警示的 Discord 通知。
            if let Some(pref_account_id) = pref_account {
                match spawner
                    .accounts_db()
                    .notification_preference_enabled(pref_account_id, "discord", "monitor_alert")
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        debug!(
                            account = %pref_account_id,
                            "看門狗通知 Discord 已由使用者關閉"
                        );
                        continue;
                    }
                    Err(error) => {
                        warn!(
                            account = %pref_account_id,
                            error = %error,
                            "讀取看門狗 Discord 通知設定失敗，仍發送"
                        );
                    }
                }
            }
            if let Err(error) = messenger.push(to, &message).await {
                warn!(to = %to, error = %error, "看門狗通知 Discord 推送失敗");
            }
        }
    }
    if !admin_account_ids.is_empty() {
        crate::web_push_notifications::notify_admin_alert(
            admin_account_ids,
            title,
            body,
            format!("tronclass-watchdog-{account_id}"),
        );
    }
}

