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
//! - [`passkeys`]    WebAuthn passkey 憑證
//! - [`timetable`]   課表與課表模板
//! - [`preferences`] 通知 / 上課提醒 / 偵測排程偏好
//! - [`web_push`]    Web Push 設定與訂閱
//! - [`sessions`]    Dashboard 登入 session
//! - [`homework`]    作業截止提醒去重與快照
//!
//! 對外仍以 `crate::db::AccountsDb` 與既有型別使用，公開 API 不變；所有型別
//! 透過下方的 `pub use` 再匯出，維持 `crate::db::Xxx` 路徑。

mod accounts;
mod activity;
mod auth_throttle;
mod blacklist;
mod homework;
mod materials;
mod passkeys;
mod preferences;
mod schema;
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
pub use blacklist::IpBlacklistRow;
#[allow(unused_imports)]
pub use materials::{MaterialEventInput, MaterialEventRow};
#[allow(unused_imports)]
pub use preferences::{
    ClassReminderDueRow, ClassReminderPreferenceRow, DetectionSchedulePreferenceRow,
    NotificationPreferenceRow,
};
// HOMEWORK_LADDER_V1：提醒引擎（homework_reminders）與通知頁 API 共用的預設階梯。
pub(crate) use preferences::{
    DEFAULT_HOMEWORK_LADDER_HOURS, HOMEWORK_LADDER_MAX_HOURS, HOMEWORK_LADDER_MAX_STAGES,
    HOMEWORK_LADDER_MIN_HOURS,
};
#[allow(unused_imports)]
pub use sessions::{DashboardSessionInsert, DashboardSessionRow};
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
            // SQLITE_POOL_PRAGMAS_V1：busy_timeout 是「每條連線」的設定；放在 connect options
            // 才會套到連線池裡每一條連線（下面 apply_sqlite_runtime_pragmas 只會跑在拿到的
            // 那一條上）。15 秒足以撐過 WAL checkpoint／批次寫入的短暫鎖定，不會回 SQLITE_BUSY。
            .busy_timeout(Duration::from_secs(15))
            .foreign_keys(true)
            // SQLITE_POOL_PRAGMAS_V1：mmap_size／cache_size／temp_store 同樣是 per-connection；
            // 原本只在其中一條連線生效，其餘 9 條仍是預設（2 MB cache、無 mmap）。
            .pragma("mmap_size", "268435456")
            .pragma("cache_size", "-64000")
            .pragma("temp_store", "MEMORY");

        // SQLITE_POOL_PRAGMAS_V1：連線池設定
        // - max_connections 10：SQLite WAL 下多讀單寫；Dashboard 輪詢＋監控迴圈＋活動／教材批次
        //   寫入者同時最多也就個位數連線，10 已足夠且不會把寫鎖競爭放大。
        // - min_connections 1、idle_timeout 10 分鐘：離峰釋放連線（每條連線各自持有 64 MB cache
        //   上限與 mmap 對映），尖峰再開。
        // - acquire_timeout 15 秒：與 busy_timeout 對齊，池子被佔滿時等待而非立刻失敗。
        // - test_before_acquire(false)：本機檔案 DB 連線不會「斷線」，省掉每次借連線的 ping。
        let pool = SqlitePoolOptions::new()
            .max_connections(10)
            .min_connections(1)
            .idle_timeout(Some(Duration::from_secs(600)))
            .acquire_timeout(Duration::from_secs(15))
            .test_before_acquire(false)
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

    // Keep it lightweight; do not VACUUM at startup.
    let _ = sqlx::query("PRAGMA optimize").execute(pool).await;
    let _ = sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
        .execute(pool)
        .await;
    Ok(())
}

pub(crate) async fn ensure_table_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn open_migrates_existing_accounts_table() {
        let file = NamedTempFile::new().unwrap();
        let url = format!("sqlite://{}?mode=rwc", file.path().display());
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE accounts (
                id TEXT PRIMARY KEY NOT NULL,
                provider TEXT NOT NULL DEFAULT '',
                username TEXT NOT NULL DEFAULT '',
                password TEXT NOT NULL DEFAULT '',
                enabled INTEGER NOT NULL DEFAULT 1,
                line_user_id TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        drop(pool);

        let db = AccountsDb::open(file.path()).await.unwrap();
        assert!(!db.set_discord_user_id("missing", "1").await.unwrap());
        assert!(!db.set_session_cookie("missing", "session=x").await.unwrap());
    }
}
