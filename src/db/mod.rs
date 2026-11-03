//! 帳號資料庫模組
//!
//! 資料庫檔案預設為 `accounts.db`，可透過 `-a/--accounts` 指定路徑。
//!
//! 本模組依領域拆分成多個子模組，每個子模組以 `impl AccountsDb` 提供該領域
//! 的查詢與寫入方法（原先全數堆疊在單一 `db.rs` 中）：
//!
//! - [`accounts`]    帳號 CRUD 與 Discord / Line 綁定
//! - [`materials`]   教材操作稽核事件
//! - [`activity`]    登入 / 安全活動事件
//! - [`blacklist`]   IP 黑名單
//! - [`account_suspensions`] 帳號暫時停權（到期自動解除）
//! - [`passkeys`]    WebAuthn passkey 憑證
//! - [`timetable`]   課表與課表模板
//! - [`preferences`] 通知 / 上課提醒 / 偵測排程偏好
//! - [`web_push`]    Web Push 設定與訂閱
//! - [`sessions`]    Dashboard 登入 session
//! - [`homework`]    作業截止提醒去重與快照
//! - [`rollcalls`]   點名送出紀錄（出席統計與送出者查詢的來源）
//! - [`security_settings`] 全站性安全開關（與單一帳號無關；目前有
//!   「僅限台灣 IP 存取 /admin」與「封鎖雲端／VPS機房 IP」兩個）
//!
//! 對外仍以 `crate::db::AccountsDb` 與既有型別使用，公開 API 不變；所有型別
//! 透過下方的 `pub use` 再匯出，維持 `crate::db::Xxx` 路徑。

mod account_suspensions;
mod accounts;
mod activity;
mod auth_throttle;
mod blacklist;
// CREDITS_EMIS_V1：學分報告快照（impl AccountsDb 的分散實作）。
mod credit_reports;
mod geo_access_requests;
mod homework;
mod materials;
mod passkeys;
mod preferences;
mod radar;
// ROLLCALL_ATTENDANCE_V1（fix66）：點名送出紀錄（出席統計）。
mod rollcalls;
mod schema;
// ADMIN_GEO_RESTRICT_V1：全站性安全開關（目前只有「僅限台灣 IP 存取 /admin」）。
mod security_settings;
mod sessions;
mod timetable;
mod timetable_sync;
mod web_push;

use std::{path::Path, str::FromStr, time::Duration};

use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    SqlitePool,
};
use tokio::sync::mpsc;

use crate::account::{AccountConfig, AccountsFile};
use crate::config::AppConfig;

// ─── 共用常數 ────────────────────────────────────────────────────────────────

// PERFORMANCE_SQLITE_V1: amortize retention cleanup instead of DELETE per write.
pub(crate) const EVENT_PRUNE_INTERVAL: usize = 25;
pub(crate) const EVENT_IDENTITY_MAX_BYTES: usize = 256;
pub(crate) const EVENT_ACTION_MAX_BYTES: usize = 128;
pub(crate) const EVENT_PATH_MAX_BYTES: usize = 1024;
pub(crate) const EVENT_FILE_NAME_MAX_BYTES: usize = 255;
pub(crate) const EVENT_RESULT_MAX_BYTES: usize = 64;
pub(crate) const EVENT_DETAILS_MAX_BYTES: usize = 4 * 1024;
pub(crate) const EVENT_IP_MAX_BYTES: usize = 64;
/// USER_AGENT（登入事件的裝置資訊；`SessionOrigin` 會先截到 256 字元）。
pub(crate) const EVENT_USER_AGENT_MAX_BYTES: usize = 512;
pub(crate) const EVENT_TRUNCATED_SUFFIX: &str = "…[truncated]";
/// 作業截止時間延後多久才算「顯著延期」，值得重新跑一輪倒數提醒。
///
/// MEANINGFUL_EXTENSION_ONLY_V1
/// 設為 6 小時：對應提醒階梯的中間級距。老師微調截止時間（往後挪數分鐘或
/// 一兩小時）不會重置紀錄，避免同一階段重複轟炸；真正的延期（延到隔天、
/// 延長一週）才會讓 24h/6h/1h 重新提醒一輪。
pub(crate) const MEANINGFUL_EXTENSION_SECS: i64 = 6 * 60 * 60;

