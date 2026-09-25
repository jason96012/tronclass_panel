//! REST_DAY_NO_POLL_V1：週末與國定假日判定（全專案共用）。
//!
//! - 週六、週日一律視為休息日。
//! - 內建臺灣行政院人事行政總處核定的放假日（目前收錄 2026、2027 年，含補假日）。
//! - SEMESTER_BREAK_V1：內建寒暑假區間（依淡江大學 115 學年度行事曆），
//!   開學日前、學期結束後（寒假／暑假）整段不自動輪詢；開學日（開始上課日）當天恢復。
//! - `config/holidays.json` 可額外增減：
//!   ```json
//!   {
//!     "extra": ["2026-11-20"],
//!     "workdays": ["2026-04-06"],
//!     "breaks": [{ "from": "2027-06-28", "to": "2027-09-12", "name": "暑假" }],
//!     "disable_builtin_breaks": false
//!   }
//!   ```
//!   `extra` = 額外放假（例如校慶、颱風假）；`workdays` = 強制視為上課日（例如補課，
//!   優先權最高，寒暑假期間也能靠它強制輪詢）；`breaks` = 額外的停課區間（含頭尾）；
//!   `disable_builtin_breaks` = true 時忽略內建寒暑假（例如另一所學校的帳號）。
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
const BUILTIN_HOLIDAYS: &[(&str, &str)] = &[
    // ── 2026（民國 115 年）──
    ("2026-01-01", "開國紀念日"),
    ("2026-02-14", "農曆春節（小年夜）"),
    ("2026-02-15", "農曆春節（除夕）"),
    ("2026-02-16", "農曆春節（初一）"),
    ("2026-02-17", "農曆春節（初二）"),
    ("2026-02-18", "農曆春節（初三）"),
    ("2026-02-19", "農曆春節（初四，補假）"),
    ("2026-02-20", "農曆春節（初五，補假）"),
    ("2026-02-21", "農曆春節連假"),
    ("2026-02-22", "農曆春節連假"),
    ("2026-02-27", "和平紀念日補假"),
    ("2026-02-28", "和平紀念日"),
    ("2026-04-03", "兒童節補假"),
    ("2026-04-04", "兒童節"),
    ("2026-04-05", "清明節"),
    ("2026-04-06", "清明節補假"),
    ("2026-05-01", "勞動節"),
    ("2026-06-19", "端午節"),
    ("2026-09-25", "中秋節"),
    ("2026-09-28", "教師節"),
    ("2026-10-09", "國慶日補假"),
    ("2026-10-10", "國慶日"),
    ("2026-10-24", "光復節連假"),
    ("2026-10-25", "臺灣光復暨金門古寧頭大捷紀念日"),
    ("2026-10-26", "光復節補假"),
    ("2026-12-25", "行憲紀念日"),
    // ── 2027（民國 116 年）──
    ("2027-01-01", "開國紀念日"),
    ("2027-02-04", "農曆春節（小年夜）"),
    ("2027-02-05", "農曆春節（除夕）"),
    ("2027-02-06", "農曆春節（初一）"),
    ("2027-02-07", "農曆春節（初二）"),
    ("2027-02-08", "農曆春節（初三）"),
    ("2027-02-09", "農曆春節（補假）"),
    ("2027-02-10", "農曆春節（補假）"),
    ("2027-02-27", "和平紀念日連假"),
    ("2027-02-28", "和平紀念日"),
    ("2027-03-01", "和平紀念日補假"),
    ("2027-04-03", "兒童節連假"),
    ("2027-04-04", "兒童節"),
    ("2027-04-05", "清明節"),
    ("2027-04-06", "兒童節補假"),
    ("2027-04-30", "勞動節補假"),
    ("2027-05-01", "勞動節"),
    ("2027-06-09", "端午節"),
    ("2027-09-15", "中秋節"),
    ("2027-09-28", "教師節"),
    ("2027-10-09", "國慶日連假"),
    ("2027-10-10", "國慶日"),
    ("2027-10-11", "國慶日補假"),
    ("2027-10-23", "光復節連假"),
    ("2027-10-24", "臺灣光復暨金門古寧頭大捷紀念日"),
    ("2027-10-25", "光復節補假"),
    ("2027-12-24", "行憲紀念日補假"),
    ("2027-12-25", "行憲紀念日"),
    ("2027-12-31", "開國紀念日補假（補 2028/1/1）"),
];

