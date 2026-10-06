//! ADMIN_GEO_RESTRICT_V1：全站共用、與單一帳號無關的安全性開關（key/value）。
//!
//! 目前唯一用途：「僅限台灣 IP 存取 /admin」開關
//! （鍵＝[`GEO_RESTRICT_ADMIN_KEY`]）。日後若有其他全站性的安全開關，
//! 可以沿用同一張 `security_settings` 表，不必每個開關都另開一張表。
//!
//! 這裡只提供最基本的讀寫；「讀不到鍵時的預設值」與「讀取失敗時要不要
//! fail-open」這類屬於業務政策的判斷，刻意留給呼叫端（`adapters::admin_web`）
//! 決定，這一層只負責誠實回報資料庫裡實際存了什麼。

use chrono::Local;
use miette::{IntoDiagnostic, Result, WrapErr};

use super::AccountsDb;

/// 「僅限台灣 IP 存取 /admin」開關的鍵名。值＝`"1"`（開）或 `"0"`（關）。
pub const GEO_RESTRICT_ADMIN_KEY: &str = "geo_restrict_admin_enabled";

impl AccountsDb {
    /// 讀取一個全站安全性開關的原始值；鍵不存在回 `None`（由呼叫端決定預設值）。
    pub async fn get_security_setting(&self, key: &str) -> Result<Option<String>> {
        // 與 `get_web_push_setting`（同樣的 key/value 表結構）同一種寫法：
        // `query_as::<_, (String,)>` + `fetch_optional`，不用 `query_scalar`。
        let row = sqlx::query_as::<_, (String,)>(
            "SELECT value FROM security_settings WHERE key = ?",
        )
        .bind(key.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取安全性開關 `{key}` 失敗"))?;
        Ok(row.map(|(value,)| value))
    }

    /// 寫入（或覆蓋）一個全站安全性開關。`updated_by` 記錄是哪個管理員改的，供事後稽核。
    pub async fn set_security_setting(&self, key: &str, value: &str, updated_by: &str) -> Result<()> {
        let now = Local::now();
        sqlx::query(
            "INSERT INTO security_settings (key, value, updated_at, updated_by)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at,
                updated_by = excluded.updated_by",
        )
        .bind(key.trim())
        .bind(value.trim())
        .bind(now.timestamp())
        .bind(updated_by.trim())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("寫入安全性開關 `{key}` 失敗"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_set_roundtrip_and_missing_key_is_none() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        assert_eq!(
            db.get_security_setting(GEO_RESTRICT_ADMIN_KEY).await.unwrap(),
            None
        );

        db.set_security_setting(GEO_RESTRICT_ADMIN_KEY, "0", "admin-a")
            .await
            .unwrap();
        assert_eq!(
            db.get_security_setting(GEO_RESTRICT_ADMIN_KEY).await.unwrap(),
            Some("0".to_string())
        );

        // 覆寫同一個鍵：ON CONFLICT 分支要生效，不能變成第二筆。
        db.set_security_setting(GEO_RESTRICT_ADMIN_KEY, "1", "admin-b")
            .await
            .unwrap();
        assert_eq!(
            db.get_security_setting(GEO_RESTRICT_ADMIN_KEY).await.unwrap(),
            Some("1".to_string())
        );
    }
}
