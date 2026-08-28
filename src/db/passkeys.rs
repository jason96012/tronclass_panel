//! WebAuthn passkey 憑證。

use miette::{IntoDiagnostic, Result, WrapErr};
use passkey_auth::PasskeyCredential;

use super::{
    AccountsDb, EVENT_IDENTITY_MAX_BYTES, MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT,
    MAX_PASSKEY_CREDENTIAL_JSON_BYTES,
};

impl AccountsDb {
    pub async fn list_passkey_credentials(
        &self,
        account_id: &str,
    ) -> Result<Vec<PasskeyCredential>> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT credential_json FROM passkey_credentials
             WHERE account_id = ? AND length(CAST(credential_json AS BLOB)) <= ?
             ORDER BY id ASC LIMIT ?",
        )
        .bind(account_id)
        .bind(MAX_PASSKEY_CREDENTIAL_JSON_BYTES as i64)
        .bind((MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT + 1) as i64)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` passkey 憑證失敗"))?;

        let mut credentials = Vec::with_capacity(rows.len());
        for (json,) in rows {
            let credential = serde_json::from_str::<PasskeyCredential>(&json)
                .into_diagnostic()
                .wrap_err("解析 passkey credential JSON 失敗")?;
            credentials.push(credential);
        }
        Ok(credentials)
    }

    pub async fn upsert_passkey_credential(
        &self,
        account_id: &str,
        credential: &PasskeyCredential,
    ) -> Result<()> {
        if account_id.len() > EVENT_IDENTITY_MAX_BYTES {
            return Err(miette::miette!("passkey account_id is too long"));
        }
        let credential_id = credential.id.to_b64url();
        if credential_id.len() > 4096 {
            return Err(miette::miette!("passkey credential id is too long"));
        }
        let credential_json = serde_json::to_string(credential)
            .into_diagnostic()
            .wrap_err("序列化 passkey credential 失敗")?;
        if credential_json.len() > MAX_PASSKEY_CREDENTIAL_JSON_BYTES {
            return Err(miette::miette!("passkey credential payload is too large"));
        }
        let now = chrono::Local::now().timestamp();
        let result = sqlx::query(
            "INSERT INTO passkey_credentials (account_id, credential_id, credential_json, created_at, updated_at, last_used_at)
             SELECT ?, ?, ?, ?, ?, ?
             WHERE EXISTS (
                 SELECT 1 FROM passkey_credentials WHERE account_id = ? AND credential_id = ?
             ) OR (
                 SELECT COUNT(*) FROM passkey_credentials WHERE account_id = ?
             ) < ?
             ON CONFLICT(account_id, credential_id) DO UPDATE SET
               credential_json = excluded.credential_json,
               updated_at = excluded.updated_at,
               last_used_at = excluded.last_used_at",
        )
        .bind(account_id)
        .bind(&credential_id)
        .bind(credential_json)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(account_id)
        .bind(&credential_id)
        .bind(account_id)
        .bind(MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT as i64)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("寫入 `{account_id}` passkey credential 失敗"))?;
        if result.rows_affected() == 0 {
            return Err(miette::miette!(
                "passkey credential limit reached; max {}",
                MAX_PASSKEY_CREDENTIALS_PER_ACCOUNT
            ));
        }
        Ok(())
    }

    pub async fn count_passkey_credentials(&self, account_id: &str) -> Result<i64> {
        let row = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM passkey_credentials WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("計算 `{account_id}` passkey credentials 失敗"))?;
        Ok(row.0)
    }
}
