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
    /// LOGIN_DEVICES_V1：登入來源 IP（登入當下解析的 client IP，不含 port）。
    #[sqlx(default)]
    #[serde(default)]
    pub ip: String,
    /// LOGIN_DEVICES_V1：登入時的 User-Agent（截斷至 256 bytes）。
    #[sqlx(default)]
    #[serde(default)]
    pub user_agent: String,
    /// LOGIN_DEVICES_V1：最近一次使用此 session 的 Unix 秒（每 60 秒最多回寫一次）。
    #[sqlx(default)]
    #[serde(default)]
    pub last_seen_at: i64,
    /// VIEWER_ROLE_V1：檢視者（唯讀）session。
    #[sqlx(default)]
    #[serde(default)]
    pub is_viewer: bool,
}

/// LOGIN_DEVICES_V1 / VIEWER_ROLE_V1：完整 session 寫入參數。
/// 舊的 7 參數 `insert_dashboard_session` 仍保留（測試與相容），內部轉呼叫此版本。
#[derive(Debug, Clone, Default)]
pub struct DashboardSessionInsert {
    pub token: String,
    pub account_id: String,
    pub is_admin: bool,
    pub is_viewer: bool,
    pub must_change_password: bool,
    pub expires_at: i64,
    pub refresh_token: String,
    pub refresh_expires_at: i64,
    pub ip: String,
    pub user_agent: String,
    /// 0 = 使用現在時間；refresh 輪換時傳入原 created_at 以保留「登入時間」。
    pub created_at: i64,
}

const SESSION_COLUMNS: &str = "token, account_id, is_admin, must_change_password, created_at, expires_at,
                    refresh_token, refresh_expires_at, ip, user_agent, last_seen_at, is_viewer";

impl AccountsDb {
    // ── Dashboard Session 持久化 ──────────────────────────────────────────────