/// SEMESTER_BREAK_V1：內建寒暑假（含頭尾）。來源：淡江大學 115 學年度行事曆
/// （1150410 第 208 次行政會議通過）：
/// - 第 1 學期 9/14 開始上課，1/18 起寒假（至 2/21）
/// - 第 2 學期 2/22 開始上課，6/28 起暑假
/// 116 學年度行事曆尚未公布，最後一段暑假保守地只排到 8/31（寧可多輪詢，不要漏點名）；
/// 公布後請更新這裡，或用 config/holidays.json 的 `breaks` 補上。
const BUILTIN_BREAKS: &[(&str, &str, &str)] = &[
    ("2026-06-29", "2026-09-13", "暑假"), // 115 學年度第 1 學期 9/14 開始上課
    ("2027-01-18", "2027-02-21", "寒假"), // 115 學年度第 2 學期 2/22 開始上課
    ("2027-06-28", "2027-08-31", "暑假"), // 116 學年度開學日待公布（保守值）
];

const OVERRIDE_FILE: &str = "config/holidays.json";
const OVERRIDE_RELOAD_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct HolidayOverrides {
    #[serde(default)]
    extra: Vec<String>,
    #[serde(default)]
    workdays: Vec<String>,
    /// SEMESTER_BREAK_V1：額外停課區間。
    #[serde(default)]
    breaks: Vec<BreakOverride>,
    /// SEMESTER_BREAK_V1：true 時不套用內建寒暑假。
    #[serde(default)]
    disable_builtin_breaks: bool,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct BreakOverride {
    from: String,
    to: String,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Default)]
struct OverrideCache {
    loaded_at: Option<Instant>,
    extra: HashSet<NaiveDate>,
    workdays: HashSet<NaiveDate>,
    /// 已解析的自訂停課區間（含頭尾）。
    breaks: Vec<(NaiveDate, NaiveDate)>,
    disable_builtin_breaks: bool,
}

/// SEMESTER_BREAK_V1：`date` 是否落在內建寒暑假內；回傳區間名稱。
fn builtin_break_name(date: NaiveDate) -> Option<&'static str> {
    BUILTIN_BREAKS.iter().find_map(|(from, to, name)| {
        let from = NaiveDate::parse_from_str(from, "%Y-%m-%d").ok()?;
        let to = NaiveDate::parse_from_str(to, "%Y-%m-%d").ok()?;
        (from <= date && date <= to).then_some(*name)
    })
}

static BUILTIN: OnceLock<HashSet<NaiveDate>> = OnceLock::new();
static OVERRIDES: OnceLock<Mutex<OverrideCache>> = OnceLock::new();

fn builtin() -> &'static HashSet<NaiveDate> {
    BUILTIN.get_or_init(|| {
        BUILTIN_HOLIDAYS
            .iter()
            .filter_map(|(raw, _)| NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok())
            .collect()
    })
}

/// REST_DAY_PAGE_V1：內建國定假日的名稱（給頁面顯示用）。
fn builtin_holiday_name(date: NaiveDate) -> Option<&'static str> {
    BUILTIN_HOLIDAYS.iter().find_map(|(raw, name)| {
        (NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()? == date).then_some(*name)
    })
}

fn parse_dates(list: &[String]) -> HashSet<NaiveDate> {
    list.iter()
        .filter_map(|raw| NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").ok())
        .collect()
}

/// 讀 `config/holidays.json` 成結構；檔案不存在或格式錯誤都回預設值（不算錯誤）。
fn read_override_file() -> HolidayOverrides {
    std::fs::read_to_string(OVERRIDE_FILE)
        .ok()
        .and_then(|text| serde_json::from_str::<HolidayOverrides>(&text).ok())
        .unwrap_or_default()
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
        let parsed = read_override_file();
        guard.extra = parse_dates(&parsed.extra);
        guard.workdays = parse_dates(&parsed.workdays);
        guard.breaks = parsed
            .breaks
            .iter()
            .filter_map(|b| {
                let from = NaiveDate::parse_from_str(b.from.trim(), "%Y-%m-%d").ok()?;
                let to = NaiveDate::parse_from_str(b.to.trim(), "%Y-%m-%d").ok()?;
                (from <= to).then_some((from, to))
            })
            .collect();
        guard.disable_builtin_breaks = parsed.disable_builtin_breaks;
        guard.loaded_at = Some(Instant::now());
    }
    f(&guard)
}

/// REST_DAY_EDIT_V1：從頁面直接調整某一天的覆寫。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayOverride {
    /// 加入 `extra`（不用上課），並自 `workdays` 移除。
    Rest,
    /// 加入 `workdays`（強制上課），並自 `extra` 移除。
    Work,
    /// 從 `extra`、`workdays` 都移除，回到內建判定。
    Clear,
}

