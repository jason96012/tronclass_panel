//! Tronclass 作業/待辦事項截止倒數提醒排程模組
//!
//! 核心職責：
//! - 定期（預設每 15 分鐘）線上輪詢各帳號的 `/api/todos`。
//! - 識別未繳交作業，依剩餘時間進行階梯式倒數（24 小時、6 小時、最後 1 小時）。
//! - 透過 SQLite 進行防重複發送與截止時間延期重置。
//! - 透過 Discord / LINE / 手機 Web Push 發送即時提醒。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, Timelike, Utc};
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

pub(crate) const CHECK_INTERVAL_SECS: u64 = 15 * 60; // 每 15 分鐘檢查一次
const RETENTION_DAYS: i64 = 14;

/// HOMEWORK_NEW_NOTIFY_V1：每一輪最多發送幾則「新上架作業」通知。
///
/// 達到上限時，其餘項目**不會**被標記成已評估，下一輪（4 分鐘後）自然接著發，
/// 因此老師一次上架十幾份作業時不會變成十幾則通知同時洗版。
const NEW_HOMEWORK_BATCH_LIMIT: usize = 5;

/// 背景預抓間隔。
///
/// ALWAYS_FRESH_ON_OPEN_V1
/// 刻意設得比 `homework_cache::CACHE_TTL`（5 分鐘）短，
/// 讓快取在使用者開頁前就已經被刷新，永遠處於「新鮮」狀態。
/// 這樣一進網站就是命中快取（0 上游請求、立即顯示），且資料是即時的。
pub(crate) const PREFETCH_INTERVAL_SECS: u64 = 4 * 60;

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
            night_pause = %if app_config.homework.prefetch_night_pause {
                format!(
                    "{}–{}",
                    app_config.homework.prefetch_night_start,
                    app_config.homework.prefetch_night_end
                )
            } else {
                "off".to_string()
            },
            "作業清單背景預抓器已啟動（開頁即為最新資料）"
        );

        tokio::time::sleep(Duration::from_secs(PREFETCH_STARTUP_DELAY_SECS)).await;

        let mut ticker = tokio::time::interval(Duration::from_secs(PREFETCH_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // HOMEWORK_NIGHT_PAUSE_V1：進入夜間窗時只記一條 info，其餘輪次靜默跳過。
        let mut night_logged = false;

        loop {
            ticker.tick().await;
            // 夜間暫停預抓（提醒排程器照跑，截止通知不斷線）。
            // chrono 的 DateTime 沒有「當日分鐘數」捷徑（那在 NaiveTime 上），
            // 以 hour()*60+minute() 自行組出。
            let now = Local::now();
            let minute_of_day = now.hour() * 60 + now.minute();
            if prefetch_night_pause_active(&app_config.homework, minute_of_day) {
                if !night_logged {
                    info!(
                        start = %app_config.homework.prefetch_night_start,
                        end = %app_config.homework.prefetch_night_end,
                        "夜間時段：暫停作業預抓（截止提醒每 15 分鐘照常檢查）"
                    );
                    night_logged = true;
                }
                // UPDATE_TIMES_PAGE_V1：夜間跳過也記一筆，頁面才不會顯示成「卡住」。
                crate::update_times::record_global(
                    crate::update_times::KIND_HOMEWORK_PREFETCH,
                    true,
                    "夜間暫停，略過本輪",
                );
                continue;
            }
            night_logged = false;
            prefetch_all_accounts(&accounts_db, &app_config, messenger.as_deref()).await;
        }
    })
}

