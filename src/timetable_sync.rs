//! 課表自動同步排程器（TIMETABLE_AUTO_SYNC_V1）。
//!
//! 定期登入學校校務系統、抓回課表，並覆寫 `source='auto'` 的課表列。
//!
//! ## 為什麼是獨立的 task，而不是掛在 monitor loop 上
//!
//! `monitor.rs` 的主迴圈以「秒」為單位輪詢點名，而課表一學期只會變動幾次。
//! 兩者的節奏差了三個數量級。把課表同步塞進 monitor loop 會有兩個壞處：
//!
//! 1. 一次爬蟲逾時（學校系統很慢，甚至有維護時段）會拖累點名偵測，
//!    而點名是本專案的核心功能，延遲的代價遠高於課表晚幾小時更新。
//! 2. `monitor.rs::timetable_poll_policy()` 會**讀取**課表來決定輪詢頻率；
//!    讓同一個迴圈同時讀寫課表會產生不必要的耦合。
//!
//! 因此這裡用獨立的 `tokio::spawn`，失敗完全隔離。
//!
//! ## Session 隔離
//!
//! 每次同步都建立**全新的** `Arc<Jar>` 與 `Client`，用完即丟。
//! 學校校務系統的 cookie 絕不會混進 Tronclass 的點名 session，
//! 反之亦然——爬蟲掛掉不會把使用者踢出 Tronclass。

use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, Timelike};
use miette::{IntoDiagnostic, Result, WrapErr};
use once_cell::sync::Lazy;
use tracing::{debug, info, warn};

use crate::account::AccountConfig;
use crate::adapters::events::{AdapterAccountTarget, AdapterMessenger, OutboundMessage};
use crate::config::AppConfig;
use crate::db::{AccountsDb, TimetableEntryInput};
use crate::timetable_source::{compress_periods, get_timetable_source, RawTimetableRow};

/// 排程器喚醒間隔。實際是否同步還要看每個帳號的 `SYNC_INTERVAL_SECS`。
const TICK_INTERVAL_SECS: u64 = 6 * 60 * 60;

/// 每個帳號兩次成功同步之間的最短間隔（24 小時）。
const SYNC_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// 啟動後延遲多久才跑第一輪，讓監控與 adapter 先就緒。
const STARTUP_DELAY_SECS: u64 = 90;

/// 帳號之間的間隔，平滑對學校伺服器的請求。
const ACCOUNT_GAP_SECS: u64 = 5;

/// 單次爬取的逾時。校務系統偶爾很慢，但不能無限等。
const SCRAPE_TIMEOUT_SECS: u64 = 45;

/// 連續失敗達到此次數後，暫停到隔天再試（避免對學校系統狂打）。
const MAX_CONSECUTIVE_FAILURES: i64 = 3;

/// 一份課表最多寫入的列數，與手動匯入的上限一致。
const MAX_SYNC_ENTRIES: usize = 300;

/// 啟動課表自動同步排程器。
///
/// 沒有任何帳號啟用時，這個 task 幾乎不耗資源：每 6 小時查一次資料庫就睡回去。
pub fn start_timetable_sync_scheduler(
    accounts_db: AccountsDb,
    app_config: Arc<AppConfig>,
    messenger: Option<Arc<dyn AdapterMessenger>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("課表自動同步排程器已啟動（每 6 小時檢查，每帳號每日同步一次）");
        tokio::time::sleep(Duration::from_secs(STARTUP_DELAY_SECS)).await;

        let mut ticker = tokio::time::interval(Duration::from_secs(TICK_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            run_once(&accounts_db, &app_config, messenger.as_deref()).await;
        }
    })
}

/// 學校系統的資料更新／維護時段，這些時間不要去打擾它。
///
/// - 每週一 08:00–10:00 是 emis 的維護窗口。
/// - 課程查詢系統每日 04:00–04:30、10:00–10:30、12:00–12:30、17:00–17:30 會刷新資料。
fn in_blackout_window(now: chrono::DateTime<Local>) -> bool {
    use chrono::Datelike;
    let (hour, minute) = (now.hour(), now.minute());

    if now.weekday() == chrono::Weekday::Mon && (8..10).contains(&hour) {
        return true;
    }
    matches!(hour, 4 | 10 | 12 | 17) && minute < 30
}

