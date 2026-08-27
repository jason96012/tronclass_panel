//! Persistent system/audit log storage for the Tronclass admin panel.
//!
//! Logs are kept in SQLite table `web_logs` inside the accounts database and
//! pruned to the latest 100 rows. An in-memory mirror is kept as a fallback.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};

use chrono::Local;
use miette::{IntoDiagnostic, Result, WrapErr};
use once_cell::sync::{Lazy, OnceCell};
use serde::Serialize;
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};
use tokio::sync::mpsc;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

const MAX_LOG_ENTRIES: usize = 100;
// The writer must never retain an unlimited warning/error burst while SQLite is
// busy. Every queued entry is byte-bounded below, so this also creates a
// predictable upper bound for the queue's owned strings.
const LOG_SQLITE_QUEUE_CAPACITY: usize = 256;
// PERFORMANCE_LOGS_V1: avoid a retention DELETE after every inserted log.
const LOG_PRUNE_INTERVAL: usize = 25;
const LOG_SQLITE_BATCH_MAX: usize = 50;

const LOG_TIME_MAX_BYTES: usize = 64;
const LOG_KIND_MAX_BYTES: usize = 16;
const LOG_LEVEL_MAX_BYTES: usize = 16;
const LOG_SOURCE_MAX_BYTES: usize = 128;
const LOG_IDENTITY_MAX_BYTES: usize = 256;
const LOG_ACTION_MAX_BYTES: usize = 128;
const LOG_MESSAGE_MAX_BYTES: usize = 4 * 1024;
const LOG_DETAILS_MAX_BYTES: usize = 8 * 1024;
const LOG_EVENT_FIELD_MAX_BYTES: usize = 1024;
const TRUNCATED_SUFFIX: &str = "…[truncated]";

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static DROPPED_DB_LOGS: AtomicU64 = AtomicU64::new(0);
static LOGS: Lazy<Mutex<VecDeque<WebLogEntry>>> = Lazy::new(|| Mutex::new(VecDeque::new()));
static PANIC_HOOK: Once = Once::new();
static DB_POOL: OnceCell<SqlitePool> = OnceCell::new();
static DB_SENDER: OnceCell<mpsc::Sender<WebLogEntry>> = OnceCell::new();

#[derive(Debug, Clone, Serialize)]
pub struct WebLogEntry {
    pub id: u64,
    pub timestamp: i64,
    pub time: String,
    /// `audit` or `system`.
    pub kind: String,
    /// INFO/WARN/ERROR.
    pub level: String,
    /// e.g. web, discord, line, panic, module target.
    pub source: String,
    /// User id / account id that initiated the action, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl WebLogEntry {
    fn truncate_fields(mut self) -> Self {
        self.time = truncate_owned(self.time, LOG_TIME_MAX_BYTES);
        self.kind = truncate_owned(self.kind, LOG_KIND_MAX_BYTES);
        self.level = truncate_owned(self.level, LOG_LEVEL_MAX_BYTES);
        self.source = truncate_owned(self.source, LOG_SOURCE_MAX_BYTES);
        self.actor = self
            .actor
            .map(|value| truncate_owned(value, LOG_IDENTITY_MAX_BYTES));
        self.action = self
            .action
            .map(|value| truncate_owned(value, LOG_ACTION_MAX_BYTES));
        self.account_id = self
            .account_id
            .map(|value| truncate_owned(value, LOG_IDENTITY_MAX_BYTES));
        self.target = self
            .target
            .map(|value| truncate_owned(value, LOG_IDENTITY_MAX_BYTES));
        self.message = truncate_owned(self.message, LOG_MESSAGE_MAX_BYTES);
        self.details = self
            .details
            .map(|value| truncate_owned(value, LOG_DETAILS_MAX_BYTES));
        self
    }
}

#[derive(Debug, FromRow)]
struct DbLogRow {
    id: i64,
    timestamp: i64,
    time: String,
    kind: String,
    level: String,
    source: String,
    actor: Option<String>,
    action: Option<String>,
    account_id: Option<String>,
    target: Option<String>,
    message: String,
    details: Option<String>,
}

impl From<DbLogRow> for WebLogEntry {
    fn from(row: DbLogRow) -> Self {
        Self {
            id: row.id.max(0) as u64,
            timestamp: row.timestamp,
            time: row.time,
            kind: row.kind,
            level: row.level,
            source: row.source,
            actor: row.actor,
            action: row.action,
            account_id: row.account_id,
            target: row.target,
            message: row.message,
            details: row.details,
        }
        .truncate_fields()
    }
}

