//! 課表自動同步狀態（TIMETABLE_AUTO_SYNC_V1）。
//!
//! 本模組只負責「同步狀態機」的持久化，以及 `source='auto'` 課表列的
//! 覆寫。設計上刻意與 `timetable.rs`（手動匯入／模板）分離：
//!
//! - 自動同步**只會**刪除並重建 `source='auto'` 的列。
//!   使用者手動輸入（`manual`）、CSV 匯入（`import`）、模板套用（`template`）
//!   的資料永遠不會被爬蟲覆蓋掉。
//! - `enabled` 預設為 0（opt-in）。沒有明確開啟的帳號不會被排程器碰到，
//!   也就不會有任何非預期的對外連線。
//! - `last_error` 由呼叫端負責清洗（不得含密碼／cookie），這裡只做長度截斷。

use chrono::Local;
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use sqlx::FromRow;

use super::{AccountsDb, MAX_TIMETABLE_ENTRIES_PER_ACCOUNT};

/// `last_error` / `last_summary` 的儲存上限，避免上游錯誤訊息把資料庫撐大。
pub(crate) const MAX_SYNC_TEXT_BYTES: usize = 512;

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct TimetableSyncStateRow {
    pub account_id: String,
    pub enabled: i64,
    pub last_success_at: i64,
    pub last_attempt_at: i64,
    pub consecutive_failures: i64,
    pub last_error: String,
    pub last_summary: String,
}

impl TimetableSyncStateRow {
    pub fn disabled_default(account_id: &str) -> Self {
        Self {
            account_id: account_id.to_string(),
            enabled: 0,
            last_success_at: 0,
            last_attempt_at: 0,
            consecutive_failures: 0,
            last_error: String::new(),
            last_summary: String::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled != 0
    }
}

/// 截斷並清掉控制字元，確保寫進 DB 的訊息是安全、有界的。
fn clamp_sync_text(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.len() <= MAX_SYNC_TEXT_BYTES {
        return cleaned.to_string();
    }
    let mut end = MAX_SYNC_TEXT_BYTES;
    while end > 0 && !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = cleaned[..end].to_string();
    out.push('…');
    out
}

impl AccountsDb {
    /// 讀取單一帳號的同步狀態；不存在時回傳「停用」的預設值。
    pub async fn get_timetable_sync_state(
        &self,
        account_id: &str,
    ) -> Result<TimetableSyncStateRow> {
        let account_id = account_id.trim();
        let row = sqlx::query_as::<_, TimetableSyncStateRow>(
            "SELECT account_id, enabled, last_success_at, last_attempt_at,
                    consecutive_failures, last_error, last_summary
             FROM timetable_sync_state
             WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 課表同步狀態失敗"))?;

        Ok(row.unwrap_or_else(|| TimetableSyncStateRow::disabled_default(account_id)))
    }

    /// 列出所有已啟用自動同步的帳號狀態。排程器每輪只會處理這些帳號。
    pub async fn list_enabled_timetable_sync_states(&self) -> Result<Vec<TimetableSyncStateRow>> {
        let rows = sqlx::query_as::<_, TimetableSyncStateRow>(
            "SELECT account_id, enabled, last_success_at, last_attempt_at,
                    consecutive_failures, last_error, last_summary
             FROM timetable_sync_state
             WHERE enabled != 0
             ORDER BY account_id",
        )
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取已啟用的課表同步帳號失敗")?;
        Ok(rows)
    }

    /// 開啟／關閉某帳號的自動同步。關閉時一併清掉錯誤計數，
    /// 讓使用者「關掉再開」等同於重置退避狀態。
    pub async fn set_timetable_sync_enabled(&self, account_id: &str, enabled: bool) -> Result<()> {
        let account_id = account_id.trim();
        let flag = i64::from(enabled);
        sqlx::query(
            "INSERT INTO timetable_sync_state (account_id, enabled)
             VALUES (?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
               enabled = excluded.enabled,
               consecutive_failures = 0,
               last_error = ''",
        )
        .bind(account_id)
        .bind(flag)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("更新 `{account_id}` 課表同步開關失敗"))?;
        Ok(())
    }

    /// 記錄一次「嘗試」。在真正發出請求前呼叫，讓當機／逾時也留有痕跡。
    pub async fn record_timetable_sync_attempt(&self, account_id: &str) -> Result<()> {
        let account_id = account_id.trim();
        let now = Local::now().timestamp();
        sqlx::query(
            "INSERT INTO timetable_sync_state (account_id, last_attempt_at)
             VALUES (?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
               last_attempt_at = excluded.last_attempt_at",
        )
        .bind(account_id)
        .bind(now)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("記錄 `{account_id}` 課表同步嘗試失敗"))?;
        Ok(())
    }

    /// 記錄一次成功：清空錯誤、歸零連續失敗次數。
    pub async fn record_timetable_sync_success(
        &self,
        account_id: &str,
        summary: &str,
    ) -> Result<()> {
        let account_id = account_id.trim();
        let now = Local::now().timestamp();
        let summary = clamp_sync_text(summary);
        sqlx::query(
            "INSERT INTO timetable_sync_state
               (account_id, last_success_at, last_attempt_at, consecutive_failures,
                last_error, last_summary)
             VALUES (?, ?, ?, 0, '', ?)
             ON CONFLICT(account_id) DO UPDATE SET
               last_success_at = excluded.last_success_at,
               last_attempt_at = excluded.last_attempt_at,
               consecutive_failures = 0,
               last_error = '',
               last_summary = excluded.last_summary",
        )
        .bind(account_id)
        .bind(now)
        .bind(now)
        .bind(summary)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("記錄 `{account_id}` 課表同步成功失敗"))?;
        Ok(())
    }