async fn run_once(
    accounts_db: &AccountsDb,
    app_config: &AppConfig,
    messenger: Option<&dyn AdapterMessenger>,
) {
    let states = match accounts_db.list_enabled_timetable_sync_states().await {
        Ok(states) => states,
        Err(e) => {
            debug!(error = %e, "課表同步：讀取啟用清單失敗，略過本輪");
            return;
        }
    };
    if states.is_empty() {
        return;
    }

    let accounts = match accounts_db.resolve(app_config).await {
        Ok(accounts) => accounts,
        Err(e) => {
            debug!(error = %e, "課表同步：讀取帳號設定失敗，略過本輪");
            return;
        }
    };

    let now = Local::now();
    if in_blackout_window(now) {
        debug!("課表同步：目前處於學校系統維護／資料更新時段，略過本輪");
        return;
    }
    let now_ts = now.timestamp();

    for state in states {
        let Some(account) = accounts.iter().find(|a| a.id == state.account_id) else {
            debug!(account = %state.account_id, "課表同步：找不到對應帳號設定，略過");
            continue;
        };
        if !account.enabled {
            continue;
        }

        // 距離上次成功還不到一天 → 跳過。
        if state.last_success_at > 0 && now_ts - state.last_success_at < SYNC_INTERVAL_SECS {
            continue;
        }

        // 指數退避：連續失敗越多，等越久；達到上限則一天只試一次。
        if state.consecutive_failures > 0 {
            let backoff = if state.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                SYNC_INTERVAL_SECS
            } else {
                let factor = 1i64 << state.consecutive_failures.min(8);
                (TICK_INTERVAL_SECS as i64) * factor
            };
            if now_ts - state.last_attempt_at < backoff {
                debug!(
                    account = %account.id,
                    failures = state.consecutive_failures,
                    "課表同步：退避中，略過本輪"
                );
                continue;
            }
        }

        if let Err(e) = sync_account(account, accounts_db, messenger).await {
            warn!(account = %account.id, error = %e, "課表自動同步失敗");
        }

        tokio::time::sleep(Duration::from_secs(ACCOUNT_GAP_SECS)).await;
    }
}

/// 對單一帳號執行一次完整同步。手動觸發的 API 也走這裡。
pub async fn sync_account(
    account: &AccountConfig,
    accounts_db: &AccountsDb,
    messenger: Option<&dyn AdapterMessenger>,
) -> Result<TimetableSyncOutcome> {
    let Some(source) = get_timetable_source(account) else {
        return Err(miette::miette!(
            "此帳號的學校（provider = {}）尚未支援課表自動同步",
            account.provider
        ));
    };

    accounts_db
        .record_timetable_sync_attempt(&account.id)
        .await
        .ok();

    let result = scrape_with_timeout(source.as_ref(), account).await;

    let rows = match result {
        Ok(rows) => rows,
        Err(e) => {
            let sanitized = sanitize_error(&format!("{e}"), account);
            let failures = accounts_db
                .record_timetable_sync_failure(&account.id, &sanitized)
                .await
                .unwrap_or(0);
            warn!(
                account = %account.id,
                failures,
                "課表同步失敗：{sanitized}"
            );
            return Err(miette::miette!("{sanitized}"));
        }
    };

    // 安全閥：解析出 0 筆時**不覆寫**既有資料。
    // 學校系統在學期交替、維護、或版面改版時都可能回空頁面，
    // 直接清空使用者課表會讓上課提醒整個失效，代價太大。
    if rows.is_empty() {
        let message = "未從校務系統解析到課程；已保留原有課表未做更動。\
             可能原因：(1) 本學期尚未開放查詢或確實無選課資料；\
             (2) emis 頁面版面與預期不同。詳細的頁面結構摘要已寫入伺服器日誌\
             （搜尋「課表解析診斷」），可據此調整解析規則。";
        accounts_db
            .record_timetable_sync_failure(&account.id, message)
            .await
            .ok();
        return Err(miette::miette!("{message}"));
    }

    let entries = to_db_entries(&rows);
    let entry_count = entries.len();
    if entry_count > MAX_SYNC_ENTRIES {
        let message = format!("解析到 {entry_count} 筆課表，超過上限 {MAX_SYNC_ENTRIES}");
        accounts_db
            .record_timetable_sync_failure(&account.id, &message)
            .await
            .ok();
        return Err(miette::miette!("{message}"));
    }

    let previous = accounts_db
        .list_auto_timetable_entries(&account.id)
        .await
        .unwrap_or_default();
    let diff = diff_entries(&previous, &entries);

    let written = accounts_db
        .replace_auto_timetable_entries(&account.id, entries)
        .await
        .wrap_err("寫入自動同步課表失敗")?;

    let summary = if diff.is_empty() {
        format!("課表無變動（{written} 筆）")
    } else {
        format!(
            "課表已更新：新增 {} 堂、移除 {} 堂（共 {written} 筆）",
            diff.added.len(),
            diff.removed.len()
        )
    };
    accounts_db
        .record_timetable_sync_success(&account.id, &summary)
        .await
        .ok();

    info!(account = %account.id, entries = written, "課表自動同步完成：{summary}");

    // 只有真的有變動才發通知，避免每天一則沒有資訊量的訊息。
    if !diff.is_empty() {
        notify_change(account, messenger, &diff).await;
    }

    Ok(TimetableSyncOutcome {
        entries: written,
        added: diff.added.len(),
        removed: diff.removed.len(),
        summary,
    })
}

