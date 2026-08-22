//! Runtime-tunable settings controlled from the Web admin panel.
//!
//! These settings are intentionally in-memory: changing them from `/admin` takes
//! effect immediately, and restarting the service restores values from config / defaults.

use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveTime, Weekday};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

pub const DEFAULT_GROUP_SUBMIT_INTERVAL_MS: u64 = 500;
pub const MAX_RUNTIME_REST_PERIODS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSettings {
    /// Override provider poll interval. None means use provider config.
    pub poll_interval_secs: Option<u64>,
    /// Delay between each account submission for the same rollcall.
    pub group_submit_interval_ms: u64,
    /// Local time HH:MM after which timer polling auto-pauses. None means disabled.
    ///
    /// Kept for backwards compatibility; new UI uses `rest_periods` instead.
    pub auto_stop_time: Option<String>,
    /// Runtime rest windows. Timer polling is skipped while the local time falls in
    /// any of these windows; manual Force/Reauth still works.
    #[serde(default)]
    pub rest_periods: Vec<RuntimeRestPeriod>,
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

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            poll_interval_secs: None,
            group_submit_interval_ms: DEFAULT_GROUP_SUBMIT_INTERVAL_MS,
            auto_stop_time: None,
            rest_periods: Vec::new(),
        }
    }
}

static SETTINGS: Lazy<RwLock<RuntimeSettings>> =
    Lazy::new(|| RwLock::new(RuntimeSettings::default()));

pub async fn snapshot() -> RuntimeSettings {
    SETTINGS.read().await.clone()
}

pub async fn update(settings: RuntimeSettings) {
    *SETTINGS.write().await = normalize(settings);
}

pub async fn poll_interval(default_secs: u64) -> Duration {
    let settings = SETTINGS.read().await;
    Duration::from_secs(settings.poll_interval_secs.unwrap_or(default_secs).max(1))
}

pub async fn group_submit_interval_ms() -> u64 {
    SETTINGS.read().await.group_submit_interval_ms
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
    settings.group_submit_interval_ms = settings.group_submit_interval_ms.min(10_000);
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
