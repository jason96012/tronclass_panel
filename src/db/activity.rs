//! 登入 / 安全活動事件（含背景批次寫入）。

use chrono::Local;
use hmac::{Hmac, Mac};
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use sha2::Sha256;
use sqlx::{FromRow, SqlitePool};
use tokio::sync::mpsc;

type HmacSha256 = Hmac<Sha256>;

use super::{
    truncate_event_text, AccountsDb, EVENT_ACTION_MAX_BYTES, EVENT_DETAILS_MAX_BYTES,
    EVENT_IDENTITY_MAX_BYTES, EVENT_IP_MAX_BYTES, EVENT_PATH_MAX_BYTES, EVENT_PRUNE_INTERVAL,
    EVENT_RESULT_MAX_BYTES, EVENT_USER_AGENT_MAX_BYTES,
};

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
    /// BLACKLIST_UX_V1（fix58）：發起請求的 User-Agent（登入事件才有；其他事件留空）。
    /// 舊資料列沒有這個欄位，讀出來是空字串。
    pub user_agent: String,
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
    /// BLACKLIST_UX_V1（fix58）：裝置資訊（User-Agent）。舊資料列為空字串。
    #[sqlx(default)]
    #[serde(default)]
    pub user_agent: String,
    /// AUDIT_TAMPER_EVIDENT_V1：每筆事件的 HMAC-SHA256 簽章，用於偵測資料庫層級的竄改。
    #[sqlx(default)]
    #[serde(default)]
    pub event_hash: String,
}

impl AccountsDb {
    pub async fn record_activity_event(&self, mut event: ActivityEventInput) -> Result<()> {
        event.kind = truncate_event_text(event.kind, EVENT_ACTION_MAX_BYTES);
        event.actor = truncate_event_text(event.actor, EVENT_IDENTITY_MAX_BYTES);
        event.action = truncate_event_text(event.action, EVENT_ACTION_MAX_BYTES);
        event.account_id = truncate_event_text(event.account_id, EVENT_IDENTITY_MAX_BYTES);
        event.path = truncate_event_text(event.path, EVENT_PATH_MAX_BYTES);
        event.result = truncate_event_text(event.result, EVENT_RESULT_MAX_BYTES);
        event.details = truncate_event_text(event.details, EVENT_DETAILS_MAX_BYTES);
        event.ip = truncate_event_text(event.ip, EVENT_IP_MAX_BYTES);
        event.user_agent = truncate_event_text(event.user_agent, EVENT_USER_AGENT_MAX_BYTES);

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
                "SELECT id, timestamp, time, kind, actor, action, account_id, path, result, details, ip, user_agent, event_hash
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
                "SELECT id, timestamp, time, kind, actor, action, account_id, path, result, details, ip, user_agent, event_hash
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
}

/// BLACKLIST_RETENTION_V1：`/admin/blacklist` 頁「最近登入失敗」只保留最新
/// 這麼多筆——跟同一頁另外兩份清單（已封鎖的 IP、國外存取申請）用同一個
/// 數字。這張 `activity_events` 表是全站共用的活動紀錄（不只登入事件），
/// 所以不能整張表砍到只剩 100 筆，必須用跟顯示端一樣的「登入失敗」判斷條件
/// 篩選過再刪，才不會波及其他種類的活動紀錄（例如一般的操作稽核）。
const LOGIN_FAILURE_RETENTION_ROWS: i64 = 100;

/// 跟 `adapters::admin_web::blacklist::admin_blacklist_recent_logins_handler`
/// 用的是同一套「算不算登入失敗」規則，只是這裡要寫成 SQL 判斷式。兩邊分別
/// 維護、但條件必須保持一致，否則這裡清掉的和畫面上顯示的會對不上。
const LOGIN_FAILURE_SQL_FILTER: &str = "(
    LOWER(TRIM(action)) = 'login_failed'
    OR LOWER(TRIM(result)) IN ('fail', 'failed', 'error', 'false')
    OR (LOWER(TRIM(action)) = 'login' AND LOWER(TRIM(result)) <> 'success')
)";

/// 串鏈簽章的版本號（寫入 `chain_version`）。
const CHAIN_VERSION_CURRENT: i64 = 2;

/// 找不到設定時使用的預設 HMAC 密鑰。這個字串寫在原始碼裡，任何人都知道，
/// 所以只要用到它，驗證結果就必須明確警告。
const DEFAULT_AUDIT_HMAC_KEY: &str = "default-audit-hmac-key-change-in-production";

