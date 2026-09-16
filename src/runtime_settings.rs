//! Runtime-tunable settings controlled from the Web admin panel.
//!
//! These settings are intentionally in-memory: changing them from `/admin` takes
//! effect immediately, and restarting the service restores values from config / defaults.

use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveTime, Weekday};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// GROUP_SUBMIT_RELAY_V1（fix67）：多帳號送出間隔的**基準**（毫秒）。
///
/// 同一場點名由多個帳號送出時，第一位立即送出，之後每位間隔
/// `base + U(0, jitter)`；游標只前進不後退，因此兩人至少間隔 `base`。
pub const DEFAULT_GROUP_SUBMIT_INTERVAL_MS: u64 = 600;
/// GROUP_SUBMIT_RELAY_V1（fix67）：送出間隔的**隨機上限**（毫秒，預設 2000）。
///
/// 0 ＝ 完全固定間隔（fix67 之前的行為）。
pub const DEFAULT_GROUP_SUBMIT_JITTER_MS: u64 = 2000;
/// 隨機上限的硬上限（面板／API 都不能超過）。
pub const MAX_GROUP_SUBMIT_JITTER_MS: u64 = 5000;
/// 基準的硬上限（沿用既有 10 秒上限）。
pub const MAX_GROUP_SUBMIT_INTERVAL_MS: u64 = 10_000;
pub const MAX_RUNTIME_REST_PERIODS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSettings {
    /// Override provider poll interval. None means use provider config.
    pub poll_interval_secs: Option<u64>,
    /// Delay between each account submission for the same rollcall.
    ///
    /// GROUP_SUBMIT_RELAY_V1（fix67）：此值現在是**基準**，實際間隔會加上
    /// `group_submit_jitter_ms` 的隨機值。
    pub group_submit_interval_ms: u64,
    /// GROUP_SUBMIT_RELAY_V1（fix67）：送出間隔的隨機上限（毫秒）。
    ///
    /// 實際間隔 = `group_submit_interval_ms + U(0, group_submit_jitter_ms)`；
    /// 0 ＝ 固定間隔。預設見 [`DEFAULT_GROUP_SUBMIT_JITTER_MS`]。
    #[serde(default = "default_group_submit_jitter_ms")]
    pub group_submit_jitter_ms: u64,
    /// Local time HH:MM after which timer polling auto-pauses. None means disabled.
    ///
    /// Kept for backwards compatibility; new UI uses `rest_periods` instead.
    pub auto_stop_time: Option<String>,
    /// Runtime rest windows. Timer polling is skipped while the local time falls in
    /// any of these windows; manual Force/Reauth still works.
    #[serde(default)]
    pub rest_periods: Vec<RuntimeRestPeriod>,
    /// CURRENT_SEMESTER_OVERRIDE_V1: runtime semester override, e.g. "1151".
    ///
    /// Empty / invalid values normalize to None (auto). When present it takes
    /// precedence over config.toml `[semester] current` (see
    /// `config::semester_override`); consumers read the parsed cache via the
    /// synchronous [`semester_override`] accessor.
    #[serde(default)]
    pub semester_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRestPeriod {
    pub weekday: RuntimeWeekday,
    pub start_time: String,
    pub end_time: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeWeekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl RuntimeWeekday {
    fn matches(self, weekday: Weekday) -> bool {
        matches!(
            (self, weekday),
            (RuntimeWeekday::Mon, Weekday::Mon)
                | (RuntimeWeekday::Tue, Weekday::Tue)
                | (RuntimeWeekday::Wed, Weekday::Wed)
                | (RuntimeWeekday::Thu, Weekday::Thu)
                | (RuntimeWeekday::Fri, Weekday::Fri)
                | (RuntimeWeekday::Sat, Weekday::Sat)
                | (RuntimeWeekday::Sun, Weekday::Sun)
        )
    }

    fn next(self) -> Self {
        match self {
            RuntimeWeekday::Mon => RuntimeWeekday::Tue,
            RuntimeWeekday::Tue => RuntimeWeekday::Wed,
            RuntimeWeekday::Wed => RuntimeWeekday::Thu,
            RuntimeWeekday::Thu => RuntimeWeekday::Fri,
            RuntimeWeekday::Fri => RuntimeWeekday::Sat,
            RuntimeWeekday::Sat => RuntimeWeekday::Sun,
            RuntimeWeekday::Sun => RuntimeWeekday::Mon,
        }
    }
}

/// serde 預設值：舊的設定檔／請求沒有這個欄位時，補上預設隨機上限（2 秒）。
fn default_group_submit_jitter_ms() -> u64 {
    DEFAULT_GROUP_SUBMIT_JITTER_MS
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            poll_interval_secs: None,
            group_submit_interval_ms: DEFAULT_GROUP_SUBMIT_INTERVAL_MS,
            group_submit_jitter_ms: DEFAULT_GROUP_SUBMIT_JITTER_MS,
            auto_stop_time: None,
            rest_periods: Vec::new(),
            semester_override: None,
        }
    }
}

