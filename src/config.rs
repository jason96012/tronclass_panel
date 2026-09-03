//! 設定管理模組

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{NaiveTime, Weekday};
use miette::{IntoDiagnostic, Result, WrapErr};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// 全域設定單例
static CONFIG: OnceCell<AppConfig> = OnceCell::new();

/// 取得全域設定（必須先呼叫 `init_config` 或 `set_global_config`）
///
/// # Panics
/// 全域設定尚未初始化時 panic。Web handler 等「不該讓整條連線斷掉」的情境，
/// 請改用 [`try_get_config`]，否則 panic 會讓 axum 直接中斷連線，
/// 反向代理（Caddy / Cloudflare）會回 502 給使用者。
pub fn get_config() -> &'static AppConfig {
    CONFIG
        .get()
        .expect("Config not initialized. Call init_config() first.")
}

/// 取得全域設定；尚未初始化時回傳 `None` 而不是 panic。
pub fn try_get_config() -> Option<&'static AppConfig> {
    CONFIG.get()
}

/// 以既有的 `AppConfig` 填入全域單例。
///
/// `main` 是用 [`AppConfig::load`] 讀設定（而非 [`init_config`]），
/// 所以必須另外呼叫這個函式，否則 [`get_config`] 會 panic。
/// 重複呼叫是安全的：第一次以後直接忽略。
pub fn set_global_config(config: AppConfig) -> &'static AppConfig {
    let _ = CONFIG.set(config);
    CONFIG.get().expect("CONFIG was just set")
}

/// 從指定路徑初始化全域設定
pub fn init_config(path: impl AsRef<Path>) -> Result<&'static AppConfig> {
    let cfg = AppConfig::load(path)?;
    CONFIG
        .set(cfg)
        .map_err(|_| miette::miette!("Config already initialized"))?;
    Ok(CONFIG.get().unwrap())
}

// ─── 根設定 ───────────────────────────────────────────────────────────────────
/// 應用程式全域設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    /// Providers (學校/Tronclass 系統) 設定
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Adapters 設定
    #[serde(default)]
    pub adapters: AdapterConfig,
    /// 時區設定
    #[serde(default)]
    pub time: TimeConfig,
    /// 學期判定設定
    #[serde(default)]
    pub semester: SemesterConfig,
    /// 作業截止追蹤設定
    #[serde(default)]
    pub homework: HomeworkConfig,
    /// 日誌設定
    pub logging: LoggingConfig,
    /// 監控設定
    pub monitor: MonitorConfig,
}

// ─── 子設定結構 ───────────────────────────────────────────────────────────────

// ======== Provider ========
/// Provider 設定結構
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    /// Tronclass API (學校系統) Base URL
    #[serde(default)]
    pub base_url: String,
    /// API 設定
    pub api: ApiConfig,
    /// 雷達簽到設定
    pub radar: RadarConfig,
    /// 數字爆破設定
    pub brute_force: BruteForceConfig,
    /// QR Code 設定
    pub qrcode: QrCodeConfig,
    /// 課堂時段與輪詢時間設定
    #[serde(default)]
    pub schedule: PollingScheduleConfig,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::default(),
            base_url: default_base_url(),
            api: ApiConfig {
                base_url: default_base_url(),
                poll_interval_secs: default_poll_interval(),
                request_timeout_secs: default_request_timeout(),
            },
            radar: RadarConfig {
                default_coords: default_coords(),
                accuracy: default_accuracy(),
                altitude: 0,
            },
            brute_force: BruteForceConfig {
                concurrency: default_concurrency(),
                request_delay_ms: 0,
                transient_failure_threshold: default_transient_failure_threshold(),
                transient_failure_ratio: default_transient_failure_ratio(),
                cooldown_secs: default_cooldown_secs(),
                max_cooldowns: default_max_cooldowns(),
                min_concurrency: default_min_concurrency(),
            },
            qrcode: QrCodeConfig {
                scanner_base_url: default_scanner_base_url(),
                scan_timeout_secs: default_scan_timeout(),
            },
            schedule: PollingScheduleConfig::default(),
        }
    }
}

impl ProviderConfig {
    fn apply_kind_defaults(&mut self) {
        if self.kind == ProviderKind::TKU {
            // Omitted fields in a partially specified TOML table deserialize as empty strings.
            if self.base_url.trim().is_empty() || self.base_url == default_base_url() {
                self.base_url = default_tku_base_url();
            }
            if self.api.base_url.trim().is_empty() || self.api.base_url == default_base_url() {
                self.api.base_url = self.base_url.clone();
            }
            if self.qrcode.scanner_base_url.trim().is_empty()
                || self.qrcode.scanner_base_url == default_scanner_base_url()
            {
                self.qrcode.scanner_base_url =
                    format!("{}/scanner-jumper", self.base_url.trim_end_matches('/'));
            }
        }
        if self.schedule.periods.is_empty() && self.kind == ProviderKind::FJU {
            self.schedule.periods = default_fju_schedule_periods();
        }
        if self.schedule.rest_weekdays.is_empty() && self.kind == ProviderKind::FJU {
            self.schedule.rest_weekdays = vec![ScheduleWeekday::Sun];
        }
    }
}

/// Provider 種類
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Tronclass 原生系統
    Tronclass,
    /// 輔仁大學系統（接受 "fju" 或 "f_j_u"）
    #[serde(alias = "fju")]
    FJU,
    /// 淡江大學系統（接受 "tku" 或 "t_k_u"）
    #[serde(alias = "tku")]
    TKU,
}

impl Default for ProviderKind {
    fn default() -> Self {
        Self::Tronclass
    }
}

/// API 設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiConfig {
    /// Tronclass 基礎 URL
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// 輪詢間隔（秒）
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// 請求逾時（秒）
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
}

/// 雷達簽到設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RadarConfig {
    /// 預設雷達點名座標陣列
    #[serde(default = "default_coords")]
    pub default_coords: Vec<[f64; 2]>,
    /// 精確度
    #[serde(default = "default_accuracy")]
    pub accuracy: u32,
    /// 海拔高度
    #[serde(default)]
    pub altitude: i32,
}