/// 所有會改動 activity_events 鏈頭的操作（寫入批次、清理）都要先拿這把鎖，
/// 確保「讀取前一筆 hash → 寫入新列」不會被別的寫入插隊，避免鏈分岔。
static ACTIVITY_CHAIN_LOCK: once_cell::sync::Lazy<tokio::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(()));

/// 讀取 HMAC 密鑰：優先 config.toml 的 `audit_hmac_key`，其次環境變數
/// `TRONCLASS_AUDIT_HMAC_KEY`，都沒有就退回預設值。回傳 (密鑰, 是否為預設值)。
fn audit_hmac_key() -> (String, bool) {
    let configured = crate::config::try_get_config()
        .and_then(|cfg| {
            let k = cfg.audit_hmac_key.trim();
            if k.is_empty() {
                None
            } else {
                Some(k.to_string())
            }
        })
        .or_else(|| std::env::var("TRONCLASS_AUDIT_HMAC_KEY").ok());
    match configured {
        Some(k) => (k, false),
        None => {
            static WARNED: once_cell::sync::OnceCell<()> = once_cell::sync::OnceCell::new();
            WARNED.get_or_init(|| {
                tracing::warn!("audit_hmac_key 未設定（config.toml 或 TRONCLASS_AUDIT_HMAC_KEY 環境變數），審計日誌防竄改使用預設密鑰，生產環境請設定");
            });
            (DEFAULT_AUDIT_HMAC_KEY.to_string(), true)
        }
    }
}

/// 一筆活動事件中參與簽章的欄位（寫入與驗證共用，確保兩邊算的是同一組資料）。
struct SignedFields<'a> {
    timestamp: i64,
    time: &'a str,
    kind: &'a str,
    actor: &'a str,
    action: &'a str,
    account_id: &'a str,
    path: &'a str,
    result: &'a str,
    details: &'a str,
    ip: &'a str,
    user_agent: &'a str,
}

impl SignedFields<'_> {
    fn text_fields(&self) -> [&str; 9] {
        [
            self.kind,
            self.actor,
            self.action,
            self.account_id,
            self.path,
            self.result,
            self.details,
            self.ip,
            self.user_agent,
        ]
    }

    /// 舊格式（chain_version=1）：欄位直接串接，不含前一筆。只用來驗證既有資料，
    /// 寫入時不再使用。公式必須與舊版程式完全相同，否則舊列會全部驗證失敗。
    fn hash_v1(&self, key: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts keys of any length");
        mac.update(self.timestamp.to_string().as_bytes());
        mac.update(self.time.as_bytes());
        for field in self.text_fields() {
            mac.update(field.as_bytes());
        }
        hex::encode(mac.finalize().into_bytes())
    }

    /// 串鏈格式（chain_version=2）：把前一筆的 event_hash 也放進簽章，並且每個欄位
    /// 都加上長度前綴，避免「欄位邊界不同但拼接結果相同」的歧義。
    fn hash_v2(&self, key: &[u8], prev_hash: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts keys of any length");
        mac.update(b"audit-chain-v2\n");
        let timestamp = self.timestamp.to_string();
        let mut parts: Vec<&str> = vec![prev_hash, timestamp.as_str(), self.time];
        parts.extend(self.text_fields());
        for part in parts {
            mac.update(format!("{}:", part.len()).as_bytes());
            mac.update(part.as_bytes());
            mac.update(b"\n");
        }
        hex::encode(mac.finalize().into_bytes())
    }
}

/// 只給 hash 鏈相關的讀取用（含 prev_hash / chain_version，不影響對外的列表 API）。
#[derive(Debug, Clone, FromRow)]
struct ChainRow {
    id: i64,
    timestamp: i64,
    time: String,
    kind: String,
    actor: String,
    action: String,
    account_id: String,
    path: String,
    result: String,
    details: String,
    ip: String,
    user_agent: String,
    event_hash: String,
    prev_hash: String,
    chain_version: i64,
}

