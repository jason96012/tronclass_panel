//! 登入 / 安全活動事件（含背景批次寫入）。

use chrono::Local;
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};
use tokio::sync::mpsc;

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
                "SELECT id, timestamp, time, kind, actor, action, account_id, path, result, details, ip, user_agent
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
                "SELECT id, timestamp, time, kind, actor, action, account_id, path, result, details, ip, user_agent
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
            let _ = sqlx::query(
                "DELETE FROM activity_events WHERE id NOT IN (
                    SELECT id FROM activity_events ORDER BY id DESC LIMIT 500
                )",
            )
            .execute(&pool)
            .await;
            // 再多清一道：只鎖定「登入失敗」這個子集合，只留最新 100 筆——
            // 不符合這個條件的其他活動紀錄（成功登入、其他操作稽核等）完全
            // 不受影響，所以即使上面那道 500 筆的全站上限還沒觸發，這裡也可能
            // 先把舊的登入失敗紀錄清掉。
            let prune_sql = format!(
                "DELETE FROM activity_events
                 WHERE {LOGIN_FAILURE_SQL_FILTER}
                   AND id NOT IN (
                     SELECT id FROM activity_events
                     WHERE {LOGIN_FAILURE_SQL_FILTER}
                     ORDER BY id DESC LIMIT ?
                   )"
            );
            let _ = sqlx::query(&prune_sql)
                .bind(LOGIN_FAILURE_RETENTION_ROWS)
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
            "INSERT INTO activity_events (timestamp, time, kind, actor, action, account_id, path, result, details, ip, user_agent)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::MaterialEventInput;
    use std::time::Duration;
    use tempfile::NamedTempFile;

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
                user_agent: "Mozilla/5.0 (Macintosh) Chrome/120".to_string(),
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
}
