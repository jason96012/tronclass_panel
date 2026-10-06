//! ADMIN_GEO_RESTRICT_V1：被「僅限台灣 IP」擋下的訪客按「聯絡管理員」後的申請。
//!
//! 流程：訪客在 `geo_blocked.html` 按「聯絡管理員」→
//! `admin_geo_access_request_handler`（公開端點，不需登入，因為會被擋下的人
//! 本來就無法登入）呼叫 [`create_or_reuse_geo_access_request`] 寫一筆 `pending`
//! →（如果是新申請）通知管理員 Discord＋Web Push → 管理員在 Discord 按
//! 「暫時放行」或在 `/admin/blacklist` 頁面按鈕核准 → [`approve_geo_access_request`]
//! 把狀態改成 `allowed` 並設定 `allowed_until`（時限到前，這個 IP 在
//! `reject_foreign_admin_ip_middleware` 一律放行，不查地區）。
//!
//! 刻意不做「自動放行」：核准動作一定要管理員本人按過 Discord 按鈕或網頁按鈕
//! 才會生效，寫申請紀錄這一步本身完全不會讓任何人多拿到任何存取權限，
//! 所以公開端點是安全的——它只是「排隊通知」，不是「後門」。

use chrono::Local;
use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::FromRow;
use uuid::Uuid;

use super::{truncate_event_text, AccountsDb, EVENT_IDENTITY_MAX_BYTES, EVENT_PATH_MAX_BYTES};

/// 核准「暫時放行」後的有效時間——使用者確認過的預設值：4 小時。
pub const GEO_ACCESS_ALLOW_SECONDS: i64 = 4 * 60 * 60;

/// 同一個 IP 在這個時間窗內重複按「聯絡管理員」，視為同一筆申請（回傳既有的
/// pending row，不重新通知），避免被連點或重整頁面灌爆 Discord／Web Push。
const GEO_ACCESS_DEDUP_SECS: i64 = 15 * 60;

/// 舊紀錄保留上限——超過這個天數的紀錄（不論狀態）在每次新增申請時順便清掉，
/// 避免這張表隨時間無限長大。
const GEO_ACCESS_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

/// BLACKLIST_RETENTION_V1：除了上面的「30 天就清」，再加一道「最多留這麼多
/// 筆」的硬上限（不分狀態）——跟 `/admin/blacklist` 頁另外兩份清單（已封鎖的
/// IP、最近登入失敗）用同一個數字，是使用者明確要求的行為。只影響歷史紀錄
/// 的保留筆數，不影響「暫時放行」本身的有效期限（`allowed_until` 到了就是
/// 到了，跟這張表還留不留著這筆紀錄無關）。
const GEO_ACCESS_RETENTION_ROWS: i64 = 100;

/// GEO_ACCESS_BURST_MUTE_V1：短時間大量申請（洗版）時，暫停逐筆通知用的
/// 全站旗標鍵，沿用 `security_settings` 這張既有的 key/value 表
/// （見 `db::security_settings`），不另開新表。值＝`"1"`（靜音中）或
/// `"0"`／不存在（正常逐筆通知）。是否要觸發靜音、多久算「太多」，這些是
/// 呼叫端（`adapters::admin_web`）的政策判斷，這裡只提供鍵名常數。
pub const GEO_ACCESS_NOTIFY_MUTED_KEY: &str = "geo_access_request_muted";

#[derive(Debug, Clone, FromRow)]
pub struct GeoAccessRequestRow {
    pub id: String,
    pub ip: String,
    pub location: String,
    pub path: String,
    pub status: String,
    pub requested_at: i64,
    pub requested_time: String,
    pub allowed_until: i64,
    pub handled_by: String,
    pub handled_at: i64,
}

