use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, Local, NaiveTime, Timelike, Weekday};
use tracing::{debug, info, warn};

use crate::adapters::events::{AdapterMessenger, ClassReminderEvent, OutboundMessage};
use crate::db::{AccountsDb, ClassReminderDueRow};

pub(crate) const CHECK_INTERVAL_SECS: u64 = 60;
const SENT_RETENTION_DAYS: i64 = 21;

/// CLASS_REMINDER_NIGHT_PAUSE_V1：23:00（含）到隔天 06:00（不含）之間幾乎不可能
/// 有課，暫停逐分鐘查詢。即使剛好有極端早的課（例如 06:10 開始、提前 180 分鐘
/// 提醒＝02:10 就該提醒），暫停期間錯過的提醒也不會真的消失：`run_once` 原本就
/// 有「只要還沒超過上課時間，晚一點檢查到一樣會補送」的邏輯（見下方
/// `now_minutes > start_minutes` 判斷），06:00 恢復查詢時會立刻補發，不會漏掉。
const NIGHT_PAUSE_START_SECS: u32 = 23 * 60 * 60;
const NIGHT_PAUSE_END_SECS: u32 = 6 * 60 * 60;

fn is_night_pause(now: NaiveTime) -> bool {
    let seconds = now.num_seconds_from_midnight();
    seconds >= NIGHT_PAUSE_START_SECS || seconds < NIGHT_PAUSE_END_SECS
}

pub fn start_class_reminder_scheduler(
    accounts_db: AccountsDb,
    discord: Arc<dyn AdapterMessenger>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("上課前 Discord 提醒排程器已啟動");
        let mut ticker = tokio::time::interval(Duration::from_secs(CHECK_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // CLASS_REMINDER_NIGHT_PAUSE_V1：進入夜間窗只記一筆 log，其餘每分鐘的
        // tick 靜默跳過，避免洗版；離開夜間窗時重置旗標，下次進入時再記一筆。
        let mut night_logged = false;
        loop {
            ticker.tick().await;
            let now_time = Local::now().time();
            if is_night_pause(now_time) {
                if !night_logged {
                    info!("深夜時段（23:00-06:00）：暫停上課提醒查詢，06:00 後自動恢復");
                    night_logged = true;
                }
                // UPDATE_TIMES_PAGE_V1：夜間跳過也記一筆，「資料更新時間」頁才不會把
                // 這段時間顯示成排程卡住。
                crate::update_times::record_global(
                    crate::update_times::KIND_CLASS_REMINDER_CHECK,
                    true,
                    "深夜時段，暫停查詢".to_string(),
                );
                continue;
            }
            night_logged = false;
            if let Err(e) = run_once(&accounts_db, Arc::clone(&discord)).await {
                warn!(error = %e, "上課前 Discord 提醒排程器執行失敗");
            }
        }
    })
}