/// 一次同步的結果，回給手動觸發的 API 用。
#[derive(Debug, Clone)]
pub struct TimetableSyncOutcome {
    pub entries: usize,
    pub added: usize,
    pub removed: usize,
    pub summary: String,
}

/// 以全新的 cookie jar 執行爬取，並套用逾時。
///
/// 這裡是 session 隔離的關鍵：`Jar::default()` 每次都是新的，
/// 函式回傳後就被丟棄，不可能與 Tronclass 的 session 互相污染。
async fn scrape_with_timeout(
    source: &dyn crate::timetable_source::TimetableSource,
    account: &AccountConfig,
) -> Result<Vec<RawTimetableRow>> {
    let cookie_jar = Arc::new(reqwest::cookie::Jar::default());
    let client = crate::api::build_http_client(Arc::clone(&cookie_jar), SCRAPE_TIMEOUT_SECS)
        .into_diagnostic()
        .wrap_err("建立課表同步 HTTP client 失敗")?;

    tokio::time::timeout(
        Duration::from_secs(SCRAPE_TIMEOUT_SECS),
        source.fetch(&client, &cookie_jar, account),
    )
    .await
    .unwrap_or_else(|_| Err(miette::miette!("課表爬取逾時（{SCRAPE_TIMEOUT_SECS}s）")))
}

/// 把爬回來的原始資料轉成資料庫列，並合併連續節次。
fn to_db_entries(rows: &[RawTimetableRow]) -> Vec<TimetableEntryInput> {
    let mut entries = Vec::new();
    for row in rows {
        for (start, end) in compress_periods(&row.periods) {
            let (start_time, end_time) = period_range_times(start, end);
            entries.push(TimetableEntryInput {
                weekday: row.weekday.clone(),
                period_start: start,
                period_end: end,
                start_time,
                end_time,
                course_name: row.course_name.clone(),
                teacher: row.teacher.clone(),
                classroom: row.classroom.clone(),
                course_code: row.course_code.clone(),
                // 這個字串是整個功能的安全邊界：
                // `replace_auto_timetable_entries` 只會刪除 source='auto' 的列。
                source: "auto".to_string(),
            });
        }
    }
    entries
}

fn period_range_times(start: i64, end: i64) -> (String, String) {
    let start_time = crate::timetable_source::tku::period_to_times(start)
        .map(|(s, _)| s)
        .unwrap_or_default();
    let end_time = crate::timetable_source::tku::period_to_times(end)
        .map(|(_, e)| e)
        .unwrap_or_default();
    (start_time, end_time)
}

// ─── 差異比對 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct TimetableDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl TimetableDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

fn entry_key(
    weekday: &str,
    period_start: i64,
    period_end: i64,
    course_name: &str,
    classroom: &str,
) -> String {
    format!("{weekday}|{period_start}-{period_end}|{course_name}|{classroom}")
}

fn diff_entries(
    previous: &[crate::db::TimetableEntryRow],
    next: &[TimetableEntryInput],
) -> TimetableDiff {
    let before: std::collections::BTreeSet<String> = previous
        .iter()
        .map(|r| {
            entry_key(
                &r.weekday,
                r.period_start,
                r.period_end,
                &r.course_name,
                &r.classroom,
            )
        })
        .collect();
    let after: std::collections::BTreeSet<String> = next
        .iter()
        .map(|r| {
            entry_key(
                &r.weekday,
                r.period_start,
                r.period_end,
                &r.course_name,
                &r.classroom,
            )
        })
        .collect();

    TimetableDiff {
        added: after.difference(&before).cloned().collect(),
        removed: before.difference(&after).cloned().collect(),
    }
}

