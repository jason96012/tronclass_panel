//! 帳號資料庫模組
//!
//! 資料庫檔案預設為 `accounts.db`，可透過 `-a/--accounts` 指定路徑。

use std::{
    path::Path,
    str::FromStr,
    sync::atomic::AtomicUsize,
    time::Duration,
};

use chrono::Local;
use miette::{IntoDiagnostic, Result, WrapErr};
use passkey_auth::PasskeyCredential;
use serde::Serialize;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    FromRow, SqlitePool,
};
use tokio::sync::mpsc;

use crate::account::{AccountConfig, AccountsFile, RawAccountConfig};
use crate::config::AppConfig;

// PERFORMANCE_SQLITE_V1: amortize retention cleanup instead of DELETE per write.
const EVENT_PRUNE_INTERVAL: usize = 25;
const EVENT_IDENTITY_MAX_BYTES: usize = 256;
const EVENT_ACTION_MAX_BYTES: usize = 128;
const EVENT_PATH_MAX_BYTES: usize = 1024;
const EVENT_FILE_NAME_MAX_BYTES: usize = 255;
const EVENT_RESULT_MAX_BYTES: usize = 64;
const EVENT_DETAILS_MAX_BYTES: usize = 4 * 1024;
const EVENT_IP_MAX_BYTES: usize = 64;
const EVENT_TRUNCATED_SUFFIX: &str = "…[truncated]";
/// 作業截止時間延後多久才算「顯著延期」，值得重新跑一輪倒數提醒。
///
/// MEANINGFUL_EXTENSION_ONLY_V1
/// 設為 6 小時：對應提醒階梯的中間級距。老師微調截止時間（往後挪數分鐘或
/// 一兩小時）不會重置紀錄，避免同一階段重複轟炸；真正的延期（延到隔天、
/// 延長一週）才會讓 24h/6h/1h 重新提醒一輪。
const MEANINGFUL_EXTENSION_SECS: i64 = 6 * 60 * 60;

const MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT: usize = 32;
const MAX_PASSKEY_CREDENTIAL_JSON_BYTES: usize = 64 * 1024;
const MAX_USERNAME_MATCH_ROWS: i64 = 64;
const MAX_TIMETABLE_ENTRIES_PER_ACCOUNT: usize = 300;
const MAX_TIMETABLE_LIST_ALL_ROWS: usize = 5000;
const MAX_TIMETABLE_TEMPLATES: usize = 64;
const MAX_TIMETABLE_TEMPLATE_NAME_BYTES: usize = 128;
const MAX_TIMETABLE_TEMPLATE_DESCRIPTION_BYTES: usize = 8 * 1024;
const MAX_TIMETABLE_TEMPLATE_JSON_BYTES: usize = 256 * 1024;
const MAX_TIMETABLE_TEMPLATE_CREATOR_BYTES: usize = 256;
static MATERIAL_EVENT_WRITES_SINCE_PRUNE: AtomicUsize = AtomicUsize::new(0);
static ACTIVITY_EVENT_WRITES_SINCE_PRUNE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsernameUpdateResult {
    Updated { account_id: String },
    NotFound,
    Ambiguous { account_ids: Vec<String> },
}

// ─── 資料庫列映射 ─────────────────────────────────────────────────────────────

#[derive(Debug, FromRow)]
struct AccountRow {
    id: String,
    provider: String,
    username: String,
    password: String,
    dashboard_password_hash: String,
    enabled: bool,
    line_user_id: String,
    discord_user_id: String,
    session_cookie: String,
    is_admin: bool,
}

