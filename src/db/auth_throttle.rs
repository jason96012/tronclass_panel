//! LOGIN_THROTTLE_SQLITE_V1：登入節流／失敗計數／帳號鎖定的持久化。
//!
//! 取代 admin_web 內純記憶體的 `RateLimiter`、`SECURITY_WATCH.fails`、`LOGIN_LOCKS`
//! 作為「主要」儲存：重啟後計數與鎖定仍在。記憶體版本保留為 DB 失敗時的退路
//! （admin_web 端 `Err` 時 fallback），所以這裡的每個方法都回傳 `Result` 而不吞錯。
//!
//! scope 慣例：
//! - `rate`：短窗（60 秒）速率限制，key 例如 `login:<user>:<ip>`、`login-ip:<ip>`。
//! - `fail`：15 分鐘觀察窗的密碼失敗計數，key 為 `user:<name>` 或 `ip:<ip>`。

use miette::{IntoDiagnostic, Result, WrapErr};

use super::AccountsDb;

/// 單一 (scope, key) 允許保留的最大列數；超過就砍最舊的，避免被灌爆。
const MAX_ROWS_PER_KEY: i64 = 256;

impl AccountsDb {
    /// 記錄一次嘗試並回傳 window 內（含這次）的次數。
    pub async fn throttle_record(&self, scope: &str, key: &str, now: i64, window_secs: i64) -> Result<u32> {
        let since = now - window_secs;
        sqlx::query("INSERT INTO auth_throttle_events (scope, key, at) VALUES (?, ?, ?)")
            .bind(scope)
            .bind(key)
            .bind(now)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("寫入節流事件失敗")?;
        // 同鍵列數上限：刪掉超出的最舊列（一次 INSERT 最多多一列，所以只在超過時才掃）。
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth_throttle_events WHERE scope = ? AND key = ?",
        )
        .bind(scope)
        .bind(key)
        .fetch_one(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取節流事件列數失敗")?;
        if total > MAX_ROWS_PER_KEY {
            sqlx::query(
                "DELETE FROM auth_throttle_events WHERE id IN (
                    SELECT id FROM auth_throttle_events WHERE scope = ? AND key = ?
                    ORDER BY at ASC LIMIT ?
                )",
            )
            .bind(scope)
            .bind(key)
            .bind(total - MAX_ROWS_PER_KEY)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("修剪節流事件失敗")?;
        }
        self.throttle_count(scope, key, since).await
    }

    /// window 內的嘗試次數（不寫入）。`since` 為 window 起點（Unix 秒）。
    pub async fn throttle_count(&self, scope: &str, key: &str, since: i64) -> Result<u32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth_throttle_events WHERE scope = ? AND key = ? AND at > ?",
        )
        .bind(scope)
        .bind(key)
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取節流事件失敗")?;
        Ok(count.max(0) as u32)
    }

    /// 清掉某鍵的全部事件（登入成功後歸零）。
    pub async fn throttle_clear(&self, scope: &str, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM auth_throttle_events WHERE scope = ? AND key = ?")
            .bind(scope)
            .bind(key)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("清除節流事件失敗")?;
        Ok(())
    }

    /// 刪除所有早於 `before` 的事件與已到期的鎖（背景任務每 5 分鐘呼叫）。
    pub async fn throttle_prune(&self, before: i64, now: i64) -> Result<u64> {
        let events = sqlx::query("DELETE FROM auth_throttle_events WHERE at <= ?")
            .bind(before)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("修剪節流事件失敗")?
            .rows_affected();
        let locks = sqlx::query("DELETE FROM login_locks WHERE until <= ?")
            .bind(now)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("修剪登入鎖定失敗")?
            .rows_affected();
        Ok(events + locks)
    }

    /// 對一組鍵上鎖到 `until`；已有生效中鎖的鍵不延長。回傳是否有「新建立」的鎖。
    pub async fn login_lock_insert(&self, keys: &[String], until: i64, now: i64) -> Result<bool> {
        let mut created = false;
        for key in keys {
            let key = key.trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            // 到期的舊鎖可被覆蓋；生效中的不動（INSERT OR IGNORE + 先刪過期）。
            sqlx::query("DELETE FROM login_locks WHERE key = ? AND until <= ?")
                .bind(&key)
                .bind(now)
                .execute(&self.pool)
                .await
                .into_diagnostic()
                .wrap_err("清除過期登入鎖定失敗")?;
            let inserted = sqlx::query(
                "INSERT OR IGNORE INTO login_locks (key, until, created_at) VALUES (?, ?, ?)",
            )
            .bind(&key)
            .bind(until)
            .bind(now)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("寫入登入鎖定失敗")?
            .rows_affected();
            if inserted > 0 {
                created = true;
            }
        }
        Ok(created)
    }

    /// 任一鍵仍在鎖定中 → 回傳剩餘秒數（取最大值）。
    pub async fn login_lock_remaining(&self, keys: &[String], now: i64) -> Result<Option<u64>> {
        let mut best: Option<u64> = None;
        for key in keys {
            let key = key.trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            let until: Option<i64> =
                sqlx::query_scalar("SELECT until FROM login_locks WHERE key = ? AND until > ?")
                    .bind(&key)
                    .bind(now)
                    .fetch_optional(&self.pool)
                    .await
                    .into_diagnostic()
                    .wrap_err("讀取登入鎖定失敗")?;
            if let Some(until) = until {
                let remaining = (until - now).max(1) as u64;
                best = Some(best.map_or(remaining, |b| b.max(remaining)));
            }
        }
        Ok(best)
    }

    /// 解除一組鍵的鎖定；回傳是否真的有鎖被移除。
    pub async fn login_lock_remove(&self, keys: &[String]) -> Result<bool> {
        let mut removed = false;
        for key in keys {
            let key = key.trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            let affected = sqlx::query("DELETE FROM login_locks WHERE key = ?")
                .bind(&key)
                .execute(&self.pool)
                .await
                .into_diagnostic()
                .wrap_err("移除登入鎖定失敗")?
                .rows_affected();
            if affected > 0 {
                removed = true;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn throttle_counts_within_window_and_clears() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let now = 1_800_000_000;
        assert_eq!(db.throttle_record("rate", "login:a:1.2.3.4", now, 60).await.unwrap(), 1);
        assert_eq!(db.throttle_record("rate", "login:a:1.2.3.4", now + 1, 60).await.unwrap(), 2);
        // 舊事件落在 window 之外就不算。
        assert_eq!(db.throttle_count("rate", "login:a:1.2.3.4", now + 100).await.unwrap(), 0);
        db.throttle_clear("rate", "login:a:1.2.3.4").await.unwrap();
        assert_eq!(db.throttle_count("rate", "login:a:1.2.3.4", 0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn login_locks_do_not_extend_and_can_be_removed() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let now = 1_800_000_000;
        let keys = vec!["Acct".to_string(), "S123".to_string()];
        assert!(db.login_lock_insert(&keys, now + 600, now).await.unwrap());
        assert!(!db.login_lock_insert(&keys, now + 6000, now).await.unwrap());
        let remaining = db.login_lock_remaining(&["acct".to_string()], now + 10).await.unwrap();
        assert_eq!(remaining, Some(590));
        assert!(db.login_lock_remove(&["s123".to_string()]).await.unwrap());
        assert!(db.login_lock_remaining(&["s123".to_string()], now).await.unwrap().is_none());
        // 過期後自動視為不存在，且可以重新上鎖。
        assert!(db.login_lock_remaining(&["acct".to_string()], now + 601).await.unwrap().is_none());
        assert!(db.login_lock_insert(&["acct".to_string()], now + 1200, now + 601).await.unwrap());
        assert_eq!(db.throttle_prune(now, now + 700).await.unwrap(), 0);
    }
}