pub struct MemoryLogLayer;

impl MemoryLogLayer {
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for MemoryLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = *event.metadata().level();
        if level != Level::ERROR && level != Level::WARN {
            return;
        }

        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let message = visitor
            .message
            .unwrap_or_else(|| event.metadata().name().to_string());
        let details = if visitor.fields.is_empty() {
            None
        } else {
            let mut writer = LimitedWriter::new(LOG_DETAILS_MAX_BYTES);
            for (index, field) in visitor.fields.iter().enumerate() {
                if index > 0 {
                    let _ = writer.write_str(", ");
                }
                let _ = writer.write_str(field);
            }
            Some(writer.finish())
        };

        let level = match level {
            Level::ERROR => "ERROR",
            Level::WARN => "WARN",
            _ => "INFO",
        };
        record_system(level, event.metadata().target(), message, details);
    }
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: Vec<String>,
}

impl EventVisitor {
    fn record_value(&mut self, field: &tracing::field::Field, value: String) {
        let value = trim_debug_string(&value);
        if field.name() == "message" {
            self.message = Some(truncate_owned(value, LOG_MESSAGE_MAX_BYTES));
        } else {
            let field_value = truncate_owned(value, LOG_EVENT_FIELD_MAX_BYTES);
            self.fields.push(format!("{}={field_value}", field.name()));
        }
    }
}

impl tracing::field::Visit for EventVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        // Formatting directly into a bounded writer prevents a large Display or
        // Debug value from allocating an unbounded temporary String before the
        // log sink gets a chance to truncate it.
        self.record_value(field, limited_debug(value, LOG_EVENT_FIELD_MAX_BYTES));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record_value(field, truncate_str(value, LOG_EVENT_FIELD_MAX_BYTES));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record_value(field, value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.record_value(field, value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record_value(field, value.to_string());
    }
}

pub async fn init_sqlite(path: impl AsRef<Path>) -> Result<()> {
    if DB_POOL.get().is_some() {
        return Ok(());
    }

    let path = path.as_ref();
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("無法開啟 logs SQLite：`{}`", path.display()))?;

    sqlx::query("PRAGMA busy_timeout = 5000")
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("設定 logs SQLite busy_timeout 失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS web_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL,
            time TEXT NOT NULL,
            kind TEXT NOT NULL,
            level TEXT NOT NULL,
            source TEXT NOT NULL,
            actor TEXT,
            action TEXT,
            account_id TEXT,
            target TEXT,
            message TEXT NOT NULL,
            details TEXT
        )",
    )
    .execute(&pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 web_logs 資料表失敗")?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_web_logs_kind_id ON web_logs(kind, id DESC)")
        .execute(&pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 web_logs kind index 失敗")?;

    prune_sqlite_logs(&pool).await?;

    let recent = query_recent_from_pool(&pool, None, MAX_LOG_ENTRIES).await?;
    {
        let mut logs = LOGS.lock().expect("logs mutex poisoned");
        logs.clear();
        for entry in recent.iter().rev().cloned() {
            logs.push_back(entry);
        }
    }
    if let Some(max_id) = recent.iter().map(|entry| entry.id).max() {
        NEXT_ID.store(max_id.saturating_add(1), Ordering::Relaxed);
    }

    let (tx, rx) = mpsc::channel(LOG_SQLITE_QUEUE_CAPACITY);
    let _ = DB_POOL.set(pool.clone());
    let _ = DB_SENDER.set(tx);
    tokio::spawn(sqlite_writer(pool, rx));

    Ok(())
}

pub fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = panic_message(info);
            let location = info
                .location()
                .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()));
            record_system("ERROR", "panic", format!("panic: {message}"), location);
            previous(info);
        }));
    });
}

pub fn record_audit(
    source: &str,
    actor: &str,
    action: &str,
    account_id: Option<&str>,
    target: Option<&str>,
    message: impl Into<String>,
) {
    push(WebLogEntry {
        id: next_id(),
        timestamp: Local::now().timestamp(),
        time: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        kind: "audit".to_string(),
        level: "INFO".to_string(),
        source: truncate_str(source, LOG_SOURCE_MAX_BYTES),
        actor: empty_to_none(actor, LOG_IDENTITY_MAX_BYTES),
        action: empty_to_none(action, LOG_ACTION_MAX_BYTES),
        account_id: account_id.and_then(|value| empty_to_none(value, LOG_IDENTITY_MAX_BYTES)),
        target: target.and_then(|value| empty_to_none(value, LOG_IDENTITY_MAX_BYTES)),
        message: truncate_owned(message.into(), LOG_MESSAGE_MAX_BYTES),
        details: None,
    });
}