/// 雜湊鏈驗證結果（給管理員看，不含任何密鑰內容）。
#[derive(Debug, Clone, Serialize)]
pub struct ActivityChainReport {
    /// 沒有任何列驗證失敗。注意：這不代表未受保護的舊列是可信的，見 `legacy_unsigned_rows`。
    pub ok: bool,
    pub total_rows: usize,
    /// 舊版寫入、從未簽章的列（event_hash 為空），無法補簽，單獨列出。
    pub legacy_unsigned_rows: usize,
    /// 舊公式（chain_version=1）簽章且通過驗證的列。
    pub signed_v1_rows: usize,
    /// 串鏈公式（chain_version=2）且通過驗證的列。
    pub chained_v2_rows: usize,
    /// 驗證失敗的列數。
    pub broken_rows: usize,
    /// 第一筆驗證失敗的列 id。
    pub first_broken_id: Option<i64>,
    pub first_broken_reason: Option<String>,
    /// 墓碑數量（清理時刪除、並有留下紀錄的列）。
    pub pruned_tombstones: usize,
    pub hmac_key_is_default: bool,
    pub warning: Option<String>,
}

impl AccountsDb {
    /// 逐筆驗證 activity_events 的雜湊鏈。
    pub async fn verify_activity_chain(&self) -> Result<ActivityChainReport> {
        let _chain_guard = ACTIVITY_CHAIN_LOCK.lock().await;
        let (key, key_is_default) = audit_hmac_key();

        let rows = sqlx::query_as::<_, ChainRow>(
            "SELECT id, timestamp, time, kind, actor, action, account_id, path, result, details,
                    ip, user_agent, event_hash, prev_hash, chain_version
             FROM activity_events ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取 activity_events 失敗")?;

        let pruned_tombstones: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM activity_events_pruned")
            .fetch_one(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("讀取 activity_events_pruned 失敗")?;

        let mut report = ActivityChainReport {
            ok: true,
            total_rows: rows.len(),
            legacy_unsigned_rows: 0,
            signed_v1_rows: 0,
            chained_v2_rows: 0,
            broken_rows: 0,
            first_broken_id: None,
            first_broken_reason: None,
            pruned_tombstones: pruned_tombstones.max(0) as usize,
            hmac_key_is_default: key_is_default,
            warning: key_is_default.then(|| {
                "正在使用預設 HMAC 密鑰（原始碼中公開）。攻擊者只要能讀到原始碼，就能重新計算簽章。請在 config.toml 設定 audit_hmac_key 或環境變數 TRONCLASS_AUDIT_HMAC_KEY。".to_string()
            }),
        };

        // 上一筆「存在於資料表中」的列（id, event_hash）。None 代表這是第一筆（錨點）。
        let mut prev: Option<(i64, String)> = None;
        for row in &rows {
            if row.event_hash.is_empty() {
                report.legacy_unsigned_rows += 1;
                prev = Some((row.id, String::new()));
                continue;
            }

            let fields = SignedFields {
                timestamp: row.timestamp,
                time: &row.time,
                kind: &row.kind,
                actor: &row.actor,
                action: &row.action,
                account_id: &row.account_id,
                path: &row.path,
                result: &row.result,
                details: &row.details,
                ip: &row.ip,
                user_agent: &row.user_agent,
            };

            let mut problem: Option<String> = None;
            if row.chain_version >= CHAIN_VERSION_CURRENT {
                if let Some((prev_id, prev_hash)) = prev.as_ref().map(|(i, h)| (*i, h.as_str())) {
                    // 兩筆相鄰存在列之間，若 id 有缺口，缺口中的每一個 id 都必須有墓碑；
                    // 這時 prev_hash 應指向最後一筆被清掉的列（墓碑中 id 最大者）。
                    let tombs: Vec<(i64, String)> = sqlx::query_as(
                        "SELECT id, event_hash FROM activity_events_pruned
                         WHERE id > ? AND id < ? ORDER BY id ASC",
                    )
                    .bind(prev_id)
                    .bind(row.id)
                    .fetch_all(&self.pool)
                    .await
                    .into_diagnostic()
                    .wrap_err("讀取 activity_events_pruned 失敗")?;
                    let gap = row.id - prev_id - 1;
                    if tombs.len() as i64 != gap {
                        problem = Some(format!(
                            "第 {prev_id} 筆與第 {} 筆之間有 {} 個 id 不在資料表中，但沒有清理紀錄（可能被刪除）",
                            row.id,
                            gap - tombs.len() as i64
                        ));
                    } else {
                        let expected_prev = match tombs.last() {
                            Some((_, h)) => h.as_str(),
                            None => prev_hash,
                        };
                        if row.prev_hash != expected_prev {
                            problem = Some("prev_hash 與前一筆不連續（鏈被切斷或改寫）".to_string());
                        }
                    }
                }
            }

            if problem.is_none() {
                let recomputed = if row.chain_version >= CHAIN_VERSION_CURRENT {
                    fields.hash_v2(key.as_bytes(), &row.prev_hash)
                } else {
                    fields.hash_v1(key.as_bytes())
                };
                if recomputed != row.event_hash {
                    problem = Some("HMAC 不符（列內容可能已被修改）".to_string());
                }
            }

            match problem {
                Some(reason) => {
                    report.broken_rows += 1;
                    if report.first_broken_id.is_none() {
                        report.first_broken_id = Some(row.id);
                        report.first_broken_reason = Some(reason);
                    }
                }
                None if row.chain_version >= CHAIN_VERSION_CURRENT => report.chained_v2_rows += 1,
                None => report.signed_v1_rows += 1,
            }
            prev = Some((row.id, row.event_hash.clone()));
        }

        report.ok = report.broken_rows == 0;
        Ok(report)
    }
}

pub(crate) async fn activity_event_batch_writer(
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
            if let Err(e) = prune_activity_events(&pool).await {
                eprintln!("⚠️ 清理 activity_events 失敗：{e}");
            }
        }
    }
}

