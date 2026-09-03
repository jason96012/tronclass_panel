//! 帳號 CRUD 與 Discord / Line 綁定。

use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::FromRow;

use super::{AccountsDb, MAX_USERNAME_MATCH_ROWS};
use crate::account::RawAccountConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsernameUpdateResult {
    Updated { account_id: String },
    NotFound,
    Ambiguous { account_ids: Vec<String> },
}

#[derive(Debug, FromRow)]
struct AccountRow {
    id: String,
    provider: String,
    username: String,
    password: String,
    dashboard_password_hash: String,
    enabled: bool,
    line_user_id: String,
    discord_user_id: String,
    session_cookie: String,
    is_admin: bool,
}

impl From<AccountRow> for RawAccountConfig {
    fn from(row: AccountRow) -> Self {
        RawAccountConfig {
            id: row.id,
            provider: row.provider,
            username: row.username,
            // Decrypt at-rest credentials so in-memory callers get plaintext.
            password: crate::secret_encryption::StoreSecret::from_stored(&row.password)
                .as_str()
                .to_string(),
            dashboard_password_hash: row.dashboard_password_hash,
            enabled: row.enabled,
            line_user_id: row.line_user_id,
            discord_user_id: row.discord_user_id,
            session_cookie: crate::secret_encryption::StoreSecret::from_stored(&row.session_cookie)
                .as_str()
                .to_string(),
            is_admin: row.is_admin,
        }
    }
}

impl AccountsDb {
    // ── 查詢 ──────────────────────────────────────────────────────────────────