/// 數字爆破設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BruteForceConfig {
    /// 最大並行數
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// 請求延遲（毫秒）
    #[serde(default)]
    pub request_delay_ms: u64,
    /// 自上次冷卻後可容忍的異常失敗次數
    #[serde(default = "default_transient_failure_threshold")]
    pub transient_failure_threshold: usize,
    /// 單批異常失敗比例門檻（0.0 < ratio <= 1.0）
    #[serde(default = "default_transient_failure_ratio")]
    pub transient_failure_ratio: f32,
    /// 異常失敗觸發後的冷卻時間（秒）
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    /// 最大冷卻次數；0 表示達門檻時立即失敗
    #[serde(default = "default_max_cooldowns")]
    pub max_cooldowns: usize,
    /// 動態降速後的最低並行數
    #[serde(default = "default_min_concurrency")]
    pub min_concurrency: usize,
}

/// QR Code 簽到設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QrCodeConfig {
    #[serde(default = "default_scanner_base_url")]
    pub scanner_base_url: String,

    #[serde(default = "default_scan_timeout")]
    pub scan_timeout_secs: u64,
}

/// Provider 輪詢時段設定
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PollingScheduleConfig {
    /// 允許輪詢的課堂時段
    #[serde(default)]
    pub periods: Vec<String>,
    /// 休息日；這些星期不會進行自動輪詢
    #[serde(default)]
    pub rest_weekdays: Vec<ScheduleWeekday>,
}

impl PollingScheduleConfig {
    pub fn is_configured(&self) -> bool {
        !self.periods.is_empty()
    }

    pub fn is_rest_day(&self, weekday: Weekday) -> bool {
        self.rest_weekdays.iter().any(|day| day.matches(weekday))
    }
}

/// 星期設定（用於課表休息日）
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleWeekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl ScheduleWeekday {
    pub fn matches(self, weekday: Weekday) -> bool {
        matches!(
            (self, weekday),
            (ScheduleWeekday::Mon, Weekday::Mon)
                | (ScheduleWeekday::Tue, Weekday::Tue)
                | (ScheduleWeekday::Wed, Weekday::Wed)
                | (ScheduleWeekday::Thu, Weekday::Thu)
                | (ScheduleWeekday::Fri, Weekday::Fri)
                | (ScheduleWeekday::Sat, Weekday::Sat)
                | (ScheduleWeekday::Sun, Weekday::Sun)
        )
    }
}

// ======== Adapter ========
/// Adapter 設定結構
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AdapterConfig {
    #[serde(default)]
    pub line_bot: LineBotConfig,
    #[serde(default)]
    pub discord: DiscordBotConfig,
    #[serde(default)]
    pub admin_web: AdminWebConfig,
}

/// Line Bot 設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LineBotConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    pub channel_secret: String,

    #[serde(default)]
    pub channel_access_token: String,

    #[serde(default = "default_webhook_port")]
    pub webhook_port: u16,

    /// Origin bind address. Keep the default loopback-only when using a
    /// Cloudflare Tunnel; set 0.0.0.0 explicitly only for a trusted direct
    /// webhook deployment.
    #[serde(default = "default_webhook_bind_addr")]
    pub webhook_bind_addr: String,

    #[serde(default = "default_webhook_path")]
    pub webhook_path: String,

    #[serde(default)]
    pub public_base_url: String,

    #[serde(default)]
    pub admin_user_id: String,
}

impl Default for LineBotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel_secret: String::new(),
            channel_access_token: String::new(),
            webhook_port: default_webhook_port(),
            webhook_bind_addr: default_webhook_bind_addr(),
            webhook_path: default_webhook_path(),
            public_base_url: String::new(),
            admin_user_id: String::new(),
        }
    }
}

/// Discord Bot 設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiscordBotConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    pub bot_token: String,

    #[serde(default)]
    pub admin_user_id: String,

    #[serde(default)]
    pub admin_channel_id: String,

    #[serde(default)]
    pub public_base_url: String,

    #[serde(default = "default_register_commands")]
    pub register_commands: bool,

    #[serde(default)]
    pub guild_ids: Vec<String>,
}

impl Default for DiscordBotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            admin_user_id: String::new(),
            admin_channel_id: String::new(),
            public_base_url: String::new(),
            register_commands: default_register_commands(),
            guild_ids: vec![],
        }
    }
}

/// Web admin panel settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdminWebConfig {
    #[serde(default)]
    pub enabled: bool,

    /// Deprecated legacy field retained only so existing config files load.
    /// It is intentionally not accepted as a Dashboard login credential.
    #[serde(default)]
    pub access_token: String,

    /// Reverse proxies whose forwarded client-IP headers are trusted for rate
    /// limiting and blacklisting. Each entry is an exact IP, a CIDR block, or
    /// the keyword `cloudflare` / `loopback`. Loopback is always trusted.
    /// Requests from any other source use the real peer address, so a public
    /// origin cannot forge `X-Forwarded-For` to bypass limits or poison the
    /// blacklist.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

impl Default for AdminWebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            access_token: String::new(),
            trusted_proxies: Vec::new(),
        }
    }
}

// ======== Time ======
/// 時區設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TimeConfig {
    /// 執行時區；可設為 `local`、`UTC` 或 IANA 時區名稱（例如 `Asia/Taipei`）
    #[serde(default = "default_timezone")]
    pub timezone: String,
}

impl Default for TimeConfig {
    fn default() -> Self {
        Self {
            timezone: default_timezone(),
        }
    }
}

// ======== Semester ========
/// 學期判定設定（CURRENT_SEMESTER_OVERRIDE_V1）。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SemesterConfig {
    /// 手動覆蓋「當前學期」，支援格式：
    /// - 民國學年＋學期：`"1151"`、`"115-1"`、`"115上"`、`"115-2"`、`"115下"`
    /// - 西元年＋學期：`"2026-1"`、`"2026 Fall"`、`"2025-2"`、`"2025 Spring"`
    ///
    /// 未設定或留空＝依台北日期自動推算（8 月起為上學期、2 月起為下學期）。
    /// 格式無效時記一次 warn 後忽略（退回自動推算）。
    ///
    /// 同時影響三處學期判斷：
    /// 1. 作業頁的學期起點過濾（`api::todos`）
    /// 2. 課程學期碼是否視為「當前」（`api::todos`）
    /// 3. 淡江課表同步的 YrSem 代碼（`timetable_source::tku`）
    ///
    /// 使用時機：跨學期交界學校資料尚未更新（例：2 月課表仍列上學期）、
    /// 或學校學期編碼與日期推算不一致時，手動釘選實際學期。
    #[serde(default)]
    pub current: Option<String>,
}