pub(crate) const MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT: usize = 32;
pub(crate) const MAX_PASSKEY_CREDENTIAL_JSON_BYTES: usize = 64 * 1024;
pub(crate) const MAX_USERNAME_MATCH_ROWS: i64 = 64;
pub(crate) const MAX_TIMETABLE_ENTRIES_PER_ACCOUNT: usize = 300;
pub(crate) const MAX_TIMETABLE_LIST_ALL_ROWS: usize = 5000;
pub(crate) const MAX_TIMETABLE_TEMPLATES: usize = 64;
pub(crate) const MAX_TIMETABLE_TEMPLATE_NAME_BYTES: usize = 128;
pub(crate) const MAX_TIMETABLE_TEMPLATE_DESCRIPTION_BYTES: usize = 8 * 1024;
pub(crate) const MAX_TIMETABLE_TEMPLATE_JSON_BYTES: usize = 256 * 1024;
pub(crate) const MAX_TIMETABLE_TEMPLATE_CREATOR_BYTES: usize = 256;

// ─── 對外型別再匯出（維持 crate::db::Xxx 路徑不變）──────────────────────────
//
// 這些 re-export 保留公開 API 面。部分型別（*Row）目前僅作為方法回傳型別被
// 間接使用，在 binary crate 中從未被 by-name 引用，會觸發 unused_imports lint；
// 每行標 `#[allow(unused_imports)]` 以明確表達「保留 API、非遺漏」。
// （attribute 只作用於緊接的下一項，所以必須逐行標註。）

#[allow(unused_imports)]
pub use accounts::UsernameUpdateResult;
// SECRET_ROTATE_CLI_V1：main.rs 只透過 `db.rotate_all_secrets()` 的回傳值使用，
// 不直接 import 這個型別；re-export 是為了讓 pub fn 的回傳型別可見（E0446）。
#[allow(unused_imports)]
pub use accounts::SecretRotationReport;
#[allow(unused_imports)]
pub use activity::{ActivityEventInput, ActivityEventRow};
#[allow(unused_imports)]
pub use blacklist::{IpBlacklistRow, WAF_ANOMALY_REASON_PREFIX};
#[allow(unused_imports)]
pub use geo_access_requests::{GeoAccessRequestRow, GEO_ACCESS_ALLOW_SECONDS, GEO_ACCESS_NOTIFY_MUTED_KEY};
// MaterialEventInput 有外部呼叫端（files.rs／activity.rs 測試／mod.rs 的 material_tx），
// 不會觸發警告；MaterialEventRow 目前只在 materials.rs 內部用到（list_material_events
// 的回傳型別），從未經由 `crate::db::MaterialEventRow` 這條路徑被 by-name 引用——跟上面
// 幾行一樣，保留 re-export 是為了維持對外 API 路徑一致，不是遺漏，所以明示 allow。
#[allow(unused_imports)]
pub use materials::{MaterialEventInput, MaterialEventRow};
// ROLLCALL_STATS_V1（fix66）：點名統計。函式與列型別在 API 層直接使用；兩個統計結構
// （RollcallCourseStat／RollcallSenderStat）目前只有本模組內部在建構與序列化時用到值，
// 對外沒有具名引用——依本檔慣例（同 TimetableSyncStateRow 等）明示 allow，
// 保留 `crate::db::…` 這條取用路徑，同時不留編譯警告。
#[allow(unused_imports)]
pub use rollcalls::{
    aggregate_courses, aggregate_senders, rollcall_success_rate, select_display_session_rows,
    RollcallCourseStat, RollcallEventInput, RollcallEventRow, RollcallLatestSession,
    RollcallSenderStat, RollcallUnattendedUser,
};
#[allow(unused_imports)]
pub use preferences::{
    ClassReminderDueRow, ClassReminderPreferenceRow, DetectionSchedulePreferenceRow,
    NotificationPreferenceRow,
};
// RADAR_PANEL_V1（fix70）：雷達設定列（面板 API 只用欄位，不具名引用型別）。
#[allow(unused_imports)]
pub use radar::RadarPreferenceRow;
pub use security_settings::{GEO_RESTRICT_ADMIN_KEY, HOSTING_IP_BLOCK_KEY};
// HOMEWORK_LADDER_V1：提醒引擎（homework_reminders）與通知頁 API 共用的預設階梯。
pub(crate) use preferences::{
    DEFAULT_HOMEWORK_LADDER_HOURS, HOMEWORK_LADDER_MAX_HOURS, HOMEWORK_LADDER_MAX_STAGES,
    HOMEWORK_LADDER_MIN_HOURS,
};
#[allow(unused_imports)]
pub use sessions::{DashboardSessionInsert, DashboardSessionRow};
// SESSION_TOKEN_HASH_V1（fix125）：給 admin_web 比對記憶體鍵與 no-rehash 比對用。
pub(crate) use sessions::session_lookup_key;
#[allow(unused_imports)]
pub use timetable::{TimetableEntryInput, TimetableEntryRow, TimetableTemplateRow};