    /// 取得所有帳號（依 id 排序）。
    pub async fn list(&self) -> Result<Vec<RawAccountConfig>> {
        let rows = sqlx::query_as::<_, AccountRow>(
            "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash
             FROM accounts ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("列出帳號失敗")?;

        // MASTER_KEY_ROTATION_V1：讀取時背景重加密舊信封（每帳號一次）。
        // LOG_QUIET_V1：轉換全程靜靜執行，不發任何 info 通知（細節僅 debug 層級）。
        for row in &rows {
            self.rewrap_row_secrets(row).await;
        }

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// 以 id 取得單一帳號。
    pub async fn get(&self, id: &str) -> Result<Option<RawAccountConfig>> {
        let row = sqlx::query_as::<_, AccountRow>(
            "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash
             FROM accounts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("取得帳號 `{id}` 失敗"))?;

        if let Some(row) = &row {
            self.rewrap_row_secrets(row).await;
        }
        Ok(row.map(Into::into))
    }

    /// 以 Tronclass username 查詢帳號；provider 可用來消除同 username 多 provider 的歧義。
    pub async fn find_by_username(
        &self,
        username: &str,
        provider: Option<&str>,
    ) -> Result<Vec<RawAccountConfig>> {
        let rows = if let Some(provider) = provider.filter(|v| !v.trim().is_empty()) {
            sqlx::query_as::<_, AccountRow>(
                "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash
                 FROM accounts WHERE username = ? AND provider = ? ORDER BY id LIMIT ?",
            )
            .bind(username)
            .bind(provider)
            .bind(MAX_USERNAME_MATCH_ROWS)
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("以 username `{username}` 查詢帳號失敗"))?
        } else {
            sqlx::query_as::<_, AccountRow>(
                "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash
                 FROM accounts WHERE username = ? ORDER BY id LIMIT ?",
            )
            .bind(username)
            .bind(MAX_USERNAME_MATCH_ROWS)
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("以 username `{username}` 查詢帳號失敗"))?
        };

        // MASTER_KEY_ROTATION_V1：讀取時背景重加密舊信封（每帳號一次）。
        for row in &rows {
            self.rewrap_row_secrets(row).await;
        }

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// MASTER_KEY_ROTATION_V1：把舊版／舊 key 加密的密文背景重加密為目前 key。
    ///
    /// 在帳號讀取路徑上呼叫——每個帳號只需重寫一次，完成後不再產生寫入；
    /// 所有帳號都完成後，operator 即可自 `TRONCLASS_MASTER_KEY_OLD` 移除舊 key。
    /// 失敗不影響讀取結果（保留原值，下輪讀取再試）。
    /// 回傳是否有重寫（供 list 彙總計數）。
    async fn rewrap_row_secrets(&self, row: &AccountRow) -> bool {
        let password = crate::secret_encryption::rewrap_stored(&row.password);
        let session_cookie = crate::secret_encryption::rewrap_stored(&row.session_cookie);
        if password.is_none() && session_cookie.is_none() {
            return false;
        }
        let new_password = password.unwrap_or_else(|| row.password.clone());
        let new_cookie = session_cookie.unwrap_or_else(|| row.session_cookie.clone());
        match sqlx::query("UPDATE accounts SET password = ?, session_cookie = ? WHERE id = ?")
            .bind(&new_password)
            .bind(&new_cookie)
            .bind(&row.id)
            .execute(&self.pool)
            .await
        {
            // LOG_QUIET_V1：逐帳號細節降 debug；list() 端以單行彙總替代，避免洗版。
            Ok(_) => {
                tracing::debug!(
                    account_id = %row.id,
                    "master key 輪替：已將儲存密文重加密為目前 key"
                );
                true
            }
            Err(error) => {
                tracing::debug!(
                    account_id = %row.id,
                    %error,
                    "master key 重加密寫回失敗（下輪讀取再試）"
                );
                false
            }
        }
    }

    // ── 寫入 ──────────────────────────────────────────────────────────────────

    /// 新增帳號；若 id 已存在則回傳錯誤。
    pub async fn insert(&self, account: &RawAccountConfig) -> Result<()> {
        let password = crate::secret_encryption::StoreSecret::new(&account.password).store_value();
        let session_cookie =
            crate::secret_encryption::StoreSecret::new(&account.session_cookie).store_value();
        sqlx::query(
            "INSERT INTO accounts (id, provider, username, password, dashboard_password_hash, enabled, line_user_id, discord_user_id, session_cookie, is_admin)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&account.id)
        .bind(&account.provider)
        .bind(&account.username)
        .bind(&password)
        .bind(&account.dashboard_password_hash)
        .bind(account.enabled)
        .bind(&account.line_user_id)
        .bind(&account.discord_user_id)
        .bind(&session_cookie)
        .bind(account.is_admin)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("新增帳號 `{}` 失敗", account.id))?;
        Ok(())
    }

    /// 新增或更新帳號（upsert）。
    pub async fn upsert(&self, account: &RawAccountConfig) -> Result<()> {
        let password = crate::secret_encryption::StoreSecret::new(&account.password).store_value();
        let session_cookie =
            crate::secret_encryption::StoreSecret::new(&account.session_cookie).store_value();
        sqlx::query(
            "INSERT INTO accounts (id, provider, username, password, dashboard_password_hash, enabled, line_user_id, discord_user_id, session_cookie, is_admin)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               provider      = excluded.provider,
               username      = excluded.username,
               password      = excluded.password,
               dashboard_password_hash = excluded.dashboard_password_hash,
               enabled       = excluded.enabled,
               line_user_id  = excluded.line_user_id,
               discord_user_id = excluded.discord_user_id,
               session_cookie = excluded.session_cookie,
               is_admin = excluded.is_admin",
        )
        .bind(&account.id)
        .bind(&account.provider)
        .bind(&account.username)
        .bind(&password)
        .bind(&account.dashboard_password_hash)
        .bind(account.enabled)
        .bind(&account.line_user_id)
        .bind(&account.discord_user_id)
        .bind(&session_cookie)
        .bind(account.is_admin)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("Upsert 帳號 `{}` 失敗", account.id))?;
        Ok(())
    }