impl SemesterConfig {
    /// 將 `current` 解析為 `(西元年, 學期 1|2)`。
    ///
    /// 未設定或空字串回 `None`（使用自動推算）；格式無效也回 `None`，
    /// 但只 warn 一次，避免每輪輪詢洗版日誌。
    pub(crate) fn resolved(&self) -> Option<(i32, u32)> {
        let text = self.current.as_deref()?.trim();
        if text.is_empty() {
            return None;
        }
        match parse_semester_current(text) {
            Some(found) => Some(found),
            None => {
                static WARNED: OnceCell<()> = OnceCell::new();
                WARNED.get_or_init(|| {
                    warn!(
                        value = %text,
                        "semester.current 格式無效，忽略覆蓋（可用例：1151、115-2、115上、2026-1、2026 Fall）"
                    );
                });
                None
            }
        }
    }
}

/// 解析學期字串為 `(西元年, 學期 1|2)`；無法判讀回 `None`。
///
/// 與 `api::todos::semester_text_is_current` 支援的格式一致：
/// - 前 3 碼為民國學年（100–199）＋第 4 碼學期（1/2），如 `"1151"`、`"115-2"`
/// - 前 4 碼為西元年（2000–2099）＋第 5 碼學期，如 `"2026-1"`
/// - 學期也可用文字標記：`上`/`Fall` → 1、`下`/`Spring` → 2，如 `"115上"`、`"2026 Fall"`
/// - 只給年份而無法判讀學期視為無效（學期決定起點月份，不可用猜的）
pub(crate) fn parse_semester_current(text: &str) -> Option<(i32, u32)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    let lower = text.to_ascii_lowercase();
    let term_from_text = || {
        if lower.contains("fall") || lower.contains('上') {
            Some(1)
        } else if lower.contains("spring") || lower.contains('下') {
            Some(2)
        } else {
            None
        }
    };

    // 民國學年：前 3 碼 100–199。
    if let Some(roc_year) = digits.get(..3).and_then(|s| s.parse::<i32>().ok()) {
        if (100..200).contains(&roc_year) {
            let term = digits
                .get(3..4)
                .and_then(|s| s.parse::<u32>().ok())
                .or_else(term_from_text)?;
            if term == 1 || term == 2 {
                return Some((roc_year + 1911, term));
            }
            return None;
        }
    }

    // 西元年：前 4 碼 2000–2099。
    if let Some(western_year) = digits.get(..4).and_then(|s| s.parse::<i32>().ok()) {
        if (2000..2100).contains(&western_year) {
            let term = digits
                .get(4..5)
                .and_then(|s| s.parse::<u32>().ok())
                .or_else(term_from_text)?;
            if term == 1 || term == 2 {
                return Some((western_year, term));
            }
            return None;
        }
    }

    None
}

/// 全域捷徑：目前生效的學期覆蓋。
///
/// 優先順序：網頁設定頁的執行期覆蓋（RUNTIME_SEMESTER_OVERRIDE_V1，
/// 即時生效、重啟後清除）＞ config.toml `[semester] current` ＞ 依日期自動推算。
/// 設定尚未初始化（測試情境）或皆未設定時回 `None`，呼叫端退回自動推算。
pub(crate) fn semester_override() -> Option<(i32, u32)> {
    crate::runtime_settings::semester_override()
        .or_else(|| try_get_config().and_then(|config| config.semester.resolved()))
}

// ======== Homework ========
/// 作業截止追蹤設定。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HomeworkConfig {
    /// 是否放寬為「包含所有學期」的作業/測驗。
    ///
    /// 預設 `false`：作業頁只顯示「當學期」的項目（正式使用時的預期行為，
    /// 讓作業頁與截止提醒維持乾淨的被定義內容）。
    ///
    /// 設為 `true` 可「暫時放寬」以供測試：所有學期的作業與測驗
    /// （含上學期、已過期）都會出現在作業頁的「全部 / 已逾期」分頁，
    /// 方便確認測驗（`type=exam`，來自 `/api/courses/{id}/exams`）確實被解析、
    /// 圖示顯示「🧪 測驗/考試」是否正確。
    ///
    /// 注意：已過期的項目**不會**觸發 24h/6h/1h 截止提醒（那需要 `end_time` 仍在未來）。
    /// 過期項目只會顯示在「全部 / 已逾期」分頁，提醒仍以未來截止者為準。
    #[serde(default)]
    pub include_all_semesters: bool,

    /// STRICT_CURRENT_SEMESTER_V1：嚴格模式——「無法判定為當前學期」的作業不顯示。
    ///
    /// 預設 `true`（作業頁維持乾淨）：除了既有的兩道防線外，再加第三道——
    /// 沒有截止時間、且所屬課程無法判定為「當前學期」（`is_current != Some(true)`，
    /// 含無法判定 `None` 與舊課 `Some(false)`）的作業一律隱藏。
    /// 有截止時間者仍以學期起點為界（不變）。
    ///
    /// 設為 `false` 回到「寧可多顯示」：無法判定＝保留（舊行為，避免誤刪
    /// 長期開放、無截止時間的作業）。
    ///
    /// 注意：修課清單（/api/my-courses）抓取失敗時不套用嚴格過濾
    /// （沒有課程資料可判斷，避免整頁清空）。
    #[serde(default = "default_strict_current_semester")]
    pub strict_current_semester: bool,
}

fn default_strict_current_semester() -> bool {
    true
}

impl Default for HomeworkConfig {
    fn default() -> Self {
        Self {
            include_all_semesters: false,
            strict_current_semester: default_strict_current_semester(),
        }
    }
}

// ======== Logging ========
/// 日誌設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,

    #[serde(default)]
    pub log_to_file: bool,

    #[serde(default = "default_log_file_path")]
    pub log_file_path: String,
}

// ======== Monitor ========
/// 監控設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MonitorConfig {
    #[serde(default = "default_startup_delay")]
    pub startup_delay_secs: u64,

    #[serde(default = "default_retry_interval")]
    pub retry_interval_secs: u64,

    #[serde(default = "default_max_failures")]
    pub max_failures_before_reauth: u32,
}

