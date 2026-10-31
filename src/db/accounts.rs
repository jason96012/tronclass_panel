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
    /// VIEWER_NO_SCHOOL_API_V1
    is_viewer: bool,
}

/// SECRET_ROTATE_CLI_V1：`rotate_all_secrets` 的統計結果。
#[derive(Debug, Default, Clone)]
pub struct SecretRotationReport {
    pub total: usize,
    /// password 已是目前 key 的 v2 信封（且 cookie 也不需動）。
    pub already_current: usize,
    /// password 原為明文、本次加密。
    pub encrypted_plaintext: usize,
    /// password 原為舊 key／v1、本次重加密。
    pub rewrapped: usize,
    /// 實際寫回的帳號數。
    pub updated: usize,
    /// dry-run 時「會被更新」的帳號數。
    pub would_update: usize,
    /// password 解不開的帳號 id。
    pub undecryptable_password: Vec<String>,
    /// session_cookie 解不開的帳號 id（影響較小：重登即可重建）。
    pub undecryptable_cookie: Vec<String>,
    /// 寫回失敗的 `id: error`。
    pub write_failed: Vec<String>,
}

impl From<AccountRow> for RawAccountConfig {
    fn from(row: AccountRow) -> Self {
        RawAccountConfig {
            id: row.id,
            provider: row.provider,
            username: row.username,
            // Decrypt at-rest credentials so in-memory callers get plaintext.
            password: crate::secret_encryption::StoreSecret::from_stored_with_purpose(
                &row.password,
                "accounts.password",
            )
                .as_str()
                .to_string(),
            dashboard_password_hash: row.dashboard_password_hash,
            enabled: row.enabled,
            line_user_id: row.line_user_id,
            discord_user_id: row.discord_user_id,
            session_cookie: crate::secret_encryption::StoreSecret::from_stored_with_purpose(
                &row.session_cookie,
                "accounts.session_cookie",
            )
                .as_str()
                .to_string(),
            is_admin: row.is_admin,
            is_viewer: row.is_viewer,
        }
    }
}

impl AccountsDb {
    // ── 查詢 ──────────────────────────────────────────────────────────────────