    /// 記錄一次失敗並累加連續失敗次數（排程器據此做指數退避）。
    ///
    /// `error` 必須已由呼叫端清洗過機敏字串。
    pub async fn record_timetable_sync_failure(
        &self,
        account_id: &str,
        error: &str,
    ) -> Result<i64> {
        let account_id = account_id.trim();
        let now = Local::now().timestamp();
        let error = clamp_sync_text(error);
        sqlx::query(
            "INSERT INTO timetable_sync_state
               (account_id, last_attempt_at, consecutive_failures, last_error)
             VALUES (?, ?, 1, ?)
             ON CONFLICT(account_id) DO UPDATE SET
               last_attempt_at = excluded.last_attempt_at,
               consecutive_failures = timetable_sync_state.consecutive_failures + 1,
               last_error = excluded.last_error",
        )
        .bind(account_id)
        .bind(now)
        .bind(error)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("記錄 `{account_id}` 課表同步失敗狀態失敗"))?;

        let state = self.get_timetable_sync_state(account_id).await?;
        Ok(state.consecutive_failures)
    }

    /// 只讀取某帳號 `source='auto'` 的課表列，供差異比對使用。
    pub async fn list_auto_timetable_entries(
        &self,
        account_id: &str,
    ) -> Result<Vec<super::TimetableEntryRow>> {
        let rows = sqlx::query_as::<_, super::TimetableEntryRow>(
            "SELECT id, account_id, weekday, period_start, period_end, start_time, end_time,
                    course_name, teacher, classroom, course_code, source, updated_at, updated_time
             FROM timetable_entries
             WHERE account_id = ? AND source = 'auto'
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
        .wrap_err_with(|| format!("讀取 `{account_id}` 自動同步課表失敗"))?;
        Ok(rows)
    }

    /// 以單一交易覆寫某帳號 `source='auto'` 的課表列。
    ///
    /// 關鍵不變量：`DELETE` 帶有 `source = 'auto'` 條件，因此手動／匯入／模板
    /// 來源的列不受影響。這是自動同步不會毀掉使用者手工課表的保證。
    pub async fn replace_auto_timetable_entries(
        &self,
        account_id: &str,
        entries: Vec<super::TimetableEntryInput>,
    ) -> Result<usize> {
        let account_id = account_id.trim();
        let now = Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();

        let mut tx = self
            .pool
            .begin()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("開始 `{account_id}` 自動課表交易失敗"))?;

        sqlx::query("DELETE FROM timetable_entries WHERE account_id = ? AND source = 'auto'")
            .bind(account_id)
            .execute(&mut *tx)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("清除 `{account_id}` 舊自動課表失敗"))?;

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
            .wrap_err_with(|| format!("寫入 `{account_id}` 自動課表失敗"))?;
            inserted += 1;
        }

        tx.commit()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("提交 `{account_id}` 自動課表交易失敗"))?;
        Ok(inserted)
    }
}