    /// 儲存或更新 Web Dashboard Session（含 refresh token）。
    ///
    /// 相容版本：不帶登入來源（ip / user_agent）與 viewer 旗標。
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
        self.insert_dashboard_session_full(&DashboardSessionInsert {
            token: token.to_string(),
            account_id: account_id.to_string(),
            is_admin,
            is_viewer: false,
            must_change_password,
            expires_at,
            refresh_token: refresh_token.to_string(),
            refresh_expires_at,
            ip: String::new(),
            user_agent: String::new(),
            created_at: 0,
        })
        .await
    }

    /// LOGIN_DEVICES_V1 / VIEWER_ROLE_V1：完整版寫入（含登入來源與 viewer 旗標）。
    pub async fn insert_dashboard_session_full(&self, row: &DashboardSessionInsert) -> Result<()> {
        let now = chrono::Local::now().timestamp();
        let created_at = if row.created_at > 0 { row.created_at } else { now };
        sqlx::query(
            "INSERT OR REPLACE INTO dashboard_sessions
                (token, account_id, is_admin, must_change_password, created_at, expires_at,
                 refresh_token, refresh_expires_at, ip, user_agent, last_seen_at, is_viewer)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(row.token.trim())
        .bind(row.account_id.trim())
        .bind(row.is_admin)
        .bind(row.must_change_password)
        .bind(created_at)
        .bind(row.expires_at)
        .bind(row.refresh_token.trim())
        .bind(row.refresh_expires_at)
        .bind(row.ip.trim())
        .bind(row.user_agent.trim())
        .bind(now)
        .bind(row.is_viewer)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 dashboard_sessions 失敗")?;
        Ok(())
    }

    /// LOGIN_DEVICES_V1：更新 session 最近活動時間（呼叫端已做 60 秒節流）。
    pub async fn touch_dashboard_session(&self, token: &str, last_seen_at: i64) -> Result<()> {
        sqlx::query("UPDATE dashboard_sessions SET last_seen_at = ? WHERE token = ?")
            .bind(last_seen_at)
            .bind(token.trim())
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("更新 dashboard_sessions.last_seen_at 失敗")?;
        Ok(())
    }

    /// LOGIN_DEVICES_V1：列出某帳號目前仍有效（access 或 refresh 未過期）的 session，
    /// 供「登入裝置管理」頁顯示。呼叫端負責把 token 轉成不可逆的識別碼再回傳前端。
    pub async fn list_dashboard_sessions_by_account(
        &self,
        account_id: &str,
        now: i64,
    ) -> Result<Vec<DashboardSessionRow>> {
        let sql = format!(
            "SELECT {SESSION_COLUMNS}
             FROM dashboard_sessions
             WHERE account_id = ? AND (expires_at > ? OR refresh_expires_at > ?)
             ORDER BY last_seen_at DESC, created_at DESC"
        );
        let rows = sqlx::query_as::<_, DashboardSessionRow>(&sql)
            .bind(account_id.trim())
            .bind(now)
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("讀取帳號 dashboard_sessions 失敗")?;
        Ok(rows)
    }

    /// 依 Access Token 查詢單一 Dashboard Session
    pub async fn get_dashboard_session(&self, token: &str) -> Result<Option<DashboardSessionRow>> {
        let sql = format!("SELECT {SESSION_COLUMNS} FROM dashboard_sessions WHERE token = ?");
        let row = sqlx::query_as::<_, DashboardSessionRow>(&sql)
            .bind(token.trim())
            .fetch_optional(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("讀取 dashboard_sessions 失敗")?;
        Ok(row)
    }

    /// 依 Refresh Token 查詢 Dashboard Session（用於輪換）。
    pub async fn get_dashboard_session_by_refresh(
        &self,
        refresh_token: &str,
    ) -> Result<Option<DashboardSessionRow>> {
        let sql =
            format!("SELECT {SESSION_COLUMNS} FROM dashboard_sessions WHERE refresh_token = ?");
        let row = sqlx::query_as::<_, DashboardSessionRow>(&sql)
            .bind(refresh_token.trim())
            .fetch_optional(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("依 refresh token 讀取 dashboard_sessions 失敗")?;
        Ok(row)
    }

    /// 列出所有 access token 尚未過期的 Dashboard Session
    pub async fn list_valid_dashboard_sessions(
        &self,
        now: i64,
    ) -> Result<Vec<DashboardSessionRow>> {
        let sql = format!(
            "SELECT {SESSION_COLUMNS}
             FROM dashboard_sessions WHERE expires_at > ?
             ORDER BY created_at ASC"
        );
        let rows = sqlx::query_as::<_, DashboardSessionRow>(&sql)
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("讀取有效 dashboard_sessions 失敗")?;
        Ok(rows)
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

    // ── LOGIN_LOCATION_ALERT_V1：登入 IP 記憶 ────────────────────────────────

    /// 此帳號是否曾從這個 IP 成功登入過。
    pub async fn is_known_login_ip(&self, account_id: &str, ip: &str) -> Result<bool> {
        let row = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(1) FROM login_known_ips WHERE account_id = ? AND ip = ?",
        )
        .bind(account_id.trim())
        .bind(ip.trim())
        .fetch_one(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢 login_known_ips 失敗")?;
        Ok(row.0 > 0)
    }

    /// 此帳號已記錄的登入 IP 數量（0 = 從未成功登入過，第一次不算異常）。
    pub async fn count_known_login_ips(&self, account_id: &str) -> Result<i64> {
        let row = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(1) FROM login_known_ips WHERE account_id = ?",
        )
        .bind(account_id.trim())
        .fetch_one(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("統計 login_known_ips 失敗")?;
        Ok(row.0)
    }

    /// 記錄一次成功登入的 IP（新 IP 建檔；舊 IP 更新 last_seen 與次數）。
    pub async fn remember_login_ip(&self, account_id: &str, ip: &str, now: i64) -> Result<()> {
        let ip = ip.trim();
        if ip.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO login_known_ips (account_id, ip, first_seen_at, last_seen_at, login_count)
             VALUES (?, ?, ?, ?, 1)
             ON CONFLICT(account_id, ip) DO UPDATE SET
               last_seen_at = excluded.last_seen_at,
               login_count = login_known_ips.login_count + 1",
        )
        .bind(account_id.trim())
        .bind(ip)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 login_known_ips 失敗")?;
        Ok(())
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
