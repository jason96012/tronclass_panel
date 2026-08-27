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
use crate::adapters::events::{AdapterMessenger, OutboundMessage};
use crate::api::ApiClient;
use crate::auth::AuthClient;
use crate::config::AppConfig;
use crate::db::AccountsDb;
use crate::web_push_notifications;

const CHECK_INTERVAL_SECS: u64 = 15 * 60; // 每 15 分鐘檢查一次
const RETENTION_DAYS: i64 = 14;

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
        let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
        if let Ok(url) = reqwest::Url::parse(account.base_url()) {
            for part in saved_cookie.split(';') {
                let part = part.trim();
                if !part.is_empty() {
                    jar.add_cookie_str(part, &url);
                }
            }

            if let Ok(client) =
                crate::api::build_http_client(jar, account.request_timeout_secs)
            {
                let api = ApiClient::new(client, account.base_url());
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
    for todo in todos.iter() {
        if !todo.is_active_pending(now_utc) {
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
            ("1h", "🚨 最後 1 小時緊急提醒", format!("{total_minutes} 分鐘"))
        } else if total_minutes <= 6 * 60 {
            let hours = total_minutes / 60;
            let mins = total_minutes % 60;
            ("6h", "⚠️ 6 小時倒數提醒", format!("{hours} 小時 {mins} 分鐘"))
        } else if total_minutes <= 24 * 60 {
            let hours = total_minutes / 60;
            ("24h", "⏳ 24 小時倒數提醒", format!("約 {hours} 小時"))
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

        let message = format!(
            "📝 Tronclass 作業截止提醒\n\
             【{}】\n\n\
             📚 課程：{}\n\
             📌 標題：{}\n\
             ⏳ 截止時間：{}\n\
             ⏱️ 剩餘時間：{}\n\
             👤 帳號：{}\n\n\
             👉 請儘速至 Tronclass 平台確認並繳交！",
            stage_badge, course_name, title, deadline_str, time_desc, account.id
        );

        let mut delivered = false;

        // 1. 透過 Messenger 發送（Discord / LINE）
        if let Some(m) = messenger {
            if !account.discord_user_id.is_empty() {
                if let Err(e) = m
                    .push(&account.discord_user_id, &OutboundMessage::Text(message.clone()))
                    .await
                {
                    warn!(account = %account.id, error = %e, "Discord 作業提醒發送失敗");
                } else {
                    delivered = true;
                }
            }

            if !account.line_user_id.is_empty() {
                if let Err(e) = m
                    .push(&account.line_user_id, &OutboundMessage::Text(message.clone()))
                    .await
                {
                    warn!(account = %account.id, error = %e, "LINE 作業提醒發送失敗");
                } else {
                    delivered = true;
                }
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
