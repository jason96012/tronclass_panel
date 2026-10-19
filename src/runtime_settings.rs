//! Runtime-tunable settings controlled from the Web admin panel.
//!
//! PERSISTED_RUNTIME_SETTINGS_V1（fix104）：這些設定**持久化在 `config/runtime_settings.json`** ——
//! 從 `/admin` 改完即時生效並原子寫檔，服務重啟後沿用上次存的值；要回預設請按面板的「恢復預設」。
//! 以前是純記憶體（重啟＝全部回預設），使用者回報：
//! 「我在設定所儲存的送出節奏不要因為重啟就重置」。

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveTime, Weekday};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// GROUP_SUBMIT_RELAY_V1（fix67）：多帳號送出間隔的**基準**（毫秒）。
///
/// 同一場點名由多個帳號送出時，第一位立即送出，之後每位間隔
/// `base + U(0, jitter)`；游標只前進不後退，因此兩人至少間隔 `base`。
pub const DEFAULT_GROUP_SUBMIT_INTERVAL_MS: u64 = 500;
/// GROUP_SUBMIT_RELAY_V1（fix67）：送出間隔的**隨機上限**（毫秒，預設 500）。
///
/// 0 ＝ 完全固定間隔（fix67 之前的行為）。
pub const DEFAULT_GROUP_SUBMIT_JITTER_MS: u64 = 500;
/// 隨機上限的硬上限（面板／API 都不能超過）。
pub const MAX_GROUP_SUBMIT_JITTER_MS: u64 = 5000;
/// QR_FRESH_PAYLOAD_V1（fix68）：QR Code 點名的送出間隔**基準**（毫秒）。
pub const DEFAULT_GROUP_SUBMIT_QRCODE_INTERVAL_MS: u64 = 500;
/// QR_FRESH_PAYLOAD_V1（fix68）：QR Code 點名的送出間隔**隨機上限**（毫秒）。
///
/// 比數字點名小：課堂 QR 會輪換，等太久只是白白送出過期的碼
/// （送出前會再抓一次最新 payload，這裡控制的是帳號之間的間距）。
pub const DEFAULT_GROUP_SUBMIT_QRCODE_JITTER_MS: u64 = 500;
/// GROUP_SUBMIT_RELAY_V1（fix67）：**雷達點名**的送出間隔基準（毫秒）。
///
/// 雷達也要跟著「每位間隔」走：同一個 provider 的同一場點名，多個帳號各自要送出座標時，
/// 第一位立即送出，之後每位間隔 `base + U(0, jitter)`。預設與數字點名相同（500）。
pub const DEFAULT_GROUP_SUBMIT_RADAR_INTERVAL_MS: u64 = 500;
/// GROUP_SUBMIT_RELAY_V1（fix67）：**雷達點名**送出間隔的隨機上限（毫秒，預設 500）。
pub const DEFAULT_GROUP_SUBMIT_RADAR_JITTER_MS: u64 = 500;
/// 基準的硬上限（沿用既有 10 秒上限）。
pub const MAX_GROUP_SUBMIT_INTERVAL_MS: u64 = 10_000;
pub const MAX_RUNTIME_REST_PERIODS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]   // 讀檔時缺欄位＝用 Default（送出節奏的預設是 500/500…，不會變成 0）
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
    /// QR_FRESH_PAYLOAD_V1（fix68）：QR Code 點名的送出間隔基準（毫秒）。
    #[serde(default = "default_group_submit_qrcode_interval_ms")]
    pub group_submit_qrcode_interval_ms: u64,
    /// QR_FRESH_PAYLOAD_V1（fix68）：QR Code 點名的送出間隔隨機上限（毫秒）。
    #[serde(default = "default_group_submit_qrcode_jitter_ms")]
    pub group_submit_qrcode_jitter_ms: u64,
    /// GROUP_SUBMIT_RELAY_V1（fix67）：**雷達點名**的送出間隔基準（毫秒）。
    #[serde(default = "default_group_submit_radar_interval_ms")]
    pub group_submit_radar_interval_ms: u64,
    /// GROUP_SUBMIT_RELAY_V1（fix67）：**雷達點名**送出間隔的隨機上限（毫秒）。
    #[serde(default = "default_group_submit_radar_jitter_ms")]
    pub group_submit_radar_jitter_ms: u64,
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

/// serde 預設值：舊的設定檔／請求沒有 QR 欄位時，補上 QR 專屬預設。
fn default_group_submit_qrcode_interval_ms() -> u64 {
    DEFAULT_GROUP_SUBMIT_QRCODE_INTERVAL_MS
}

fn default_group_submit_qrcode_jitter_ms() -> u64 {
    DEFAULT_GROUP_SUBMIT_QRCODE_JITTER_MS
}

/// serde 預設值：舊的設定檔／請求沒有雷達欄位時，補上雷達預設（與數字點名相同）。
fn default_group_submit_radar_interval_ms() -> u64 {
    DEFAULT_GROUP_SUBMIT_RADAR_INTERVAL_MS
}