// ─── 預設值 ───────────────────────────────────────────────────────────────────
fn default_base_url() -> String {
    "https://www.tronclass.com".to_string()
}
fn default_tku_base_url() -> String {
    "https://iclass.tku.edu.tw".to_string()
}
fn default_poll_interval() -> u64 {
    10
}
fn default_request_timeout() -> u64 {
    30
}
fn default_coords() -> Vec<[f64; 2]> {
    vec![[24.3, 118.0], [24.6, 118.2]]
}
fn default_accuracy() -> u32 {
    35
}
fn default_concurrency() -> usize {
    200
}
fn default_transient_failure_threshold() -> usize {
    50
}
fn default_transient_failure_ratio() -> f32 {
    0.20
}
fn default_cooldown_secs() -> u64 {
    10
}
fn default_max_cooldowns() -> usize {
    3
}
fn default_min_concurrency() -> usize {
    10
}
fn default_scanner_base_url() -> String {
    "https://elearn2.fju.edu.tw/scanner-jumper".to_string()
}
fn default_scan_timeout() -> u64 {
    60
}
fn default_fju_schedule_periods() -> Vec<String> {
    vec!["07:10~17:30".to_string()]
}
fn default_webhook_port() -> u16 {
    8080
}
fn default_webhook_bind_addr() -> String {
    "127.0.0.1".to_string()
}
fn default_webhook_path() -> String {
    "/webhook".to_string()
}
fn default_register_commands() -> bool {
    true
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_timezone() -> String {
    "local".to_string()
}
fn default_log_file_path() -> String {
    "tronclass_rollcall.log".to_string()
}
fn default_startup_delay() -> u64 {
    3
}
fn default_retry_interval() -> u64 {
    30
}
fn default_max_failures() -> u32 {
    5
}

// ─── AppConfig 實作 ───────────────────────────────────────────────────────────────────
impl AppConfig {
pub fn load(path: impl AsRef<Path>) -> Result<Self> {
    let path = path.as_ref();

    let cfg = config::Config::builder()
        .add_source(
            config::File::from(path)
                .required(true)
                .format(config::FileFormat::Toml),
        )
        .add_source(
            config::Environment::with_prefix("TRONCLASS_ROLLCALL")
                .separator("__")
                .try_parsing(true),
        )
        .build()
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to load config from `{}`", path.display()))?;

    let mut app = cfg
        .try_deserialize::<AppConfig>()
        .into_diagnostic()
        .wrap_err("Failed to deserialize config")?;

    for provider in app.providers.values_mut() {
        provider.apply_kind_defaults();
    }

    // AUTO_GLOBAL_CONFIG_V1
    // Loading a config is the single canonical point through which a
    // configuration enters the process (main, init, tests). Filling the global
    // `OnceCell` right here removes the footgun where callers had to remember
    // `set_global_config()` after loading, or `get_config()` would panic. Now
    // any code that needs the settings can safely rely on `try_get_config()`
    // once a config has been loaded anywhere.
    set_global_config(app.clone());

    Ok(app)
}

    pub fn load_default() -> Result<Self> {
        Self::load("config.toml")
    }

    pub fn validate(&self) -> Result<()> {
        if self.providers.is_empty() {
            return Err(miette::miette!("至少需要一個 provider"));
        }

        for (name, p) in &self.providers {
            if p.api.base_url.is_empty() {
                return Err(miette::miette!("providers.{name}.api.base_url 不可為空"));
            }
            if p.api.poll_interval_secs == 0 {
                return Err(miette::miette!(
                    "providers.{name}.api.poll_interval_secs 必須大於 0"
                ));
            }
            if p.api.request_timeout_secs == 0 {
                return Err(miette::miette!(
                    "providers.{name}.api.request_timeout_secs 必須大於 0"
                ));
            }
            validate_provider_schedule(name, &p.schedule)?;
            if p.brute_force.concurrency == 0 {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.concurrency 必須大於 0"
                ));
            }
            if p.brute_force.transient_failure_threshold == 0 {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.transient_failure_threshold 必須大於 0"
                ));
            }
            if !(0.0..=1.0).contains(&p.brute_force.transient_failure_ratio)
                || p.brute_force.transient_failure_ratio == 0.0
            {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.transient_failure_ratio 必須大於 0 且小於等於 1"
                ));
            }
            if p.brute_force.max_cooldowns > 0 && p.brute_force.cooldown_secs == 0 {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.cooldown_secs 在啟用冷卻時必須大於 0"
                ));
            }
            if p.brute_force.min_concurrency == 0 {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.min_concurrency 必須大於 0"
                ));
            }
            if p.brute_force.min_concurrency > p.brute_force.concurrency {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.min_concurrency 不可大於 concurrency"
                ));
            }
            for (i, coords) in p.radar.default_coords.iter().enumerate() {
                let [lat, lon] = coords;
                if !(-90.0..=90.0).contains(lat) {
                    return Err(miette::miette!(
                        "providers.{name}.radar.default_coords[{i}] 緯度 {lat} 超出範圍（-90 ~ 90）"
                    ));
                }
                if !(-180.0..=180.0).contains(lon) {
                    return Err(miette::miette!(
                        "providers.{name}.radar.default_coords[{i}] 經度 {lon} 超出範圍（-180 ~ 180）"
                    ));
                }
            }
        }

        if self.adapters.line_bot.enabled {
            if self.adapters.line_bot.channel_secret.is_empty() {
                return Err(miette::miette!(
                    "adapters.line_bot.channel_secret 不可為空（line_bot.enabled = true）"
                ));
            }
            if self.adapters.line_bot.channel_access_token.is_empty() {
                return Err(miette::miette!(
                    "adapters.line_bot.channel_access_token 不可為空（line_bot.enabled = true）"
                ));
            }
            if self.adapters.line_bot.admin_user_id.is_empty() {
                return Err(miette::miette!(
                    "adapters.line_bot.admin_user_id 不可為空（line_bot.enabled = true）"
                ));
            }
        }

        if !self.adapters.line_bot.public_base_url.trim().is_empty() {
            let parsed =
                url::Url::parse(self.adapters.line_bot.public_base_url.trim()).map_err(|_| {
                    miette::miette!("adapters.line_bot.public_base_url 必須是有效的 http(s) URL")
                })?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err(miette::miette!(
                    "adapters.line_bot.public_base_url 必須是有效的 http(s) URL"
                ));
            }
        }
        if self
            .adapters
            .line_bot
            .webhook_bind_addr
            .trim()
            .parse::<std::net::IpAddr>()
            .is_err()
        {
            return Err(miette::miette!(
                "adapters.line_bot.webhook_bind_addr 必須是有效的 IP 位址（建議 Cloudflare Tunnel 使用 127.0.0.1）"
            ));
        }

        if self.adapters.discord.enabled {
            if self.adapters.discord.bot_token.trim().is_empty() {
                return Err(miette::miette!(
                    "adapters.discord.bot_token 不可為空（discord.enabled = true）"
                ));
            }
            if self.adapters.discord.admin_user_id.trim().is_empty() {
                return Err(miette::miette!(
                    "adapters.discord.admin_user_id 不可為空（discord.enabled = true）"
                ));
            }
            if self.adapters.discord.admin_channel_id.trim().is_empty() {
                return Err(miette::miette!(
                    "adapters.discord.admin_channel_id 不可為空（discord.enabled = true）"
                ));
            }
            validate_discord_id(
                "adapters.discord.admin_user_id",
                &self.adapters.discord.admin_user_id,
            )?;
            validate_discord_id(
                "adapters.discord.admin_channel_id",
                &self.adapters.discord.admin_channel_id,
            )?;
            for (i, guild_id) in self.adapters.discord.guild_ids.iter().enumerate() {
                validate_discord_id(&format!("adapters.discord.guild_ids[{i}]"), guild_id)?;
            }
        }
        if !self.adapters.discord.public_base_url.trim().is_empty() {
            validate_http_url(
                "adapters.discord.public_base_url",
                self.adapters.discord.public_base_url.trim(),
            )?;
        }

        if self.adapters.admin_web.enabled {
            // `access_token` is a deprecated compatibility field. A static
            // numeric token must not bypass Dashboard authentication when the
            // service is reachable through a public Tunnel.
            if self.adapters.line_bot.public_base_url.trim().is_empty()
                && self.adapters.discord.public_base_url.trim().is_empty()
            {
                return Err(miette::miette!(
                    "adapters.admin_web.enabled = true 時，請設定 adapters.discord.public_base_url 或 adapters.line_bot.public_base_url"
                ));
            }
            validate_trusted_proxies(&self.adapters.admin_web.trusted_proxies)?;
        }

        if self.monitor.retry_interval_secs == 0 {
            return Err(miette::miette!("monitor.retry_interval_secs 必須大於 0"));
        }
        if self.monitor.max_failures_before_reauth == 0 {
            return Err(miette::miette!(
                "monitor.max_failures_before_reauth 必須大於 0"
            ));
        }
        if self.time.timezone.trim().is_empty() {
            return Err(miette::miette!("time.timezone 不可為空"));
        }

        Ok(())
    }
}