/// REST_DAY_EDIT_V1：寫回 `config/holidays.json`（原子寫入，保留 breaks／disable_builtin_breaks），
/// 並讓快取立刻失效，所以監控迴圈下一輪就會看到新結果。
/// 在快取鎖內完成「讀檔 → 改 → 寫檔」，避免兩個請求同時寫互相蓋掉。
pub fn set_day_override(date: NaiveDate, mode: DayOverride) -> std::io::Result<()> {
    set_day_override_range(date, date, mode)
}

/// REST_DAY_RANGE_V1：一次調整 `from..=to` 連續多天（含頭尾），只讀寫一次檔案。
/// `from > to` 會自動對調；超過 366 天視為誤操作直接拒絕。
pub fn set_day_override_range(
    from: NaiveDate,
    to: NaiveDate,
    mode: DayOverride,
) -> std::io::Result<()> {
    let (from, to) = if from <= to { (from, to) } else { (to, from) };
    let span_days = (to - from).num_days();
    if span_days > 366 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "日期範圍不可超過 366 天",
        ));
    }
    let in_range = |d: &NaiveDate| *d >= from && *d <= to;
    let cache = OVERRIDES.get_or_init(|| Mutex::new(OverrideCache::default()));
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());

    let mut file = read_override_file();
    let mut extra: Vec<NaiveDate> = parse_dates(&file.extra).into_iter().collect();
    let mut workdays: Vec<NaiveDate> = parse_dates(&file.workdays).into_iter().collect();
    extra.retain(|d| !in_range(d));
    workdays.retain(|d| !in_range(d));
    let range_days = from.iter_days().take_while(|d| *d <= to);
    match mode {
        DayOverride::Rest => extra.extend(range_days),
        DayOverride::Work => workdays.extend(range_days),
        DayOverride::Clear => {}
    }
    extra.sort();
    workdays.sort();
    file.extra = extra.iter().map(|d| d.format("%Y-%m-%d").to_string()).collect();
    file.workdays = workdays.iter().map(|d| d.format("%Y-%m-%d").to_string()).collect();
    let text = serde_json::to_string_pretty(&file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    crate::atomic_write::write_atomic(OVERRIDE_FILE, text)?;
    // 讓下一次 with_overrides 重新讀檔
    guard.loaded_at = None;
    Ok(())
}

/// 這一天是否為「不自動輪詢」的休息日：週末、內建國定假日、extra 內的日期，
/// 或寒暑假／自訂停課區間內；`workdays` 內的日期一律視為上課日（優先權最高）。
pub fn is_rest_day(date: NaiveDate) -> bool {
    rest_day_reason(date).is_some()
}

/// 給 log／UI 用：這一天是哪種休息日；上課日回傳 None。
/// 判定順序：workdays（強制上課）→ 自訂假日 → 週末 → 國定假日 → 寒暑假／自訂停課。
pub fn rest_day_reason(date: NaiveDate) -> Option<&'static str> {
    with_overrides(|o| {
        if o.workdays.contains(&date) {
            return None;
        }
        if o.extra.contains(&date) {
            return Some("自訂假日");
        }
        if matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
            return Some("週末");
        }
        if builtin().contains(&date) {
            return Some("國定假日");
        }
        // SEMESTER_BREAK_V1
        if !o.disable_builtin_breaks {
            if let Some(name) = builtin_break_name(date) {
                return Some(name);
            }
        }
        if o.breaks.iter().any(|(from, to)| *from <= date && date <= *to) {
            return Some("停課期間");
        }
        None
    })
}

