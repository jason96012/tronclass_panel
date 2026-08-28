//! 課表與課表模板。

use chrono::Local;
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use sqlx::FromRow;

use super::{
    AccountsDb, MAX_TIMETABLE_ENTRIES_PER_ACCOUNT, MAX_TIMETABLE_LIST_ALL_ROWS,
    MAX_TIMETABLE_TEMPLATE_CREATOR_BYTES, MAX_TIMETABLE_TEMPLATE_DESCRIPTION_BYTES,
    MAX_TIMETABLE_TEMPLATE_JSON_BYTES, MAX_TIMETABLE_TEMPLATE_NAME_BYTES,
    MAX_TIMETABLE_TEMPLATES,
};

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

impl AccountsDb {
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
}
