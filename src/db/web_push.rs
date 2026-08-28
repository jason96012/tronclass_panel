//! Web Push 設定與訂閱。

use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::FromRow;

use super::AccountsDb;

#[derive(Debug, Clone, FromRow)]
pub struct WebPushSubscriptionRow {
    pub account_id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

impl AccountsDb {
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
}