    /// 取得所有帳號（依 id 排序）。
    pub async fn list(&self) -> Result<Vec<RawAccountConfig>> {
        let rows = sqlx::query_as::<_, AccountRow>(
            "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash, is_viewer
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
            "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash, is_viewer
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
                "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash, is_viewer
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
                "SELECT id, provider, username, password, enabled, line_user_id, discord_user_id, session_cookie, is_admin, dashboard_password_hash, is_viewer
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
        let password =
            crate::secret_encryption::rewrap_stored_with_purpose(&row.password, "accounts.password");
        let session_cookie = crate::secret_encryption::rewrap_stored_with_purpose(
            &row.session_cookie,
            "accounts.session_cookie",
        );
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

    /// SECRET_ROTATE_CLI_V1：把所有帳號的 password / session_cookie 收斂成
    /// 目前 master key 的 v2 信封（明文→加密、舊 key→重加密、解不開→回報）。
    ///
    /// 直接操作原始儲存值，不經過 `RawAccountConfig` 的解密路徑，所以
    /// 解不開的值不會被誤寫成空字串。`dry_run` 為 true 時只統計不寫入。
    pub async fn rotate_all_secrets(&self, dry_run: bool) -> Result<SecretRotationReport> {
        use crate::secret_encryption::{rotate_stored_with_purpose, RotateOutcome};

        let rows = sqlx::query_as::<_, (String, String, String)>(
            "SELECT id, password, session_cookie FROM accounts ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取帳號原始密文失敗")?;

        let mut report = SecretRotationReport::default();
        for (id, password, session_cookie) in rows {
            report.total += 1;
            let pw = rotate_stored_with_purpose(&password, "accounts.password");
            let ck = rotate_stored_with_purpose(&session_cookie, "accounts.session_cookie");

            let mut failed = false;
            let mut changed = false;
            let mut new_password = password.clone();
            let mut new_cookie = session_cookie.clone();
            for (outcome, slot, is_password) in [(pw, &mut new_password, true), (ck, &mut new_cookie, false)] {
                match outcome {
                    RotateOutcome::Unchanged => {}
                    RotateOutcome::Encrypted(v) => {
                        *slot = v;
                        changed = true;
                        if is_password {
                            report.encrypted_plaintext += 1;
                        }
                    }
                    RotateOutcome::Rewrapped(v) => {
                        *slot = v;
                        changed = true;
                        if is_password {
                            report.rewrapped += 1;
                        }
                    }
                    RotateOutcome::Undecryptable => {
                        failed = true;
                        if is_password {
                            report.undecryptable_password.push(id.clone());
                        } else {
                            report.undecryptable_cookie.push(id.clone());
                        }
                    }
                }
            }

            if !changed {
                if !failed {
                    report.already_current += 1;
                }
                continue;
            }
            if dry_run {
                report.would_update += 1;
                continue;
            }
            match sqlx::query("UPDATE accounts SET password = ?, session_cookie = ? WHERE id = ?")
                .bind(&new_password)
                .bind(&new_cookie)
                .bind(&id)
                .execute(&self.pool)
                .await
            {
                Ok(_) => report.updated += 1,
                Err(error) => report.write_failed.push(format!("{id}: {error}")),
            }
        }
        Ok(report)
    }

    // ── 寫入 ──────────────────────────────────────────────────────────────────

    /// 新增帳號；若 id 已存在則回傳錯誤。
    pub async fn insert(&self, account: &RawAccountConfig) -> Result<()> {
        let password = crate::secret_encryption::StoreSecret::new(&account.password)
            .store_value_with_purpose("accounts.password");
        let session_cookie = crate::secret_encryption::StoreSecret::new(&account.session_cookie)
            .store_value_with_purpose("accounts.session_cookie");
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
        let password = crate::secret_encryption::StoreSecret::new(&account.password)
            .store_value_with_purpose("accounts.password");
        let session_cookie = crate::secret_encryption::StoreSecret::new(&account.session_cookie)
            .store_value_with_purpose("accounts.session_cookie");
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

    /// SET_PASSWORD_VERIFY_V1：只更新儲存的學校密碼（加密寫入），不動其他欄位。回傳是否找到帳號。
    pub async fn set_password(&self, id: &str, password: &str) -> Result<bool> {
        let stored = crate::secret_encryption::StoreSecret::new(password)
            .store_value_with_purpose("accounts.password");
        let result = sqlx::query("UPDATE accounts SET password = ? WHERE id = ?")
            .bind(&stored)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` 密碼失敗"))?;
        Ok(result.rows_affected() > 0)
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
        let stored = crate::secret_encryption::StoreSecret::new(session_cookie)
            .store_value_with_purpose("accounts.session_cookie");
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
    ///
    /// VIEWER_ROLE_V1：管理員與檢視者互斥——升為管理員時同時清掉 is_viewer，
    /// 避免一個帳號同時帶著「全站可寫」與「唯讀」兩個旗標造成判斷歧義。
    pub async fn set_admin(&self, id: &str, is_admin: bool) -> Result<bool> {
        let sql = if is_admin {
            "UPDATE accounts SET is_admin = 1, is_viewer = 0 WHERE id = ?"
        } else {
            "UPDATE accounts SET is_admin = 0 WHERE id = ?"
        };
        let result = sqlx::query(sql)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` 管理員權限失敗"))?;

        Ok(result.rows_affected() > 0)
    }

    // ── VIEWER_ROLE_V1：檢視者（唯讀）角色 ───────────────────────────────────
    //
    // 為了不動到 `RawAccountConfig`（全專案 12 處 struct literal）與所有讀取
    // 帳號的 SELECT，viewer 旗標以獨立查詢提供：登入時查一次寫進 session，
    // 狀態頁以 `list_viewer_flags` 一次撈全部。

    /// 帳號是否為檢視者（唯讀）。找不到帳號時回傳 false。
    pub async fn is_viewer(&self, id: &str) -> Result<bool> {
        let row = sqlx::query_as::<_, (bool,)>("SELECT is_viewer FROM accounts WHERE id = ?")
            .bind(id.trim())
            .fetch_optional(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("讀取帳號 `{id}` 檢視者旗標失敗"))?;
        Ok(row.map(|(v,)| v).unwrap_or(false))
    }

    /// 設定檢視者（唯讀）角色，回傳是否找到並更新。設為檢視者時同步取消管理員。
    pub async fn set_viewer(&self, id: &str, is_viewer: bool) -> Result<bool> {
        let sql = if is_viewer {
            "UPDATE accounts SET is_viewer = 1, is_admin = 0 WHERE id = ?"
        } else {
            "UPDATE accounts SET is_viewer = 0 WHERE id = ?"
        };
        let result = sqlx::query(sql)
            .bind(id)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新帳號 `{id}` 檢視者旗標失敗"))?;
        Ok(result.rows_affected() > 0)
    }

    /// 所有帳號的檢視者旗標（account_id → is_viewer）。
    pub async fn list_viewer_flags(&self) -> Result<std::collections::HashMap<String, bool>> {
        let rows = sqlx::query_as::<_, (String, bool)>("SELECT id, is_viewer FROM accounts")
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("讀取帳號檢視者旗標失敗")?;
        Ok(rows.into_iter().collect())
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