/// 清理舊的活動紀錄。每一次刪除之前都先在墓碑表留下被刪列的 id 與 hash，
/// 讓驗證能區分「正常清理」與「被竄改」。整個清理在同一個交易中完成。
async fn prune_activity_events(pool: &SqlitePool) -> Result<()> {
    let _chain_guard = ACTIVITY_CHAIN_LOCK.lock().await;
    let mut tx = pool
        .begin()
        .await
        .into_diagnostic()
        .wrap_err("開始 activity_events 清理交易失敗")?;

    // 全站上限：只保留最新 500 筆。
    const KEEP_ALL_SQL: &str =
        "SELECT id FROM activity_events ORDER BY id DESC LIMIT 500";
    sqlx::query(&format!(
        "INSERT OR IGNORE INTO activity_events_pruned (id, event_hash)
         SELECT id, event_hash FROM activity_events WHERE id NOT IN ({KEEP_ALL_SQL})"
    ))
    .execute(&mut *tx)
    .await
    .into_diagnostic()
    .wrap_err("記錄清理墓碑失敗")?;
    sqlx::query(&format!(
        "DELETE FROM activity_events WHERE id NOT IN ({KEEP_ALL_SQL})"
    ))
    .execute(&mut *tx)
    .await
    .into_diagnostic()
    .wrap_err("清理 activity_events 失敗")?;

    // 再多清一道：只鎖定「登入失敗」這個子集合，只留最新 100 筆——
    // 不符合這個條件的其他活動紀錄（成功登入、其他操作稽核等）完全不受影響。
    let keep_failures_sql = format!(
        "SELECT id FROM activity_events WHERE {LOGIN_FAILURE_SQL_FILTER}
         ORDER BY id DESC LIMIT ?"
    );
    let failure_delete_where = format!(
        "{LOGIN_FAILURE_SQL_FILTER} AND id NOT IN ({keep_failures_sql})"
    );
    sqlx::query(&format!(
        "INSERT OR IGNORE INTO activity_events_pruned (id, event_hash)
         SELECT id, event_hash FROM activity_events WHERE {failure_delete_where}"
    ))
    .bind(LOGIN_FAILURE_RETENTION_ROWS)
    .execute(&mut *tx)
    .await
    .into_diagnostic()
    .wrap_err("記錄登入失敗清理墓碑失敗")?;
    sqlx::query(&format!(
        "DELETE FROM activity_events WHERE {failure_delete_where}"
    ))
    .bind(LOGIN_FAILURE_RETENTION_ROWS)
    .execute(&mut *tx)
    .await
    .into_diagnostic()
    .wrap_err("清理登入失敗紀錄失敗")?;

    tx.commit()
        .await
        .into_diagnostic()
        .wrap_err("提交 activity_events 清理交易失敗")?;
    Ok(())
}

