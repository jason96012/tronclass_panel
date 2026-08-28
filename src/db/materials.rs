//! 教材操作稽核事件（含背景批次寫入）。

use chrono::Local;
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};
use tokio::sync::mpsc;

use super::{
    truncate_event_text, AccountsDb, EVENT_ACTION_MAX_BYTES, EVENT_DETAILS_MAX_BYTES,
    EVENT_FILE_NAME_MAX_BYTES, EVENT_IDENTITY_MAX_BYTES, EVENT_PATH_MAX_BYTES,
    EVENT_PRUNE_INTERVAL, EVENT_RESULT_MAX_BYTES,
};

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

impl AccountsDb {
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
}

pub(crate) async fn material_event_batch_writer(
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