impl From<AccountRow> for RawAccountConfig {
    fn from(row: AccountRow) -> Self {
        RawAccountConfig {
            id: row.id,
            provider: row.provider,
            username: row.username,
            password: row.password,
            dashboard_password_hash: row.dashboard_password_hash,
            enabled: row.enabled,
            line_user_id: row.line_user_id,
            discord_user_id: row.discord_user_id,
            session_cookie: row.session_cookie,
            is_admin: row.is_admin,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MaterialEventInput {
    pub actor: String,
    pub action: String,
    pub path: String,
    pub folder: String,
    pub file_name: String,
    pub size: i64,
    pub result: String,
    pub details: String,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct MaterialEventRow {
    pub id: i64,
    pub timestamp: i64,
    pub time: String,
    pub actor: String,
    pub action: String,
    pub path: String,
    pub folder: String,
    pub file_name: String,
    pub size: i64,
    pub size_label: String,
    pub result: String,
    pub details: String,
}

#[derive(Debug, Clone)]
pub struct ActivityEventInput {
    pub kind: String,
    pub actor: String,
    pub action: String,
    pub account_id: String,
    pub path: String,
    pub result: String,
    pub details: String,
    pub ip: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct ActivityEventRow {
    pub id: i64,
    pub timestamp: i64,
    pub time: String,
    pub kind: String,
    pub actor: String,
    pub action: String,
    pub account_id: String,
    pub path: String,
    pub result: String,
    pub details: String,
    #[sqlx(default)]
    #[serde(default)]
    pub ip: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct IpBlacklistRow {
    pub ip: String,
    pub blocked_at: i64,
    pub blocked_time: String,
    pub blocked_by: String,
    pub reason: String,
    /// Number of failed Dashboard logins that caused an automatic block.
    /// Manual and legacy blocks use zero.
    pub failure_count: i64,
}

#[derive(Debug, Clone)]
pub struct TimetableEntryInput {
    pub weekday: String,
    pub period_start: i64,
    pub period_end: i64,
    pub start_time: String,
    pub end_time: String,
    pub course_name: String,
    pub teacher: String,
    pub classroom: String,
    pub course_code: String,
    pub source: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct TimetableEntryRow {
    pub id: i64,
    pub account_id: String,
    pub weekday: String,
    pub period_start: i64,
    pub period_end: i64,
    pub start_time: String,
    pub end_time: String,
    pub course_name: String,
    pub teacher: String,
    pub classroom: String,
    pub course_code: String,
    pub source: String,
    pub updated_at: i64,
    pub updated_time: String,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct TimetableTemplateRow {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub entries_json: String,
    pub created_by: String,
    pub updated_at: i64,
    pub updated_time: String,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct NotificationPreferenceRow {
    pub account_id: String,
    pub channel: String,
    pub notification_key: String,
    pub enabled: bool,
    pub updated_at: i64,
    pub updated_time: String,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct ClassReminderPreferenceRow {
    pub account_id: String,
    pub discord_enabled: bool,
    pub minutes_before: i64,
    pub updated_at: i64,
    pub updated_time: String,
}

/// Per-account timetable-driven rollcall detection settings.
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct DetectionSchedulePreferenceRow {
    pub account_id: String,
    pub enabled: bool,
    pub minutes_before: i64,
    pub minutes_after: i64,
    pub outside_interval_secs: i64,
    pub updated_at: i64,
    pub updated_time: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct DashboardSessionRow {
    pub token: String,
    pub account_id: String,
    pub is_admin: bool,
    pub must_change_password: bool,
    pub created_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct WebPushSubscriptionRow {
    pub account_id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct ClassReminderDueRow {
    pub timetable_id: i64,
    pub account_id: String,
    pub weekday: String,
    pub period_start: i64,
    pub period_end: i64,
    pub start_time: String,
    pub end_time: String,
    pub course_name: String,
    pub teacher: String,
    pub classroom: String,
    pub discord_user_id: String,
    pub minutes_before: i64,
}

// ─── AccountsDb ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AccountsDb {
    pool: SqlitePool,
    activity_tx: mpsc::Sender<ActivityEventInput>,
    material_tx: mpsc::Sender<MaterialEventInput>,
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
            .busy_timeout(Duration::from_secs(15))
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

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS accounts (
                id           TEXT PRIMARY KEY NOT NULL,
                provider     TEXT NOT NULL DEFAULT '',
                username     TEXT NOT NULL DEFAULT '',
                password     TEXT NOT NULL DEFAULT '',
                dashboard_password_hash TEXT NOT NULL DEFAULT '',
                enabled      INTEGER NOT NULL DEFAULT 1,
                line_user_id TEXT NOT NULL DEFAULT '',
                discord_user_id TEXT NOT NULL DEFAULT '',
                session_cookie TEXT NOT NULL DEFAULT '',
                is_admin INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 accounts 資料表失敗")?;

        ensure_column(&pool, "discord_user_id", "TEXT NOT NULL DEFAULT ''").await?;
        ensure_column(&pool, "session_cookie", "TEXT NOT NULL DEFAULT ''").await?;
        ensure_column(&pool, "is_admin", "INTEGER NOT NULL DEFAULT 0").await?;
        ensure_column(&pool, "dashboard_password_hash", "TEXT NOT NULL DEFAULT ''").await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS material_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                time TEXT NOT NULL,
                actor TEXT NOT NULL DEFAULT '',
                action TEXT NOT NULL DEFAULT '',
                path TEXT NOT NULL DEFAULT '',
                folder TEXT NOT NULL DEFAULT '',
                file_name TEXT NOT NULL DEFAULT '',
                size INTEGER NOT NULL DEFAULT 0,
                size_label TEXT NOT NULL DEFAULT '',
                result TEXT NOT NULL DEFAULT '',
                details TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 material_events 資料表失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_material_events_id ON material_events(id DESC)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 material_events index 失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS activity_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL DEFAULT 0,
                time TEXT NOT NULL DEFAULT '',
                kind TEXT NOT NULL DEFAULT '',
                actor TEXT NOT NULL DEFAULT '',
                action TEXT NOT NULL DEFAULT '',
                account_id TEXT NOT NULL DEFAULT '',
                path TEXT NOT NULL DEFAULT '',
                result TEXT NOT NULL DEFAULT '',
                details TEXT NOT NULL DEFAULT '',
                ip TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 activity_events 資料表失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_activity_events_kind_timestamp ON activity_events(kind, timestamp DESC)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 activity_events kind/timestamp index 失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_activity_events_action_timestamp ON activity_events(action, timestamp DESC)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 activity_events action/timestamp index 失敗")?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_activity_events_kind_id ON activity_events(kind, id DESC)")
            .execute(&pool).await.into_diagnostic().wrap_err("初始化 activity_events kind/id index 失敗")?;
        ensure_table_column(&pool, "activity_events", "ip", "TEXT NOT NULL DEFAULT ''").await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ip_blacklist (
                ip TEXT PRIMARY KEY NOT NULL,
                blocked_at INTEGER NOT NULL DEFAULT 0,
                blocked_time TEXT NOT NULL DEFAULT '',
                blocked_by TEXT NOT NULL DEFAULT '',
                reason TEXT NOT NULL DEFAULT '',
                failure_count INTEGER NOT NULL DEFAULT 0,
                blocked_account_id TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 ip_blacklist 資料表失敗")?;
        // Migration for installations that created ip_blacklist before failed
        // login counts were recorded with each automatic block.
        ensure_table_column(
            &pool,
            "ip_blacklist",
            "failure_count",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        ensure_table_column(
            &pool,
            "ip_blacklist",
            "blocked_account_id",
            "TEXT NOT NULL DEFAULT ''",
        )
        .await?;
        // Backfill automatic blocks created before blocked_account_id existed,
        // using the matching security activity record when available.
        let _ = sqlx::query(
            "UPDATE ip_blacklist
             SET blocked_account_id = COALESCE((
               SELECT account_id FROM activity_events
               WHERE action = 'ip_blacklist_auto_block'
                 AND path = ip_blacklist.ip
                 AND account_id <> ''
               ORDER BY id DESC LIMIT 1
             ), '')
             WHERE blocked_by = 'system:auto-login-failure'
               AND blocked_account_id = ''",
        )
        .execute(&pool)
        .await;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_ip_blacklist_blocked_at ON ip_blacklist(blocked_at DESC)")
            .execute(&pool)
            .await
            .into_diagnostic()
            .wrap_err("初始化 ip_blacklist index 失敗")?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_accounts_username_provider ON accounts(username, provider, id)")
            .execute(&pool).await.into_diagnostic().wrap_err("初始化 accounts username/provider index 失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS passkey_credentials (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NOT NULL DEFAULT '',
                credential_id TEXT NOT NULL DEFAULT '',
                credential_json TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0,
                last_used_at INTEGER NOT NULL DEFAULT 0,
                UNIQUE(account_id, credential_id)
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 passkey_credentials 資料表失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_passkey_credentials_account ON passkey_credentials(account_id)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 passkey_credentials account index 失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_passkey_credentials_credential ON passkey_credentials(credential_id)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 passkey_credentials credential index 失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS timetable_entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NOT NULL DEFAULT '',
                weekday TEXT NOT NULL DEFAULT '',
                period_start INTEGER NOT NULL DEFAULT 0,
                period_end INTEGER NOT NULL DEFAULT 0,
                start_time TEXT NOT NULL DEFAULT '',
                end_time TEXT NOT NULL DEFAULT '',
                course_name TEXT NOT NULL DEFAULT '',
                teacher TEXT NOT NULL DEFAULT '',
                classroom TEXT NOT NULL DEFAULT '',
                course_code TEXT NOT NULL DEFAULT '',
                source TEXT NOT NULL DEFAULT '',
                updated_at INTEGER NOT NULL DEFAULT 0,
                updated_time TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 timetable_entries 資料表失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_timetable_entries_account_weekday ON timetable_entries(account_id, weekday, period_start)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 timetable_entries account index 失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_timetable_entries_updated ON timetable_entries(updated_at DESC)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 timetable_entries updated index 失敗")?;

        // PERFORMANCE_CLASS_REMINDER_INDEX_V1:
        // The class reminder scheduler filters by weekday, then orders by
        // account/start time. This covering order avoids scanning every account's
        // full timetable as the database grows.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_timetable_entries_weekday_account_start
             ON timetable_entries(weekday, account_id, start_time, period_start)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 timetable_entries weekday/account/start index 失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS timetable_templates (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                entries_json TEXT NOT NULL DEFAULT '[]',
                created_by TEXT NOT NULL DEFAULT '',
                updated_at INTEGER NOT NULL DEFAULT 0,
                updated_time TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 timetable_templates 資料表失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_timetable_templates_updated ON timetable_templates(updated_at DESC)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 timetable_templates updated index 失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS notification_preferences (
                account_id TEXT NOT NULL DEFAULT '',
                channel TEXT NOT NULL DEFAULT '',
                notification_key TEXT NOT NULL DEFAULT '',
                enabled INTEGER NOT NULL DEFAULT 1,
                updated_at INTEGER NOT NULL DEFAULT 0,
                updated_time TEXT NOT NULL DEFAULT '',
                PRIMARY KEY(account_id, channel, notification_key)
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 notification_preferences 資料表失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_notification_preferences_account_channel ON notification_preferences(account_id, channel)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 notification_preferences index 失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS class_reminder_preferences (
                account_id TEXT PRIMARY KEY NOT NULL,
                discord_enabled INTEGER NOT NULL DEFAULT 1,
                minutes_before INTEGER NOT NULL DEFAULT 10,
                updated_at INTEGER NOT NULL DEFAULT 0,
                updated_time TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 class_reminder_preferences 資料表失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS detection_schedule_preferences (
                account_id TEXT PRIMARY KEY NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                minutes_before INTEGER NOT NULL DEFAULT 10,
                minutes_after INTEGER NOT NULL DEFAULT 10,
                outside_interval_secs INTEGER NOT NULL DEFAULT 300,
                updated_at INTEGER NOT NULL DEFAULT 0,
                updated_time TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 detection_schedule_preferences 資料表失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS web_push_settings (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 web_push_settings 資料表失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS web_push_subscriptions (
                endpoint TEXT PRIMARY KEY NOT NULL,
                account_id TEXT NOT NULL DEFAULT '',
                p256dh TEXT NOT NULL DEFAULT '',
                auth TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 web_push_subscriptions 資料表失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_web_push_subscriptions_account
             ON web_push_subscriptions(account_id)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 web_push_subscriptions index 失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS class_reminder_sent (
                account_id TEXT NOT NULL DEFAULT '',
                reminder_key TEXT NOT NULL DEFAULT '',
                sent_at INTEGER NOT NULL DEFAULT 0,
                sent_time TEXT NOT NULL DEFAULT '',
                PRIMARY KEY(account_id, reminder_key)
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 class_reminder_sent 資料表失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_class_reminder_sent_at ON class_reminder_sent(sent_at)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 class_reminder_sent index 失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS dashboard_sessions (
                token TEXT PRIMARY KEY NOT NULL,
                account_id TEXT NOT NULL DEFAULT '',
                is_admin INTEGER NOT NULL DEFAULT 0,
                must_change_password INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT 0,
                expires_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 dashboard_sessions 資料表失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_dashboard_sessions_expires_at ON dashboard_sessions(expires_at)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 dashboard_sessions expires_at index 失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_dashboard_sessions_account_id ON dashboard_sessions(account_id)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 dashboard_sessions account_id index 失敗")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS homework_reminder_sent (
                account_id TEXT NOT NULL DEFAULT '',
                homework_id TEXT NOT NULL DEFAULT '',
                stage TEXT NOT NULL DEFAULT '',
                deadline_timestamp INTEGER NOT NULL DEFAULT 0,
                sent_at INTEGER NOT NULL DEFAULT 0,
                sent_time TEXT NOT NULL DEFAULT '',
                PRIMARY KEY(account_id, homework_id, stage)
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 homework_reminder_sent 資料表失敗")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_homework_reminder_sent_at ON homework_reminder_sent(sent_at)",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 homework_reminder_sent index 失敗")?;

        // HOMEWORK_SNAPSHOT_V1
        // 保存每個帳號最近一次成功抓取的作業清單，讓作業頁一開啟就能立即顯示
        // 上次的結果（stale-while-revalidate），不必空等 Tronclass 回應。
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS homework_snapshot (
                account_id  TEXT PRIMARY KEY NOT NULL,
                items_json  TEXT NOT NULL DEFAULT '[]',
                item_count  INTEGER NOT NULL DEFAULT 0,
                fetched_at  INTEGER NOT NULL DEFAULT 0,
                fetched_time TEXT NOT NULL DEFAULT ''
            )",
        )
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 homework_snapshot 資料表失敗")?;

        let (activity_tx, activity_rx) = mpsc::channel(1024);
        let (material_tx, material_rx) = mpsc::channel(1024);

        let activity_pool = pool.clone();
        tokio::spawn(activity_event_batch_writer(activity_pool, activity_rx));

        let material_pool = pool.clone();
        tokio::spawn(material_event_batch_writer(material_pool, material_rx));

        Ok(Self {
            pool,
            activity_tx,
            material_tx,
        })
    }

    pub async fn record_material_event(&self, mut event: MaterialEventInput) -> Result<()> {
        event.actor = truncate_event_text(event.actor, EVENT_IDENTITY_MAX_BYTES);
        event.action = truncate_event_text(event.action, EVENT_ACTION_MAX_BYTES);
        event.path = truncate_event_text(event.path, EVENT_PATH_MAX_BYTES);
        event.folder = truncate_event_text(event.folder, EVENT_PATH_MAX_BYTES);
        event.file_name = truncate_event_text(event.file_name, EVENT_FILE_NAME_MAX_BYTES);
        event.result = truncate_event_text(event.result, EVENT_RESULT_MAX_BYTES);
        event.details = truncate_event_text(event.details, EVENT_DETAILS_MAX_BYTES);

        match self.material_tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(event)) => {
                insert_material_events_batch(&self.pool, &[event]).await
            }
            Err(mpsc::error::TrySendError::Closed(event)) => {
                insert_material_events_batch(&self.pool, &[event]).await
            }
        }
    }

    pub async fn list_material_events(&self, limit: i64) -> Result<Vec<MaterialEventRow>> {
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, MaterialEventRow>(
            "SELECT id, timestamp, time, actor, action, path, folder, file_name, size, size_label, result, details
             FROM material_events ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取教材操作紀錄失敗")?;
        Ok(rows)
    }

    // ── 查詢 ──────────────────────────────────────────────────────────────────

    /// 取得所有帳號（依 id 排序）。
    pub async fn list(&self) -> Result<Vec<RawAccountConfig>> {
        let rows = sqlx::query_as::<_, AccountRow>(
            "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash
             FROM accounts ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("列出帳號失敗")?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// 以 id 取得單一帳號。
    pub async fn get(&self, id: &str) -> Result<Option<RawAccountConfig>> {
        let row = sqlx::query_as::<_, AccountRow>(
            "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash
             FROM accounts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("取得帳號 `{id}` 失敗"))?;

        Ok(row.map(Into::into))
    }

    /// 以 Tronclass username 查詢帳號；provider 可用來消除同 username 多 provider 的歧義。
    pub async fn find_by_username(
        &self,
        username: &str,
        provider: Option<&str>,
    ) -> Result<Vec<RawAccountConfig>> {
        let rows = if let Some(provider) = provider.filter(|v| !v.trim().is_empty()) {
            sqlx::query_as::<_, AccountRow>(
                "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash
                 FROM accounts WHERE username = ? AND provider = ? ORDER BY id LIMIT ?",
            )
            .bind(username)
            .bind(provider)
            .bind(MAX_USERNAME_MATCH_ROWS)
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("以 username `{username}` 查詢帳號失敗"))?
        } else {
            sqlx::query_as::<_, AccountRow>(
                "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash
                 FROM accounts WHERE username = ? ORDER BY id LIMIT ?",
            )
            .bind(username)
            .bind(MAX_USERNAME_MATCH_ROWS)
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("以 username `{username}` 查詢帳號失敗"))?
        };

        Ok(rows.into_iter().map(Into::into).collect())
    }

    // ── 寫入 ──────────────────────────────────────────────────────────────────

    /// 新增帳號；若 id 已存在則回傳錯誤。
    pub async fn insert(&self, account: &RawAccountConfig) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (id, provider, username, password, dashboard_password_hash, enabled, line_user_id, discord_user_id, session_cookie, is_admin)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&account.id)
        .bind(&account.provider)
        .bind(&account.username)
        .bind(&account.password)
        .bind(&account.dashboard_password_hash)
        .bind(account.enabled)
        .bind(&account.line_user_id)
        .bind(&account.discord_user_id)
        .bind(&account.session_cookie)
        .bind(account.is_admin)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("新增帳號 `{}` 失敗", account.id))?;
        Ok(())
    }

    /// 新增或更新帳號（upsert）。
    pub async fn upsert(&self, account: &RawAccountConfig) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (id, provider, username, password, dashboard_password_hash, enabled, line_user_id, discord_user_id, session_cookie, is_admin)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               provider      = excluded.provider,
               username      = excluded.username,
               password      = excluded.password,
               dashboard_password_hash = excluded.dashboard_password_hash,
               enabled       = excluded.enabled,
               line_user_id  = excluded.line_user_id,
               discord_user_id = excluded.discord_user_id,
               session_cookie = excluded.session_cookie,
               is_admin = excluded.is_admin",
        )
        .bind(&account.id)
        .bind(&account.provider)
        .bind(&account.username)
        .bind(&account.password)
        .bind(&account.dashboard_password_hash)
        .bind(account.enabled)
        .bind(&account.line_user_id)
        .bind(&account.discord_user_id)
        .bind(&account.session_cookie)
        .bind(account.is_admin)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("Upsert 帳號 `{}` 失敗", account.id))?;
        Ok(())
    }

    /// 刪除帳號，回傳是否找到並刪除。
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let _ = self.delete_dashboard_sessions_by_account(id).await;
        let result = sqlx::query("DELETE FROM accounts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("刪除帳號 `{id}` 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 設定帳號啟用狀態，回傳是否找到並更新。
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET enabled = ? WHERE id = ?")
            .bind(enabled)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` 啟用狀態失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 更新帳號綁定的 Line User ID，回傳是否找到並更新。
    pub async fn set_line_user_id(&self, id: &str, line_user_id: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET line_user_id = ? WHERE id = ?")
            .bind(line_user_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` Line User ID 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 更新帳號綁定的 Discord User ID，回傳是否找到並更新。
    pub async fn set_discord_user_id(&self, id: &str, discord_user_id: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET discord_user_id = ? WHERE id = ?")
            .bind(discord_user_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` Discord User ID 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 更新帳號最近一次成功登入的 session cookie，回傳是否找到並更新。
    pub async fn set_session_cookie(&self, id: &str, session_cookie: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET session_cookie = ? WHERE id = ?")
            .bind(session_cookie)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` session cookie 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 儲存獨立 Dashboard 密碼的慢雜湊驗證字串。
    ///
    /// 這個欄位絕不取代 `password`；後者仍是 Tronclass provider 的登入憑證。
    pub async fn set_dashboard_password_hash(
        &self,
        id: &str,
        dashboard_password_hash: &str,
    ) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET dashboard_password_hash = ? WHERE id = ?")
            .bind(dashboard_password_hash)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` Dashboard 密碼失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 設定 Web 管理頁管理員權限，回傳是否找到並更新。
    pub async fn set_admin(&self, id: &str, is_admin: bool) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET is_admin = ? WHERE id = ?")
            .bind(is_admin)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` 管理員權限失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 以 Tronclass username 更新 Discord User ID。
    pub async fn set_discord_user_id_by_username(
        &self,
        username: &str,
        provider: Option<&str>,
        discord_user_id: &str,
    ) -> Result<UsernameUpdateResult> {
        let matches = self.find_by_username(username, provider).await?;
        match matches.as_slice() {
            [] => Ok(UsernameUpdateResult::NotFound),
            [account] => {
                self.set_discord_user_id(&account.id, discord_user_id)
                    .await?;
                Ok(UsernameUpdateResult::Updated {
                    account_id: account.id.clone(),
                })
            }
            accounts => Ok(UsernameUpdateResult::Ambiguous {
                account_ids: accounts.iter().map(|account| account.id.clone()).collect(),
            }),
        }
    }

    pub async fn record_activity_event(&self, mut event: ActivityEventInput) -> Result<()> {
        event.kind = truncate_event_text(event.kind, EVENT_ACTION_MAX_BYTES);
        event.actor = truncate_event_text(event.actor, EVENT_IDENTITY_MAX_BYTES);
        event.action = truncate_event_text(event.action, EVENT_ACTION_MAX_BYTES);
        event.account_id = truncate_event_text(event.account_id, EVENT_IDENTITY_MAX_BYTES);
        event.path = truncate_event_text(event.path, EVENT_PATH_MAX_BYTES);
        event.result = truncate_event_text(event.result, EVENT_RESULT_MAX_BYTES);
        event.details = truncate_event_text(event.details, EVENT_DETAILS_MAX_BYTES);
        event.ip = truncate_event_text(event.ip, EVENT_IP_MAX_BYTES);

        match self.activity_tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(event)) => {
                insert_activity_events_batch(&self.pool, &[event]).await
            }
            Err(mpsc::error::TrySendError::Closed(event)) => {
                insert_activity_events_batch(&self.pool, &[event]).await
            }
        }
    }

    pub async fn list_activity_events(
        &self,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ActivityEventRow>> {
        let limit = limit.clamp(1, 1000);
        let rows = if let Some(kind) = kind.map(str::trim).filter(|v| !v.is_empty() && *v != "all")
        {
            sqlx::query_as::<_, ActivityEventRow>(
                "SELECT id, timestamp, time, kind, actor, action, account_id, path, result, details, ip
                 FROM activity_events WHERE kind = ? ORDER BY id DESC LIMIT ?",
            )
            .bind(kind)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("讀取 activity_events 失敗")?
        } else {
            sqlx::query_as::<_, ActivityEventRow>(
                "SELECT id, timestamp, time, kind, actor, action, account_id, path, result, details, ip
                 FROM activity_events ORDER BY id DESC LIMIT ?",
            )
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("讀取 activity_events 失敗")?
        };
        Ok(rows)
    }

    pub async fn is_ip_blacklisted(&self, ip: &str) -> Result<bool> {
        let exists: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM ip_blacklist WHERE ip = ?)",
        )
        .bind(ip.trim())
        .fetch_one(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("檢查 IP 黑名單失敗")?;
        Ok(exists != 0)
    }

    pub async fn list_ip_blacklist(&self, limit: i64) -> Result<Vec<IpBlacklistRow>> {
        let limit = limit.clamp(1, 4096);
        sqlx::query_as::<_, IpBlacklistRow>(
            "SELECT ip, blocked_at, blocked_time, blocked_by, reason, failure_count
             FROM ip_blacklist ORDER BY blocked_at DESC, ip ASC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取 IP 黑名單失敗")
    }

    pub async fn block_ip(&self, ip: &str, blocked_by: &str, reason: &str) -> Result<bool> {
        self.block_ip_with_failure_count(ip, blocked_by, reason, 0).await
    }

    pub async fn block_ip_with_failure_count(
        &self,
        ip: &str,
        blocked_by: &str,
        reason: &str,
        failure_count: u32,
    ) -> Result<bool> {
        let now = Local::now();
        let result = sqlx::query(
            "INSERT OR IGNORE INTO ip_blacklist
                (ip, blocked_at, blocked_time, blocked_by, reason, failure_count)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(ip.trim())
        .bind(now.timestamp())
        .bind(now.format("%Y-%m-%d %H:%M:%S").to_string())
        .bind(truncate_event_text(blocked_by.to_string(), EVENT_IDENTITY_MAX_BYTES))
        .bind(truncate_event_text(reason.to_string(), EVENT_DETAILS_MAX_BYTES))
        .bind(i64::from(failure_count))
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 IP 黑名單失敗")?;
        Ok(result.rows_affected() > 0)
    }

    /// Mark a newly-created automatic block as belonging to the Dashboard
    /// account that triggered it. This is used to authorize a self-service
    /// Discord unblock without granting general blacklist access.
    pub async fn set_auto_block_owner(&self, ip: &str, account_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE ip_blacklist SET blocked_account_id = ?
             WHERE ip = ? AND blocked_by = 'system:auto-login-failure'",
        )
        .bind(account_id.trim())
        .bind(ip.trim())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入自動封鎖帳號識別失敗")?;
        Ok(())
    }

    /// Returns the owning account for an automatic IP block, if one was
    /// recorded. This allows legacy Discord buttons to enforce the same
    /// self-service ownership rule without trusting their component payload.
    pub async fn auto_block_owner_for_ip(&self, ip: &str) -> Result<Option<String>> {
        sqlx::query_scalar(
            "SELECT blocked_account_id FROM ip_blacklist
             WHERE ip = ? AND blocked_by = 'system:auto-login-failure'
               AND blocked_account_id <> ''",
        )
        .bind(ip.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取自動 IP 封鎖帳號識別失敗")
    }

    /// Removes an automatic block only when the Discord user is bound to the
    /// account that triggered it. The activity-event fallback safely supports
    /// blocks created before blocked_account_id was introduced.
    pub async fn unblock_automatic_ip_for_discord_user(
        &self,
        ip: &str,
        discord_user_id: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM ip_blacklist
             WHERE ip = ? AND blocked_by = 'system:auto-login-failure'
               AND (
                 EXISTS (
                   SELECT 1 FROM accounts a
                   WHERE a.discord_user_id = ?
                     AND (a.id = ip_blacklist.blocked_account_id
                          OR a.username = ip_blacklist.blocked_account_id)
                 )
                 OR EXISTS (
                   SELECT 1 FROM activity_events e
                   JOIN accounts a ON (a.id = e.account_id OR a.username = e.account_id)
                   WHERE e.action = 'ip_blacklist_auto_block'
                     AND e.path = ip_blacklist.ip
                     AND a.discord_user_id = ?
                 )
               )",
        )
        .bind(ip.trim())
        .bind(discord_user_id.trim())
        .bind(discord_user_id.trim())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("依 Discord 帳號解除自己的自動 IP 封鎖失敗")?;
        Ok(result.rows_affected() > 0)
    }

    /// Removes only an automatic block owned by the supplied account.
    pub async fn unblock_own_automatic_ip(&self, ip: &str, account_id: &str) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM ip_blacklist
             WHERE ip = ? AND blocked_by = 'system:auto-login-failure'
               AND blocked_account_id = ?",
        )
        .bind(ip.trim())
        .bind(account_id.trim())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("解除自己的自動 IP 封鎖失敗")?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn unblock_ip(&self, ip: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM ip_blacklist WHERE ip = ?")
            .bind(ip.trim())
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("解除 IP 黑名單失敗")?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_passkey_credentials(
        &self,
        account_id: &str,
    ) -> Result<Vec<PasskeyCredential>> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT credential_json FROM passkey_credentials
             WHERE account_id = ? AND length(CAST(credential_json AS BLOB)) <= ?
             ORDER BY id ASC LIMIT ?",
        )
        .bind(account_id)
        .bind(MAX_PASSKEY_CREDENTIAL_JSON_BYTES as i64)
        .bind((MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT + 1) as i64)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` passkey 憑證失敗"))?;

        let mut credentials = Vec::with_capacity(rows.len());
        for (json,) in rows {
            let credential = serde_json::from_str::<PasskeyCredential>(&json)
                .into_diagnostic()
                .wrap_err("解析 passkey credential JSON 失敗")?;
            credentials.push(credential);
        }
        Ok(credentials)
    }

    pub async fn upsert_passkey_credential(
        &self,
        account_id: &str,
        credential: &PasskeyCredential,
    ) -> Result<()> {
        if account_id.len() > EVENT_IDENTITY_MAX_BYTES {
            return Err(miette::miette!("passkey account_id is too long"));
        }
        let credential_id = credential.id.to_b64url();
        if credential_id.len() > 4096 {
            return Err(miette::miette!("passkey credential id is too long"));
        }
        let credential_json = serde_json::to_string(credential)
            .into_diagnostic()
            .wrap_err("序列化 passkey credential 失敗")?;
        if credential_json.len() > MAX_PASSKEY_CREDENTIAL_JSON_BYTES {
            return Err(miette::miette!("passkey credential payload is too large"));
        }
        let now = chrono::Local::now().timestamp();
        let result = sqlx::query(
            "INSERT INTO passkey_credentials (account_id, credential_id, credential_json, created_at, updated_at, last_used_at)
             SELECT ?, ?, ?, ?, ?, ?
             WHERE EXISTS (
                 SELECT 1 FROM passkey_credentials WHERE account_id = ? AND credential_id = ?
             ) OR (
                 SELECT COUNT(*) FROM passkey_credentials WHERE account_id = ?
             ) < ?
             ON CONFLICT(account_id, credential_id) DO UPDATE SET
               credential_json = excluded.credential_json,
               updated_at = excluded.updated_at,
               last_used_at = excluded.last_used_at",
        )
        .bind(account_id)
        .bind(&credential_id)
        .bind(credential_json)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(account_id)
        .bind(&credential_id)
        .bind(account_id)
        .bind(MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT as i64)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("寫入 `{account_id}` passkey credential 失敗"))?;
        if result.rows_affected() == 0 {
            return Err(miette::miette!(
                "passkey credential limit reached; max {}",
                MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT
            ));
        }
        Ok(())
    }

    pub async fn count_passkey_credentials(&self, account_id: &str) -> Result<i64> {
        let row = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM passkey_credentials WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("計算 `{account_id}` passkey credentials 失敗"))?;
        Ok(row.0)
    }

    pub async fn replace_timetable_entries(
        &self,
        account_id: &str,
        entries: Vec<TimetableEntryInput>,
    ) -> Result<usize> {
        let account_id = account_id.trim();
        let now = Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();

        // PERFORMANCE_TIMETABLE_IMPORT_V1:
        // Replace the full timetable in one SQLite transaction.  Without this,
        // every row is its own write transaction, so importing a few hundred rows
        // pays fsync/lock overhead repeatedly and can leave a partially imported
        // timetable if an insert fails midway.
        let mut tx = self
            .pool
            .begin()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("開始 `{account_id}` 課表交易失敗"))?;

        sqlx::query("DELETE FROM timetable_entries WHERE account_id = ?")
            .bind(account_id)
            .execute(&mut *tx)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("清除 `{account_id}` 舊課表失敗"))?;

        let mut inserted = 0usize;
        for entry in entries {
            sqlx::query(
                "INSERT INTO timetable_entries (
                    account_id, weekday, period_start, period_end, start_time, end_time,
                    course_name, teacher, classroom, course_code, source, updated_at, updated_time
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(account_id)
            .bind(entry.weekday)
            .bind(entry.period_start)
            .bind(entry.period_end)
            .bind(entry.start_time)
            .bind(entry.end_time)
            .bind(entry.course_name)
            .bind(entry.teacher)
            .bind(entry.classroom)
            .bind(entry.course_code)
            .bind(entry.source)
            .bind(timestamp)
            .bind(&time_label)
            .execute(&mut *tx)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("寫入 `{account_id}` 課表失敗"))?;
            inserted += 1;
        }

        tx.commit()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("提交 `{account_id}` 課表交易失敗"))?;
        Ok(inserted)
    }

    pub async fn clear_timetable_entries(&self, account_id: &str) -> Result<u64> {
        let result = sqlx::query("DELETE FROM timetable_entries WHERE account_id = ?")
            .bind(account_id.trim())
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("清除 `{account_id}` 課表失敗"))?;
        Ok(result.rows_affected())
    }

    pub async fn list_timetable_entries_for_account(
        &self,
        account_id: &str,
    ) -> Result<Vec<TimetableEntryRow>> {
        let rows = sqlx::query_as::<_, TimetableEntryRow>(
            "SELECT id, account_id, weekday, period_start, period_end, start_time, end_time,
                    course_name, teacher, classroom, course_code, source, updated_at, updated_time
             FROM timetable_entries
             WHERE account_id = ?
             ORDER BY
                CASE weekday
                    WHEN 'mon' THEN 1 WHEN 'tue' THEN 2 WHEN 'wed' THEN 3 WHEN 'thu' THEN 4
                    WHEN 'fri' THEN 5 WHEN 'sat' THEN 6 WHEN 'sun' THEN 7 ELSE 9
                END,
                period_start,
                start_time
             LIMIT ?",
        )
        .bind(account_id.trim())
        .bind((MAX_TIMETABLE_ENTRIES_PER_ACCOUNT + 1) as i64)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 課表失敗"))?;
        Ok(rows)
    }

    pub async fn list_all_timetable_entries(&self) -> Result<Vec<TimetableEntryRow>> {
        let rows = sqlx::query_as::<_, TimetableEntryRow>(
            "SELECT id, account_id, weekday, period_start, period_end, start_time, end_time,
                    course_name, teacher, classroom, course_code, source, updated_at, updated_time
             FROM timetable_entries
             ORDER BY account_id,
                CASE weekday
                    WHEN 'mon' THEN 1 WHEN 'tue' THEN 2 WHEN 'wed' THEN 3 WHEN 'thu' THEN 4
                    WHEN 'fri' THEN 5 WHEN 'sat' THEN 6 WHEN 'sun' THEN 7 ELSE 9
                END,
                period_start,
                start_time
             LIMIT ?",
        )
        .bind((MAX_TIMETABLE_LIST_ALL_ROWS + 1) as i64)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取所有課表失敗")?;
        Ok(rows)
    }

    pub async fn save_timetable_template(
        &self,
        name: &str,
        description: &str,
        entries_json: &str,
        created_by: &str,
    ) -> Result<()> {
        let name = name.trim();
        let description = description.trim();
        let created_by = created_by.trim();
        if name.is_empty()
            || name.len() > MAX_TIMETABLE_TEMPLATE_NAME_BYTES
            || description.len() > MAX_TIMETABLE_TEMPLATE_DESCRIPTION_BYTES
            || entries_json.len() > MAX_TIMETABLE_TEMPLATE_JSON_BYTES
            || created_by.len() > MAX_TIMETABLE_TEMPLATE_CREATOR_BYTES
        {
            return Err(miette::miette!("課表模板欄位過長或名稱空白"));
        }

        let now = chrono::Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();
        let mut tx = self
            .pool
            .begin()
            .await
            .into_diagnostic()
            .wrap_err("開始儲存課表模板交易失敗")?;
        let exists =
            sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM timetable_templates WHERE name = ?")
                .bind(name)
                .fetch_one(&mut *tx)
                .await
                .into_diagnostic()
                .wrap_err_with(|| format!("檢查課表模板 `{name}` 失敗"))?
                .0
                > 0;
        if !exists {
            let count = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM timetable_templates")
                .fetch_one(&mut *tx)
                .await
                .into_diagnostic()
                .wrap_err("計算課表模板數量失敗")?
                .0;
            if count >= MAX_TIMETABLE_TEMPLATES as i64 {
                return Err(miette::miette!(
                    "課表模板數量已達上限 {MAX_TIMETABLE_TEMPLATES}"
                ));
            }
        }

        sqlx::query(
            "INSERT INTO timetable_templates (name, description, entries_json, created_by, updated_at, updated_time)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(name) DO UPDATE SET
               description = excluded.description,
               entries_json = excluded.entries_json,
               created_by = excluded.created_by,
               updated_at = excluded.updated_at,
               updated_time = excluded.updated_time",
        )
        .bind(name)
        .bind(description)
        .bind(entries_json)
        .bind(created_by)
        .bind(timestamp)
        .bind(time_label)
        .execute(&mut *tx)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("儲存課表模板 `{name}` 失敗"))?;
        tx.commit()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("提交課表模板 `{name}` 失敗"))?;
        Ok(())
    }

    pub async fn list_timetable_templates(&self) -> Result<Vec<TimetableTemplateRow>> {
        let rows = sqlx::query_as::<_, TimetableTemplateRow>(
            "SELECT id, name, description, entries_json, created_by, updated_at, updated_time
             FROM timetable_templates
             WHERE length(CAST(name AS BLOB)) <= ?
               AND length(CAST(description AS BLOB)) <= ?
               AND length(CAST(entries_json AS BLOB)) <= ?
               AND length(CAST(created_by AS BLOB)) <= ?
             ORDER BY updated_at DESC, name ASC
             LIMIT ?",
        )
        .bind(MAX_TIMETABLE_TEMPLATE_NAME_BYTES as i64)
        .bind(MAX_TIMETABLE_TEMPLATE_DESCRIPTION_BYTES as i64)
        .bind(MAX_TIMETABLE_TEMPLATE_JSON_BYTES as i64)
        .bind(MAX_TIMETABLE_TEMPLATE_CREATOR_BYTES as i64)
        .bind((MAX_TIMETABLE_TEMPLATES + 1) as i64)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取課表模板失敗")?;
        Ok(rows)
    }

    pub async fn get_timetable_template(&self, name: &str) -> Result<Option<TimetableTemplateRow>> {
        let row = sqlx::query_as::<_, TimetableTemplateRow>(
            "SELECT id, name, description, entries_json, created_by, updated_at, updated_time
             FROM timetable_templates
             WHERE name = ?
               AND length(CAST(name AS BLOB)) <= ?
               AND length(CAST(description AS BLOB)) <= ?
               AND length(CAST(entries_json AS BLOB)) <= ?
               AND length(CAST(created_by AS BLOB)) <= ?",
        )
        .bind(name.trim())
        .bind(MAX_TIMETABLE_TEMPLATE_NAME_BYTES as i64)
        .bind(MAX_TIMETABLE_TEMPLATE_DESCRIPTION_BYTES as i64)
        .bind(MAX_TIMETABLE_TEMPLATE_JSON_BYTES as i64)
        .bind(MAX_TIMETABLE_TEMPLATE_CREATOR_BYTES as i64)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取課表模板 `{name}` 失敗"))?;
        Ok(row)
    }

    pub async fn list_notification_preferences(
        &self,
        account_id: &str,
        channel: &str,
    ) -> Result<Vec<NotificationPreferenceRow>> {
        let rows = sqlx::query_as::<_, NotificationPreferenceRow>(
            "SELECT account_id, channel, notification_key, enabled, updated_at, updated_time
             FROM notification_preferences
             WHERE account_id = ? AND channel = ?
             ORDER BY notification_key",
        )
        .bind(account_id.trim())
        .bind(channel.trim())
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 通知設定失敗"))?;
        Ok(rows)
    }

    pub async fn set_notification_preference(
        &self,
        account_id: &str,
        channel: &str,
        notification_key: &str,
        enabled: bool,
    ) -> Result<()> {
        let preferences = [(notification_key.trim().to_string(), enabled)];
        self.set_notification_preferences(account_id, channel, &preferences)
            .await
    }

    pub async fn set_notification_preferences(
        &self,
        account_id: &str,
        channel: &str,
        preferences: &[(String, bool)],
    ) -> Result<()> {
        let account_id = account_id.trim();
        let channel = channel.trim();
        if preferences.is_empty() {
            return Ok(());
        }

        // PERFORMANCE_NOTIFICATION_PREF_BATCH_V1:
        // Save all checkbox preferences in a single SQLite transaction. The
        // previous handler upserted each key separately, multiplying write-lock
        // and fsync overhead for one UI save action.
        let now = chrono::Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();
        let mut tx = self
            .pool
            .begin()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("開始 `{account_id}` 通知設定交易失敗"))?;

        for (notification_key, enabled) in preferences {
            sqlx::query(
                "INSERT INTO notification_preferences (account_id, channel, notification_key, enabled, updated_at, updated_time)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(account_id, channel, notification_key) DO UPDATE SET
                   enabled = excluded.enabled,
                   updated_at = excluded.updated_at,
                   updated_time = excluded.updated_time",
            )
            .bind(account_id)
            .bind(channel)
            .bind(notification_key.trim())
            .bind(*enabled)
            .bind(timestamp)
            .bind(&time_label)
            .execute(&mut *tx)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新 `{account_id}` 通知設定失敗"))?;
        }

        tx.commit()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("提交 `{account_id}` 通知設定交易失敗"))?;
        Ok(())
    }

    pub async fn notification_preference_enabled(
        &self,
        account_id: &str,
        channel: &str,
        notification_key: &str,
    ) -> Result<bool> {
        let row = sqlx::query_as::<_, (bool,)>(
            "SELECT enabled FROM notification_preferences
             WHERE account_id = ? AND channel = ? AND notification_key = ?",
        )
        .bind(account_id.trim())
        .bind(channel.trim())
        .bind(notification_key.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 通知設定失敗"))?;
        Ok(row.map(|(enabled,)| enabled).unwrap_or(true))
    }

    pub async fn get_web_push_setting(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query_as::<_, (String,)>(
            "SELECT value FROM web_push_settings WHERE key = ?",
        )
        .bind(key.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取 Web Push 設定失敗")?;
        Ok(row.map(|(value,)| value))
    }

    pub async fn set_web_push_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO web_push_settings (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key.trim())
        .bind(value)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("儲存 Web Push 設定失敗")?;
        Ok(())
    }

    pub async fn upsert_web_push_subscription(
        &self,
        account_id: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
    ) -> Result<()> {
        let now = chrono::Local::now().timestamp();
        sqlx::query(
            "INSERT INTO web_push_subscriptions
                (endpoint, account_id, p256dh, auth, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(endpoint) DO UPDATE SET
                account_id = excluded.account_id,
                p256dh = excluded.p256dh,
                auth = excluded.auth,
                updated_at = excluded.updated_at",
        )
        .bind(endpoint.trim())
        .bind(account_id.trim())
        .bind(p256dh.trim())
        .bind(auth.trim())
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("儲存 `{account_id}` Web Push 訂閱失敗"))?;
        Ok(())
    }

    pub async fn list_web_push_subscriptions(
        &self,
        account_id: &str,
    ) -> Result<Vec<WebPushSubscriptionRow>> {
        sqlx::query_as::<_, WebPushSubscriptionRow>(
            "SELECT account_id, endpoint, p256dh, auth
             FROM web_push_subscriptions WHERE account_id = ?
             ORDER BY updated_at DESC LIMIT 16",
        )
        .bind(account_id.trim())
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` Web Push 訂閱失敗"))
    }

    pub async fn count_web_push_subscriptions(&self, account_id: &str) -> Result<i64> {
        let (count,) = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM web_push_subscriptions WHERE account_id = ?",
        )
        .bind(account_id.trim())
        .fetch_one(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("統計 Web Push 訂閱失敗")?;
        Ok(count)
    }

    pub async fn clear_all_web_push_subscriptions(&self) -> Result<u64> {
        let result = sqlx::query("DELETE FROM web_push_subscriptions")
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("清除舊版 Web Push 自動綁定資料失敗")?;
        Ok(result.rows_affected())
    }

    pub async fn delete_web_push_subscription(
        &self,
        account_id: &str,
        endpoint: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM web_push_subscriptions WHERE account_id = ? AND endpoint = ?",
        )
        .bind(account_id.trim())
        .bind(endpoint.trim())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("刪除 Web Push 訂閱失敗")?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn get_detection_schedule_preference(
        &self,
        account_id: &str,
    ) -> Result<DetectionSchedulePreferenceRow> {
        let account_id = account_id.trim();
        let row = sqlx::query_as::<_, DetectionSchedulePreferenceRow>(
            "SELECT account_id, enabled, minutes_before, minutes_after,
                    outside_interval_secs, updated_at, updated_time
             FROM detection_schedule_preferences
             WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 點名偵測排程失敗"))?;

        Ok(row.unwrap_or_else(|| DetectionSchedulePreferenceRow {
            account_id: account_id.to_string(),
            enabled: true,
            minutes_before: 10,
            minutes_after: 10,
            outside_interval_secs: 300,
            updated_at: 0,
            updated_time: String::new(),
        }))
    }

    pub async fn set_detection_schedule_preference(
        &self,
        account_id: &str,
        enabled: bool,
        minutes_before: i64,
        minutes_after: i64,
        outside_interval_secs: i64,
    ) -> Result<()> {
        let account_id = account_id.trim();
        let minutes_before = minutes_before.clamp(0, 180);
        let minutes_after = minutes_after.clamp(0, 180);
        let outside_interval_secs = outside_interval_secs.clamp(60, 3600);
        let now = chrono::Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();
        sqlx::query(
            "INSERT INTO detection_schedule_preferences
                (account_id, enabled, minutes_before, minutes_after,
                 outside_interval_secs, updated_at, updated_time)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
                enabled = excluded.enabled,
                minutes_before = excluded.minutes_before,
                minutes_after = excluded.minutes_after,
                outside_interval_secs = excluded.outside_interval_secs,
                updated_at = excluded.updated_at,
                updated_time = excluded.updated_time",
        )
        .bind(account_id)
        .bind(enabled)
        .bind(minutes_before)
        .bind(minutes_after)
        .bind(outside_interval_secs)
        .bind(timestamp)
        .bind(&time_label)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("更新 `{account_id}` 點名偵測排程失敗"))?;
        Ok(())
    }

    pub async fn get_class_reminder_preference(
        &self,
        account_id: &str,
    ) -> Result<ClassReminderPreferenceRow> {
        let account_id = account_id.trim();
        let row = sqlx::query_as::<_, ClassReminderPreferenceRow>(
            "SELECT account_id, discord_enabled, minutes_before, updated_at, updated_time
             FROM class_reminder_preferences
             WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 上課提醒設定失敗"))?;

        Ok(row.unwrap_or_else(|| ClassReminderPreferenceRow {
            account_id: account_id.to_string(),
            discord_enabled: true,
            minutes_before: 10,
            updated_at: 0,
            updated_time: String::new(),
        }))
    }

    pub async fn set_class_reminder_preference(
        &self,
        account_id: &str,
        discord_enabled: bool,
        minutes_before: i64,
    ) -> Result<()> {
        let account_id = account_id.trim();
        let minutes_before = minutes_before.clamp(0, 180);
        let now = chrono::Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();
        sqlx::query(
            "INSERT INTO class_reminder_preferences (account_id, discord_enabled, minutes_before, updated_at, updated_time)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
                discord_enabled = excluded.discord_enabled,
                minutes_before = excluded.minutes_before,
                updated_at = excluded.updated_at,
                updated_time = excluded.updated_time",
        )
        .bind(account_id)
        .bind(discord_enabled)
        .bind(minutes_before)
        .bind(timestamp)
        .bind(&time_label)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("更新 `{account_id}` 上課提醒設定失敗"))?;
        Ok(())
    }

    pub async fn list_due_class_reminders_for_weekday(
        &self,
        weekday: &str,
    ) -> Result<Vec<ClassReminderDueRow>> {
        let rows = sqlx::query_as::<_, ClassReminderDueRow>(
            "SELECT
                t.id AS timetable_id,
                t.account_id AS account_id,
                t.weekday AS weekday,
                t.period_start AS period_start,
                t.period_end AS period_end,
                t.start_time AS start_time,
                t.end_time AS end_time,
                t.course_name AS course_name,
                t.teacher AS teacher,
                t.classroom AS classroom,
                a.discord_user_id AS discord_user_id,
                COALESCE(p.minutes_before, 10) AS minutes_before
             FROM timetable_entries t
             JOIN accounts a ON a.id = t.account_id
             LEFT JOIN class_reminder_preferences p ON p.account_id = t.account_id
             WHERE t.weekday = ?
               AND a.enabled = 1
               AND TRIM(a.discord_user_id) <> ''
               AND COALESCE(p.discord_enabled, 1) = 1
               AND TRIM(t.course_name) <> ''
             ORDER BY t.account_id, t.start_time, t.period_start",
        )
        .bind(weekday.trim())
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| {
            format!("讀取 `{weekday}` 上課提醒清單失敗；請確認 timetable_entries 課表表格已建立")
        })?;
        Ok(rows)
    }

    pub async fn mark_class_reminder_sent(
        &self,
        account_id: &str,
        reminder_key: &str,
    ) -> Result<bool> {
        let now = chrono::Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();
        let result = sqlx::query(
            "INSERT OR IGNORE INTO class_reminder_sent (account_id, reminder_key, sent_at, sent_time)
             VALUES (?, ?, ?, ?)",
        )
        .bind(account_id.trim())
        .bind(reminder_key.trim())
        .bind(timestamp)
        .bind(&time_label)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("記錄 `{account_id}` 上課提醒已送出失敗"))?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn cleanup_old_class_reminder_sent(&self, older_than_timestamp: i64) -> Result<u64> {
        let result = sqlx::query("DELETE FROM class_reminder_sent WHERE sent_at < ?")
            .bind(older_than_timestamp)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("清理舊上課提醒紀錄失敗")?;
        Ok(result.rows_affected())
    }

    // ── Dashboard Session 持久化 ──────────────────────────────────────────────

    /// 儲存或更新 Web Dashboard Session
    pub async fn insert_dashboard_session(
        &self,
        token: &str,
        account_id: &str,
        is_admin: bool,
        must_change_password: bool,
        expires_at: i64,
    ) -> Result<()> {
        let now = chrono::Local::now().timestamp();
        sqlx::query(
            "INSERT OR REPLACE INTO dashboard_sessions
                (token, account_id, is_admin, must_change_password, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(token.trim())
        .bind(account_id.trim())
        .bind(is_admin)
        .bind(must_change_password)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 dashboard_sessions 失敗")?;
        Ok(())
    }

    /// 依 Token 查詢單一 Dashboard Session
    pub async fn get_dashboard_session(&self, token: &str) -> Result<Option<DashboardSessionRow>> {
        sqlx::query_as::<_, DashboardSessionRow>(
            "SELECT token, account_id, is_admin, must_change_password, created_at, expires_at
             FROM dashboard_sessions WHERE token = ?",
        )
        .bind(token.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取 dashboard_sessions 失敗")
    }

    /// 列出所有尚未過期的 Dashboard Session
    pub async fn list_valid_dashboard_sessions(
        &self,
        now: i64,
    ) -> Result<Vec<DashboardSessionRow>> {
        sqlx::query_as::<_, DashboardSessionRow>(
            "SELECT token, account_id, is_admin, must_change_password, created_at, expires_at
             FROM dashboard_sessions WHERE expires_at > ?
             ORDER BY created_at ASC",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取有效 dashboard_sessions 失敗")
    }

    /// 刪除指定 Token 的 Dashboard Session
    pub async fn delete_dashboard_session(&self, token: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM dashboard_sessions WHERE token = ?")
            .bind(token.trim())
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("刪除 dashboard_session 失敗")?;
        Ok(result.rows_affected() > 0)
    }

    /// 刪除指定帳號的所有 Dashboard Session
    pub async fn delete_dashboard_sessions_by_account(&self, account_id: &str) -> Result<u64> {
        let result = sqlx::query("DELETE FROM dashboard_sessions WHERE account_id = ?")
            .bind(account_id.trim())
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("依帳號刪除 dashboard_sessions 失敗")?;
        Ok(result.rows_affected())
    }

    /// 清理所有已過期的 Dashboard Session
    pub async fn delete_expired_dashboard_sessions(&self, now: i64) -> Result<u64> {
        let result = sqlx::query("DELETE FROM dashboard_sessions WHERE expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("清理過期 dashboard_sessions 失敗")?;
        Ok(result.rows_affected())
    }

    // ── 作業截止提醒去重 ──────────────────────────────────────────────────────

    /// 檢查某個作業在指定階段（如 24h, 6h, 1h）是否已發送過提醒。
    /// 若作業截止時間被老師顯著延長，則自動重置歷史並回傳 false，讓倒數重新提醒。
    ///
    /// MEANINGFUL_EXTENSION_ONLY_V1
    /// 只有延期超過 [`MEANINGFUL_EXTENSION_SECS`] 才視為「新的一輪」。
    /// 否則老師只把截止時間往後挪幾分鐘（很常見），會把已發過的階梯紀錄清掉，
    /// 導致使用者在同一個階段收到重複的緊急提醒。
    pub async fn is_homework_reminder_sent(
        &self,
        account_id: &str,
        homework_id: &str,
        stage: &str,
        deadline_timestamp: i64,
    ) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT deadline_timestamp FROM homework_reminder_sent
             WHERE account_id = ? AND homework_id = ? AND stage = ?",
        )
        .bind(account_id.trim())
        .bind(homework_id.trim())
        .bind(stage.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢 homework_reminder_sent 失敗")?;

        match row {
            Some((stored_deadline,)) => {
                // 只有「顯著延期」才重置。微幅順延（例如往後挪 10 分鐘）
                // 不應清掉紀錄，否則同一階段會重複發送提醒。
                if deadline_timestamp.saturating_sub(stored_deadline) >= MEANINGFUL_EXTENSION_SECS {
                    let _ = sqlx::query(
                        "DELETE FROM homework_reminder_sent WHERE account_id = ? AND homework_id = ?",
                    )
                    .bind(account_id.trim())
                    .bind(homework_id.trim())
                    .execute(&self.pool)
                    .await;
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
            None => Ok(false),
        }
    }

    /// 標記某個作業的特定階梯提醒已送出
    pub async fn mark_homework_reminder_sent(
        &self,
        account_id: &str,
        homework_id: &str,
        stage: &str,
        deadline_timestamp: i64,
    ) -> Result<bool> {
        let now = chrono::Local::now();
        let sent_at = now.timestamp();
        let sent_time = now.format("%Y-%m-%d %H:%M:%S").to_string();

        let result = sqlx::query(
            "INSERT OR REPLACE INTO homework_reminder_sent
                (account_id, homework_id, stage, deadline_timestamp, sent_at, sent_time)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(account_id.trim())
        .bind(homework_id.trim())
        .bind(stage.trim())
        .bind(deadline_timestamp)
        .bind(sent_at)
        .bind(sent_time)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 homework_reminder_sent 失敗")?;

        Ok(result.rows_affected() > 0)
    }

    // ── 作業清單快照（開頁秒顯示用） ──────────────────────────────────────────

    /// 讀取帳號最近一次成功抓取的作業清單快照。
    ///
    /// 回傳 `(items_json, fetched_at)`；沒有快照時為 `None`。
    pub async fn get_homework_snapshot(&self, account_id: &str) -> Result<Option<(String, i64)>> {
        let row: Option<(String, i64)> = sqlx::query_as(
            "SELECT items_json, fetched_at FROM homework_snapshot WHERE account_id = ?",
        )
        .bind(account_id.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢 homework_snapshot 失敗")?;
        Ok(row)
    }

    /// 寫入／更新帳號的作業清單快照。
    pub async fn save_homework_snapshot(
        &self,
        account_id: &str,
        items_json: &str,
        item_count: i64,
    ) -> Result<()> {
        let now = chrono::Local::now();
        sqlx::query(
            "INSERT INTO homework_snapshot
                (account_id, items_json, item_count, fetched_at, fetched_time)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
                items_json = excluded.items_json,
                item_count = excluded.item_count,
                fetched_at = excluded.fetched_at,
                fetched_time = excluded.fetched_time",
        )
        .bind(account_id.trim())
        .bind(items_json)
        .bind(item_count)
        .bind(now.timestamp())
        .bind(now.format("%Y-%m-%d %H:%M:%S").to_string())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 homework_snapshot 失敗")?;
        Ok(())
    }

    /// 清理 14 天前的歷史作業提醒已送出紀錄
    pub async fn cleanup_old_homework_reminders_sent(
        &self,
        older_than_timestamp: i64,
    ) -> Result<u64> {
        let result = sqlx::query("DELETE FROM homework_reminder_sent WHERE sent_at < ?")
            .bind(older_than_timestamp)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("清理舊作業提醒紀錄失敗")?;
        Ok(result.rows_affected())
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

fn truncate_event_text(mut value: String, max_bytes: usize) -> String {
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

// ─── 非同步批次寫入任務 ───────────────────────────────────────────────────────

async fn activity_event_batch_writer(
    pool: SqlitePool,
    mut rx: mpsc::Receiver<ActivityEventInput>,
) {
    let mut writes_since_prune = 0usize;
    while let Some(first_entry) = rx.recv().await {
        let mut batch = Vec::with_capacity(50);
        batch.push(first_entry);
        while batch.len() < 50 {
            match rx.try_recv() {
                Ok(entry) => batch.push(entry),
                Err(_) => break,
            }
        }

        let batch_len = batch.len();
        if let Err(e) = insert_activity_events_batch(&pool, &batch).await {
            eprintln!("⚠️ 批次寫入 activity_events 失敗（{batch_len} 筆）：{e}");
            continue;
        }

        writes_since_prune += batch_len;
        if writes_since_prune >= EVENT_PRUNE_INTERVAL {
            writes_since_prune = 0;
            let _ = sqlx::query(
                "DELETE FROM activity_events WHERE id NOT IN (
                    SELECT id FROM activity_events ORDER BY id DESC LIMIT 500
                )",
            )
            .execute(&pool)
            .await;
        }
    }
}

async fn insert_activity_events_batch(
    pool: &SqlitePool,
    entries: &[ActivityEventInput],
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut tx = pool
        .begin()
        .await
        .into_diagnostic()
        .wrap_err("開始 activity_events 批次寫入交易失敗")?;

    let now = Local::now();
    let timestamp = now.timestamp();
    let time_str = now.format("%Y-%m-%d %H:%M:%S").to_string();

    for event in entries {
        sqlx::query(
            "INSERT INTO activity_events (timestamp, time, kind, actor, action, account_id, path, result, details, ip)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(timestamp)
        .bind(&time_str)
        .bind(&event.kind)
        .bind(&event.actor)
        .bind(&event.action)
        .bind(&event.account_id)
        .bind(&event.path)
        .bind(&event.result)
        .bind(&event.details)
        .bind(&event.ip)
        .execute(&mut *tx)
        .await
        .into_diagnostic()
        .wrap_err("寫入單筆 activity_event 失敗")?;
    }

    tx.commit()
        .await
        .into_diagnostic()
        .wrap_err("提交 activity_events 批次寫入交易失敗")?;

    Ok(())
}

async fn material_event_batch_writer(
    pool: SqlitePool,
    mut rx: mpsc::Receiver<MaterialEventInput>,
) {
    let mut writes_since_prune = 0usize;
    while let Some(first_entry) = rx.recv().await {
        let mut batch = Vec::with_capacity(50);
        batch.push(first_entry);
        while batch.len() < 50 {
            match rx.try_recv() {
                Ok(entry) => batch.push(entry),
                Err(_) => break,
            }
        }

        let batch_len = batch.len();
        if let Err(e) = insert_material_events_batch(&pool, &batch).await {
            eprintln!("⚠️ 批次寫入 material_events 失敗（{batch_len} 筆）：{e}");
            continue;
        }

        writes_since_prune += batch_len;
        if writes_since_prune >= EVENT_PRUNE_INTERVAL {
            writes_since_prune = 0;
            let _ = sqlx::query(
                "DELETE FROM material_events WHERE id NOT IN (
                    SELECT id FROM material_events ORDER BY id DESC LIMIT 300
                )",
            )
            .execute(&pool)
            .await;
        }
    }
}

async fn insert_material_events_batch(
    pool: &SqlitePool,
    entries: &[MaterialEventInput],
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut tx = pool
        .begin()
        .await
        .into_diagnostic()
        .wrap_err("開始 material_events 批次寫入交易失敗")?;

    let now = Local::now();
    let timestamp = now.timestamp();
    let time_str = now.format("%Y-%m-%d %H:%M:%S").to_string();

    for event in entries {
        let size_label = format_material_event_size(event.size.max(0) as u64);
        sqlx::query(
            "INSERT INTO material_events (
                timestamp, time, actor, action, path, folder, file_name, size, size_label, result, details
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(timestamp)
        .bind(&time_str)
        .bind(&event.actor)
        .bind(&event.action)
        .bind(&event.path)
        .bind(&event.folder)
        .bind(&event.file_name)
        .bind(event.size)
        .bind(&size_label)
        .bind(&event.result)
        .bind(&event.details)
        .execute(&mut *tx)
        .await
        .into_diagnostic()
        .wrap_err("寫入單筆 material_event 失敗")?;
    }

    tx.commit()
        .await
        .into_diagnostic()
        .wrap_err("提交 material_events 批次寫入交易失敗")?;

    Ok(())
}

async fn ensure_table_column(
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

async fn ensure_column(pool: &SqlitePool, column: &str, definition: &str) -> Result<()> {
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

fn format_material_event_size(size: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if size < 1024 {
        format!("{size} B")
    } else if size < 1024 * 1024 {
        format!("{:.1} KB", size as f64 / KB)
    } else {
        format!("{:.1} MB", size as f64 / MB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn discord_user_id_is_inserted_and_updated() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let account = RawAccountConfig {
            id: "acc1".to_string(),
            provider: "fju".to_string(),
            username: "student".to_string(),
            password: "secret".to_string(),
            enabled: true,
            line_user_id: "Uline".to_string(),
            discord_user_id: "100".to_string(),
            session_cookie: String::new(),
            is_admin: false,
            dashboard_password_hash: String::new(),
        };

        db.insert(&account).await.unwrap();
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.discord_user_id, "100");

        assert!(db.set_discord_user_id("acc1", "200").await.unwrap());
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.discord_user_id, "200");

        assert!(db
            .set_session_cookie("acc1", "session=fresh-cookie")
            .await
            .unwrap());
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.session_cookie, "session=fresh-cookie");

        assert!(db
            .set_dashboard_password_hash(
                "acc1",
                "pbkdf2-sha256$600000$00112233445566778899aabbccddeeff$0123"
            )
            .await
            .unwrap());
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert!(loaded.dashboard_password_hash.starts_with("pbkdf2-sha256$"));
    }

    #[tokio::test]
    async fn discord_user_id_can_update_by_tronclass_username() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        db.insert(&RawAccountConfig {
            id: "acc1".to_string(),
            provider: "fju".to_string(),
            username: "student001".to_string(),
            password: "secret".to_string(),
            enabled: true,
            line_user_id: String::new(),
            discord_user_id: String::new(),
            session_cookie: String::new(),
            is_admin: false,
            dashboard_password_hash: String::new(),
        })
        .await
        .unwrap();

        let result = db
            .set_discord_user_id_by_username("student001", None, "123")
            .await
            .unwrap();
        assert_eq!(
            result,
            UsernameUpdateResult::Updated {
                account_id: "acc1".to_string()
            }
        );
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.discord_user_id, "123");
    }

    #[tokio::test]
    async fn username_binding_reports_ambiguous_matches() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        for (id, provider) in [("acc1", "fju"), ("acc2", "tku")] {
            db.insert(&RawAccountConfig {
                id: id.to_string(),
                provider: provider.to_string(),
                username: "student001".to_string(),
                password: "secret".to_string(),
                enabled: true,
                line_user_id: String::new(),
                discord_user_id: String::new(),
                session_cookie: String::new(),
                is_admin: false,
                dashboard_password_hash: String::new(),
            })
            .await
            .unwrap();
        }

        let result = db
            .set_discord_user_id_by_username("student001", None, "123")
            .await
            .unwrap();
        assert_eq!(
            result,
            UsernameUpdateResult::Ambiguous {
                account_ids: vec!["acc1".to_string(), "acc2".to_string()]
            }
        );

        let result = db
            .set_discord_user_id_by_username("student001", Some("fju"), "123")
            .await
            .unwrap();
        assert_eq!(
            result,
            UsernameUpdateResult::Updated {
                account_id: "acc1".to_string()
            }
        );
    }

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

    #[tokio::test]
    async fn dashboard_sessions_crud_and_expiry_operations() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        let now = chrono::Local::now().timestamp();
        let future_expiry = now + 3600;
        let past_expiry = now - 100;

        db.insert_dashboard_session("token1", "acc1", true, false, future_expiry)
            .await
            .unwrap();
        db.insert_dashboard_session("token2", "acc1", false, true, future_expiry)
            .await
            .unwrap();
        db.insert_dashboard_session("token3", "acc2", false, false, past_expiry)
            .await
            .unwrap();

        // Get single session
        let s1 = db.get_dashboard_session("token1").await.unwrap().unwrap();
        assert_eq!(s1.token, "token1");
        assert_eq!(s1.account_id, "acc1");
        assert!(s1.is_admin);
        assert!(!s1.must_change_password);
        assert_eq!(s1.expires_at, future_expiry);

        let s2 = db.get_dashboard_session("token2").await.unwrap().unwrap();
        assert!(!s2.is_admin);
        assert!(s2.must_change_password);

        // List valid sessions (should exclude expired token3)
        let valid = db.list_valid_dashboard_sessions(now).await.unwrap();
        assert_eq!(valid.len(), 2);
        assert!(valid.iter().any(|s| s.token == "token1"));
        assert!(valid.iter().any(|s| s.token == "token2"));

        // Cleanup expired sessions
        let pruned = db.delete_expired_dashboard_sessions(now).await.unwrap();
        assert_eq!(pruned, 1);
        assert!(db.get_dashboard_session("token3").await.unwrap().is_none());

        // Delete single session
        assert!(db.delete_dashboard_session("token1").await.unwrap());
        assert!(db.get_dashboard_session("token1").await.unwrap().is_none());

        // Delete sessions by account
        db.insert_dashboard_session("token4", "acc1", false, false, future_expiry)
            .await
            .unwrap();
        let deleted_acc = db.delete_dashboard_sessions_by_account("acc1").await.unwrap();
        assert_eq!(deleted_acc, 2); // token2 and token4
        assert!(db.get_dashboard_session("token2").await.unwrap().is_none());
        assert!(db.get_dashboard_session("token4").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn activity_and_material_events_batch_writing() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        // 寫入多筆活動事件
        for i in 1..=5 {
            db.record_activity_event(ActivityEventInput {
                kind: "security".to_string(),
                actor: "user".to_string(),
                action: format!("login_attempt_{i}"),
                account_id: "acc1".to_string(),
                path: "/admin/login".to_string(),
                result: "success".to_string(),
                details: format!("batch test {i}"),
                ip: "127.0.0.1".to_string(),
            })
            .await
            .unwrap();
        }

        // 寫入教材事件
        db.record_material_event(MaterialEventInput {
            actor: "admin".to_string(),
            action: "upload".to_string(),
            path: "/materials/test.pdf".to_string(),
            folder: "materials".to_string(),
            file_name: "test.pdf".to_string(),
            size: 2048,
            result: "success".to_string(),
            details: "batch upload".to_string(),
        })
        .await
        .unwrap();

        // 等候背景寫入任務處理佇列
        tokio::time::sleep(Duration::from_millis(150)).await;

        let activities = db.list_activity_events(Some("security"), 10).await.unwrap();
        assert_eq!(activities.len(), 5);
        assert!(activities.iter().any(|a| a.action == "login_attempt_1"));
        assert!(activities.iter().any(|a| a.action == "login_attempt_5"));

        let materials = db.list_material_events(10).await.unwrap();
        assert_eq!(materials.len(), 1);
        assert_eq!(materials[0].file_name, "test.pdf");
        assert_eq!(materials[0].size_label, "2.0 KB");
    }

    /// 微幅延期（例如老師把截止時間往後挪 10 分鐘）不可重置提醒紀錄，
    /// 否則使用者會在同一個階段收到重複的緊急提醒。
    #[tokio::test]
    async fn minor_deadline_extension_does_not_resend_reminder() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        let original = 1788100000;
        let nudged = original + 10 * 60; // 只延後 10 分鐘

        db.mark_homework_reminder_sent("acc1", "hw_201", "1h", original)
            .await
            .unwrap();

        // 微幅延期後仍應視為「已發送」，不重複通知
        assert!(
            db.is_homework_reminder_sent("acc1", "hw_201", "1h", nudged)
                .await
                .unwrap(),
            "延後 10 分鐘不應觸發重複提醒"
        );

        // 延後 2 小時（仍小於 6 小時門檻）也不該重置
        let two_hours = original + 2 * 60 * 60;
        assert!(
            db.is_homework_reminder_sent("acc1", "hw_201", "1h", two_hours)
                .await
                .unwrap(),
            "延後 2 小時仍在門檻內，不應重複提醒"
        );

        // 延後 8 小時（超過門檻）才重置，重新走一輪倒數
        let eight_hours = original + 8 * 60 * 60;
        assert!(
            !db.is_homework_reminder_sent("acc1", "hw_201", "1h", eight_hours)
                .await
                .unwrap(),
            "顯著延期應重置紀錄並重新提醒"
        );
    }

    #[tokio::test]
    async fn homework_reminder_deduplication_and_extension_reset() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        let initial_deadline = 1788100000;
        let extended_deadline = 1788200000;

        // 1. 初始階段尚未發送
        assert!(
            !db.is_homework_reminder_sent("acc1", "hw_101", "24h", initial_deadline)
                .await
                .unwrap()
        );

        // 2. 標記已發送 24h 提醒
        assert!(
            db.mark_homework_reminder_sent("acc1", "hw_101", "24h", initial_deadline)
                .await
                .unwrap()
        );

        // 3. 再次檢查相同截止時間的 24h 提醒，應回傳已發送 (true)
        assert!(
            db.is_homework_reminder_sent("acc1", "hw_101", "24h", initial_deadline)
                .await
                .unwrap()
        );

        // 4. 作業截止時間被老師延期 (extended_deadline > initial_deadline)
        // 應自動重置該作業的所有階梯歷史，回傳 false
        assert!(
            !db.is_homework_reminder_sent("acc1", "hw_101", "24h", extended_deadline)
                .await
                .unwrap()
        );

        // 5. 標記延期後的新通知
        assert!(
            db.mark_homework_reminder_sent("acc1", "hw_101", "24h", extended_deadline)
                .await
                .unwrap()
        );
        assert!(
            db.is_homework_reminder_sent("acc1", "hw_101", "24h", extended_deadline)
                .await
                .unwrap()
        );
    }
}
