//! REST_DAY_NO_POLL_V1：週末與國定假日判定（全專案共用）。
//!
//! - 週六、週日一律視為休息日。
//! - 內建臺灣行政院人事行政總處核定的放假日（目前收錄 2026、2027 年，含補假日）。
//! - `config/holidays.json` 可額外增減：
//!   ```json
//!   { "extra": ["2026-11-20"], "workdays": ["2026-04-06"] }
//!   ```
//!   `extra` = 額外放假（例如校慶、颱風假）；`workdays` = 強制視為上課日（例如補課）。
//!   檔案每 5 分鐘重讀一次，改完不用重啟。
//!
//! 監控主迴圈在休息日不做「自動」輪詢（手動「立即檢查」不受影響）；
//! 晨間摘要沿用同一份判定。

use chrono::{Datelike, NaiveDate, Weekday};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 內建國定假日（含補假、連假中的週間日）。日期格式 YYYY-MM-DD。
/// 來源：行政院人事行政總處《政府行政機關辦公日曆表》115（2026）、116（2027）年。
const BUILTIN_HOLIDAYS: &[&str] = &[
    // ── 2026（民國 115 年）──
    "2026-01-01", // 開國紀念日
    "2026-02-14", "2026-02-15", "2026-02-16", "2026-02-17", "2026-02-18", // 小年夜／除夕／春節
    "2026-02-19", "2026-02-20", "2026-02-21", "2026-02-22",
    "2026-02-27", "2026-02-28", // 和平紀念日（2/28 六，2/27 補假）
    "2026-04-03", "2026-04-04", "2026-04-05", "2026-04-06", // 兒童節／清明節（含補假）
    "2026-05-01", // 勞動節
    "2026-06-19", // 端午節
    "2026-09-25", // 中秋節
    "2026-09-28", // 孔子誕辰紀念日（教師節）
    "2026-10-09", "2026-10-10", // 國慶日（10/10 六，10/9 補假）
    "2026-10-24", "2026-10-25", "2026-10-26", // 臺灣光復暨金門古寧頭大捷紀念日（10/25 日，10/26 補假）
    "2026-12-25", // 行憲紀念日
    // ── 2027（民國 116 年）──
    "2027-01-01", // 開國紀念日
    "2027-02-04", "2027-02-05", "2027-02-06", "2027-02-07", "2027-02-08", // 小年夜／除夕／春節
    "2027-02-09", "2027-02-10",
    "2027-02-27", "2027-02-28", "2027-03-01", // 和平紀念日（2/28 日，3/1 補假）
    "2027-04-03", "2027-04-04", "2027-04-05", "2027-04-06", // 兒童節／清明節（含補假）
    "2027-04-30", "2027-05-01", // 勞動節（5/1 六，4/30 補假）
    "2027-06-09", // 端午節
    "2027-09-15", // 中秋節
    "2027-09-28", // 孔子誕辰紀念日（教師節）
    "2027-10-09", "2027-10-10", "2027-10-11", // 國慶日（10/10 日，10/11 補假）
    "2027-10-23", "2027-10-24", "2027-10-25", // 臺灣光復暨金門古寧頭大捷紀念日（10/24 日，10/25 補假）
    "2027-12-24", "2027-12-25", // 行憲紀念日（12/25 六，12/24 補假）
    "2027-12-31", // 補 2028/1/1（六）開國紀念日
];

const OVERRIDE_FILE: &str = "config/holidays.json";
const OVERRIDE_RELOAD_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Debug, Default, Clone, serde::Deserialize)]
struct HolidayOverrides {
    #[serde(default)]
    extra: Vec<String>,
    #[serde(default)]
    workdays: Vec<String>,
}

#[derive(Debug, Default)]
struct OverrideCache {
    loaded_at: Option<Instant>,
    extra: HashSet<NaiveDate>,
    workdays: HashSet<NaiveDate>,
}