    /// 刪除帳號，回傳是否找到並刪除。
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let _ = self.delete_dashboard_sessions_by_account(id).await;
        let result = sqlx::query("DELETE FROM accounts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("刪除帳號 `{id}` 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 設定帳號啟用狀態，回傳是否找到並更新。
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET enabled = ? WHERE id = ?")
            .bind(enabled)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` 啟用狀態失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 更新帳號綁定的 Line User ID，回傳是否找到並更新。
    pub async fn set_line_user_id(&self, id: &str, line_user_id: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET line_user_id = ? WHERE id = ?")
            .bind(line_user_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` Line User ID 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 更新帳號綁定的 Discord User ID，回傳是否找到並更新。
    pub async fn set_discord_user_id(&self, id: &str, discord_user_id: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET discord_user_id = ? WHERE id = ?")
            .bind(discord_user_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` Discord User ID 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 更新帳號最近一次成功登入的 session cookie，回傳是否找到並更新。
    pub async fn set_session_cookie(&self, id: &str, session_cookie: &str) -> Result<bool> {
        let stored = crate::secret_encryption::StoreSecret::new(session_cookie).store_value();
        let result = sqlx::query("UPDATE accounts SET session_cookie = ? WHERE id = ?")
            .bind(&stored)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` session cookie 失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 儲存獨立 Dashboard 密碼的慢雜湊驗證字串。
    ///
    /// 這個欄位絕不取代 `password`；後者仍是 Tronclass provider 的登入憑證。
    pub async fn set_dashboard_password_hash(
        &self,
        id: &str,
        dashboard_password_hash: &str,
    ) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET dashboard_password_hash = ? WHERE id = ?")
            .bind(dashboard_password_hash)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` Dashboard 密碼失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 設定 Web 管理頁管理員權限，回傳是否找到並更新。
    pub async fn set_admin(&self, id: &str, is_admin: bool) -> Result<bool> {
        let result = sqlx::query("UPDATE accounts SET is_admin = ? WHERE id = ?")
            .bind(is_admin)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` 管理員權限失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    /// 以 Tronclass username 更新 Discord User ID。
    pub async fn set_discord_user_id_by_username(
        &self,
        username: &str,
        provider: Option<&str>,
        discord_user_id: &str,
    ) -> Result<UsernameUpdateResult> {
        let matches = self.find_by_username(username, provider).await?;
        match matches.as_slice() {
            [] => Ok(UsernameUpdateResult::NotFound),
            [account] => {
                self.set_discord_user_id(&account.id, discord_user_id)
                    .await?;
                Ok(UsernameUpdateResult::Updated {
                    account_id: account.id.clone(),
                })
            }
            accounts => Ok(UsernameUpdateResult::Ambiguous {
                account_ids: accounts.iter().map(|account| account.id.clone()).collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn discord_user_id_is_inserted_and_updated() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        let account = RawAccountConfig {
            id: "acc1".to_string(),
            provider: "fju".to_string(),
            username: "student".to_string(),
            password: "secret".to_string(),
            enabled: true,
            line_user_id: "Uline".to_string(),
            discord_user_id: "100".to_string(),
            session_cookie: String::new(),
            is_admin: false,
            dashboard_password_hash: String::new(),
        };

        db.insert(&account).await.unwrap();
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.discord_user_id, "100");

        assert!(db.set_discord_user_id("acc1", "200").await.unwrap());
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.discord_user_id, "200");

        assert!(db
            .set_session_cookie("acc1", "session=fresh-cookie")
            .await
            .unwrap());
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.session_cookie, "session=fresh-cookie");

        assert!(db
            .set_dashboard_password_hash(
                "acc1",
                "pbkdf2-sha256$600000$00112233445566778899aabbccddeeff$0123"
            )
            .await
            .unwrap());
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert!(loaded.dashboard_password_hash.starts_with("pbkdf2-sha256$"));
    }

    #[tokio::test]
    async fn discord_user_id_can_update_by_tronclass_username() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        db.insert(&RawAccountConfig {
            id: "acc1".to_string(),
            provider: "fju".to_string(),
            username: "student001".to_string(),
            password: "secret".to_string(),
            enabled: true,
            line_user_id: String::new(),
            discord_user_id: String::new(),
            session_cookie: String::new(),
            is_admin: false,
            dashboard_password_hash: String::new(),
        })
        .await
        .unwrap();

        let result = db
            .set_discord_user_id_by_username("student001", None, "123")
            .await
            .unwrap();
        assert_eq!(
            result,
            UsernameUpdateResult::Updated {
                account_id: "acc1".to_string()
            }
        );
        let loaded = db.get("acc1").await.unwrap().unwrap();
        assert_eq!(loaded.discord_user_id, "123");
    }

    #[tokio::test]
    async fn username_binding_reports_ambiguous_matches() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();
        for (id, provider) in [("acc1", "fju"), ("acc2", "tku")] {
            db.insert(&RawAccountConfig {
                id: id.to_string(),
                provider: provider.to_string(),
                username: "student001".to_string(),
                password: "secret".to_string(),
                enabled: true,
                line_user_id: String::new(),
                discord_user_id: String::new(),
                session_cookie: String::new(),
                is_admin: false,
                dashboard_password_hash: String::new(),
            })
            .await
            .unwrap();
        }

        let result = db
            .set_discord_user_id_by_username("student001", None, "123")
            .await
            .unwrap();
        assert_eq!(
            result,
            UsernameUpdateResult::Ambiguous {
                account_ids: vec!["acc1".to_string(), "acc2".to_string()]
            }
        );

        let result = db
            .set_discord_user_id_by_username("student001", Some("fju"), "123")
            .await
            .unwrap();
        assert_eq!(
            result,
            UsernameUpdateResult::Updated {
                account_id: "acc1".to_string()
            }
        );
    }
}