impl std::fmt::Display for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AppConfig {{ providers: {}, line_bot.enabled: {}, discord.enabled: {} }}",
            self.providers.len(),
            self.adapters.line_bot.enabled,
            self.adapters.discord.enabled,
        )
    }
}

/// Validate `adapters.admin_web.trusted_proxies` entries. Each must be an IP,
/// a CIDR block, or the keyword `cloudflare` / `loopback`.
fn validate_trusted_proxies(entries: &[String]) -> Result<()> {
    for (i, entry) in entries.iter().enumerate() {
        let trimmed = entry.trim();
        match trimmed.to_ascii_lowercase().as_str() {
            "cloudflare" | "loopback" | "localhost" => continue,
            _ => {
                let (ip_part, _len) = match trimmed.split_once('/') {
                    Some((ip, len)) => (ip, len.parse::<u32>().ok()),
                    None => (trimmed, None),
                };
                if ip_part.trim().parse::<std::net::IpAddr>().is_err() {
                    return Err(miette::miette!(
                        "adapters.admin_web.trusted_proxies[{i}] 必須是 IP、CIDR 或 cloudflare/loopback：`{entry}`"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_discord_id(field: &str, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Err(miette::miette!("{field} 必須是 Discord snowflake 數字 ID"));
    }

    Ok(())
}

fn validate_http_url(field: &str, value: &str) -> Result<()> {
    let parsed =
        url::Url::parse(value).map_err(|_| miette::miette!("{field} 必須是有效的 http(s) URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(miette::miette!("{field} 必須是有效的 http(s) URL"));
    }

    Ok(())
}

pub(crate) fn parse_hhmm(value: &str) -> Result<NaiveTime> {
    let (hour_str, minute_str) = value
        .split_once(':')
        .ok_or_else(|| miette::miette!("時間格式必須為 HH:MM，收到 `{value}`"))?;
    let hour: u32 = hour_str
        .parse()
        .into_diagnostic()
        .wrap_err_with(|| format!("無法解析小時：`{value}`"))?;
    let minute: u32 = minute_str
        .parse()
        .into_diagnostic()
        .wrap_err_with(|| format!("無法解析分鐘：`{value}`"))?;

    NaiveTime::from_hms_opt(hour, minute, 0)
        .ok_or_else(|| miette::miette!("時間 `{value}` 超出有效範圍"))
}

pub(crate) fn parse_schedule_period(period: &str) -> Result<(NaiveTime, NaiveTime)> {
    let (start, end) = period
        .split_once('~')
        .ok_or_else(|| miette::miette!("時段格式必須為 HH:MM~HH:MM，收到 `{period}`"))?;
    let start = parse_hhmm(start.trim())?;
    let end = parse_hhmm(end.trim())?;

    if start >= end {
        return Err(miette::miette!(
            "時段 `{period}` 的開始時間必須早於結束時間"
        ));
    }

    Ok((start, end))
}

fn validate_provider_schedule(name: &str, schedule: &PollingScheduleConfig) -> Result<()> {
    let mut parsed = Vec::with_capacity(schedule.periods.len());
    let mut periods = std::collections::BTreeSet::new();
    let mut rest_weekdays = std::collections::BTreeSet::new();

    for weekday in &schedule.rest_weekdays {
        if !rest_weekdays.insert(*weekday) {
            return Err(miette::miette!(
                "providers.{name}.schedule.rest_weekdays 中有重複的星期設定"
            ));
        }
    }

    for (i, period) in schedule.periods.iter().enumerate() {
        if period.trim().is_empty() {
            return Err(miette::miette!(
                "providers.{name}.schedule.periods[{i}] 不可為空"
            ));
        }

        if !periods.insert(period.clone()) {
            return Err(miette::miette!(
                "providers.{name}.schedule.periods 中的 `{period}` 重複"
            ));
        }

        let (start, end) = parse_schedule_period(period)
            .wrap_err_with(|| format!("providers.{name}.schedule.periods[{i}] 格式錯誤"))?;

        parsed.push((start, end, period.clone()));
    }

    parsed.sort_by_key(|(start, _, _)| *start);
    for pair in parsed.windows(2) {
        let (_, prev_end, prev_label) = &pair[0];
        let (next_start, _, next_label) = &pair[1];
        if next_start < prev_end {
            return Err(miette::miette!(
                "providers.{name}.schedule.periods 中的 `{prev_label}` 與 `{next_label}` 時段重疊"
            ));
        }
    }

    Ok(())
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_toml(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    // Minimal valid TOML: logging + monitor are required; providers is optional.
    fn minimal_toml() -> &'static str {
        r#"
[logging]
[monitor]

[providers.default]
[providers.default.api]
[providers.default.radar]
[providers.default.brute_force]
[providers.default.qrcode]
"#
    }

    // ── Load ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_load_minimal_config() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.providers.contains_key("default"));
        assert_eq!(cfg.providers.len(), 1);
    }

    #[test]
    fn test_load_nonexistent_file_returns_error() {
        assert!(AppConfig::load("/nonexistent/config.toml").is_err());
    }

    #[test]
    fn test_load_invalid_toml_returns_error() {
        let f = write_toml("not valid toml !!! [[[");
        assert!(AppConfig::load(f.path()).is_err());
    }

    // ── Defaults ─────────────────────────────────────────────────────────────

    #[test]
    fn test_adapters_section_is_optional() {
        // Config without [adapters] loads fine; line_bot defaults to disabled.
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(!cfg.adapters.line_bot.enabled);
    }

    #[test]
    fn test_logging_defaults_when_section_empty() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.logging.level, "info");
        assert!(!cfg.logging.log_to_file);
        assert_eq!(cfg.logging.log_file_path, "tronclass_rollcall.log");
    }

    #[test]
    fn test_time_defaults_when_section_missing() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.time.timezone, "local");
    }

    #[test]
    fn test_monitor_defaults_when_section_empty() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.monitor.startup_delay_secs, 3);
        assert_eq!(cfg.monitor.retry_interval_secs, 30);
        assert_eq!(cfg.monitor.max_failures_before_reauth, 5);
    }

    #[test]
    fn test_provider_config_defaults() {
        let p = ProviderConfig::default();
        assert_eq!(p.kind, ProviderKind::Tronclass);
        assert_eq!(p.base_url, "https://www.tronclass.com");
        assert_eq!(p.api.base_url, "https://www.tronclass.com");
        assert_eq!(p.api.poll_interval_secs, 10);
        assert_eq!(p.api.request_timeout_secs, 30);
        assert_eq!(p.radar.accuracy, 35);
        assert_eq!(p.radar.altitude, 0);
        assert!(!p.radar.default_coords.is_empty());
        assert_eq!(p.brute_force.concurrency, 200);
        assert_eq!(p.brute_force.request_delay_ms, 0);
        assert_eq!(p.brute_force.transient_failure_threshold, 50);
        assert!((p.brute_force.transient_failure_ratio - 0.20).abs() < f32::EPSILON);
        assert_eq!(p.brute_force.cooldown_secs, 10);
        assert_eq!(p.brute_force.max_cooldowns, 3);
        assert_eq!(p.brute_force.min_concurrency, 10);
        assert_eq!(p.qrcode.scan_timeout_secs, 60);
        assert!(!p.qrcode.scanner_base_url.is_empty());
        assert!(p.schedule.periods.is_empty());
        assert!(p.schedule.rest_weekdays.is_empty());
    }

    #[test]
    fn test_provider_kind_default_is_tronclass() {
        assert_eq!(ProviderKind::default(), ProviderKind::Tronclass);
    }

    #[test]
    fn test_line_bot_config_defaults() {
        let lb = LineBotConfig::default();
        assert!(!lb.enabled);
        assert!(lb.channel_secret.is_empty());
        assert!(lb.channel_access_token.is_empty());
        assert!(lb.admin_user_id.is_empty());
        assert_eq!(lb.webhook_port, 8080);
        assert_eq!(lb.webhook_bind_addr, "127.0.0.1");
        assert_eq!(lb.webhook_path, "/webhook");
        assert!(lb.public_base_url.is_empty());
    }

    #[test]
    fn test_discord_bot_config_defaults() {
        let discord = DiscordBotConfig::default();
        assert!(!discord.enabled);
        assert!(discord.bot_token.is_empty());
        assert!(discord.admin_user_id.is_empty());
        assert!(discord.admin_channel_id.is_empty());
        assert!(discord.public_base_url.is_empty());
        assert!(discord.register_commands);
        assert!(discord.guild_ids.is_empty());
    }

    // ── Line Bot loading ──────────────────────────────────────────────────────

    #[test]
    fn test_load_with_line_bot_enabled() {
        let toml = format!(
            "{}\n\
             [adapters.line_bot]\n\
             enabled = true\n\
             channel_secret = \"mysecret\"\n\
             channel_access_token = \"mytoken\"\n\
             public_base_url = \"https://example.ngrok-free.app\"\n\
             admin_user_id = \"U12345\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.adapters.line_bot.enabled);
        assert_eq!(cfg.adapters.line_bot.channel_secret, "mysecret");
        assert_eq!(cfg.adapters.line_bot.channel_access_token, "mytoken");
        assert_eq!(
            cfg.adapters.line_bot.public_base_url,
            "https://example.ngrok-free.app"
        );
        assert_eq!(cfg.adapters.line_bot.admin_user_id, "U12345");
        // port and path should keep their defaults
        assert_eq!(cfg.adapters.line_bot.webhook_port, 8080);
        assert_eq!(cfg.adapters.line_bot.webhook_path, "/webhook");
    }

    #[test]
    fn test_load_with_custom_webhook_settings() {
        let toml = format!(
            "{}\n\
             [adapters.line_bot]\n\
             enabled = false\n\
             webhook_port = 9090\n\
             webhook_path = \"/line\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.adapters.line_bot.webhook_port, 9090);
        assert_eq!(cfg.adapters.line_bot.webhook_path, "/line");
    }

    #[test]
    fn test_load_with_discord_enabled() {
        let toml = format!(
            "{}\n\
             [adapters.discord]\n\
             enabled = true\n\
             bot_token = \"token\"\n\
             admin_user_id = \"123\"\n\
             admin_channel_id = \"456\"\n\
             public_base_url = \"https://discord-scanner.example.test\"\n\
             register_commands = false\n\
             guild_ids = [\"789\"]\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.adapters.discord.enabled);
        assert_eq!(cfg.adapters.discord.bot_token, "token");
        assert_eq!(cfg.adapters.discord.admin_user_id, "123");
        assert_eq!(cfg.adapters.discord.admin_channel_id, "456");
        assert_eq!(
            cfg.adapters.discord.public_base_url,
            "https://discord-scanner.example.test"
        );
        assert!(!cfg.adapters.discord.register_commands);
        assert_eq!(cfg.adapters.discord.guild_ids, vec!["789".to_string()]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_empty_timezone() {
        let toml = format!(
            "{}\n\
             [time]\n\
             timezone = \"\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.validate().is_err());
    }

    // ── Provider loading ──────────────────────────────────────────────────────

    #[test]
    fn test_load_provider_with_custom_base_url() {
        let toml = r#"
[logging]
[monitor]

[providers.fju]
base_url = "https://elearn2.fju.edu.tw"
[providers.fju.api]
poll_interval_secs = 5
[providers.fju.radar]
accuracy = 50
[providers.fju.brute_force]
concurrency = 100
[providers.fju.qrcode]
scan_timeout_secs = 30
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let p = &cfg.providers["fju"];
        assert_eq!(p.base_url, "https://elearn2.fju.edu.tw");
        assert_eq!(p.api.poll_interval_secs, 5);
        assert_eq!(p.radar.accuracy, 50);
        assert_eq!(p.brute_force.concurrency, 100);
        assert_eq!(p.qrcode.scan_timeout_secs, 30);
    }

    #[test]
    fn test_fju_kind_applies_default_schedule_and_sunday_rest() {
        let toml = r#"
[logging]
[monitor]

[providers.fju]
kind = "fju"
[providers.fju.api]
[providers.fju.radar]
[providers.fju.brute_force]
[providers.fju.qrcode]
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let p = &cfg.providers["fju"];
        assert_eq!(p.schedule.periods, vec!["07:10~17:30".to_string()]);
        assert!(p.schedule.rest_weekdays.contains(&ScheduleWeekday::Sun));
    }

    #[test]
    fn test_tku_kind_applies_iclass_defaults() {
        let toml = r#"
[logging]
[monitor]

[providers.tku]
kind = "tku"
[providers.tku.api]
[providers.tku.radar]
[providers.tku.brute_force]
[providers.tku.qrcode]
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let p = &cfg.providers["tku"];
        assert_eq!(p.base_url, "https://iclass.tku.edu.tw");
        assert_eq!(p.api.base_url, "https://iclass.tku.edu.tw");
        assert_eq!(
            p.qrcode.scanner_base_url,
            "https://iclass.tku.edu.tw/scanner-jumper"
        );
    }

    #[test]
    fn test_load_multiple_providers() {
        let toml = r#"
[logging]
[monitor]

[providers.school_a]
[providers.school_a.api]
[providers.school_a.radar]
[providers.school_a.brute_force]
[providers.school_a.qrcode]

[providers.school_b]
[providers.school_b.api]
[providers.school_b.radar]
[providers.school_b.brute_force]
[providers.school_b.qrcode]
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.providers.len(), 2);
        assert!(cfg.providers.contains_key("school_a"));
        assert!(cfg.providers.contains_key("school_b"));
    }

    // ── Validation ────────────────────────────────────────────────────────────

    #[test]
    fn test_validate_fails_when_no_providers() {
        let f = write_toml("[logging]\n[monitor]\n");
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.providers.is_empty());
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("provider"));
    }

    #[test]
    fn test_validate_passes_for_minimal_valid_config() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_fails_when_concurrency_is_zero() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
concurrency = 0
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("concurrency"), "got: {err}");
    }

    #[test]
    fn test_validate_fails_for_invalid_transient_failure_ratio() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
transient_failure_ratio = 0.0
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("transient_failure_ratio"),
            "got: {err}"
        );
    }

    #[test]
    fn test_validate_fails_for_transient_failure_ratio_above_one() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
