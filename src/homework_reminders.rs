//! Tronclass 作業/待辦事項截止倒數提醒排程模組
//!
//! 核心職責：
//! - 定期（預設每 15 分鐘）線上輪詢各帳號的 `/api/todos`。
//! - 識別未繳交作業，依剩餘時間進行階梯式倒數（24 小時、6 小時、最後 1 小時）。
//! - 透過 SQLite 進行防重複發送與截止時間延期重置。
//! - 透過 Discord / LINE / 手機 Web Push 發送即時提醒。

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use tracing::{debug, info, warn};

use crate::account::AccountConfig;
use crate::adapters::events::{
    AdapterAccountTarget, AdapterMessenger, HomeworkDeadlineEvent, OutboundMessage,
};
use crate::api::ApiClient;
use crate::auth::AuthClient;
use crate::config::AppConfig;
use crate::db::AccountsDb;
use crate::web_push_notifications;

const CHECK_INTERVAL_SECS: u64 = 15 * 60; // 每 15 分鐘檢查一次
const RETENTION_DAYS: i64 = 14;

/// 背景預抓間隔。
///
/// ALWAYS_FRESH_ON_OPEN_V1
/// 刻意設得比 `homework_cache::CACHE_TTL`（5 分鐘）短，
/// 讓快取在使用者開頁前就已經被刷新，永遠處於「新鮮」狀態。
/// 這樣一進網站就是命中快取（0 上游請求、立即顯示），且資料是即時的。
const PREFETCH_INTERVAL_SECS: u64 = 4 * 60;

/// 預抓時每個帳號之間的間隔，平滑對學校伺服器的請求流量。
const PREFETCH_ACCOUNT_GAP_SECS: u64 = 2;

/// 啟動後多久開始第一次預抓（讓監控器與 adapter 先就緒）。
const PREFETCH_STARTUP_DELAY_SECS: u64 = 5;

/// 啟動作業截止倒數提醒排程器
pub fn start_homework_reminder_scheduler(
    accounts_db: AccountsDb,
    app_config: Arc<AppConfig>,
    messenger: Option<Arc<dyn AdapterMessenger>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("Tronclass 作業截止倒數提醒排程器已啟動（每 15 分鐘檢查一次）");

        // 啟動後稍等 10 秒，讓主監控和 Adapter 完整就緒
        tokio::time::sleep(Duration::from_secs(10)).await;

        let mut ticker = tokio::time::interval(Duration::from_secs(CHECK_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            if let Err(e) = run_once(&accounts_db, &app_config, messenger.as_deref()).await {
                warn!(error = %e, "作業截止提醒排程器執行失敗");
            }
        }
    })
}

/// 啟動作業清單背景預抓器。
///
/// ALWAYS_FRESH_ON_OPEN_V1
/// 目的：讓使用者「一進網站就看到即時的作業」。
///
/// - 開機後 5 秒立刻抓一輪 → 首次開頁就不必空等
/// - 之後每 4 分鐘刷新（短於 5 分鐘的快取 TTL）→ 快取永遠是熱的
/// - 抓完同時評估截止提醒，通知不必等 15 分鐘排程
///
/// 與 15 分鐘的提醒排程器並存：這裡負責「資料新鮮度」，
/// 那裡負責「保底」，兩者共用 single-flight 不會重複打上游。
pub fn start_homework_prefetcher(
    accounts_db: AccountsDb,
    app_config: Arc<AppConfig>,
    messenger: Option<Arc<dyn AdapterMessenger>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            interval_secs = PREFETCH_INTERVAL_SECS,
            "作業清單背景預抓器已啟動（開頁即為最新資料）"
        );

        tokio::time::sleep(Duration::from_secs(PREFETCH_STARTUP_DELAY_SECS)).await;

        let mut ticker = tokio::time::interval(Duration::from_secs(PREFETCH_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            prefetch_all_accounts(&accounts_db, &app_config, messenger.as_deref()).await;
        }
    })
}

