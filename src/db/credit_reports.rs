//! 學分／成績報告快照的存取（CREDITS_EMIS_V1）。
//!
//! 報告由 `timetable_source::tku_credits::fetch_credit_report` 產出，
//! 以 JSON 快照形式存進 `credit_reports` 表（每帳號一列，upsert）。
//! 讀取時回傳 `(json, fetched_at)`，由 API 層決定要不要再解析。

use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::Row;

use super::AccountsDb;

impl AccountsDb {
    /// 寫入（或覆蓋）某帳號的學分報告快照。
    pub async fn upsert_credit_report(
        &self,
        account_id: &str,
        report_json: &str,
        fetched_at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO credit_reports (account_id, report_json, fetched_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id) DO UPDATE SET
                report_json = excluded.report_json,
                fetched_at = excluded.fetched_at",
        )
        .bind(account_id)
        .bind(report_json)
        .bind(fetched_at)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入學分報告快照失敗")?;
        Ok(())
    }

    /// 讀取某帳號的學分報告快照（`(report_json, fetched_at)`；無資料為 None）。
    pub async fn get_credit_report(&self, account_id: &str) -> Result<Option<(String, i64)>> {
        let row = sqlx::query(
            "SELECT report_json, fetched_at FROM credit_reports WHERE account_id = ?1",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取學分報告快照失敗")?;
        Ok(row.map(|r| {
            let json: String = r.try_get("report_json").unwrap_or_default();
            let fetched_at: i64 = r.try_get("fetched_at").unwrap_or(0);
            (json, fetched_at)
        }))
    }
}