transient_failure_ratio = 1.1
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("transient_failure_ratio"),
            "got: {err}"
        );
    }

    #[test]
    fn test_validate_fails_for_zero_transient_failure_threshold() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
transient_failure_threshold = 0
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("transient_failure_threshold"),
            "got: {err}"
        );
    }

    #[test]
    fn test_validate_fails_for_zero_cooldown_when_cooldowns_enabled() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
cooldown_secs = 0
max_cooldowns = 1
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("cooldown_secs"), "got: {err}");
    }

    #[test]
    fn test_validate_allows_zero_max_cooldowns() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
max_cooldowns = 0
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_fails_for_invalid_min_concurrency() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
concurrency = 10
min_concurrency = 20
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("min_concurrency"), "got: {err}");
    }

    #[test]
    fn test_validate_fails_for_invalid_latitude() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
default_coords = [[999.0, 121.0]]
[providers.p.brute_force]
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("緯度"), "got: {err}");
    }

    #[test]
    fn test_validate_fails_for_invalid_schedule_overlap() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
[providers.p.api]
[providers.p.radar]
[providers.p.brute_force]
[providers.p.qrcode]
[providers.p.schedule]
periods = ["08:10~09:00", "08:50~09:30"]
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("重疊"), "got: {err}");
    }

    #[test]
    fn test_validate_fails_for_duplicate_rest_weekdays() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