static SETTINGS: Lazy<RwLock<RuntimeSettings>> =
    Lazy::new(|| RwLock::new(RuntimeSettings::default()));

/// CURRENT_SEMESTER_OVERRIDE_V1：解析後的學期覆蓋快取。
///
/// 學期判定会被呼叫在同步的解析函數深處（如 `api::todos` 的課程/作業
/// 過濾），無法 await tokio 鎖，因此以 std 鎖另行快取解析結果；
/// [`update`] 時同步刷新。
static SEMESTER_OVERRIDE: Lazy<std::sync::RwLock<Option<(i32, u32)>>> =
    Lazy::new(|| std::sync::RwLock::new(None));

/// 目前生效的執行期學期覆蓋（同步讀取，可在同步程式碼中呼叫）。
///
/// `None` 代表未覆蓋——呼叫端應退回 config.toml `[semester] current`，
/// 再退回依日期自動推算（見 `config::semester_override`）。
pub fn semester_override() -> Option<(i32, u32)> {
    *SEMESTER_OVERRIDE.read().unwrap_or_else(|p| p.into_inner())
}

fn refresh_semester_override_cache(raw: Option<&str>) {
    let parsed = raw.and_then(crate::config::parse_semester_current);
    *SEMESTER_OVERRIDE.write().unwrap_or_else(|p| p.into_inner()) = parsed;
}

pub async fn snapshot() -> RuntimeSettings {
    SETTINGS.read().await.clone()
}

pub async fn update(settings: RuntimeSettings) {
    let settings = normalize(settings);
    refresh_semester_override_cache(settings.semester_override.as_deref());
    *SETTINGS.write().await = settings;
}

pub async fn poll_interval(default_secs: u64) -> Duration {
    let settings = SETTINGS.read().await;
    Duration::from_secs(settings.poll_interval_secs.unwrap_or(default_secs).max(1))
}

pub async fn group_submit_interval_ms() -> u64 {
    SETTINGS.read().await.group_submit_interval_ms
}

/// GROUP_SUBMIT_RELAY_V1（fix67）：送出間隔的隨機上限（毫秒）。
pub async fn group_submit_jitter_ms() -> u64 {
    SETTINGS.read().await.group_submit_jitter_ms
}

pub async fn auto_stop_reached(now: NaiveTime) -> bool {
    let settings = SETTINGS.read().await;
    let Some(value) = settings.auto_stop_time.as_deref() else {
        return false;
    };
    let Some(stop_time) = parse_hhmm(value) else {
        return false;
    };
    now >= stop_time
}

pub async fn is_rest_time(now: DateTime<Local>) -> bool {
    let settings = SETTINGS.read().await;
    let weekday = now.weekday();
    let time = now.time();
    settings
        .rest_periods
        .iter()
        .any(|period| rest_period_contains(period, weekday, time))
}