/// REST_DAY_PAGE_V1：給「休息日與寒暑假」頁面用的完整快照。
/// 內容全部由同一套判定產生，頁面不需要自己重算規則。
pub fn calendar_snapshot(today: NaiveDate) -> serde_json::Value {
    use serde_json::json;

    let today_reason = rest_day_reason(today);
    let next_working_day = if today_reason.is_some() {
        next_working_day_after(today)
    } else {
        Some(today)
    };

    // 內建國定假日（含名稱）
    let holidays: Vec<serde_json::Value> = BUILTIN_HOLIDAYS
        .iter()
        .map(|(date, name)| json!({ "date": date, "name": name }))
        .collect();

    // 寒暑假（內建 + 自訂）
    let (extra, workdays, custom_breaks, disable_builtin_breaks) = with_overrides(|o| {
        let mut extra: Vec<NaiveDate> = o.extra.iter().copied().collect();
        extra.sort();
        let mut workdays: Vec<NaiveDate> = o.workdays.iter().copied().collect();
        workdays.sort();
        (extra, workdays, o.breaks.clone(), o.disable_builtin_breaks)
    });
    let mut breaks: Vec<serde_json::Value> = Vec::new();
    if !disable_builtin_breaks {
        for (from, to, name) in BUILTIN_BREAKS {
            let resume = NaiveDate::parse_from_str(to, "%Y-%m-%d")
                .ok()
                .and_then(next_working_day_after);
            breaks.push(json!({
                "from": from,
                "to": to,
                "name": name,
                "builtin": true,
                "resume": resume.map(|d| d.format("%Y-%m-%d").to_string()),
            }));
        }
    }
    for (from, to) in &custom_breaks {
        breaks.push(json!({
            "from": from.format("%Y-%m-%d").to_string(),
            "to": to.format("%Y-%m-%d").to_string(),
            "name": "自訂停課",
            "builtin": false,
            "resume": next_working_day_after(*to).map(|d| d.format("%Y-%m-%d").to_string()),
        }));
    }

    // 逐日判定：從今天所在月份的 1 號起算 14 個月，讓月曆直接照著畫。
    let range_start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today);
    let mut days: Vec<serde_json::Value> = Vec::new();
    let mut cursor = range_start;
    for _ in 0..430 {
        let reason = rest_day_reason(cursor);
        let label = match reason {
            Some("國定假日") => builtin_holiday_name(cursor).map(str::to_string),
            _ => None,
        };
        days.push(json!([
            cursor.format("%Y-%m-%d").to_string(),
            reason,
            label,
        ]));
        match cursor.succ_opt() {
            Some(next) => cursor = next,
            None => break,
        }
    }

    json!({
        "today": today.format("%Y-%m-%d").to_string(),
        "today_reason": today_reason,
        "next_working_day": next_working_day.map(|d| d.format("%Y-%m-%d").to_string()),
        "holidays": holidays,
        "breaks": breaks,
        "overrides": {
            "file": OVERRIDE_FILE,
            "loaded": std::path::Path::new(OVERRIDE_FILE).exists(),
            "extra": extra.iter().map(|d| d.format("%Y-%m-%d").to_string()).collect::<Vec<_>>(),
            "workdays": workdays.iter().map(|d| d.format("%Y-%m-%d").to_string()).collect::<Vec<_>>(),
            "disable_builtin_breaks": disable_builtin_breaks,
        },
        "reload_interval_secs": OVERRIDE_RELOAD_INTERVAL.as_secs(),
        "days": days,
    })
}

/// 從 `date`（不含）往後找最近一個非休息日；最多往後看 120 天（涵蓋整個暑假）。
pub fn next_working_day_after(date: NaiveDate) -> Option<NaiveDate> {
    let mut cursor = date;
    for _ in 0..120 {
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
    fn semester_breaks_are_rest_days() {
        // SEMESTER_BREAK_V1：開學前一天放假、開學日恢復
        assert_eq!(rest_day_reason(d("2026-09-11")), Some("暑假")); // Fri，開學前
        assert!(!is_rest_day(d("2026-09-14"))); // Mon，開始上課
        assert_eq!(rest_day_reason(d("2027-01-18")), Some("寒假")); // Mon
        assert!(is_rest_day(d("2027-02-19"))); // Fri，仍在寒假
        assert!(!is_rest_day(d("2027-02-22"))); // Mon，第 2 學期開始上課
        assert_eq!(rest_day_reason(d("2027-06-28")), Some("暑假"));
        // 週末的優先權高於寒暑假（顯示用）
        assert_eq!(rest_day_reason(d("2026-09-12")), Some("週末"));
    }

    #[test]
    fn snapshot_has_expected_shape() {
        let snap = calendar_snapshot(d("2026-09-08"));
        assert_eq!(snap["today_reason"], "暑假");
        assert_eq!(snap["next_working_day"], "2026-09-14");
        assert!(snap["days"].as_array().map(|a| a.len() >= 400).unwrap_or(false));
        assert!(snap["holidays"].as_array().map(|a| !a.is_empty()).unwrap_or(false));
    }

    #[test]
    fn builtin_breaks_parse_and_are_ordered() {
        for (from, to, _) in BUILTIN_BREAKS {
            let from = d(from);
            let to = d(to);
            assert!(from <= to, "{from} > {to}");
        }
    }

    #[test]
    fn next_working_day_skips_long_weekend() {
        // 2026-10-09 Fri 補假 → 10/10 Sat, 10/11 Sun → 10/12 Mon
        assert_eq!(next_working_day_after(d("2026-10-08")), Some(d("2026-10-12")));
    }
}