[providers.p.api]
[providers.p.radar]
[providers.p.brute_force]
[providers.p.qrcode]
[providers.p.schedule]
rest_weekdays = ["sun", "sun"]
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("rest_weekdays"), "got: {err}");
    }

    #[test]
    fn test_validate_fails_when_line_bot_enabled_without_credentials() {
        let toml = format!("{}\n[adapters.line_bot]\nenabled = true\n", minimal_toml());
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("channel_secret"), "got: {err}");
    }

    #[test]
    fn test_validate_passes_when_line_bot_enabled_with_credentials() {
        let toml = format!(
            "{}\n[adapters.line_bot]\nenabled = true\nchannel_secret = \"s\"\nchannel_access_token = \"t\"\nadmin_user_id = \"u\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_fails_when_discord_enabled_without_credentials() {
        let toml = format!("{}\n[adapters.discord]\nenabled = true\n", minimal_toml());
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("bot_token"), "got: {err}");
    }

    #[test]
    fn test_validate_rejects_invalid_discord_id() {
        let toml = format!(
            "{}\n[adapters.discord]\nenabled = true\nbot_token = \"t\"\nadmin_user_id = \"not-id\"\nadmin_channel_id = \"456\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("snowflake"), "got: {err}");
    }

    #[test]
    fn test_validate_rejects_invalid_public_base_url() {
        let toml = format!(
            "{}\n[adapters.line_bot]\nenabled = true\nchannel_secret = \"s\"\nchannel_access_token = \"t\"\nadmin_user_id = \"u\"\npublic_base_url = \"not a url\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("public_base_url"), "got: {err}");
    }

    // ── Serialization ─────────────────────────────────────────────────────────

    #[test]
    fn test_provider_kind_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::Tronclass).unwrap(),
            r#""tronclass""#
        );
        // FJU → "f_j_u" per serde snake_case rules
        assert_eq!(
            serde_json::to_string(&ProviderKind::FJU).unwrap(),
            r#""f_j_u""#
        );
        assert_eq!(
            serde_json::to_string(&ProviderKind::TKU).unwrap(),
            r#""t_k_u""#
        );
    }

    #[test]
    fn test_provider_kind_round_trips() {
        for kind in [
            ProviderKind::Tronclass,
            ProviderKind::FJU,
            ProviderKind::TKU,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ProviderKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn test_provider_kind_accepts_short_aliases() {
        let fju: ProviderKind = serde_json::from_str(r#""fju""#).unwrap();
        assert_eq!(fju, ProviderKind::FJU);

        let tku: ProviderKind = serde_json::from_str(r#""tku""#).unwrap();
        assert_eq!(tku, ProviderKind::TKU);
    }

    // ── Display ───────────────────────────────────────────────────────────────

    #[test]
    fn test_display_shows_provider_count_and_line_bot_status() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        let s = format!("{cfg}");
        assert!(s.contains("providers: 1"), "got: {s}");
        assert!(s.contains("line_bot.enabled: false"), "got: {s}");
        assert!(s.contains("discord.enabled: false"), "got: {s}");
    }

    #[test]
    fn test_display_with_line_bot_enabled() {
        let toml = format!("{}\n[adapters.line_bot]\nenabled = true\n", minimal_toml());
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let s = format!("{cfg}");
        assert!(s.contains("line_bot.enabled: true"), "got: {s}");
    }

    // ── CURRENT_SEMESTER_OVERRIDE_V1 ──────────────────────────────────────────

    #[test]
    fn test_parse_semester_current_formats() {
        // 民國學年＋學期（數字型）
        assert_eq!(parse_semester_current("1151"), Some((2026, 1)));
        assert_eq!(parse_semester_current("115-2"), Some((2026, 2)));
        assert_eq!(parse_semester_current("  115-1  "), Some((2026, 1)));
        // 文字學期標記
        assert_eq!(parse_semester_current("115上"), Some((2026, 1)));
        assert_eq!(parse_semester_current("115下"), Some((2026, 2)));
        assert_eq!(parse_semester_current("2026 Fall"), Some((2026, 1)));
        assert_eq!(parse_semester_current("2025 Spring"), Some((2025, 2)));
        // 西元年＋學期（數字型）
        assert_eq!(parse_semester_current("2026-1"), Some((2026, 1)));
        assert_eq!(parse_semester_current("20252"), Some((2025, 2)));
        // 無效：空字串、只給年份（學期不可猜）、學期碼超出 1/2、純文字
        assert_eq!(parse_semester_current(""), None);
        assert_eq!(parse_semester_current("2026"), None);
        assert_eq!(parse_semester_current("115"), None);
        assert_eq!(parse_semester_current("1153"), None);
        assert_eq!(parse_semester_current("abc"), None);
    }

    #[test]
    fn test_semester_config_resolved() {
        let cfg = SemesterConfig {
            current: Some("115-1".to_string()),
        };
        assert_eq!(cfg.resolved(), Some((2026, 1)));

        // 空／未設定 → None（自動推算），不視為錯誤
        assert_eq!(SemesterConfig { current: None }.resolved(), None);
        assert_eq!(
            SemesterConfig {
                current: Some("   ".to_string()),
            }
            .resolved(),
            None
        );

        // 格式無效 → None（退回自動推算；warn 已由 OnceCell 收斂為一次）
        assert_eq!(
            SemesterConfig {
                current: Some("not-a-semester".to_string()),
            }
            .resolved(),
            None
        );
    }

    #[test]
    fn test_load_config_with_semester_section() {
        let toml = format!("{}\n[semester]\ncurrent = \"115-1\"\n", minimal_toml());
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.semester.resolved(), Some((2026, 1)));

        // 未設定 [semester] 的舊設定檔仍可載入（serde 預設）
        let f_legacy = write_toml(minimal_toml());
        let cfg_legacy = AppConfig::load(f_legacy.path()).unwrap();
        assert_eq!(cfg_legacy.semester.resolved(), None);
    }

    #[test]
    fn test_homework_strict_current_semester_defaults() {
        // 未設定 [homework]：嚴格模式預設開啟、放寬旗標預設關閉
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.homework.strict_current_semester);
        assert!(!cfg.homework.include_all_semesters);
        // 可明確關閉
        let toml = format!(
            "{}\n[homework]\nstrict_current_semester = false\n",
            minimal_toml()
        );
        let f2 = write_toml(&toml);
        let cfg2 = AppConfig::load(f2.path()).unwrap();
        assert!(!cfg2.homework.strict_current_semester);
        // [homework] 存在但未寫 strict_current_semester → 仍是預設 true
        let toml3 = format!("{}\n[homework]\ninclude_all_semesters = true\n", minimal_toml());
        let f3 = write_toml(&toml3);
        let cfg3 = AppConfig::load(f3.path()).unwrap();
        assert!(cfg3.homework.strict_current_semester);
        assert!(cfg3.homework.include_all_semesters);
    }
}
