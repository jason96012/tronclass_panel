//! IP 黑名單。

use chrono::Local;
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use sqlx::FromRow;

use super::{
    truncate_event_text, AccountsDb, EVENT_DETAILS_MAX_BYTES, EVENT_IDENTITY_MAX_BYTES,
};

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

impl AccountsDb {
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
}