pub fn record_system(
    level: &str,
    source: &str,
    message: impl Into<String>,
    details: Option<String>,
) {
    push(WebLogEntry {
        id: next_id(),
        timestamp: Local::now().timestamp(),
        time: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        kind: "system".to_string(),
        level: truncate_str(level, LOG_LEVEL_MAX_BYTES),
        source: truncate_str(source, LOG_SOURCE_MAX_BYTES),
        actor: None,
        action: None,
        account_id: None,
        target: None,
        message: truncate_owned(message.into(), LOG_MESSAGE_MAX_BYTES),
        details: details.map(|value| truncate_owned(value, LOG_DETAILS_MAX_BYTES)),
    });
}

pub fn snapshot(kind: Option<&str>, limit: usize) -> Vec<WebLogEntry> {
    let limit = limit.clamp(1, MAX_LOG_ENTRIES);
    let kind = kind
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "all");
    let logs = LOGS.lock().expect("logs mutex poisoned");
    logs.iter()
        .rev()
        .filter(|entry| kind.map(|k| entry.kind == k).unwrap_or(true))
        .take(limit)
        .cloned()
        .collect()
}

pub async fn query_recent(kind: Option<&str>, limit: usize) -> Vec<WebLogEntry> {
    // Mirror is hydrated at startup and updated before SQLite writer queueing.
    snapshot(kind, limit)
}

fn push(entry: WebLogEntry) {
    let entry = entry.truncate_fields();
    push_memory(entry.clone());
    if let Some(sender) = DB_SENDER.get() {
        match sender.try_send(entry) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => report_dropped_db_log("queue full"),
            Err(mpsc::error::TrySendError::Closed(_)) => report_dropped_db_log("writer closed"),
        }
    }
}

fn report_dropped_db_log(reason: &str) {
    let dropped = DROPPED_DB_LOGS.fetch_add(1, Ordering::Relaxed) + 1;
    // Avoid turning an overload into another log storm. Powers of two provide
    // exponentially backed-off diagnostics without re-entering tracing.
    if dropped.is_power_of_two() {
        eprintln!("⚠️ web_logs SQLite queue dropped {dropped} record(s): {reason}");
    }
}

pub fn dropped_sqlite_log_count() -> u64 {
    DROPPED_DB_LOGS.load(Ordering::Relaxed)
}

fn push_memory(entry: WebLogEntry) {
    let mut logs = LOGS.lock().expect("logs mutex poisoned");
    if logs.len() >= MAX_LOG_ENTRIES {
        logs.pop_front();
    }
    logs.push_back(entry);
}

async fn sqlite_writer(pool: SqlitePool, mut rx: mpsc::Receiver<WebLogEntry>) {
    let mut writes_since_prune = 0usize;
    while let Some(first_entry) = rx.recv().await {
        // PERFORMANCE_LOGS_BATCH_V2:
        // Drain a short burst from the bounded queue and write it in one
        // SQLite transaction. This keeps single-log latency low while reducing
        // write-lock/fsync overhead during warning/error bursts.
        let mut batch = Vec::with_capacity(LOG_SQLITE_BATCH_MAX);
        batch.push(first_entry);
        while batch.len() < LOG_SQLITE_BATCH_MAX {
            match rx.try_recv() {
                Ok(entry) => batch.push(entry),
                Err(_) => break,
            }
        }

        let batch_len = batch.len();
        if let Err(e) = insert_sqlite_logs_batch(&pool, &batch).await {
            eprintln!("⚠️ 批次寫入 web_logs SQLite 失敗（{batch_len} 筆）：{e}");
            continue;
        }
        writes_since_prune += batch_len;
        if writes_since_prune >= LOG_PRUNE_INTERVAL {
            writes_since_prune = 0;
            if let Err(e) = prune_sqlite_logs(&pool).await {
                eprintln!("⚠️ 清理 web_logs SQLite 舊紀錄失敗：{e}");
            }
        }
    }
}

