use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, Local, Timelike, Weekday};
use tracing::{debug, info, warn};

use crate::adapters::events::{AdapterMessenger, OutboundMessage};
use crate::db::{AccountsDb, ClassReminderDueRow};

const CHECK_INTERVAL_SECS: u64 = 60;
const SENT_RETENTION_DAYS: i64 = 21;

pub fn start_class_reminder_scheduler(
    accounts_db: AccountsDb,
    discord: Arc<dyn AdapterMessenger>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("上課前 Discord 提醒排程器已啟動");
        let mut ticker = tokio::time::interval(Duration::from_secs(CHECK_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
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
        let message = render_message(&entry, minutes_until, minutes_before);
        if let Err(e) = discord
            .push(&entry.discord_user_id, &OutboundMessage::Text(message))
            .await
        {
            warn!(
                account = %entry.account_id,
                discord_user_id = %entry.discord_user_id,
                error = %e,
                "上課前 Discord DM 提醒送出失敗"
            );
        } else {
            info!(
                account = %entry.account_id,
                course = %entry.course_name,
                minutes_before = minutes_before,
                "上課前 Discord DM 提醒已送出"
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

fn render_message(
    entry: &ClassReminderDueRow,
    minutes_until: i64,
    configured_minutes: i64,
) -> String {
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

    let headline = if minutes_until <= 0 {
        "現在上課".to_string()
    } else {
        format!("{} 分鐘後上課", minutes_until)
    };

    format!(
        "⏰ 上課提醒\n{}\n\n課程：{}\n時間：{}{}（{}）\n教室：{}\n老師：{}\n帳號：{}\n\n你設定的是上課前 {} 分鐘提醒。",
        headline,
        entry.course_name.trim(),
        start,
        end,
        period,
        room,
        teacher,
        entry.account_id,
        configured_minutes
    )
}

fn format_minutes(minutes: i64) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}