impl AccountsDb {
    /// 建立一筆新的國外存取申請；若同一個 IP 在 [`GEO_ACCESS_DEDUP_SECS`] 內
    /// 已經有一筆 `pending` 申請，直接回傳那一筆（第二個回傳值 `false`），呼叫端
    /// 據此判斷「這次不用再通知一次」。`ip` 會先以
    /// `blacklist_group_key()` 正規化，與黑名單、暫時放行的查詢使用同一把尺。
    pub async fn create_or_reuse_geo_access_request(
        &self,
        ip: &str,
        location: &str,
        path: &str,
    ) -> Result<(GeoAccessRequestRow, bool)> {
        let ip = crate::trusted_ip::blacklist_group_key(ip);
        let now = Local::now();
        let since = now.timestamp() - GEO_ACCESS_DEDUP_SECS;
        let existing = sqlx::query_as::<_, GeoAccessRequestRow>(
            "SELECT id, ip, location, path, status, requested_at, requested_time, allowed_until, handled_by, handled_at FROM geo_access_requests
             WHERE ip = ? AND status = 'pending' AND requested_at > ?
             ORDER BY requested_at DESC LIMIT 1",
        )
        .bind(ip.as_str())
        .bind(since)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢既有國外存取申請失敗")?;
        if let Some(row) = existing {
            return Ok((row, false));
        }

        // 舊紀錄清理：best-effort，失敗不影響本次申請建立。
        let retention_cutoff = now.timestamp() - GEO_ACCESS_RETENTION_SECS;
        let _ = sqlx::query("DELETE FROM geo_access_requests WHERE requested_at < ?")
            .bind(retention_cutoff)
            .execute(&self.pool)
            .await;
        // 再加一道筆數上限：就算 30 天內的申請數量異常多，也只留最新的這些。
        let _ = sqlx::query(
            "DELETE FROM geo_access_requests WHERE id NOT IN (
                SELECT id FROM geo_access_requests ORDER BY requested_at DESC LIMIT ?
            )",
        )
        .bind(GEO_ACCESS_RETENTION_ROWS)
        .execute(&self.pool)
        .await;

        let id = Uuid::new_v4().simple().to_string();
        let location = truncate_event_text(location.to_string(), EVENT_IDENTITY_MAX_BYTES);
        let path = truncate_event_text(path.to_string(), EVENT_PATH_MAX_BYTES);
        let requested_time = now.format("%Y-%m-%d %H:%M:%S").to_string();
        sqlx::query(
            "INSERT INTO geo_access_requests
                (id, ip, location, path, status, requested_at, requested_time)
             VALUES (?, ?, ?, ?, 'pending', ?, ?)",
        )
        .bind(&id)
        .bind(ip.as_str())
        .bind(&location)
        .bind(&path)
        .bind(now.timestamp())
        .bind(&requested_time)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入國外存取申請失敗")?;