async fn insert_activity_events_batch(
    pool: &SqlitePool,
    entries: &[ActivityEventInput],
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let (hmac_key, _) = audit_hmac_key();

    // 鎖要在開始交易之前拿，並持有到提交之後。
    let _chain_guard = ACTIVITY_CHAIN_LOCK.lock().await;

    let mut tx = pool
        .begin()
        .await
        .into_diagnostic()
        .wrap_err("開始 activity_events 批次寫入交易失敗")?;

    // 鏈頭：目前資料表中最新一筆的 event_hash（沒有列或最新列是舊版未簽章時為空字串）。
    let mut prev_hash: String = sqlx::query_scalar(
        "SELECT event_hash FROM activity_events ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await
    .into_diagnostic()
    .wrap_err("讀取 activity_events 鏈頭失敗")?
    .unwrap_or_default();

    let now = Local::now();
    let timestamp = now.timestamp();
    let time_str = now.format("%Y-%m-%d %H:%M:%S").to_string();

    for event in entries {
        let fields = SignedFields {
            timestamp,
            time: &time_str,
            kind: &event.kind,
            actor: &event.actor,
            action: &event.action,
            account_id: &event.account_id,
            path: &event.path,
            result: &event.result,
            details: &event.details,
            ip: &event.ip,
            user_agent: &event.user_agent,
        };
        let event_hash = fields.hash_v2(hmac_key.as_bytes(), &prev_hash);

        sqlx::query(
            "INSERT INTO activity_events (timestamp, time, kind, actor, action, account_id, path, result, details, ip, user_agent, event_hash, prev_hash, chain_version)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(&event.user_agent)
        .bind(&event_hash)
        .bind(&prev_hash)
        .bind(CHAIN_VERSION_CURRENT)
        .execute(&mut *tx)
        .await
        .into_diagnostic()
        .wrap_err("寫入單筆 activity_event 失敗")?;

        prev_hash = event_hash;
    }

    tx.commit()
        .await
        .into_diagnostic()
        .wrap_err("提交 activity_events 批次寫入交易失敗")?;

    Ok(())
}

#[cfg(test)]
mod chain_tests {
    use super::*;

    fn event(i: usize, kind: &str, action: &str, result: &str) -> ActivityEventInput {
        ActivityEventInput {
            kind: kind.to_string(),
            actor: format!("actor{i}"),
            action: action.to_string(),
            account_id: format!("acct{i}"),
            path: "/admin/api/login".to_string(),
            result: result.to_string(),
            details: format!("detail {i}"),
            ip: "203.0.113.7".to_string(),
            user_agent: "test-agent".to_string(),
        }
    }

    async fn open_db() -> (tempfile::TempDir, AccountsDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = AccountsDb::open(dir.path().join("accounts.db"))
            .await
            .expect("open db");
        (dir, db)
    }

    #[tokio::test]
    async fn chained_rows_verify_across_batches() {
        let (_dir, db) = open_db().await;
        insert_activity_events_batch(&db.pool, &[event(1, "login", "login", "success")])
            .await
            .unwrap();
        insert_activity_events_batch(
            &db.pool,
            &[
                event(2, "login", "login", "failed"),
                event(3, "security", "login_lock", "locked"),
            ],
        )
        .await
        .unwrap();

        let report = db.verify_activity_chain().await.unwrap();
        assert!(report.ok, "{report:?}");
        assert_eq!(report.total_rows, 3);
        assert_eq!(report.chained_v2_rows, 3);
        assert_eq!(report.legacy_unsigned_rows, 0);
        assert!(report.first_broken_id.is_none());
    }

    #[tokio::test]
    async fn modified_content_is_reported_with_first_broken_id() {
        let (_dir, db) = open_db().await;
        insert_activity_events_batch(
            &db.pool,
            &[event(1, "login", "login", "success"), event(2, "login", "login", "success")],
        )
        .await
        .unwrap();
        sqlx::query("UPDATE activity_events SET account_id = 'forged' WHERE id = 2")
            .execute(&db.pool)
            .await
            .unwrap();

        let report = db.verify_activity_chain().await.unwrap();
        assert!(!report.ok);
        assert_eq!(report.first_broken_id, Some(2));
        assert_eq!(report.broken_rows, 1);
    }

    #[tokio::test]
    async fn deleted_middle_row_without_tombstone_is_detected() {
        let (_dir, db) = open_db().await;
        for i in 0..3 {
            insert_activity_events_batch(&db.pool, &[event(i, "login", "login", "success")])
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM activity_events WHERE id = 2")
            .execute(&db.pool)
            .await
            .unwrap();

        let report = db.verify_activity_chain().await.unwrap();
        assert!(!report.ok);
        assert_eq!(report.first_broken_id, Some(3));
    }

    #[tokio::test]
    async fn prune_leaves_tombstones_and_chain_still_verifies() {
        let (_dir, db) = open_db().await;
        // 150 筆登入失敗 + 1 筆成功：清理後只留最新 100 筆失敗，中間會有大量缺口。
        for i in 0..150 {
            insert_activity_events_batch(&db.pool, &[event(i, "login", "login", "failed")])
                .await
                .unwrap();
        }
        insert_activity_events_batch(&db.pool, &[event(999, "login", "login", "success")])
            .await
            .unwrap();
        prune_activity_events(&db.pool).await.unwrap();

        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM activity_events")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(remaining, 101);

        let report = db.verify_activity_chain().await.unwrap();
        assert!(report.ok, "{report:?}");
        assert_eq!(report.pruned_tombstones, 50);
        assert_eq!(report.chained_v2_rows, 101);
    }

    #[tokio::test]
    async fn legacy_unsigned_rows_are_counted_separately_and_chain_continues() {
        let (_dir, db) = open_db().await;
        sqlx::query(
            "INSERT INTO activity_events (timestamp, time, kind, actor, action, account_id, path, result, details, ip, user_agent, event_hash)
             VALUES (1, '2026-01-01 00:00:00', 'login', 'old', 'login', 'old', '', 'success', '', '', '', '')",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        insert_activity_events_batch(&db.pool, &[event(1, "login", "login", "success")])
            .await
            .unwrap();

        let report = db.verify_activity_chain().await.unwrap();
        assert!(report.ok, "{report:?}");
        assert_eq!(report.legacy_unsigned_rows, 1);
        assert_eq!(report.chained_v2_rows, 1);
    }

    #[tokio::test]
    async fn v1_rows_verify_under_original_formula() {
        let (_dir, db) = open_db().await;
        let key = audit_hmac_key().0;
        let e = event(1, "login", "login", "success");
        let fields = SignedFields {
            timestamp: 1_700_000_000,
            time: "2023-11-14 22:13:20",
            kind: &e.kind,
            actor: &e.actor,
            action: &e.action,
            account_id: &e.account_id,
            path: &e.path,
            result: &e.result,
            details: &e.details,
            ip: &e.ip,
            user_agent: &e.user_agent,
        };
        let v1_hash = fields.hash_v1(key.as_bytes());
        sqlx::query(
            "INSERT INTO activity_events (timestamp, time, kind, actor, action, account_id, path, result, details, ip, user_agent, event_hash, prev_hash, chain_version)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '', 1)",
        )
        .bind(1_700_000_000_i64)
        .bind("2023-11-14 22:13:20")
        .bind(&e.kind)
        .bind(&e.actor)
        .bind(&e.action)
        .bind(&e.account_id)
        .bind(&e.path)
        .bind(&e.result)
        .bind(&e.details)
        .bind(&e.ip)
        .bind(&e.user_agent)
        .bind(&v1_hash)
        .execute(&db.pool)
        .await
        .unwrap();
        insert_activity_events_batch(&db.pool, &[event(2, "login", "login", "success")])
            .await
            .unwrap();

        let report = db.verify_activity_chain().await.unwrap();
        assert!(report.ok, "{report:?}");
        assert_eq!(report.signed_v1_rows, 1);
        assert_eq!(report.chained_v2_rows, 1);
    }

    #[test]
    fn v2_hash_does_not_collide_across_field_boundaries() {
        fn base<'a>(actor: &'a str, action: &'a str) -> SignedFields<'a> {
            SignedFields {
                timestamp: 1,
                time: "t",
                kind: "k",
                actor,
                action,
                account_id: "",
                path: "",
                result: "",
                details: "",
                ip: "",
                user_agent: "",
            }
        }
        let a = base("ab", "c").hash_v2(b"key", "");
        let b = base("a", "bc").hash_v2(b"key", "");
        assert_ne!(a, b);
        // 前一筆 hash 不同，結果也要不同（鏈接）。
        assert_ne!(base("a", "b").hash_v2(b"key", "x"), base("a", "b").hash_v2(b"key", "y"));
    }
}