/// HOMEWORK_NIGHT_PAUSE_V1：解析 "HH:MM" → 當日分鐘數（0..=1439）。
/// 格式不合法（長度、分隔、範圍）回 None，呼叫端以「不暫停」處理，
/// 避免一個打錯的設定把預抓整天關掉。
pub(crate) fn parse_hh_mm(value: &str) -> Option<u32> {
    let (h, m) = value.trim().split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// 目前（當日分鐘數）是否落於夜間窗。支援跨午夜窗（start > end）；
/// start == end 視為零長度＝不暫停（防止誤配把輪詢全天關閉）；
/// 格式不合法＝不暫停（fail-open）。作業預抓與公告輪詢共用此語意。
pub(crate) fn night_window_active(start: &str, end: &str, minute_of_day: u32) -> bool {
    let (Some(start), Some(end)) = (parse_hh_mm(start), parse_hh_mm(end)) else {
        return false;
    };
    if start == end {
        return false;
    }
    if start < end {
        minute_of_day >= start && minute_of_day < end
    } else {
        minute_of_day >= start || minute_of_day < end
    }
}

/// 目前是否落於夜間暫停窗（以當日分鐘數表示本地時間）。
fn prefetch_night_pause_active(cfg: &crate::config::HomeworkConfig, minute_of_day: u32) -> bool {
    cfg.prefetch_night_pause
        && night_window_active(
            &cfg.prefetch_night_start,
            &cfg.prefetch_night_end,
            minute_of_day,
        )
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
                // UPDATE_TIMES_PAGE_V1
                crate::update_times::record_account(
                    crate::update_times::KIND_HOMEWORK,
                    &account.id,
                    true,
                    format!("{} 項", items.len()),
                );
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
                crate::update_times::record_account(
                    crate::update_times::KIND_HOMEWORK,
                    &account.id,
                    false,
                    e.to_string(),
                );
            }
        }

        tokio::time::sleep(Duration::from_secs(PREFETCH_ACCOUNT_GAP_SECS)).await;
    }

    if refreshed > 0 {
        debug!(accounts = refreshed, "作業清單預抓完成，快取已更新");
    }
    // UPDATE_TIMES_PAGE_V1：整輪完成（不論成功幾個帳號）。
    crate::update_times::record_global(
        crate::update_times::KIND_HOMEWORK_PREFETCH,
        true,
        format!("{refreshed} 個帳號更新"),
    );
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

    // UPDATE_TIMES_PAGE_V1
    crate::update_times::record_global(crate::update_times::KIND_HOMEWORK_REMINDER_CHECK, true, "");
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
                // AUTH_ERROR_TYPED_V1：改用 report 版走完整條錯誤鏈，
                // session 過期（可能被 wrap_err 包住）正確地只跳過此帳號。
                Err(e) if !crate::api::is_auth_error_report(&e) => return Err(e),
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

    // HOMEWORK_LADDER_V1
    // 載入此帳號自訂的提醒階梯（通知設定頁可調，由大到小）；
    // 未設定時 get_homework_reminder_ladder 會回傳預設 24/6/1。
    let ladder = match accounts_db.get_homework_reminder_ladder(&account.id).await {
        Ok(ladder) if !ladder.is_empty() => ladder,
        Ok(_) => crate::db::DEFAULT_HOMEWORK_LADDER_HOURS.to_vec(),
        Err(error) => {
            warn!(
                account = %account.id,
                %error,
                "讀取作業提醒階梯失敗，改用預設 24/6/1"
            );
            crate::db::DEFAULT_HOMEWORK_LADDER_HOURS.to_vec()
        }
    };

    // HOMEWORK_NEW_NOTIFY_V1：先處理「新上架作業」通知。
    //
    // 排在倒數迴圈之前，是因為要跳過「已經進入倒數階梯」的項目：
    // 那些作業同一輪就會收到更精確的截止提醒，不需要再多一則新作業通知。
    notify_new_homework(
        account,
        accounts_db,
        messenger,
        todos,
        &user_completed,
        &ladder,
        now_utc,
    )
    .await?;

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

        // HOMEWORK_LADDER_V1
        // 改用每帳號自訂階梯判斷階段，語意與舊版 24/6/1 的 if 鏈相同：
        // 取「剩餘時間 ≤ 門檻」的最小門檻作為目前階段；
        // 剩餘時間超過所有門檻則不發送緊急通知。
        let Some((stage, stage_badge, time_desc)) = pick_ladder_stage(total_minutes, &ladder)
        else {
            // 超過最大門檻，暫不發送緊急通知
            continue;
        };

        let hw_id_str = todo.id.to_string();
        let deadline_timestamp = deadline.timestamp();

        // 檢查此階梯是否已發過（若老師延期則會自動重置為 false）
        let already_sent = accounts_db
            .is_homework_reminder_sent(&account.id, &hw_id_str, &stage, deadline_timestamp)
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
            stage_badge: stage_badge.clone(),
            deadline: deadline_str.clone(),
            time_remaining: time_desc.clone(),
            // 這是倒數提醒（不是新作業通知），文案與事件型別共用、僅以此旗標區分。
            is_new: false,
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
            &stage_badge,
        );

        // 標記已送出
        accounts_db
            .mark_homework_reminder_sent(&account.id, &hw_id_str, &stage, deadline_timestamp)
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