async fn notify_change(
    account: &AccountConfig,
    messenger: Option<&dyn AdapterMessenger>,
    diff: &TimetableDiff,
) {
    let Some(messenger) = messenger else {
        return;
    };

    let mut lines = vec!["📅 課表已自動更新".to_string()];
    if !diff.added.is_empty() {
        lines.push(format!("新增 {} 堂：", diff.added.len()));
        for item in diff.added.iter().take(5) {
            lines.push(format!("  ＋ {}", humanize_key(item)));
        }
        if diff.added.len() > 5 {
            lines.push(format!("  …等 {} 堂", diff.added.len()));
        }
    }
    if !diff.removed.is_empty() {
        lines.push(format!("移除 {} 堂：", diff.removed.len()));
        for item in diff.removed.iter().take(5) {
            lines.push(format!("  － {}", humanize_key(item)));
        }
        if diff.removed.len() > 5 {
            lines.push(format!("  …等 {} 堂", diff.removed.len()));
        }
    }

    let target = AdapterAccountTarget::new(
        account.id.clone(),
        account.line_user_id.clone(),
        account.discord_user_id.clone(),
    );
    let message = OutboundMessage::Text(lines.join("\n"));
    if let Err(e) = messenger.push_to_account_or_admin(&target, &message).await {
        debug!(account = %account.id, error = %e, "課表更新通知送出失敗");
    }
}

fn humanize_key(key: &str) -> String {
    let parts: Vec<&str> = key.split('|').collect();
    if parts.len() < 4 {
        return key.to_string();
    }
    let weekday = match parts[0] {
        "mon" => "週一",
        "tue" => "週二",
        "wed" => "週三",
        "thu" => "週四",
        "fri" => "週五",
        "sat" => "週六",
        "sun" => "週日",
        other => other,
    };
    let room = if parts[3].is_empty() {
        String::new()
    } else {
        format!(" @{}", parts[3])
    };
    format!("{weekday} 第{}節 {}{room}", parts[1], parts[2])
}

/// 移除錯誤訊息中可能出現的機敏字串，才寫進資料庫／回給前端。
///
/// 上游錯誤有時會把請求 URL（含學號）或 cookie 帶進訊息。學號屬個資，
/// 密碼更不用說，兩者都不該落地。
fn sanitize_error(message: &str, account: &AccountConfig) -> String {
    let mut out = message.to_string();

    if !account.password.is_empty() {
        out = out.replace(&account.password, "<redacted>");
    }
    if !account.username.is_empty() {
        out = out.replace(&account.username, "<account>");
    }

    // REGEX_STATIC_V1：三條 redact regex 原本每次呼叫都重新編譯，
    // 改為 Lazy 編譯一次終身重用（pattern 與替換字串完全不變）。
    for (re, replacement) in REDACT_RULES.iter() {
        out = re.replace_all(&out, *replacement).to_string();
    }
    out
}

