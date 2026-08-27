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

async fn check_account_homework(
    account: &AccountConfig,
    accounts_db: &AccountsDb,
    messenger: Option<&dyn AdapterMessenger>,
    now_utc: DateTime<Utc>,
) -> miette::Result<()> {
    // 建立 API 客戶端（優先使用既有已驗證 Session Cookie）
    let (auth_client, session) = match AuthClient::new(account).await {
        Ok(res) => {
            let _ = accounts_db.set_session_cookie(&account.id, &res.1.cookie_string).await;
            res
        }
        Err(e) => {
            debug!(account = %account.id, error = %e, "登入認證失敗，略過本輪作業檢查");
            return Ok(());
        }
    };

    let api = ApiClient::new(auth_client.client, account.base_url());
    let todos = match api.get_todos().await {
        Ok(items) => items,
        Err(e) => {
            debug!(account = %account.id, error = %e, "取得 todos 失敗，可能該校無待辦清單端點");
            return Ok(());
        }
    };

    for todo in todos {
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