/// 為所有啟用中的帳號預抓作業清單並回填快取／快照。
async fn prefetch_all_accounts(
    accounts_db: &AccountsDb,
    app_config: &AppConfig,
    messenger: Option<&dyn AdapterMessenger>,
) {
    let accounts = match accounts_db.resolve(app_config).await {
        Ok(accounts) => accounts,
        Err(e) => {
            debug!(error = %e, "預抓：讀取啟用帳號清單失敗，略過本輪");
            return;
        }
    };

    let now_utc = Utc::now();
    let mut refreshed = 0usize;

    for account in accounts {
        if !account.enabled {
            continue;
        }

        let account_for_fetch = account.clone();
        let db_for_fetch = accounts_db.clone();
        let persist_key = account.id.clone();

        // force=true：預抓的重點就是把資料換成最新的。
        // single-flight 確保與網頁／排程器同時發生時只會抓一次。
        match crate::homework_cache::get_or_fetch(&account.id, true, || async move {
            let items = fetch_todos_with_saved_session(&account_for_fetch, &db_for_fetch).await?;
            crate::homework_cache::persist(&persist_key, &items, &db_for_fetch).await;
            Ok(items)
        })
        .await
        {
            Ok(items) => {
                refreshed += 1;
                // 預抓到新資料時順便評估提醒，讓通知即時。
                // 去重表確保不會重複發送。
                if let Err(error) =
                    evaluate_and_send_reminders(&account, accounts_db, messenger, &items, now_utc)
                        .await
                {
                    warn!(account = %account.id, %error, "預抓後評估作業提醒失敗");
                }
            }
            Err(e) => {
                debug!(account = %account.id, error = %e, "預抓作業清單失敗");
            }
        }

        tokio::time::sleep(Duration::from_secs(PREFETCH_ACCOUNT_GAP_SECS)).await;
    }

    if refreshed > 0 {
        debug!(accounts = refreshed, "作業清單預抓完成，快取已更新");
    }
}