        Ok((
            GeoAccessRequestRow {
                id,
                ip: ip.to_string(),
                location,
                path,
                status: "pending".to_string(),
                requested_at: now.timestamp(),
                requested_time,
                allowed_until: 0,
                handled_by: String::new(),
                handled_at: 0,
            },
            true,
        ))
    }

    /// 核准一筆申請：只有目前仍是 `pending` 的才會生效（避免同一筆被按兩次、
    /// 或對已經被忽略的申請重複核准）。成功回傳更新後的 row。
    pub async fn approve_geo_access_request(
        &self,
        id: &str,
        allow_seconds: i64,
        handled_by: &str,
    ) -> Result<Option<GeoAccessRequestRow>> {
        let now = Local::now();
        let allowed_until = now.timestamp() + allow_seconds;
        let result = sqlx::query(
            "UPDATE geo_access_requests
             SET status = 'allowed', allowed_until = ?, handled_by = ?, handled_at = ?
             WHERE id = ? AND status = 'pending'",
        )
        .bind(allowed_until)
        .bind(truncate_event_text(handled_by.to_string(), EVENT_IDENTITY_MAX_BYTES))
        .bind(now.timestamp())
        .bind(id)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("核准國外存取申請失敗")?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_geo_access_request(id).await
    }

    /// 忽略一筆申請（維持擋下，只是不再視為「待處理」）。同樣只對 `pending` 生效。
    pub async fn dismiss_geo_access_request(&self, id: &str, handled_by: &str) -> Result<bool> {
        let now = Local::now();
        let result = sqlx::query(
            "UPDATE geo_access_requests
             SET status = 'ignored', handled_by = ?, handled_at = ?
             WHERE id = ? AND status = 'pending'",
        )
        .bind(truncate_event_text(handled_by.to_string(), EVENT_IDENTITY_MAX_BYTES))
        .bind(now.timestamp())
        .bind(id)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("忽略國外存取申請失敗")?;
        Ok(result.rows_affected() > 0)
    }

    async fn get_geo_access_request(&self, id: &str) -> Result<Option<GeoAccessRequestRow>> {
        sqlx::query_as::<_, GeoAccessRequestRow>("SELECT id, ip, location, path, status, requested_at, requested_time, allowed_until, handled_by, handled_at FROM geo_access_requests WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("讀取國外存取申請失敗")
    }

    /// `reject_foreign_admin_ip_middleware` 用：這個 IP 目前是否持有未過期的
    /// 「暫時放行」。`ip` 一樣先正規化成黑名單群組鍵再比對。
    pub async fn geo_access_is_ip_allowed(&self, ip: &str) -> Result<bool> {
        let ip = crate::trusted_ip::blacklist_group_key(ip);
        let now = Local::now().timestamp();
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM geo_access_requests
             WHERE ip = ? AND status = 'allowed' AND allowed_until > ?
             LIMIT 1",
        )
        .bind(ip.as_str())
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢暫時放行狀態失敗")?;
        Ok(row.is_some())
    }

    /// `/admin/blacklist` 頁面備用清單：目前所有待處理的申請（最新在前）。
    pub async fn list_pending_geo_access_requests(&self, limit: i64) -> Result<Vec<GeoAccessRequestRow>> {
        sqlx::query_as::<_, GeoAccessRequestRow>(
            "SELECT id, ip, location, path, status, requested_at, requested_time, allowed_until, handled_by, handled_at FROM geo_access_requests
             WHERE status = 'pending'
             ORDER BY requested_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取待處理國外存取申請失敗")
    }

    /// GEO_ACCESS_BURST_MUTE_V1：過去 `window_secs` 秒內出現過幾個「不重複
    /// 來源 IP」的申請（不分狀態——即使後來被核准或忽略，仍算一次「有人送出
    /// 申請」，這是判斷「是不是短時間被灌爆」要看的量）。多少算「太多」是
    /// 呼叫端的政策判斷，這裡只誠實回報數字。
    pub async fn count_recent_geo_access_ips(&self, window_secs: i64) -> Result<i64> {
        let since = Local::now().timestamp() - window_secs;
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(DISTINCT ip) FROM geo_access_requests WHERE requested_at > ?")
                .bind(since)
                .fetch_one(&self.pool)
                .await
                .into_diagnostic()
                .wrap_err("統計近期國外存取申請數量失敗")?;
        Ok(row.0)
    }

    /// GEO_ACCESS_BURST_MUTE_V1：「清空並恢復通知」按鈕用——把目前所有
    /// `pending` 申請一次標成 `ignored`（維持擋下，不放行任何人，只是清掉
    /// 清單、解除洗版警示），回傳實際清掉的筆數。是否要一併解除靜音旗標，
    /// 由呼叫端決定（這一層不碰 `security_settings`，維持單一職責）。
    pub async fn dismiss_all_pending_geo_access_requests(&self, handled_by: &str) -> Result<i64> {
        let now = Local::now();
        let result = sqlx::query(
            "UPDATE geo_access_requests
             SET status = 'ignored', handled_by = ?, handled_at = ?
             WHERE status = 'pending'",
        )
        .bind(truncate_event_text(handled_by.to_string(), EVENT_IDENTITY_MAX_BYTES))
        .bind(now.timestamp())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("清空待處理國外存取申請失敗")?;
        Ok(result.rows_affected() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_dedups_within_window_and_approve_then_lookup() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        let (first, is_new) = db
            .create_or_reuse_geo_access_request("1.2.3.4", "日本 (JP)", "/admin")
            .await
            .unwrap();
        assert!(is_new);
        assert_eq!(first.status, "pending");

        // 同一個 IP 在時間窗內再按一次：要拿回同一筆，不是新建一筆。
        let (second, is_new_again) = db
            .create_or_reuse_geo_access_request("1.2.3.4", "日本 (JP)", "/admin/blacklist")
            .await
            .unwrap();
        assert!(!is_new_again);
        assert_eq!(second.id, first.id);

        assert!(!db.geo_access_is_ip_allowed("1.2.3.4").await.unwrap());

        let approved = db
            .approve_geo_access_request(&first.id, GEO_ACCESS_ALLOW_SECONDS, "admin-a")
            .await
            .unwrap()
            .expect("應該核准成功");
        assert_eq!(approved.status, "allowed");
        assert!(db.geo_access_is_ip_allowed("1.2.3.4").await.unwrap());

        // 已核准過的申請不能重複核准（第二次呼叫找不到 pending row）。
        assert!(db
            .approve_geo_access_request(&first.id, GEO_ACCESS_ALLOW_SECONDS, "admin-a")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn dismiss_marks_ignored_and_does_not_allow() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        let (row, _) = db
            .create_or_reuse_geo_access_request("5.6.7.8", "未知位置", "")
            .await
            .unwrap();
        assert!(db.dismiss_geo_access_request(&row.id, "admin-a").await.unwrap());
        assert!(!db.geo_access_is_ip_allowed("5.6.7.8").await.unwrap());
        // 忽略過的申請不會再出現在待處理清單。
        let pending = db.list_pending_geo_access_requests(10).await.unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn count_recent_geo_access_ips_counts_distinct_ips_regardless_of_status() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        let (a, _) = db
            .create_or_reuse_geo_access_request("10.10.10.1", "日本 (JP)", "/admin")
            .await
            .unwrap();
        db.create_or_reuse_geo_access_request("10.10.10.2", "日本 (JP)", "/admin")
            .await
            .unwrap();
        // 已經核准（非 pending）的也要算進去——這是「有沒有人灌爆」的統計，
        // 不是「有沒有人還在等」的統計，兩者刻意分開。
        db.approve_geo_access_request(&a.id, GEO_ACCESS_ALLOW_SECONDS, "admin-a")
            .await
            .unwrap();
        // 同一個 IP 在去重時間窗內重複按，不應該被算成第三個不同 IP。
        db.create_or_reuse_geo_access_request("10.10.10.2", "日本 (JP)", "/admin")
            .await
            .unwrap();

        let count = db.count_recent_geo_access_ips(600).await.unwrap();
        assert_eq!(count, 2);

        // 時間窗設為 0 秒：等於「現在之前」，理論上不該再算到剛剛寫入的資料。
        let count_zero_window = db.count_recent_geo_access_ips(-1).await.unwrap();
        assert_eq!(count_zero_window, 0);
    }

    #[tokio::test]
    async fn dismiss_all_pending_only_touches_pending_rows() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db = AccountsDb::open(file.path()).await.unwrap();

        let (allowed, _) = db
            .create_or_reuse_geo_access_request("20.20.20.1", "未知位置", "")
            .await
            .unwrap();
        db.approve_geo_access_request(&allowed.id, GEO_ACCESS_ALLOW_SECONDS, "admin-a")
            .await
            .unwrap();
        db.create_or_reuse_geo_access_request("20.20.20.2", "未知位置", "")
            .await
            .unwrap();
        db.create_or_reuse_geo_access_request("20.20.20.3", "未知位置", "")
            .await
            .unwrap();

        // 只清得到還在 pending 的那兩筆，已核准那筆不受影響（不會被倒退成忽略）。
        let cleared = db.dismiss_all_pending_geo_access_requests("admin-b").await.unwrap();
        assert_eq!(cleared, 2);
        assert!(db
            .list_pending_geo_access_requests(10)
            .await
            .unwrap()
            .is_empty());
        assert!(db.geo_access_is_ip_allowed("20.20.20.1").await.unwrap());

        // 已經空了，再清一次要回傳 0，不是錯誤。
        let cleared_again = db.dismiss_all_pending_geo_access_requests("admin-b").await.unwrap();
        assert_eq!(cleared_again, 0);
    }
}