async fn insert_sqlite_logs_batch(pool: &SqlitePool, entries: &[WebLogEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut tx = pool
        .begin()
        .await
        .into_diagnostic()
        .wrap_err("開始 web_logs SQLite 批次寫入交易失敗")?;

    for entry in entries {
        sqlx::query(
            "INSERT INTO web_logs (
                timestamp, time, kind, level, source, actor, action, account_id, target, message, details
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(entry.timestamp)
        .bind(&entry.time)
        .bind(&entry.kind)
        .bind(&entry.level)
        .bind(&entry.source)
        .bind(&entry.actor)
        .bind(&entry.action)
        .bind(&entry.account_id)
        .bind(&entry.target)
        .bind(&entry.message)
        .bind(&entry.details)
        .execute(&mut *tx)
        .await
        .into_diagnostic()
        .wrap_err("INSERT web_logs 失敗")?;
    }

    tx.commit()
        .await
        .into_diagnostic()
        .wrap_err("提交 web_logs SQLite 批次寫入交易失敗")?;
    Ok(())
}

async fn prune_sqlite_logs(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "DELETE FROM web_logs
         WHERE id NOT IN (
             SELECT id FROM web_logs ORDER BY id DESC LIMIT ?
         )",
    )
    .bind(MAX_LOG_ENTRIES as i64)
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("DELETE old web_logs 失敗")?;
    Ok(())
}

async fn query_recent_from_pool(
    pool: &SqlitePool,
    kind: Option<&str>,
    limit: usize,
) -> Result<Vec<WebLogEntry>> {
    let limit = limit.clamp(1, MAX_LOG_ENTRIES) as i64;
    let kind = kind
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "all");

    let rows = if let Some(kind) = kind {
        sqlx::query_as::<_, DbLogRow>(
            "SELECT id, timestamp, time, kind, level, source, actor, action, account_id, target, message, details
             FROM web_logs WHERE kind = ? ORDER BY id DESC LIMIT ?",
        )
        .bind(kind)
        .bind(limit)
        .fetch_all(pool)
        .await
        .into_diagnostic()
        .wrap_err("SELECT web_logs by kind 失敗")?
    } else {
        sqlx::query_as::<_, DbLogRow>(
            "SELECT id, timestamp, time, kind, level, source, actor, action, account_id, target, message, details
             FROM web_logs ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(pool)
        .await
        .into_diagnostic()
        .wrap_err("SELECT web_logs 失敗")?
    };

    Ok(rows.into_iter().map(Into::into).collect())
}

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn empty_to_none(value: &str, max_bytes: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(truncate_str(value, max_bytes))
    }
}

fn truncate_str(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let keep = max_bytes.saturating_sub(TRUNCATED_SUFFIX.len());
    let mut end = keep.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = String::with_capacity(max_bytes);
    output.push_str(&value[..end]);
    if TRUNCATED_SUFFIX.len() <= max_bytes {
        output.push_str(TRUNCATED_SUFFIX);
    }
    output
}

fn truncate_owned(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let keep = max_bytes.saturating_sub(TRUNCATED_SUFFIX.len());
    let mut end = keep.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    if TRUNCATED_SUFFIX.len() <= max_bytes {
        value.push_str(TRUNCATED_SUFFIX);
    }
    value.shrink_to(max_bytes);
    value
}

struct LimitedWriter {
    output: String,
    max_bytes: usize,
    truncated: bool,
}

impl LimitedWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            output: String::with_capacity(max_bytes.min(256)),
            max_bytes,
            truncated: false,
        }
    }

    fn finish(self) -> String {
        if self.truncated {
            truncate_owned(self.output + TRUNCATED_SUFFIX, self.max_bytes)
        } else {
            self.output
        }
    }
}

impl fmt::Write for LimitedWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.truncated {
            return Err(fmt::Error);
        }
        let remaining = self.max_bytes.saturating_sub(self.output.len());
        if value.len() <= remaining {
            self.output.push_str(value);
            return Ok(());
        }
        let suffix_len = TRUNCATED_SUFFIX.len().min(self.max_bytes);
        let usable = self
            .max_bytes
            .saturating_sub(self.output.len())
            .saturating_sub(suffix_len);
        let mut end = usable.min(value.len());
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        self.output.push_str(&value[..end]);
        self.truncated = true;
        Err(fmt::Error)
    }
}

fn limited_debug(value: &dyn fmt::Debug, max_bytes: usize) -> String {
    let mut writer = LimitedWriter::new(max_bytes);
    let _ = write!(&mut writer, "{value:?}");
    writer.finish()
}

fn trim_debug_string(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(message) = info.payload().downcast_ref::<&str>() {
        truncate_str(message, LOG_MESSAGE_MAX_BYTES)
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        truncate_str(message, LOG_MESSAGE_MAX_BYTES)
    } else {
        "unknown panic payload".to_string()
    }
}