// TIMETABLE_AUTO_SYNC_V1
// 與相鄰的 re-export 一致：對外提供便利路徑，本 crate 內未直接使用，
// 因此比照 TimetableEntryRow 等加上 allow，避免無意義的編譯警告。
#[allow(unused_imports)]
pub use timetable_sync::TimetableSyncStateRow;
#[allow(unused_imports)]
pub use web_push::WebPushSubscriptionRow;

// ─── AccountsDb ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AccountsDb {
    pub(crate) pool: SqlitePool,
    pub(crate) activity_tx: mpsc::Sender<ActivityEventInput>,
    pub(crate) material_tx: mpsc::Sender<MaterialEventInput>,
}

impl AccountsDb {
    /// DB_SNAPSHOT_BACKUP_V1（fix123）：線上快照——以 SQLite 內建 `VACUUM INTO`
    /// 輸出一份與目前狀態一致的完整副本到 `dest`（WAL 模式下無需停服、
    /// 不阻塞讀寫）。由 main.rs 的定時輪替任務每日呼叫；失敗時由呼叫端
    /// 記 warn，不影響服務本體。路徑以「防誤用」為設計：呼叫端只傳
    /// 與 accounts.db 同目錄的固定命名檔，此處不出現任何路徑字串拼接。
    pub async fn backup_snapshot(&self, dest: &Path) -> Result<()> {
        let dest_str = dest.display().to_string();
        sqlx::query("VACUUM INTO ?")
            .bind(&dest_str)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("備份資料庫到 `{dest_str}` 失敗"))?;
        Ok(())
    }

    /// 開啟（或建立）帳號資料庫，並確保 schema 存在。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let connect_options = SqliteConnectOptions::from_str(&url)
            .into_diagnostic()
            .wrap_err_with(|| format!("無法解析資料庫連線字串：`{}`", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(
                crate::config::try_get_config()
                    .map(|cfg| cfg.sqlite_busy_timeout_secs)
                    .unwrap_or(15),
            ))
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(10)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(15))
            .connect_with(connect_options)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("無法開啟資料庫：`{}`", path.display()))?;

        apply_sqlite_runtime_pragmas(&pool).await?;
        schema::create_schema(&pool).await?;

        let (activity_tx, activity_rx) = mpsc::channel(1024);
        let (material_tx, material_rx) = mpsc::channel(1024);

        let activity_pool = pool.clone();
        tokio::spawn(activity::activity_event_batch_writer(activity_pool, activity_rx));

        let material_pool = pool.clone();
        tokio::spawn(materials::material_event_batch_writer(material_pool, material_rx));

        Ok(Self {
            pool,
            activity_tx,
            material_tx,
        })
    }

    /// 檢查 SQLite 結構與索引是否完整。
    pub async fn integrity_check(&self) -> Result<String> {
        let result: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("SQLite integrity_check 執行失敗")?;
        Ok(result)
    }

    // ── 解析 ──────────────────────────────────────────────────────────────────

    /// 從資料庫載入所有帳號並解析成可執行的 `AccountConfig` 清單。
    pub async fn resolve(&self, app: &AppConfig) -> Result<Vec<AccountConfig>> {
        let raw = self.list().await?;
        AccountsFile { accounts: raw }.resolve(app)
    }

    /// LOGIN_FAILURE_NONFATAL_V1：啟動用的寬鬆版解析，回傳 `(可監控帳號, 失敗清單)`。
    pub async fn resolve_tolerant(
        &self,
        app: &AppConfig,
    ) -> Result<(Vec<AccountConfig>, Vec<(String, String)>)> {
        let raw = self.list().await?;
        Ok(AccountsFile { accounts: raw }.resolve_tolerant(app))
    }
}

// ─── 共用 helper ─────────────────────────────────────────────────────────────

pub(crate) fn truncate_event_text(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let keep = max_bytes.saturating_sub(EVENT_TRUNCATED_SUFFIX.len());
    let mut end = keep.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    if EVENT_TRUNCATED_SUFFIX.len() <= max_bytes {
        value.push_str(EVENT_TRUNCATED_SUFFIX);
    }
    value.shrink_to(max_bytes);
    value
}

