//! ACCOUNT_SUSPEND_V1：帳號暫時停權。
//!
//! 停權只影響「進入後台」：密碼／通行密鑰登入、session 驗證、refresh 輪換都會
//! 檢查這張表。停權是「暫時」的——`until` 到期後列即失效（判斷一律以
//! `until > now` 為準，不依賴背景清除）。與 `accounts.enabled` 不同：後者沒有
//! 到期時間，也沒有網頁介面，不能拿來做暫時停權。

use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use sqlx::FromRow;

use super::AccountsDb;

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct AccountSuspensionRow {
    pub account_id: String,
    /// 停權到期時間（Unix 秒）。
    pub until: i64,
    pub reason: String,
    pub created_at: i64,
    pub created_by: String,
}

impl AccountsDb {
    /// 設定（或覆寫）某帳號的停權到期時間。同一帳號重複設定會直接覆寫。
    pub async fn account_suspension_set(
        &self,
        account_id: &str,
        until: i64,
        reason: &str,
        created_by: &str,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO account_suspensions (account_id, until, reason, created_at, created_by)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
                until = excluded.until,
                reason = excluded.reason,
                created_at = excluded.created_at,
                created_by = excluded.created_by",
        )
        .bind(account_id)
        .bind(until)
        .bind(reason)
        .bind(now)
        .bind(created_by)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入帳號停權失敗")?;
        Ok(())
    }

    /// 解除某帳號的停權。回傳是否確實刪到列。
    pub async fn account_suspension_clear(&self, account_id: &str) -> Result<bool> {
        let rows = sqlx::query("DELETE FROM account_suspensions WHERE account_id = ?")
            .bind(account_id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("解除帳號停權失敗")?
            .rows_affected();
        Ok(rows > 0)
    }

    /// 查詢帳號目前是否處於生效中的停權（`until > now`）。沒有則回傳 `None`。
    pub async fn account_suspension_active(
        &self,
        account_id: &str,
        now: i64,
    ) -> Result<Option<AccountSuspensionRow>> {
        sqlx::query_as::<_, AccountSuspensionRow>(
            "SELECT account_id, until, reason, created_at, created_by
             FROM account_suspensions WHERE account_id = ? AND until > ?",
        )
        .bind(account_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取帳號停權失敗")
    }

    /// 列出所有生效中的停權（到期時間由近到遠）。
    pub async fn account_suspension_list_active(&self, now: i64) -> Result<Vec<AccountSuspensionRow>> {
        sqlx::query_as::<_, AccountSuspensionRow>(
            "SELECT account_id, until, reason, created_at, created_by
             FROM account_suspensions WHERE until > ? ORDER BY until ASC",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("列出帳號停權失敗")
    }

    /// 刪除已到期的停權列（僅為整潔，判斷本身不依賴它）。回傳刪除列數。
    pub async fn account_suspension_prune(&self, now: i64) -> Result<u64> {
        let rows = sqlx::query("DELETE FROM account_suspensions WHERE until <= ?")
            .bind(now)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("清除過期帳號停權失敗")?
            .rows_affected();
        Ok(rows)
    }
}
