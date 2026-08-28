//! Dashboard 登入 session（access + refresh token）。

use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use sqlx::FromRow;

use super::AccountsDb;

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct DashboardSessionRow {
    pub token: String,
    pub account_id: String,
    pub is_admin: bool,
    pub must_change_password: bool,
    pub created_at: i64,
    pub expires_at: i64,
    /// Rotatable long-lived refresh token. Short-lived access tokens are
    /// refreshed from this; rotation invalidates the previous refresh token.
    pub refresh_token: String,
    pub refresh_expires_at: i64,
}

impl AccountsDb {
    // ── Dashboard Session 持久化 ──────────────────────────────────────────────

    /// 儲存或更新 Web Dashboard Session（含 refresh token）。
    pub async fn insert_dashboard_session(
        &self,
        token: &str,
        account_id: &str,
        is_admin: bool,
        must_change_password: bool,
        expires_at: i64,
        refresh_token: &str,
        refresh_expires_at: i64,
    ) -> Result<()> {
        let now = chrono::Local::now().timestamp();
        sqlx::query(
            "INSERT OR REPLACE INTO dashboard_sessions
                (token, account_id, is_admin, must_change_password, created_at, expires_at,
                 refresh_token, refresh_expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(token.trim())
        .bind(account_id.trim())
        .bind(is_admin)
        .bind(must_change_password)
        .bind(now)
        .bind(expires_at)
        .bind(refresh_token.trim())
        .bind(refresh_expires_at)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 dashboard_sessions 失敗")?;
        Ok(())
    }

    /// 依 Access Token 查詢單一 Dashboard Session
    pub async fn get_dashboard_session(&self, token: &str) -> Result<Option<DashboardSessionRow>> {
        sqlx::query_as::<_, DashboardSessionRow>(
            "SELECT token, account_id, is_admin, must_change_password, created_at, expires_at,
                    refresh_token, refresh_expires_at
             FROM dashboard_sessions WHERE token = ?",
        )
        .bind(token.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取 dashboard_sessions 失敗")
    }

    /// 依 Refresh Token 查詢 Dashboard Session（用於輪換）。
    pub async fn get_dashboard_session_by_refresh(
        &self,
        refresh_token: &str,
    ) -> Result<Option<DashboardSessionRow>> {
        sqlx::query_as::<_, DashboardSessionRow>(
            "SELECT token, account_id, is_admin, must_change_password, created_at, expires_at,
                    refresh_token, refresh_expires_at
             FROM dashboard_sessions WHERE refresh_token = ?",
        )
        .bind(refresh_token.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("依 refresh token 讀取 dashboard_sessions 失敗")
    }

    /// 列出所有 access token 尚未過期的 Dashboard Session
    pub async fn list_valid_dashboard_sessions(
        &self,
        now: i64,
    ) -> Result<Vec<DashboardSessionRow>> {
        sqlx::query_as::<_, DashboardSessionRow>(
            "SELECT token, account_id, is_admin, must_change_password, created_at, expires_at,
                    refresh_token, refresh_expires_at
             FROM dashboard_sessions WHERE expires_at > ?
             ORDER BY created_at ASC",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取有效 dashboard_sessions 失敗")
    }

    /// 刪除指定 Token 的 Dashboard Session
    pub async fn delete_dashboard_session(&self, token: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM dashboard_sessions WHERE token = ?")
            .bind(token.trim())
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("刪除 dashboard_session 失敗")?;
        Ok(result.rows_affected() > 0)
    }

    /// 刪除指定帳號的所有 Dashboard Session
    pub async fn delete_dashboard_sessions_by_account(&self, account_id: &str) -> Result<u64> {
        let result = sqlx::query("DELETE FROM dashboard_sessions WHERE account_id = ?")
            .bind(account_id.trim())
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("依帳號刪除 dashboard_sessions 失敗")?;
        Ok(result.rows_affected())
    }

    /// 清理所有已過期的 Dashboard Session（access 與 refresh 皆過期才刪）。
    pub async fn delete_expired_dashboard_sessions(&self, now: i64) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM dashboard_sessions WHERE expires_at <= ? AND refresh_expires_at <= ?",
        )
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("清理過期 dashboard_sessions 失敗")?;
        Ok(result.rows_affected())
    }

    /// 依 Refresh Token 刪除該 Session（refresh 輪換時撤銷舊 token）。
    pub async fn delete_dashboard_session_by_refresh(&self, refresh_token: &str) -> Result<u64> {
        let result = sqlx::query("DELETE FROM dashboard_sessions WHERE refresh_token = ?")
            .bind(refresh_token.trim())
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("依 refresh token 刪除 dashboard_sessions 失敗")?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn dashboard_sessions_crud_and_expiry_operations() {
        let file = NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        let now = chrono::Local::now().timestamp();
        let future_expiry = now + 3600;
        let past_expiry = now - 100;

        db.insert_dashboard_session(
            "token1", "acc1", true, false, future_expiry, "refresh1", future_expiry,
        )
        .await
        .unwrap();
        db.insert_dashboard_session(
            "token2", "acc1", false, true, future_expiry, "refresh2", future_expiry,
        )
        .await
        .unwrap();
        db.insert_dashboard_session(
            "token3", "acc2", false, false, past_expiry, "refresh3", past_expiry,
        )
        .await
        .unwrap();

        // Get single session
        let s1 = db.get_dashboard_session("token1").await.unwrap().unwrap();
        assert_eq!(s1.token, "token1");
        assert_eq!(s1.account_id, "acc1");
        assert!(s1.is_admin);
        assert!(!s1.must_change_password);
        assert_eq!(s1.expires_at, future_expiry);

        let s2 = db.get_dashboard_session("token2").await.unwrap().unwrap();
        assert!(!s2.is_admin);
        assert!(s2.must_change_password);

        // Refresh token lookup + rotation revocation
        let by_refresh = db
            .get_dashboard_session_by_refresh("refresh1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_refresh.token, "token1");
        let rotated = db.delete_dashboard_session_by_refresh("refresh1").await.unwrap();
        assert_eq!(rotated, 1);
        assert!(db
            .get_dashboard_session_by_refresh("refresh1")
            .await
            .unwrap()
            .is_none());
        // Re-insert so the CRUD assertions below still hold.
        db.insert_dashboard_session(
            "token1", "acc1", true, false, future_expiry, "refresh1", future_expiry,
        )
        .await
        .unwrap();

        // List valid sessions (should exclude expired token3)
        let valid = db.list_valid_dashboard_sessions(now).await.unwrap();
        assert_eq!(valid.len(), 2);
        assert!(valid.iter().any(|s| s.token == "token1"));
        assert!(valid.iter().any(|s| s.token == "token2"));

        // Cleanup expired sessions
        let pruned = db.delete_expired_dashboard_sessions(now).await.unwrap();
        assert_eq!(pruned, 1);
        assert!(db.get_dashboard_session("token3").await.unwrap().is_none());

        // Delete single session
        assert!(db.delete_dashboard_session("token1").await.unwrap());
        assert!(db.get_dashboard_session("token1").await.unwrap().is_none());

        // Delete sessions by account
        db.insert_dashboard_session(
            "token4", "acc1", false, false, future_expiry, "refresh4", future_expiry,
        )
        .await
        .unwrap();
        let deleted_acc = db.delete_dashboard_sessions_by_account("acc1").await.unwrap();
        assert_eq!(deleted_acc, 2); // token2 and token4
        assert!(db.get_dashboard_session("token2").await.unwrap().is_none());
        assert!(db.get_dashboard_session("token4").await.unwrap().is_none());
    }
}