static BUILTIN: OnceLock<HashSet<NaiveDate>> = OnceLock::new();
static OVERRIDES: OnceLock<Mutex<OverrideCache>> = OnceLock::new();

fn builtin() -> &'static HashSet<NaiveDate> {
    BUILTIN.get_or_init(|| {
        BUILTIN_HOLIDAYS
            .iter()
            .filter_map(|raw| NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok())
            .collect()
    })
}

fn parse_dates(list: &[String]) -> HashSet<NaiveDate> {
    list.iter()
        .filter_map(|raw| NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").ok())
        .collect()
}

/// 讀取（或沿用快取的）`config/holidays.json`。檔案不存在＝沒有覆寫，不算錯誤。
fn with_overrides<R>(f: impl FnOnce(&OverrideCache) -> R) -> R {
    let cache = OVERRIDES.get_or_init(|| Mutex::new(OverrideCache::default()));
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    let stale = guard
        .loaded_at
        .map(|t| t.elapsed() >= OVERRIDE_RELOAD_INTERVAL)
        .unwrap_or(true);
    if stale {
        let parsed = std::fs::read_to_string(OVERRIDE_FILE)
            .ok()
            .and_then(|text| serde_json::from_str::<HolidayOverrides>(&text).ok())
            .unwrap_or_default();
        guard.extra = parse_dates(&parsed.extra);
        guard.workdays = parse_dates(&parsed.workdays);
        guard.loaded_at = Some(Instant::now());
    }
    f(&guard)
}

/// 這一天是否為「不自動輪詢」的休息日：週末、內建國定假日、或 extra 內的日期；
/// `workdays` 內的日期一律視為上課日（優先權最高）。
pub fn is_rest_day(date: NaiveDate) -> bool {
    with_overrides(|o| {
        if o.workdays.contains(&date) {
            return false;
        }
        if o.extra.contains(&date) {
            return true;
        }
        matches!(date.weekday(), Weekday::Sat | Weekday::Sun) || builtin().contains(&date)
    })
}

/// 給 log／UI 用：這一天是哪種休息日。
pub fn rest_day_reason(date: NaiveDate) -> Option<&'static str> {
    if !is_rest_day(date) {
        return None;
    }
    let is_extra = with_overrides(|o| o.extra.contains(&date));
    if is_extra {
        Some("自訂假日")
    } else if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
        Some("週末")
    } else {
        Some("國定假日")
    }
}

/// 從 `date`（不含）往後找最近一個非休息日；最多往後看 60 天。
pub fn next_working_day_after(date: NaiveDate) -> Option<NaiveDate> {
    let mut cursor = date;
    for _ in 0..60 {
        cursor = cursor.succ_opt()?;
        if !is_rest_day(cursor) {
            return Some(cursor);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn weekends_are_rest_days() {
        assert!(is_rest_day(d("2026-09-05"))); // Sat
        assert!(is_rest_day(d("2026-09-06"))); // Sun
        assert!(!is_rest_day(d("2026-09-07"))); // Mon
    }

    #[test]
    fn builtin_holidays_are_rest_days() {
        assert!(is_rest_day(d("2026-02-18"))); // 春節 Wed
        assert!(is_rest_day(d("2026-10-09"))); // 國慶補假 Fri
        assert!(is_rest_day(d("2027-12-31"))); // 補 2028 元旦
        assert!(!is_rest_day(d("2026-10-08"))); // 前一天 Thu
    }

    #[test]
    fn builtin_list_parses_completely() {
        assert_eq!(builtin().len(), BUILTIN_HOLIDAYS.len(), "有日期格式錯誤");
    }

    #[test]
    fn next_working_day_skips_long_weekend() {
        // 2026-10-09 Fri 補假 → 10/10 Sat, 10/11 Sun → 10/12 Mon
        assert_eq!(next_working_day_after(d("2026-10-08")), Some(d("2026-10-12")));
    }
}