fn default_group_submit_radar_jitter_ms() -> u64 {
    DEFAULT_GROUP_SUBMIT_RADAR_JITTER_MS
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            poll_interval_secs: None,
            group_submit_interval_ms: DEFAULT_GROUP_SUBMIT_INTERVAL_MS,
            group_submit_jitter_ms: DEFAULT_GROUP_SUBMIT_JITTER_MS,
            group_submit_qrcode_interval_ms: DEFAULT_GROUP_SUBMIT_QRCODE_INTERVAL_MS,
            group_submit_qrcode_jitter_ms: DEFAULT_GROUP_SUBMIT_QRCODE_JITTER_MS,
            group_submit_radar_interval_ms: DEFAULT_GROUP_SUBMIT_RADAR_INTERVAL_MS,
            group_submit_radar_jitter_ms: DEFAULT_GROUP_SUBMIT_RADAR_JITTER_MS,
            auto_stop_time: None,
            rest_periods: Vec::new(),
            semester_override: None,
        }
    }
}

/// PERSISTED_RUNTIME_SETTINGS_V1（fix104）：設定檔位置。
///
/// 與其他小型設定檔（`config/digest.json`、`config/ai_chat.json`…）同一個目錄，
/// 走同一套 [`crate::atomic_write`] 原子寫入。
fn settings_file() -> PathBuf {
    std::path::Path::new("config/runtime_settings.json").to_path_buf()
}

/// 把設定檔的 JSON 文字轉成 [`RuntimeSettings`]：
/// * 缺欄位 → 該欄位的預設（`#[serde(default)]` 用 [`RuntimeSettings::default`]；
///   送出節奏的預設是 500/500…，**不會變成 0** 而意外讓大家同時送出）
/// * 超界值 → [`normalize`] 夾回硬上限（手改檔也不能繞過）
/// * 整份壞掉 → 全部回預設（不讓半截 JSON 讓服務起不來）
fn parse_persisted(text: &str) -> RuntimeSettings {
    serde_json::from_str::<RuntimeSettings>(text)
        .map(normalize)
        .unwrap_or_default()
}

/// 讀持久化設定（缺檔／讀不到＝用預設）。啟動時呼叫一次。
fn load_persisted() -> RuntimeSettings {
    match std::fs::read_to_string(settings_file()) {
        Ok(text) => parse_persisted(&text),
        Err(_) => RuntimeSettings::default(),
    }
}

/// 啟動時載入持久化設定（以前是 `RuntimeSettings::default()` → 重啟就把面板存的設定打回預設）。
static SETTINGS: Lazy<RwLock<RuntimeSettings>> = Lazy::new(|| {
    let loaded = load_persisted();
    /* CURRENT_SEMESTER_OVERRIDE_V1：學期覆蓋的快取也必須用「載入後的值」算一次 ——
       以前只有 update() 會刷新，所以重啟後那個快取會是空的（覆蓋看起來失效）。 */
    refresh_semester_override_cache(loaded.semester_override.as_deref());
    tracing::debug!(
        path = %settings_file().display(),
        interval_ms = loaded.group_submit_interval_ms,
        jitter_ms = loaded.group_submit_jitter_ms,
        "執行期設定已載入（持久化）"
    );
    RwLock::new(loaded)
});

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