fn normalize(mut settings: RuntimeSettings) -> RuntimeSettings {
    settings.poll_interval_secs = settings.poll_interval_secs.filter(|v| *v > 0);
    settings.group_submit_interval_ms = settings
        .group_submit_interval_ms
        .min(MAX_GROUP_SUBMIT_INTERVAL_MS);
    settings.group_submit_jitter_ms = settings
        .group_submit_jitter_ms
        .min(MAX_GROUP_SUBMIT_JITTER_MS);
    settings.auto_stop_time = settings.auto_stop_time.and_then(|value| {
        let value = value.trim().to_string();
        if value.is_empty() || parse_hhmm(&value).is_none() {
            None
        } else {
            Some(value)
        }
    });
    settings.rest_periods = settings
        .rest_periods
        .into_iter()
        .take(MAX_RUNTIME_REST_PERIODS)
        .filter_map(normalize_rest_period)
        .collect();
    // CURRENT_SEMESTER_OVERRIDE_V1：空字串＝清除覆蓋；格式無效靜默退回 None
    // （與 auto_stop_time 同款防禦），避免壞值讓學期判定失效。
    settings.semester_override = settings.semester_override.and_then(|value| {
        let value = value.trim().to_string();
        if value.is_empty() || crate::config::parse_semester_current(&value).is_none() {
            None
        } else {
            Some(value)
        }
    });
    settings
}

fn normalize_rest_period(mut period: RuntimeRestPeriod) -> Option<RuntimeRestPeriod> {
    period.start_time = period.start_time.trim().to_string();
    period.end_time = period.end_time.trim().to_string();
    let start = parse_hhmm(&period.start_time)?;
    let end = parse_hhmm(&period.end_time)?;
    if start == end {
        return None;
    }
    period.start_time = start.format("%H:%M").to_string();
    period.end_time = end.format("%H:%M").to_string();
    Some(period)
}

fn rest_period_contains(period: &RuntimeRestPeriod, weekday: Weekday, now: NaiveTime) -> bool {
    let Some(start) = parse_hhmm(&period.start_time) else {
        return false;
    };
    let Some(end) = parse_hhmm(&period.end_time) else {
        return false;
    };

    if start < end {
        period.weekday.matches(weekday) && now >= start && now < end
    } else {
        // Cross-midnight window, e.g. Fri 23:00 ~ 07:00 means:
        // Fri 23:00-24:00 and Sat 00:00-07:00.
        (period.weekday.matches(weekday) && now >= start)
            || (period.weekday.next().matches(weekday) && now < end)
    }
}

fn parse_hhmm(value: &str) -> Option<NaiveTime> {
    let (hh, mm) = value.split_once(':')?;
    let hour = hh.parse::<u32>().ok()?;
    let minute = mm.parse::<u32>().ok()?;
    NaiveTime::from_hms_opt(hour, minute, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_semester_override_drops_invalid_and_keeps_valid() {
        let mk = |value: Option<&str>| RuntimeSettings {
            semester_override: value.map(str::to_string),
            ..RuntimeSettings::default()
        };
        // 有效值保留（含前後空白 trim）
        assert_eq!(
            normalize(mk(Some(" 115-1 "))).semester_override,
            Some("115-1".to_string())
        );
        assert_eq!(normalize(mk(Some("2026 Fall"))).semester_override, Some("2026 Fall".to_string()));
        // 空字串／格式無效 → None（清除／防禦）
        assert_eq!(normalize(mk(Some("   "))).semester_override, None);
        assert_eq!(normalize(mk(Some("not-a-semester"))).semester_override, None);
        assert_eq!(normalize(mk(Some("115"))).semester_override, None);
        assert_eq!(normalize(mk(None)).semester_override, None);
    }

    #[test]
    fn semester_override_cache_roundtrip_and_clear() {
        // 單一測試內序列操作全域快取（避免並行測試互相污染），結尾還原。
        refresh_semester_override_cache(Some("115-1"));
        assert_eq!(semester_override(), Some((2026, 1)));
        refresh_semester_override_cache(Some("1142"));
        assert_eq!(semester_override(), Some((2025, 2)));
        // 壞值＝清除（與 normalize 的防禦一致）
        refresh_semester_override_cache(Some("bogus"));
        assert_eq!(semester_override(), None);
        refresh_semester_override_cache(None);
        assert_eq!(semester_override(), None);
    }
}
