//! Timetable-driven rollcall detection windows.
//!
//! A class is considered "inside" its detection window from `minutes_before`
//! before its start until `minutes_after` after its end.  Times are evaluated
//! on a repeating seven-day timeline so windows crossing midnight work too.

use chrono::{NaiveTime, Timelike, Weekday};
use serde::Serialize;

use crate::db::{DetectionSchedulePreferenceRow, TimetableEntryRow};

const WEEK_MINUTES: i64 = 7 * 24 * 60;
const NIGHT_PAUSE_START_SECS: u32 = 19 * 60 * 60;
const NIGHT_PAUSE_END_SECS: u32 = 7 * 60 * 60;
const DAY_SECS: u32 = 24 * 60 * 60;

/// The fixed local-time quiet period is 19:00 (inclusive) through 07:00
/// (exclusive). During this period timer-triggered provider polling is paused.
pub fn is_night_pause(now: NaiveTime) -> bool {
    let seconds = now.num_seconds_from_midnight();
    seconds >= NIGHT_PAUSE_START_SECS || seconds < NIGHT_PAUSE_END_SECS
}

/// Seconds until 07:00 when currently in the quiet period.
pub fn seconds_until_daytime(now: NaiveTime) -> Option<u64> {
    let seconds = now.num_seconds_from_midnight();
    let wait = if seconds >= NIGHT_PAUSE_START_SECS {
        DAY_SECS - seconds + NIGHT_PAUSE_END_SECS
    } else if seconds < NIGHT_PAUSE_END_SECS {
        NIGHT_PAUSE_END_SECS - seconds
    } else {
        return None;
    };
    Some(u64::from(wait.max(1)))
}