async fn apply_sqlite_runtime_pragmas(pool: &SqlitePool) -> Result<()> {
    // These PRAGMAs are safe for the local single-process SQLite use case.
    // WAL improves read/write concurrency for Admin dashboard + materials/activity logs.
    sqlx::query("PRAGMA journal_mode = WAL")
        .execute(pool)
        .await
        .into_diagnostic()
        .wrap_err("設定 SQLite journal_mode=WAL 失敗")?;

    sqlx::query("PRAGMA synchronous = NORMAL")
        .execute(pool)
        .await
        .into_diagnostic()
        .wrap_err("設定 SQLite synchronous=NORMAL 失敗")?;

    let _ = sqlx::query("PRAGMA mmap_size = 268435456")
        .execute(pool)
        .await;
    let _ = sqlx::query("PRAGMA cache_size = -64000")
        .execute(pool)
        .await;
    let _ = sqlx::query("PRAGMA temp_store = MEMORY")
        .execute(pool)
        .await;
    let _ = sqlx::query("PRAGMA wal_autocheckpoint = 1000")
        .execute(pool)
        .await;

    sqlx::query("PRAGMA busy_timeout = 15000")
        .execute(pool)
        .await
        .into_diagnostic()
        .wrap_err("設定 SQLite busy_timeout 失敗")?;

    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(pool)
        .await
        .into_diagnostic()
        .wrap_err("設定 SQLite foreign_keys=ON 失敗")?;

    // MAXIMUM_SECURITY_PRAGMAS_V1:
    // 關閉危險的虛擬表與外部延伸提權風險 (trusted_schema = OFF)
    // 開啟儲存格邊界檢查 (cell_size_check = ON)，防範損毀/惡意 SQLite 檔案溢位
    // 關閉遞迴觸發器 (recursive_triggers = OFF)
    // 開啟安全物理覆寫刪除 (secure_delete = ON)，確保刪除資料以 0 實體覆蓋防範磁區復原
    let _ = sqlx::query("PRAGMA trusted_schema = OFF").execute(pool).await;
    let _ = sqlx::query("PRAGMA cell_size_check = ON").execute(pool).await;
    let _ = sqlx::query("PRAGMA recursive_triggers = OFF").execute(pool).await;
    let _ = sqlx::query("PRAGMA secure_delete = ON").execute(pool).await;

    // Keep it lightweight; do not VACUUM at startup.
    let _ = sqlx::query("PRAGMA optimize").execute(pool).await;
    let _ = sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
        .execute(pool)
        .await;
    Ok(())
}

/// L4（安全審查）：表名與欄名會被拼進 SQL（pragma_table_info、ALTER TABLE 無法參數化），
/// 只允許 ASCII 字母、數字與底線；目前所有呼叫端都是寫死的字面值，這層是防止日後誤傳入外部輸入。
fn check_sql_identifier(value: &str) -> Result<()> {
    if !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(miette::miette!("不合法的 SQL 識別字：`{value}`"))
    }
}

pub(crate) async fn ensure_table_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    check_sql_identifier(table)?;
    check_sql_identifier(column)?;
    let sql = format!("SELECT name FROM pragma_table_info('{table}')");
    let rows = sqlx::query_as::<_, (String,)>(&sql)
        .fetch_all(pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("檢查 {table} schema 失敗"))?;

    if rows.iter().any(|(name,)| name == column) {
        return Ok(());
    }

    sqlx::query(&format!(
        "ALTER TABLE {table} ADD COLUMN {column} {definition}"
    ))
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err_with(|| format!("新增 {table}.{column} 欄位失敗"))?;

    Ok(())
}

pub(crate) async fn ensure_column(pool: &SqlitePool, column: &str, definition: &str) -> Result<()> {
    check_sql_identifier(column)?;
    let rows = sqlx::query_as::<_, (String,)>("SELECT name FROM pragma_table_info('accounts')")
        .fetch_all(pool)
        .await
        .into_diagnostic()
        .wrap_err("檢查 accounts schema 失敗")?;

    if rows.iter().any(|(name,)| name == column) {
        return Ok(());
    }

    sqlx::query(&format!(
        "ALTER TABLE accounts ADD COLUMN {column} {definition}"
    ))
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err_with(|| format!("新增 accounts.{column} 欄位失敗"))?;

    Ok(())
}