// 去 query string 敏感參數與 cookie 片段的規則。
//
// ROBUST_QUOTE_FREE_REGEX_V1
// 這裡刻意「不在原始碼中出現任何雙引號字元」：字元類別裡的 `"` 一律
// 用 regex 的十六進位跳脫 \x22 表示。
//
// 背景：原本寫成 r"...\"..." 會編譯失敗（E0762），因為原始字串不處理
// 反斜線跳脫，`\"` 的 `"` 會直接把字串結束掉。改成 r#"..."# 雖然正確，
// 但只要有任何環節重新跳脫過內容就可能再次損壞。用 \x22 之後，
// 這幾行就只剩單純的 ASCII 字母與反斜線，任何轉檔都不會破壞它。
static REDACT_RULES: Lazy<Vec<(regex::Regex, &'static str)>> = Lazy::new(|| {
    [
        ("(?i)stu_no=[^&\\s\\x22']*", "stu_no=<redacted>"),
        ("(?i)password=[^&\\s\\x22']*", "password=<redacted>"),
        ("(?i)(JSESSIONID|session)=[^;\\s\\x22']*", "$1=<redacted>"),
    ]
    .into_iter()
    // UNWRAP_SWEEP_V1：pattern 是固定字面值，理論上一定能編譯；但 Lazy 內 panic
    // 會毒化整個 static，之後每次 sanitize_error 都再 panic。改為略過並 warn。
    .filter_map(|(pattern, replacement)| match regex::Regex::new(pattern) {
        Ok(re) => Some((re, replacement)),
        Err(e) => {
            warn!(pattern, error = %e, "遮罩 regex 編譯失敗，已略過該規則");
            None
        }
    })
    .collect()
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::TimetableEntryRow;

    fn raw(weekday: &str, periods: Vec<i64>, name: &str) -> RawTimetableRow {
        RawTimetableRow {
            weekday: weekday.to_string(),
            periods,
            course_name: name.to_string(),
            teacher: "王小明".to_string(),
            classroom: "Q409".to_string(),
            course_code: String::new(),
        }
    }

    fn row(weekday: &str, start: i64, end: i64, name: &str) -> TimetableEntryRow {
        TimetableEntryRow {
            id: 1,
            account_id: "a".to_string(),
            weekday: weekday.to_string(),
            period_start: start,
            period_end: end,
            start_time: String::new(),
            end_time: String::new(),
            course_name: name.to_string(),
            teacher: String::new(),
            classroom: "Q409".to_string(),
            course_code: String::new(),
            source: "auto".to_string(),
            updated_at: 0,
            updated_time: String::new(),
        }
    }

    #[test]
    fn to_db_entries_merges_consecutive_periods_and_marks_auto() {
        let rows = vec![raw("mon", vec![3, 4], "資料結構")];
        let entries = to_db_entries(&rows);

        assert_eq!(entries.len(), 1, "連續節次必須合併成一列");
        assert_eq!(entries[0].period_start, 3);
        assert_eq!(entries[0].period_end, 4);
        assert_eq!(entries[0].start_time, "10:10");
        assert_eq!(entries[0].end_time, "12:00");
        assert_eq!(entries[0].source, "auto");
    }

    #[test]
    fn to_db_entries_splits_non_consecutive_periods() {
        let rows = vec![raw("tue", vec![1, 2, 5], "微積分")];
        let entries = to_db_entries(&rows);
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[0].period_start, entries[0].period_end), (1, 2));
        assert_eq!((entries[1].period_start, entries[1].period_end), (5, 5));
    }

    #[test]
    fn diff_detects_added_and_removed() {
        let previous = vec![row("mon", 3, 4, "資料結構")];
        let next = to_db_entries(&[raw("mon", vec![3, 4], "資料結構")]);
        assert!(diff_entries(&previous, &next).is_empty(), "相同課表不應有差異");

        let next = to_db_entries(&[raw("mon", vec![3, 4], "演算法")]);
        let diff = diff_entries(&previous, &next);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.removed.len(), 1);
    }

    #[test]
    fn sanitize_error_removes_credentials_and_student_id() {
        let mut account = AccountConfig {
            id: "a".to_string(),
            provider: "tku".to_string(),
            username: "A12345678".to_string(),
            password: "s3cret".to_string(),
            enabled: true,
            line_user_id: String::new(),
            discord_user_id: String::new(),
            session_cookie: String::new(),
            is_admin: false,
            provider_config: Default::default(),
            request_timeout_secs: 30,
        };
        account.provider = "tku".to_string();

        let msg = "GET .../TMWC090_result.aspx?YrSem=1151&stu_no=A12345678 failed; password=s3cret; JSESSIONID=abc123";
        let cleaned = sanitize_error(msg, &account);

        assert!(!cleaned.contains("A12345678"), "學號必須被遮蔽：{cleaned}");
        assert!(!cleaned.contains("s3cret"), "密碼必須被遮蔽：{cleaned}");
        assert!(!cleaned.contains("abc123"), "session 必須被遮蔽：{cleaned}");
    }

    #[test]
    fn humanize_key_is_readable() {
        assert_eq!(
            humanize_key("mon|3-4|資料結構|Q409"),
            "週一 第3-4節 資料結構 @Q409"
        );
    }

    #[test]
    fn blackout_windows_are_respected() {
        use chrono::TimeZone;
        // 週一 09:00 → emis 維護時段。
        let monday = Local.with_ymd_and_hms(2026, 8, 31, 9, 0, 0).unwrap();
        assert!(in_blackout_window(monday));
        // 週二 12:10 → 資料更新時段。
        let tuesday = Local.with_ymd_and_hms(2026, 9, 1, 12, 10, 0).unwrap();
        assert!(in_blackout_window(tuesday));
        // 週二 14:00 → 正常。
        let ok = Local.with_ymd_and_hms(2026, 9, 1, 14, 0, 0).unwrap();
        assert!(!in_blackout_window(ok));
    }
}
