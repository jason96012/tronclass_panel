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
    /// PUSH_STALE_CLEANUP_V1：目前連續失敗次數（成功即歸零）。
    pub consecutive_failures: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct WebPushBindingOverviewRow {
    pub account_id: String,
    pub bindings: i64,
    pub last_updated: Option<i64>,
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
                updated_at = excluded.updated_at,
                consecutive_failures = 0",
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
            "SELECT account_id, endpoint, p256dh, auth, consecutive_failures
             FROM web_push_subscriptions WHERE account_id = ?
             ORDER BY updated_at DESC LIMIT 16",
        )
        .bind(account_id.trim())
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` Web Push 訂閱失敗"))
    }

    /// PUSH_OVERVIEW_V1：全部帳號的推播綁定統計（管理員總覽用）。
    pub async fn list_web_push_binding_overview(&self) -> Result<Vec<WebPushBindingOverviewRow>> {
        sqlx::query_as::<_, WebPushBindingOverviewRow>(
            "SELECT account_id, COUNT(*) AS bindings, MAX(updated_at) AS last_updated
             FROM web_push_subscriptions
             WHERE account_id != ''
             GROUP BY account_id
             ORDER BY account_id",
        )
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取 Web Push 綁定總覽失敗")
    }

    /// PUSH_HEALTH_V1：記錄帳號的推播發送結果（存於 web_push_settings KV）。
    pub async fn record_web_push_health(
        &self,
        account_id: &str,
        ok_count: usize,
        fail_count: usize,
        last_error: Option<&str>,
    ) -> Result<()> {
        let key = format!("push_health_v1:{}", account_id.trim());
        let now = chrono::Local::now().timestamp();
        let mut entry: serde_json::Value = match self.get_web_push_setting(&key).await? {
            Some(raw) => serde_json::from_str(&raw).unwrap_or(serde_json::json!({})),
            None => serde_json::json!({}),
        };
        if !entry.is_object() {
            entry = serde_json::json!({});
        }
        // UNWRAP_SWEEP_V1：上一行已確保是 object；仍以 let-else 收斂。
        let Some(object) = entry.as_object_mut() else {
            return Ok(());
        };
        let read_u64 = |field: &str| {
            object
                .get(field)
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        };
        let ok_total = read_u64("ok_total") + ok_count as u64;
        let fail_total = read_u64("fail_total") + fail_count as u64;
        let consecutive_fails = if fail_count > 0 {
            read_u64("consecutive_fails") + fail_count as u64
        } else {
            0
        };
        object.insert("last_attempt_at".to_string(), serde_json::json!(now));
        if ok_count > 0 {
            object.insert("last_ok_at".to_string(), serde_json::json!(now));
        }
        if fail_count > 0 {
            object.insert("last_fail_at".to_string(), serde_json::json!(now));
        }
        object.insert("ok_total".to_string(), serde_json::json!(ok_total));
        object.insert("fail_total".to_string(), serde_json::json!(fail_total));
        object.insert(
            "consecutive_fails".to_string(),
            serde_json::json!(consecutive_fails),
        );
        match last_error.map(str::trim).filter(|value| !value.is_empty()) {
            Some(error) if fail_count > 0 => {
                object.insert("last_error".to_string(), serde_json::json!(error));
            }
            _ if fail_count == 0 => {
                object.remove("last_error");
            }
            _ => {}
        }
        self.set_web_push_setting(&key, &entry.to_string()).await
    }

    /// PUSH_HEALTH_V1：讀取單一帳號的推播發送健康紀錄。
    pub async fn get_web_push_health(
        &self,
        account_id: &str,
    ) -> Result<Option<serde_json::Value>> {
        let key = format!("push_health_v1:{}", account_id.trim());
        let Some(raw) = self.get_web_push_setting(&key).await? else {
            return Ok(None);
        };
        Ok(serde_json::from_str(&raw).ok())
    }

    /// PUSH_HEALTH_V1：全部帳號的推播發送健康紀錄（管理員總覽用）。
    pub async fn list_web_push_health(&self) -> Result<Vec<(String, serde_json::Value)>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT key, value FROM web_push_settings
             WHERE substr(key, 1, 15) = 'push_health_v1:'",
        )
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取 Web Push 健康紀錄失敗")?;
        Ok(rows
            .into_iter()
            .filter_map(|(key, value)| {
                let account_id = key.strip_prefix("push_health_v1:")?;
                let parsed = serde_json::from_str(&value).ok()?;
                Some((account_id.to_string(), parsed))
            })
            .collect())
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

    /// PUSH_STALE_CLEANUP_V1：成功送達 → 連續失敗歸零（只在原本非 0 時寫入，減少 fsync）。
    pub async fn reset_web_push_failures(&self, account_id: &str, endpoint: &str) -> Result<()> {
        sqlx::query(
            "UPDATE web_push_subscriptions SET consecutive_failures = 0
             WHERE account_id = ? AND endpoint = ? AND consecutive_failures != 0",
        )
        .bind(account_id.trim())
        .bind(endpoint.trim())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("重設 Web Push 失敗計數失敗")?;
        Ok(())
    }

    /// PUSH_STALE_CLEANUP_V1：失敗一次 → 計數 +1，回傳更新後的次數（端點已不存在時回 None）。
    pub async fn bump_web_push_failures(
        &self,
        account_id: &str,
        endpoint: &str,
    ) -> Result<Option<i64>> {
        let row = sqlx::query_as::<_, (i64,)>(
            "UPDATE web_push_subscriptions
             SET consecutive_failures = consecutive_failures + 1
             WHERE account_id = ? AND endpoint = ?
             RETURNING consecutive_failures",
        )
        .bind(account_id.trim())
        .bind(endpoint.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("累計 Web Push 失敗計數失敗")?;
        Ok(row.map(|(count,)| count))
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