/// Seconds until the next 19:00 quiet-period boundary during daytime.
pub fn seconds_until_night_pause(now: NaiveTime) -> Option<u64> {
    (!is_night_pause(now)).then(|| {
        u64::from(
            NIGHT_PAUSE_START_SECS
                .saturating_sub(now.num_seconds_from_midnight())
                .max(1),
        )
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveDetectionWindow {
    pub course_name: String,
    pub weekday: String,
    pub class_start: String,
    pub class_end: String,
    pub detection_start: String,
    pub detection_end: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DetectionScheduleEvaluation {
    pub active: bool,
    pub usable_entries: usize,
    pub active_windows: Vec<ActiveDetectionWindow>,
    /// Seconds until the next detection window begins. `None` means no usable
    /// timetable entry exists.
    pub next_window_in_secs: Option<u64>,
}

pub fn evaluate(
    entries: &[TimetableEntryRow],
    weekday: Weekday,
    now: NaiveTime,
    preference: &DetectionSchedulePreferenceRow,
) -> DetectionScheduleEvaluation {
    let now_week = weekday_index(weekday) * 24 * 60 + minutes_of_day(now);
    let before = preference.minutes_before.clamp(0, 180);
    let after = preference.minutes_after.clamp(0, 180);
    let mut active_windows = Vec::new();
    let mut usable_entries = 0usize;
    let mut next_minutes: Option<i64> = None;

    for entry in entries {
        let Some(day) = weekday_key_index(&entry.weekday) else {
            continue;
        };
        let Some((start_of_day, end_of_day)) = entry_times(entry) else {
            continue;
        };
        usable_entries += 1;
        let class_start = day * 24 * 60 + start_of_day;
        let mut class_end = day * 24 * 60 + end_of_day;
        if class_end <= class_start {
            class_end += 24 * 60;
        }
        let window_start = class_start - before;
        let window_end = class_end + after;

        let is_active = [-WEEK_MINUTES, 0, WEEK_MINUTES]
            .into_iter()
            .any(|shift| now_week >= window_start + shift && now_week < window_end + shift);
        if is_active {
            active_windows.push(ActiveDetectionWindow {
                course_name: entry.course_name.clone(),
                weekday: entry.weekday.clone(),
                class_start: format_minutes(start_of_day),
                class_end: format_minutes(end_of_day),
                detection_start: format_week_minute(window_start),
                detection_end: format_week_minute(window_end),
            });
        }

        let mut delta = (window_start - now_week).rem_euclid(WEEK_MINUTES);
        // A zero delta means the boundary is now and should wake immediately.
        if delta == 0 && is_active {
            delta = WEEK_MINUTES;
        }
        if !is_active {
            next_minutes = Some(next_minutes.map_or(delta, |old| old.min(delta)));
        }
    }

    DetectionScheduleEvaluation {
        active: !active_windows.is_empty(),
        usable_entries,
        active_windows,
        next_window_in_secs: next_minutes.map(|minutes| (minutes.max(0) as u64) * 60),
    }
}

fn entry_times(entry: &TimetableEntryRow) -> Option<(i64, i64)> {
    let start = parse_hhmm(&entry.start_time).or_else(|| period_start(entry.period_start))?;
    let end = parse_hhmm(&entry.end_time).or_else(|| period_end(entry.period_end))?;
    Some((start, end))
}

fn parse_hhmm(value: &str) -> Option<i64> {
    let (hour, minute) = value.trim().split_once(':')?;
    let hour = hour.parse::<i64>().ok()?;
    let minute = minute.parse::<i64>().ok()?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    Some(hour * 60 + minute)
}

fn period_start(period: i64) -> Option<i64> {
    (1..=10)
        .contains(&period)
        .then_some(8 * 60 + 10 + (period - 1) * 60)
}

fn period_end(period: i64) -> Option<i64> {
    (1..=10)
        .contains(&period)
        .then_some(9 * 60 + (period - 1) * 60)
}

fn weekday_index(weekday: Weekday) -> i64 {
    weekday.num_days_from_monday() as i64
}

fn weekday_key_index(value: &str) -> Option<i64> {
    match value.trim().to_ascii_lowercase().as_str() {
        "mon" => Some(0),
        "tue" => Some(1),
        "wed" => Some(2),
        "thu" => Some(3),
        "fri" => Some(4),
        "sat" => Some(5),
        "sun" => Some(6),
        _ => None,
    }
}

fn minutes_of_day(time: NaiveTime) -> i64 {
    i64::from(time.hour() * 60 + time.minute())
}

fn format_minutes(minutes: i64) -> String {
    let value = minutes.rem_euclid(24 * 60);
    format!("{:02}:{:02}", value / 60, value % 60)
}

fn format_week_minute(minutes: i64) -> String {
    let value = minutes.rem_euclid(WEEK_MINUTES);
    let day = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"][(value / (24 * 60)) as usize];
    format!("{} {}", day, format_minutes(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pref() -> DetectionSchedulePreferenceRow {
        DetectionSchedulePreferenceRow {
            account_id: "a".into(),
            enabled: true,
            minutes_before: 10,
            minutes_after: 10,
            outside_interval_secs: 300,
            updated_at: 0,
            updated_time: String::new(),
        }
    }

    fn entry(day: &str, start: &str, end: &str) -> TimetableEntryRow {
        TimetableEntryRow {
            id: 1,
            account_id: "a".into(),
            weekday: day.into(),
            period_start: 1,
            period_end: 1,
            start_time: start.into(),
            end_time: end.into(),
            course_name: "測試課程".into(),
            teacher: String::new(),
            classroom: String::new(),
            course_code: String::new(),
            source: String::new(),
            updated_at: 0,
            updated_time: String::new(),
        }
    }

    #[test]
    fn active_inside_before_after_window() {
        let result = evaluate(
            &[entry("mon", "08:10", "09:00")],
            Weekday::Mon,
            NaiveTime::from_hms_opt(8, 5, 0).unwrap(),
            &pref(),
        );
        assert!(result.active);
    }

    #[test]
    fn outside_window_reports_next_boundary() {
        let result = evaluate(
            &[entry("mon", "08:10", "09:00")],
            Weekday::Mon,
            NaiveTime::from_hms_opt(7, 55, 0).unwrap(),
            &pref(),
        );
        assert!(!result.active);
        assert_eq!(result.next_window_in_secs, Some(5 * 60));
    }

    #[test]
    fn window_can_cross_midnight() {
        let result = evaluate(
            &[entry("mon", "23:55", "23:59")],
            Weekday::Tue,
            NaiveTime::from_hms_opt(0, 5, 0).unwrap(),
            &pref(),
        );
        assert!(result.active);
    }

    #[test]
    fn night_pause_uses_inclusive_19_and_exclusive_07_boundaries() {
        assert!(is_night_pause(NaiveTime::from_hms_opt(19, 0, 0).unwrap()));
        assert!(is_night_pause(NaiveTime::from_hms_opt(6, 59, 59).unwrap()));
        assert!(!is_night_pause(NaiveTime::from_hms_opt(7, 0, 0).unwrap()));
        assert!(!is_night_pause(NaiveTime::from_hms_opt(18, 59, 59).unwrap()));
    }

    #[test]
    fn night_pause_boundaries_report_exact_wake_intervals() {
        assert_eq!(
            seconds_until_daytime(NaiveTime::from_hms_opt(19, 0, 0).unwrap()),
            Some(12 * 60 * 60)
        );
        assert_eq!(
            seconds_until_daytime(NaiveTime::from_hms_opt(6, 59, 0).unwrap()),
            Some(60)
        );
        assert_eq!(
            seconds_until_night_pause(NaiveTime::from_hms_opt(7, 0, 0).unwrap()),
            Some(12 * 60 * 60)
        );
    }
}