async fn run_once(
    accounts_db: &AccountsDb,
    discord: Arc<dyn AdapterMessenger>,
) -> miette::Result<()> {
    let now = Local::now();

    // REST_DAY_NO_CLASS_REMINDER_V1：放假日（週末、內建國定假日、寒暑假，或管理員
    // 在「休息日與寒暑假」頁面手動設定的休息日／強制上課日）不提醒上課。判定邏輯
    // 直接沿用 `crate::holidays::is_rest_day`，與監控輪詢（REST_DAY_NO_POLL_V1）、
    // 晨間摘要（DIGEST_REST_DAY_V1）共用同一套休息日判定，不會各說各話——例如
    // 國定假日剛好落在星期一，當天不會再收到那堂課的上課提醒。
    if crate::holidays::is_rest_day(now.date_naive()) {
        crate::update_times::record_global(
            crate::update_times::KIND_CLASS_REMINDER_CHECK,
            true,
            "今天是休息日，略過上課提醒".to_string(),
        );
        return Ok(());
    }

    let weekday = weekday_key(now.weekday());
    let now_minutes = now.hour() as i64 * 60 + now.minute() as i64;

    let entries = match accounts_db
        .list_due_class_reminders_for_weekday(weekday)
        .await
    {
        Ok(entries) => entries,
        Err(e) => {
            // 若尚未安裝/尚未建立 timetable_entries，不讓整個服務掛掉。
            debug!(error = %e, "讀取課表提醒清單失敗，略過本輪");
            return Ok(());
        }
    };

    let today = now.format("%Y-%m-%d").to_string();
    let cutoff = now.timestamp() - SENT_RETENTION_DAYS * 24 * 60 * 60;
    let _ = accounts_db.cleanup_old_class_reminder_sent(cutoff).await;
    // UPDATE_TIMES_PAGE_V1：每分鐘一輪；落地有 60 秒節流，不會每輪都寫檔。
    crate::update_times::record_global(
        crate::update_times::KIND_CLASS_REMINDER_CHECK,
        true,
        format!("今日 {} 堂課待檢查", entries.len()),
    );

    for entry in entries {
        let Some(start_minutes) = entry_start_minutes(&entry) else {
            continue;
        };
        let minutes_before = entry.minutes_before.clamp(0, 180);
        let remind_at = start_minutes - minutes_before;

        // 服務重啟或 VM 短暫卡住時，只要還沒超過上課後 1 分鐘，就補提醒一次。
        if now_minutes < remind_at || now_minutes > start_minutes {
            continue;
        }

        let reminder_key = format!(
            "{}:tt:{}:{}:{}",
            today,
            entry.timetable_id,
            entry.start_time.trim(),
            minutes_before
        );
        if !accounts_db
            .mark_class_reminder_sent(&entry.account_id, &reminder_key)
            .await?
        {
            continue;
        }

        let minutes_until = (start_minutes - now_minutes).max(0);
        // DISCORD_NOTIFY_UNIFY_V1：結構化事件，由 Discord adapter 統一 render。
        // CLASS_REMINDER_PUSH_V1：依各渠道開關分別送 Discord DM 與手機 Web Push。
        let event = build_event(&entry, minutes_until, minutes_before);
        if entry.discord_enabled && !entry.discord_user_id.trim().is_empty() {
            match discord
                .push(
                    &entry.discord_user_id,
                    &OutboundMessage::ClassReminder(event.clone()),
                )
                .await
            {
                Err(e) => {
                    warn!(
                        account = %entry.account_id,
                        discord_user_id = %entry.discord_user_id,
                        error = %e,
                        "上課前 Discord DM 提醒送出失敗"
                    );
                }
                Ok(()) => {
                    info!(
                        account = %entry.account_id,
                        course = %entry.course_name,
                        minutes_before = minutes_before,
                        "上課前 Discord DM 提醒已送出"
                    );
                }
            }
        }
        if entry.push_enabled {
            let body_text = format!(
                "{}｜{}｜{}",
                event.time_text, event.classroom, event.teacher
            );
            crate::web_push_notifications::notify_class_reminder(
                &entry.account_id,
                &entry.course_name,
                &event.headline(),
                &body_text,
            );
            info!(
                account = %entry.account_id,
                course = %entry.course_name,
                minutes_before = minutes_before,
                "上課前 Web Push 提醒已送出"
            );
        }
    }

    Ok(())
}

fn weekday_key(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Mon => "mon",
        Weekday::Tue => "tue",
        Weekday::Wed => "wed",
        Weekday::Thu => "thu",
        Weekday::Fri => "fri",
        Weekday::Sat => "sat",
        Weekday::Sun => "sun",
    }
}

fn entry_start_minutes(entry: &ClassReminderDueRow) -> Option<i64> {
    parse_hhmm(&entry.start_time).or_else(|| period_start_minutes(entry.period_start))
}

fn parse_hhmm(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut parts = value.split(':');
    let hour = parts.next()?.trim().parse::<i64>().ok()?;
    let minute = parts.next()?.trim().parse::<i64>().ok()?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    Some(hour * 60 + minute)
}

fn period_start_minutes(period: i64) -> Option<i64> {
    match period {
        1 => Some(8 * 60 + 10),
        2 => Some(9 * 60 + 10),
        3 => Some(10 * 60 + 10),
        4 => Some(11 * 60 + 10),
        5 => Some(12 * 60 + 10),
        6 => Some(13 * 60 + 10),
        7 => Some(14 * 60 + 10),
        8 => Some(15 * 60 + 10),
        9 => Some(16 * 60 + 10),
        10 => Some(17 * 60 + 10),
        _ => None,
    }
}

fn build_event(
    entry: &ClassReminderDueRow,
    minutes_until: i64,
    configured_minutes: i64,
) -> ClassReminderEvent {
    let start = if entry.start_time.trim().is_empty() {
        period_start_minutes(entry.period_start)
            .map(format_minutes)
            .unwrap_or_else(|| "未設定".to_string())
    } else {
        entry.start_time.trim().to_string()
    };
    let end = if entry.end_time.trim().is_empty() {
        "".to_string()
    } else {
        format!(" - {}", entry.end_time.trim())
    };
    let room = if entry.classroom.trim().is_empty() {
        "未設定".to_string()
    } else {
        entry.classroom.trim().to_string()
    };
    let teacher = if entry.teacher.trim().is_empty() {
        "未設定".to_string()
    } else {
        entry.teacher.trim().to_string()
    };
    let period = if entry.period_end > entry.period_start {
        format!("第 {}-{} 節", entry.period_start, entry.period_end)
    } else if entry.period_start > 0 {
        format!("第 {} 節", entry.period_start)
    } else {
        "節次未設定".to_string()
    };

    ClassReminderEvent {
        account: entry.account_id.clone(),
        course_name: entry.course_name.trim().to_string(),
        time_text: format!("{start}{end}（{period}）"),
        classroom: room,
        teacher,
        minutes_until,
        configured_minutes,
    }
}

fn format_minutes(minutes: i64) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}