/// 更新設定：夾範圍 → **先讓記憶體生效** → 原子寫檔；回傳寫檔結果。
///
/// 為什麼是這個順序：面板的操作不該因為寫檔失敗就當作沒發生（掃描中的那一輪需要它立刻生效），
/// 但寫檔失敗**一定要回報** —— 否則又會變成「存了、重啟後卻不見」的那個老問題。
pub async fn update(settings: RuntimeSettings) -> miette::Result<()> {
    let settings = normalize(settings);
    refresh_semester_override_cache(settings.semester_override.as_deref());
    *SETTINGS.write().await = settings.clone();
    let text = serde_json::to_string_pretty(&settings)
        .map_err(|e| miette::miette!("序列化執行期設定失敗（這次的設定重啟後不會保留）：{e}"))?;
    // ATOMIC_WRITE_V1：write-to-temp → fsync → rename（不會留下半截 JSON）。
    crate::atomic_write::write_atomic(settings_file(), text).map_err(|e| {
        miette::miette!("寫入 config/runtime_settings.json 失敗（這次的設定重啟後不會保留）：{e}")
    })?;
    Ok(())
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

/// QR_FRESH_PAYLOAD_V1（fix68）：QR Code 點名的送出間隔基準（毫秒）。
pub async fn group_submit_qrcode_interval_ms() -> u64 {
    SETTINGS.read().await.group_submit_qrcode_interval_ms
}

/// QR_FRESH_PAYLOAD_V1（fix68）：QR Code 點名的送出間隔隨機上限（毫秒）。
pub async fn group_submit_qrcode_jitter_ms() -> u64 {
    SETTINGS.read().await.group_submit_qrcode_jitter_ms
}

/// GROUP_SUBMIT_RELAY_V1（fix67）：雷達點名的送出間隔基準（毫秒）。
pub async fn group_submit_radar_interval_ms() -> u64 {
    SETTINGS.read().await.group_submit_radar_interval_ms
}

/// GROUP_SUBMIT_RELAY_V1（fix67）：雷達點名送出間隔的隨機上限（毫秒）。
pub async fn group_submit_radar_jitter_ms() -> u64 {
    SETTINGS.read().await.group_submit_radar_jitter_ms
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
    settings.group_submit_qrcode_interval_ms = settings
        .group_submit_qrcode_interval_ms
        .min(MAX_GROUP_SUBMIT_INTERVAL_MS);
    settings.group_submit_qrcode_jitter_ms = settings
        .group_submit_qrcode_jitter_ms
        .min(MAX_GROUP_SUBMIT_JITTER_MS);
    settings.group_submit_radar_interval_ms = settings
        .group_submit_radar_interval_ms
        .min(MAX_GROUP_SUBMIT_INTERVAL_MS);
    settings.group_submit_radar_jitter_ms = settings
        .group_submit_radar_jitter_ms
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

    /* ─────────── PERSISTED_RUNTIME_SETTINGS_V1（fix104）：設定檔往返 ─────────── */

    /// 序列化 → 解析之後每一個欄位都要一模一樣（不然重啟後會悄悄換一組節奏）。
    #[test]
    fn persisted_settings_roundtrip_keeps_every_field() {
        let settings = RuntimeSettings {
            poll_interval_secs: Some(45),
            group_submit_interval_ms: 1_200,
            group_submit_jitter_ms: 300,
            group_submit_qrcode_interval_ms: 800,
            group_submit_qrcode_jitter_ms: 200,
            group_submit_radar_interval_ms: 900,
            group_submit_radar_jitter_ms: 1_500,
            auto_stop_time: Some("23:30".to_string()),
            rest_periods: vec![RuntimeRestPeriod {
                weekday: RuntimeWeekday::Fri,
                start_time: "23:00".to_string(),
                end_time: "07:00".to_string(),
            }],
            semester_override: Some("1151".to_string()),
        };
        let text = serde_json::to_string_pretty(&settings).unwrap();
        let back = parse_persisted(&text);
        assert_eq!(back.poll_interval_secs, Some(45));
        assert_eq!(back.group_submit_interval_ms, 1_200);
        assert_eq!(back.group_submit_jitter_ms, 300);
        assert_eq!(back.group_submit_qrcode_interval_ms, 800);
        assert_eq!(back.group_submit_qrcode_jitter_ms, 200);
        assert_eq!(back.group_submit_radar_interval_ms, 900);
        assert_eq!(back.group_submit_radar_jitter_ms, 1_500);
        assert_eq!(back.auto_stop_time.as_deref(), Some("23:30"));
        assert_eq!(back.rest_periods.len(), 1);
        assert_eq!(back.semester_override.as_deref(), Some("1151"));
    }

    /// 缺欄位（舊版設定檔、手寫的檔）＝用預設，**不是 0** —— 0 會變成「不等待、全部同時送出」。
    #[test]
    fn persisted_settings_missing_fields_fall_back_to_defaults() {
        let back = parse_persisted("{}");
        assert_eq!(back.group_submit_interval_ms, DEFAULT_GROUP_SUBMIT_INTERVAL_MS);
        assert_eq!(back.group_submit_jitter_ms, DEFAULT_GROUP_SUBMIT_JITTER_MS);
        assert_eq!(
            back.group_submit_qrcode_jitter_ms,
            DEFAULT_GROUP_SUBMIT_QRCODE_JITTER_MS
        );
        assert_eq!(
            back.group_submit_radar_interval_ms,
            DEFAULT_GROUP_SUBMIT_RADAR_INTERVAL_MS
        );
        assert_eq!(back.poll_interval_secs, None);
        assert!(back.rest_periods.is_empty());
        assert_eq!(back.semester_override, None);
    }

    /// 壞檔＝整份回預設（服務照樣起得來，只是回到預設節奏）。
    #[test]
    fn persisted_settings_garbage_falls_back_to_defaults() {
        let back = parse_persisted("{ this is not json");
        assert_eq!(back.group_submit_interval_ms, DEFAULT_GROUP_SUBMIT_INTERVAL_MS);
        assert_eq!(back.group_submit_jitter_ms, DEFAULT_GROUP_SUBMIT_JITTER_MS);
    }

    /// 手改檔塞超界值也要被夾回來（不能靠改檔案繞過硬上限）。
    #[test]
    fn persisted_settings_are_clamped() {
        let back = parse_persisted(
            r#"{"group_submit_interval_ms": 999999, "group_submit_jitter_ms": 999999,
                "auto_stop_time": "25:99", "semester_override": "not-a-semester"}"#,
        );
        assert_eq!(back.group_submit_interval_ms, MAX_GROUP_SUBMIT_INTERVAL_MS);
        assert_eq!(back.group_submit_jitter_ms, MAX_GROUP_SUBMIT_JITTER_MS);
        assert_eq!(back.auto_stop_time, None);
        assert_eq!(back.semester_override, None);
    }
}