/// HOMEWORK_NEW_NOTIFY_V1：偵測「新上架的作業」並發送通知。
///
/// 偵測方式：維護一份「已評估過」的作業集合（`homework_seen` 資料表）。
/// 不在集合裡、又符合通知條件的作業＝新上架 → 發一則通知，並把它寫進集合。
///
/// 為什麼不是「跟上一輪清單比對差異」：作業快照每次抓取都會被覆寫，
/// 服務重啟、資料庫重建、快照為空時基準就失效，會把整批既有作業當成新的；
/// 用持久化的「已評估集合」則不受重啟影響，也不需要保留期清理。
///
/// 第一次評估某帳號（剛部署、或剛新增帳號）時先建立基準：
/// 把當下看到的作業整批寫入集合但不發通知，避免上線第一輪就把既有作業推一輪。
///
/// 保守設計（寧可少發，也不要洗版或誤報）：
/// * 只通知「作業／測驗／考試」，且**只通知當學期**（`semester_current != Some(false)`，
///   與作業頁 HOMEWORK_PAGE_CURRENT_ONLY_V1 的判定一致，不會為舊學期的作業發通知）；
/// * 已繳交／已評分、使用者已手動標記完成的一律跳過；
/// * 剩餘時間已進入倒數階梯（≤ 階梯最大門檻）的作業交給截止提醒，不重複發；
/// * 每輪上限 [`NEW_HOMEWORK_BATCH_LIMIT`] 則，其餘留到下一輪。
async fn notify_new_homework(
    account: &AccountConfig,
    accounts_db: &AccountsDb,
    messenger: Option<&dyn AdapterMessenger>,
    todos: &[crate::api::todos::TronclassTodoItem],
    user_completed: &HashSet<String>,
    ladder: &[u64],
    now_utc: DateTime<Utc>,
) -> miette::Result<()> {
    let seen = match accounts_db.list_homework_seen_ids(&account.id).await {
        Ok(seen) => seen,
        Err(error) => {
            // 讀不到狀態時這輪不發（避免誤判成「全部都是新作業」而暴衝）。
            warn!(account = %account.id, %error, "讀取已評估作業清單失敗，本輪略過新作業通知");
            return Ok(());
        }
    };

    let baseline_ready = match accounts_db.is_homework_new_baseline_ready(&account.id).await {
        Ok(ready) => ready,
        Err(error) => {
            warn!(account = %account.id, %error, "讀取新作業通知基準失敗，本輪略過新作業通知");
            return Ok(());
        }
    };

    if !baseline_ready {
        // 首次評估：建立基準（記錄目前所有作業，不發通知）。
        let ids: Vec<String> = todos.iter().map(|todo| todo.id.to_string()).collect();
        accounts_db.mark_homework_seen(&account.id, &ids).await?;
        accounts_db.mark_homework_new_baseline(&account.id).await?;
        info!(
            account = %account.id,
            items = ids.len(),
            "新作業通知：建立基準（本輪不發送既有作業的通知）"
        );
        return Ok(());
    }

    // 進入倒數階梯的門檻（分鐘）：剩餘時間在此之內的作業由截止提醒負責。
    let ladder_max_minutes = ladder.iter().copied().max().unwrap_or(24) as i64 * 60;

    let mut candidates: Vec<&crate::api::todos::TronclassTodoItem> = todos
        .iter()
        .filter(|todo| is_notifiable_new_homework(todo))
        .filter(|todo| !user_completed.contains(&todo.id.to_string()))
        .filter(|todo| !seen.contains(&todo.id.to_string()))
        .filter(|todo| {
            todo.end_time
                .map(|end| end.signed_duration_since(now_utc).num_minutes() > ladder_max_minutes)
                .unwrap_or(true)
        })
        .collect();

    // 最緊急的排前面；沒有截止時間的排最後。
    candidates.sort_by_key(|todo| (todo.end_time.is_none(), todo.end_time));
    let total = candidates.len();
    let mut sent = 0usize;

    for todo in candidates.iter().take(NEW_HOMEWORK_BATCH_LIMIT) {
        let course_name = todo
            .course_name
            .clone()
            .unwrap_or_else(|| "未指定課程".to_string());
        let deadline_str = todo
            .end_time
            .map(|end| {
                end.with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "未設定截止時間".to_string());
        let (stage_badge, time_remaining) = new_homework_reminder_text(todo.end_time, now_utc);

        // 與截止提醒共用事件型別與通知偏好開關（homework_deadline），
        // 只有文案不同（is_new = true）。
        let event = OutboundMessage::HomeworkDeadline(HomeworkDeadlineEvent {
            account: account.id.clone(),
            course_name: course_name.clone(),
            title: todo.title.clone(),
            stage_badge: stage_badge.clone(),
            deadline: deadline_str,
            time_remaining,
            is_new: true,
        });

        let mut delivered = false;
        if let Some(m) = messenger {
            let target = AdapterAccountTarget::new(
                account.id.clone(),
                account.line_user_id.clone(),
                account.discord_user_id.clone(),
            );
            if let Err(e) = m.push_to_account_or_admin(&target, &event).await {
                warn!(account = %account.id, error = %e, "新作業通知發送失敗");
            } else {
                delivered = true;
            }
        }

        web_push_notifications::notify_homework_new(
            &account.id,
            todo.id,
            &course_name,
            &todo.title,
            &stage_badge,
        );

        // 另外記一筆 `homework_reminder_sent`（階段 "new"）供稽核／除錯；
        // 「已評估」的標記在迴圈後統一處理。
        let hw_id_str = todo.id.to_string();
        let deadline_timestamp = todo.end_time.map(|end| end.timestamp()).unwrap_or(0);
        accounts_db
            .mark_homework_reminder_sent(&account.id, &hw_id_str, "new", deadline_timestamp)
            .await?;
        sent += 1;

        info!(
            account = %account.id,
            course = %course_name,
            title = %todo.title,
            delivered = delivered,
            "新作業通知已發送"
        );
    }

    // 標記「已評估」：除了**因單輪上限而留待下一輪**的項目之外，其他一律記入——
    // 包含這輪沒有發通知的（教材／問卷等非追蹤類型、已繳交、已標記完成、
    // 確定非當學期、以及已進入倒數階梯者）。這樣它們日後就算狀態改變
    // （老師延期、重新開放繳交、課程學期判定改變）也不會被誤認為新作業。
    // 上限外的項目刻意不標記，下一輪會自然補上。
    let deferred: HashSet<u64> = candidates
        .iter()
        .skip(NEW_HOMEWORK_BATCH_LIMIT)
        .map(|todo| todo.id)
        .collect();
    let newly_seen: Vec<String> = todos
        .iter()
        .filter(|todo| !deferred.contains(&todo.id))
        .filter(|todo| !seen.contains(&todo.id.to_string()))
        .map(|todo| todo.id.to_string())
        .collect();
    if !newly_seen.is_empty() {
        accounts_db
            .mark_homework_seen(&account.id, &newly_seen)
            .await?;
    }

    if total > sent {
        debug!(
            account = %account.id,
            total,
            sent,
            "新作業通知超過單輪上限，其餘留待下一輪"
        );
    }

    Ok(())
}

/// 這筆項目是否適合發「新作業通知」。
///
/// 與作業頁的顯示條件一致：
/// * 只挑作業／測驗／考試（教材、連結、問卷等不通知）；
/// * 已繳交／已評分的不通知；
/// * **確定非當學期**（`Some(false)`）的不通知——與 HOMEWORK_PAGE_CURRENT_ONLY_V1
///   相同，寧可漏發也不要為舊學期作業發通知；無法判定（`None`）仍可通知。
fn is_notifiable_new_homework(todo: &crate::api::todos::TronclassTodoItem) -> bool {
    matches!(
        todo.item_type.trim().to_ascii_lowercase().as_str(),
        "homework" | "quiz" | "exam"
    ) && !todo.is_completed_or_submitted()
        && todo.semester_current != Some(false)
}

/// 新作業通知用的徽章與剩餘時間文字。
///
/// 徽章會出現在訊息標題與手機推播的內容開頭，例如「還有 3 天」「已截止」。
fn new_homework_reminder_text(
    deadline: Option<DateTime<Utc>>,
    now_utc: DateTime<Utc>,
) -> (String, String) {
    let Some(deadline) = deadline else {
        return (
            "無截止時間".to_string(),
            "這是一份沒有設定截止時間的作業".to_string(),
        );
    };

    let total_minutes = deadline.signed_duration_since(now_utc).num_minutes();
    if total_minutes <= 0 {
        return ("已截止".to_string(), "已超過截止時間".to_string());
    }

    let days = total_minutes / (24 * 60);
    let hours = (total_minutes % (24 * 60)) / 60;
    let minutes = total_minutes % 60;

    let badge = if days >= 1 {
        format!("還有 {days} 天")
    } else if hours >= 1 {
        format!("還有 {hours} 小時")
    } else {
        format!("還有 {minutes} 分鐘")
    };

    let remaining = if days >= 1 {
        format!("尚有 {days} 天 {hours} 小時")
    } else if hours >= 1 {
        format!("尚有 {hours} 小時 {minutes} 分鐘")
    } else {
        format!("尚有 {minutes} 分鐘")
    };

    (badge, remaining)
}

/// HOMEWORK_LADDER_V1
/// 依自訂階梯（小時，任意順序）挑出目前所處的階段。
///
/// 語意與舊版硬編碼 24/6/1 的 if 鏈完全一致：
/// - 取「剩餘分鐘 ≤ 門檻×60」的**最小**門檻作為階段
///   （例如預設階梯下剩 300 分鐘 → 6h 階段，而非 24h）；
/// - 剩餘時間超過所有門檻 → 回傳 `None`（不發送）。
///
/// 回傳 `(stage 鍵, 徽章文字, 剩餘時間描述)`：
/// - stage 鍵沿用 `"{h}h"` 格式，預設階梯產生的 "1h"/"6h"/"24h"
///   與既有 `homework_reminder_sent` 紀錄完全相容；
/// - 徽章與時間描述的格式比照舊版（1 小時 →「最後 1 小時」；
///   ≤6 小時門檻 →「X 小時 Y 分鐘」；更大門檻 →「約 X 小時」）。
fn pick_ladder_stage(total_minutes: i64, ladder: &[u64]) -> Option<(String, String, String)> {
    // 由小到大找第一個「剩餘時間 ≤ 門檻」的階段。
    let mut sorted = ladder.to_vec();
    sorted.sort_unstable();
    let threshold_hours = *sorted
        .iter()
        .find(|&&h| total_minutes <= (h as i64) * 60)?;

    let stage = format!("{threshold_hours}h");
    let stage_badge = if threshold_hours == 1 {
        "最後 1 小時".to_string()
    } else {
        format!("{threshold_hours} 小時內截止")
    };
    let time_desc = if total_minutes <= 60 {
        format!("{total_minutes} 分鐘")
    } else if threshold_hours <= 6 {
        format!("{} 小時 {} 分鐘", total_minutes / 60, total_minutes % 60)
    } else {
        format!("約 {} 小時", total_minutes / 60)
    };

    Some((stage, stage_badge, time_desc))
}