async fn run_once(
    accounts_db: &AccountsDb,
    app_config: &AppConfig,
    messenger: Option<&dyn AdapterMessenger>,
) -> miette::Result<()> {
    let accounts = match accounts_db.resolve(app_config).await {
        Ok(accounts) => accounts,
        Err(e) => {
            debug!(error = %e, "作業提醒：讀取啟用帳號清單失敗，略過本輪");
            return Ok(());
        }
    };

    let now_utc = Utc::now();
    let cutoff = now_utc.timestamp() - RETENTION_DAYS * 24 * 60 * 60;
    let _ = accounts_db
        .cleanup_old_homework_reminders_sent(cutoff)
        .await;

    for account in accounts {
        if !account.enabled {
            continue;
        }

        if let Err(e) = check_account_homework(&account, accounts_db, messenger, now_utc).await {
            warn!(account = %account.id, error = %e, "檢查帳號作業截止狀態失敗");
        }

        // 帳號間隔 1 秒，平滑請求流量避免學校 WAF 封鎖
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    Ok(())
}

/// 直接以資料庫中已儲存的 session cookie 取得作業清單，**不做預先驗證探測**。
///
/// 只有在 `get_todos()` 真的回報認證錯誤時，才退回完整登入流程重試一次。
/// 正常情況（cookie 有效）全程零額外請求、零額外日誌。
async fn fetch_todos_with_saved_session(
    account: &AccountConfig,
    accounts_db: &AccountsDb,
) -> miette::Result<Vec<crate::api::todos::TronclassTodoItem>> {
    let saved_cookie = account.session_cookie.trim();

    if !saved_cookie.is_empty() {
        // 透過 `session_client` 取得快取的 HTTP client：只在 session cookie
        // 變更時重建，其餘時間共享連線池，避免每 4 分鐘每個帳號重打握手。
        if let Some(client) = crate::session_client::session_http_client(account) {
            let api = ApiClient::new((*client).clone(), account.base_url());
            match api.get_todos().await {
                Ok(items) => return Ok(items),
                Err(e) if !crate::api::is_auth_error(&e.to_string()) => return Err(e),
                Err(_) => {
                    debug!(
                        account = %account.id,
                        "已儲存的 session cookie 已失效，改用帳密重新登入"
                    );
                }
            }
        }
    }

    // Cookie 不存在或已失效：走完整登入流程（此時才會出現認證日誌，屬預期行為）。
    let (auth_client, session) = AuthClient::new(account).await?;
    let _ = accounts_db
        .set_session_cookie(&account.id, &session.cookie_string)
        .await;
    let api = ApiClient::new(auth_client.client, account.base_url());
    api.get_todos().await
}

async fn check_account_homework(
    account: &AccountConfig,
    accounts_db: &AccountsDb,
    messenger: Option<&dyn AdapterMessenger>,
    now_utc: DateTime<Utc>,
) -> miette::Result<()> {
    // NO_REDUNDANT_SESSION_PROBE_V1 + FRESH_DATA_FOR_REMINDERS_V1
    // 提醒判斷必須用「夠新」的資料：快取 TTL 是 5 分鐘、排程間隔 15 分鐘，
    // 若沿用網頁填的舊快取，最壞情況會拿到近 5 分鐘前的資料來算剩餘時間，
    // 在 1 小時緊急提醒的邊界上可能延誤。因此這裡 force=true 強制取新資料。
    //
    // 這不會造成重複請求：single-flight 仍會把同時發生的抓取合併成一次，
    // 且抓完會回填快取／快照，網頁隨後開啟即可直接秒顯示。
    let account_for_fetch = account.clone();
    let db_for_fetch = accounts_db.clone();
    let persist_key = account.id.clone();
    let todos = match crate::homework_cache::get_or_fetch(&account.id, true, || async move {
        let items = fetch_todos_with_saved_session(&account_for_fetch, &db_for_fetch).await?;
        // 順便更新 SQLite 快照，讓使用者下次開頁能立即看到最新結果。
        crate::homework_cache::persist(&persist_key, &items, &db_for_fetch).await;
        Ok(items)
    })
    .await
    {
        Ok(items) => items,
        Err(e) => {
            debug!(account = %account.id, error = %e, "取得 todos 失敗，略過本輪作業檢查");
            return Ok(());
        }
    };

    evaluate_and_send_reminders(account, accounts_db, messenger, &todos, now_utc).await
}

/// 依作業清單評估三階梯倒數提醒，必要時發送並記錄。
///
/// REMINDERS_ON_ANY_REFRESH_V1
/// 抽成獨立函式，讓「任何取得新資料的路徑」都能評估提醒，而不只是背景排程器。
/// 網頁背景更新也會呼叫它，因此使用者開頁順手觸發的更新同樣能及時發出通知。
///
/// 重複呼叫是安全的：`homework_reminder_sent` 以
/// `(account_id, homework_id, stage)` 為主鍵，已送出的階梯不會重複發送。
pub async fn evaluate_and_send_reminders(
    account: &AccountConfig,
    accounts_db: &AccountsDb,
    messenger: Option<&dyn AdapterMessenger>,
    todos: &[crate::api::todos::TronclassTodoItem],
    now_utc: DateTime<Utc>,
) -> miette::Result<()> {
    // HOMEWORK_USER_COMPLETED_V1
    // 載入「使用者手動標記完成」的作業集合，這些作業不發任何截止提醒
    // （即使上游 is_submitted 尚未即時反映，例如實體繳交）。
    let user_completed = accounts_db
        .list_homework_user_completed(&account.id)
        .await
        .unwrap_or_default();

    for todo in todos.iter() {
        // HOMEWORK_TRACKED_TYPES_V1
        // 截止提醒只針對「作業」與「測驗/考試」。其餘課程項目（教材 material、
        // 連結 web_link、頁面 page、影片 online_video、討論 forum、問卷 questionnaire…）
        // 即使帶截止時間也不發送，避免誤報。
        let is_tracked_type = matches!(
            todo.item_type.trim().to_ascii_lowercase().as_str(),
            "homework" | "quiz" | "exam"
        );
        if !is_tracked_type {
            continue;
        }

        if !todo.is_active_pending(now_utc) {
            continue;
        }
        if user_completed.contains(&todo.id.to_string()) {
            continue;
        }

        let Some(deadline) = todo.end_time else {
            continue;
        };
        let duration = deadline.signed_duration_since(now_utc);
        let total_minutes = duration.num_minutes();
        if total_minutes <= 0 {
            continue;
        }

        // 判斷階梯階段 (Stage)
        let (stage, stage_badge, time_desc) = if total_minutes <= 60 {
            ("1h", "最後 1 小時", format!("{total_minutes} 分鐘"))
        } else if total_minutes <= 6 * 60 {
            let hours = total_minutes / 60;
            let mins = total_minutes % 60;
            ("6h", "6 小時內截止", format!("{hours} 小時 {mins} 分鐘"))
        } else if total_minutes <= 24 * 60 {
            let hours = total_minutes / 60;
            ("24h", "24 小時內截止", format!("約 {hours} 小時"))
        } else {
            // 超過 24 小時，暫不發送緊急通知
            continue;
        };

        let hw_id_str = todo.id.to_string();
        let deadline_timestamp = deadline.timestamp();

        // 檢查此階梯是否已發過（若老師延期則會自動重置為 false）
        let already_sent = accounts_db
            .is_homework_reminder_sent(&account.id, &hw_id_str, stage, deadline_timestamp)
            .await?;

        if already_sent {
            continue;
        }

        let course_name = todo
            .course_name
            .as_deref()
            .unwrap_or("未指定課程")
            .to_string();
        let title = todo.title.clone();
        let deadline_local = deadline.with_timezone(&Local);
        let deadline_str = deadline_local.format("%Y-%m-%d %H:%M").to_string();

        // HOMEWORK_NOTIFICATION_PREFERENCE_V1
        // 改用專屬的 HomeworkDeadline 事件（而非泛用的 Text），這樣訊息才會被
        // 對應到 "homework_deadline" 這個通知鍵，使用者能在通知設定頁自行關閉。
        // 以前直接送 Text 會繞過所有偏好檢查，關不掉。
        let event = OutboundMessage::HomeworkDeadline(HomeworkDeadlineEvent {
            account: account.id.clone(),
            course_name: course_name.clone(),
            title: title.clone(),
            stage_badge: stage_badge.to_string(),
            deadline: deadline_str.clone(),
            time_remaining: time_desc.clone(),
        });

        let mut delivered = false;

        // 1. 透過 Messenger 發送（Discord / LINE）
        // 使用 push_to_account_or_admin 讓通知偏好開關生效。
        if let Some(m) = messenger {
            let target = AdapterAccountTarget::new(
                account.id.clone(),
                account.line_user_id.clone(),
                account.discord_user_id.clone(),
            );
            if let Err(e) = m.push_to_account_or_admin(&target, &event).await {
                warn!(account = %account.id, error = %e, "作業提醒發送失敗");
            } else {
                delivered = true;
            }
        }

        // 2. 透過手機 Web Push 發送
        web_push_notifications::notify_homework(
            &account.id,
            todo.id,
            &course_name,
            &title,
            stage_badge,
        );

        // 標記已送出
        accounts_db
            .mark_homework_reminder_sent(&account.id, &hw_id_str, stage, deadline_timestamp)
            .await?;

        info!(
            account = %account.id,
            course = %course_name,
            title = %title,
            stage = %stage,
            delivered = delivered,
            "作業截止倒數提醒已發送"
        );
    }

    Ok(())
}
