//! IP 黑名單。

use chrono::Local;
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use sqlx::FromRow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    truncate_event_text, AccountsDb, EVENT_DETAILS_MAX_BYTES, EVENT_IDENTITY_MAX_BYTES,
};

/// BLACKLIST_RETENTION_V1（2026-10-06 新增，當天隨即撤回）：原本這裡有一條
/// 「黑名單只保留最新 100 筆，滿了就把最舊那筆連帶解除封鎖」的規則。使用者
/// 試用後認為「已封鎖的 IP 不應該被數量限制」（封鎖的 IP 再多也該一直生效，
/// 不該因為滿 100 筆就被自動解除），因此這條筆數上限已經移除——
/// `block_ip_with_failure_count` 現在只負責新增，不再順手清舊資料。
/// 畫面上「只顯示前 5 筆、按『顯示更多』展開」的 UI 行為不受影響，繼續保留
/// （那是純顯示層的分頁，跟這裡講的資料庫筆數上限是兩回事）。

/// WAF 異常 4xx 自動封鎖的到期秒數（30 分鐘）。只對這一類自動封鎖生效；
/// 手動封鎖、登入失敗封鎖、攻擊特徵封鎖都維持永久，直到管理員解除。
/// 先前 `waf.rs` 定義了這個常數卻從未使用，導致封鎖實際上永久生效。
pub const WAF_ANOMALY_BAN_SECS: i64 = 1800;
/// WAF 異常 4xx 自動封鎖寫入的 `reason` 前綴（`waf.rs` 寫入時也引用這個常數）。
pub const WAF_ANOMALY_REASON_PREFIX: &str = "WAF 防火牆自動封鎖（異常 4xx 行為）";
/// WAF 自動封鎖共用的 `blocked_by`（攻擊特徵、指紋、異常 4xx 都用這個）。
pub const WAF_BLOCKED_BY: &str = "system:waf-firewall-max";

/// 這一列是否屬於「會到期的 WAF 異常 4xx 自動封鎖」。
fn is_expiring_waf_anomaly_ban(blocked_by: &str, reason: &str) -> bool {
    blocked_by == WAF_BLOCKED_BY && reason.starts_with(WAF_ANOMALY_REASON_PREFIX)
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct IpBlacklistRow {
    pub ip: String,
    pub blocked_at: i64,
    pub blocked_time: String,
    pub blocked_by: String,
    pub reason: String,
    /// Number of failed Dashboard logins that caused an automatic block.
    /// Manual and legacy blocks use zero.
    pub failure_count: i64,
}


// IP_BLACKLIST_CACHE_V1（R-34）：短 TTL 快取。所有寫入都在本模組，寫入後清空並遞增 generation；
// 查詢在讀 DB 前記下 generation，回寫時若 generation 已變就捨棄，避免舊結果蓋過剛完成的封鎖或解除。
// 快取只是加速：鎖中毒時仍清空，寧可多查 DB 也不留舊值。
pub(crate) const IP_BLACKLIST_CACHE_TTL: Duration = Duration::from_secs(5);
const IP_BLACKLIST_CACHE_CAP: usize = 10_000;

#[derive(Default)]
struct IpBlacklistCacheState {
    generation: u64,
    entries: HashMap<String, (bool, Instant)>,
}

#[derive(Clone, Default)]
pub(crate) struct IpBlacklistCache {
    inner: Arc<Mutex<IpBlacklistCacheState>>,
}

impl IpBlacklistCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, IpBlacklistCacheState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn generation(&self) -> u64 {
        self.lock().generation
    }

    pub(crate) fn get(&self, key: &str, now: Instant) -> Option<bool> {
        let st = self.lock();
        match st.entries.get(key) {
            Some((value, expires)) if *expires > now => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn put(&self, key: &str, value: bool, generation: u64, now: Instant) {
        let mut st = self.lock();
        if st.generation != generation {
            return;
        }
        if st.entries.len() >= IP_BLACKLIST_CACHE_CAP && !st.entries.contains_key(key) {
            st.entries.retain(|_, (_, expires)| *expires > now);
            if st.entries.len() >= IP_BLACKLIST_CACHE_CAP {
                st.entries.clear();
            }
        }
        st.entries.insert(key.to_string(), (value, now + IP_BLACKLIST_CACHE_TTL));
    }

    pub(crate) fn clear(&self) {
        let mut st = self.lock();
        st.generation = st.generation.wrapping_add(1);
        st.entries.clear();
    }
}

impl AccountsDb {
    pub async fn is_ip_blacklisted(&self, ip: &str) -> Result<bool> {
        let trimmed = ip.trim();
        if let Some(cached) = self.blacklist_cache.get(trimmed, Instant::now()) {
            return Ok(cached);
        }
        let generation = self.blacklist_cache.generation();
        let blocked = self.query_ip_blacklisted(trimmed).await?;
        self.blacklist_cache.put(trimmed, blocked, generation, Instant::now());
        Ok(blocked)
    }

    async fn query_ip_blacklisted(&self, trimmed: &str) -> Result<bool> {
        // IP_BLACKLIST_V6_GROUP_V1：同時比對精確位址與 IPv6 /64 群組鍵——
        // 封鎖以「整段前綴」生效，位址輪換（privacy addresses）不能洗白身分。
        // IPv4 的群組鍵＝自己，行為與舊版完全一致。
        let group = crate::trusted_ip::blacklist_group_key(trimmed);
        let rows = sqlx::query_as::<_, (String, i64, String, String)>(
            "SELECT ip, blocked_at, blocked_by, reason FROM ip_blacklist WHERE ip = ? OR ip = ?",
        )
        .bind(trimmed)
        .bind(group)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("檢查 IP 黑名單失敗")?;

        if rows.is_empty() {
            return Ok(false);
        }
        // WAF 異常 4xx 自動封鎖超過到期時間 → 視為已解除，並刪除該列。
        // 必須刪除：ip 是 PRIMARY KEY，過期列若留著，之後的 INSERT OR IGNORE
        // 會擋住重新封鎖。
        let now = Local::now().timestamp();
        let mut active = false;
        let mut expired: Vec<String> = Vec::new();
        for (key, blocked_at, blocked_by, reason) in rows {
            if is_expiring_waf_anomaly_ban(&blocked_by, &reason)
                && now.saturating_sub(blocked_at) >= WAF_ANOMALY_BAN_SECS
            {
                expired.push(key);
            } else {
                active = true;
            }
        }
        for key in expired {
            sqlx::query(
                "DELETE FROM ip_blacklist
                 WHERE ip = ? AND blocked_by = ? AND reason LIKE ? || '%'",
            )
            .bind(&key)
            .bind(WAF_BLOCKED_BY)
            .bind(WAF_ANOMALY_REASON_PREFIX)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("清除已到期的 WAF 自動封鎖失敗")?;
            self.blacklist_cache.clear();
        }
        Ok(active)
    }

    /// 黑名單列表。R-30：已到期的 WAF 異常 4xx 自動封鎖不列出（與 `is_ip_blacklisted`
    /// 的到期規則相同：`now - blocked_at >= WAF_ANOMALY_BAN_SECS`）。這些列會在下次該 IP
    /// 被檢查時刪除；列表只是不再顯示「已經失效」的列。
    pub async fn list_ip_blacklist(&self, limit: i64) -> Result<Vec<IpBlacklistRow>> {
        let limit = limit.clamp(1, 4096);
        let now = Local::now().timestamp();
        let expired_before = now.saturating_sub(WAF_ANOMALY_BAN_SECS);
        sqlx::query_as::<_, IpBlacklistRow>(
            "SELECT ip, blocked_at, blocked_time, blocked_by, reason, failure_count
             FROM ip_blacklist
             WHERE NOT (blocked_by = ? AND reason LIKE ? || '%' AND blocked_at <= ?)
             ORDER BY blocked_at DESC, ip ASC LIMIT ?",
        )
        .bind(WAF_BLOCKED_BY)
        .bind(WAF_ANOMALY_REASON_PREFIX)
        .bind(expired_before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取 IP 黑名單失敗")
    }

    pub async fn block_ip(&self, ip: &str, blocked_by: &str, reason: &str) -> Result<bool> {
        self.block_ip_with_failure_count(ip, blocked_by, reason, 0).await
    }

    pub async fn block_ip_with_failure_count(
        &self,
        ip: &str,
        blocked_by: &str,
        reason: &str,
        failure_count: u32,
    ) -> Result<bool> {
        // IP_BLACKLIST_V6_GROUP_V1：IPv6 一律以 /64 群組鍵入庫（手動與自動同規則），
        // 否則被封者切換同前綴位址即可繞過；IPv4 與既有列行為不變。
        // `blacklist_group_key` 冪等：已是群組鍵的字串會原樣通過。
        let ip = crate::trusted_ip::blacklist_group_key(ip);
        let now = Local::now();
        let result = sqlx::query(
            "INSERT OR IGNORE INTO ip_blacklist
                (ip, blocked_at, blocked_time, blocked_by, reason, failure_count)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(ip.as_str())
        .bind(now.timestamp())
        .bind(now.format("%Y-%m-%d %H:%M:%S").to_string())
        .bind(truncate_event_text(blocked_by.to_string(), EVENT_IDENTITY_MAX_BYTES))
        .bind(truncate_event_text(reason.to_string(), EVENT_DETAILS_MAX_BYTES))
        .bind(i64::from(failure_count))
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 IP 黑名單失敗")?;
        self.blacklist_cache.clear();
        Ok(result.rows_affected() > 0)
    }

    /// Mark a newly-created automatic block as belonging to the Dashboard
    /// account that triggered it. This is used to authorize a self-service
    /// Discord unblock without granting general blacklist access.
    pub async fn set_auto_block_owner(&self, ip: &str, account_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE ip_blacklist SET blocked_account_id = ?
             WHERE ip = ? AND blocked_by = 'system:auto-login-failure'",
        )
        .bind(account_id.trim())
        .bind(ip.trim())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入自動封鎖帳號識別失敗")?;
        self.blacklist_cache.clear();
        Ok(())
    }

    /// Returns the owning account for an automatic IP block, if one was
    /// recorded. This allows legacy Discord buttons to enforce the same
    /// self-service ownership rule without trusting their component payload.
    pub async fn auto_block_owner_for_ip(&self, ip: &str) -> Result<Option<String>> {
        sqlx::query_scalar(
            "SELECT blocked_account_id FROM ip_blacklist
             WHERE ip = ? AND blocked_by = 'system:auto-login-failure'
               AND blocked_account_id <> ''",
        )
        .bind(ip.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取自動 IP 封鎖帳號識別失敗")
    }

    /// Removes an automatic block only when the Discord user is bound to the
    /// account that triggered it. The activity-event fallback safely supports
    /// blocks created before blocked_account_id was introduced.
    pub async fn unblock_automatic_ip_for_discord_user(
        &self,
        ip: &str,
        discord_user_id: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM ip_blacklist
             WHERE ip = ? AND blocked_by = 'system:auto-login-failure'
               AND (
                 EXISTS (
                   SELECT 1 FROM accounts a
                   WHERE a.discord_user_id = ?
                     AND (a.id = ip_blacklist.blocked_account_id
                          OR a.username = ip_blacklist.blocked_account_id)
                 )
                 OR EXISTS (
                   SELECT 1 FROM activity_events e
                   JOIN accounts a ON (a.id = e.account_id OR a.username = e.account_id)
                   WHERE e.action = 'ip_blacklist_auto_block'
                     AND e.path = ip_blacklist.ip
                     AND a.discord_user_id = ?
                 )
               )",
        )
        .bind(ip.trim())
        .bind(discord_user_id.trim())
        .bind(discord_user_id.trim())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("依 Discord 帳號解除自己的自動 IP 封鎖失敗")?;
        self.blacklist_cache.clear();
        Ok(result.rows_affected() > 0)
    }

    /// Removes only an automatic block owned by the supplied account.
    pub async fn unblock_own_automatic_ip(&self, ip: &str, account_id: &str) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM ip_blacklist
             WHERE ip = ? AND blocked_by = 'system:auto-login-failure'
               AND blocked_account_id = ?",
        )
        .bind(ip.trim())
        .bind(account_id.trim())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("解除自己的自動 IP 封鎖失敗")?;
        self.blacklist_cache.clear();
        Ok(result.rows_affected() > 0)
    }

    pub async fn unblock_ip(&self, ip: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM ip_blacklist WHERE ip = ?")
            .bind(ip.trim())
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("解除 IP 黑名單失敗")?;
        self.blacklist_cache.clear();
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod list_expiry_tests {
    use super::*;
    use chrono::Local;

    async fn open_db() -> (tempfile::TempDir, AccountsDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = AccountsDb::open(dir.path().join("accounts.db"))
            .await
            .expect("open db");
        (dir, db)
    }

    /// R-30：到期的異常自動封鎖不出現在列表；未到期的自動封鎖與手動封鎖都照常顯示。
    #[tokio::test]
    async fn list_hides_expired_anomaly_bans_only() {
        let (_dir, db) = open_db().await;
        let anomaly_reason = format!("{WAF_ANOMALY_REASON_PREFIX}（測試）");
        db.block_ip_with_failure_count("198.51.100.1", WAF_BLOCKED_BY, &anomaly_reason, 0)
            .await
            .unwrap();
        db.block_ip_with_failure_count("198.51.100.2", WAF_BLOCKED_BY, &anomaly_reason, 0)
            .await
            .unwrap();
        db.block_ip("198.51.100.3", "admin", "手動封鎖").await.unwrap();
        // 198.51.100.1 的封鎖時間往前推到已到期；其餘維持「現在」。
        let expired_at = Local::now().timestamp() - WAF_ANOMALY_BAN_SECS - 60;
        sqlx::query("UPDATE ip_blacklist SET blocked_at = ? WHERE ip = ?")
            .bind(expired_at)
            .bind("198.51.100.1")
            .execute(&db.pool)
            .await
            .unwrap();
        // 手動封鎖即使時間很舊，也不會因為「到期」被隱藏。
        sqlx::query("UPDATE ip_blacklist SET blocked_at = ? WHERE ip = ?")
            .bind(expired_at)
            .bind("198.51.100.3")
            .execute(&db.pool)
            .await
            .unwrap();

        let ips: Vec<String> = db
            .list_ip_blacklist(100)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.ip)
            .collect();
        assert!(!ips.contains(&"198.51.100.1".to_string()), "已到期的異常封鎖應隱藏");
        assert!(ips.contains(&"198.51.100.2".to_string()), "未到期的異常封鎖應顯示");
        assert!(ips.contains(&"198.51.100.3".to_string()), "手動封鎖不因時間隱藏");
    }
}

#[cfg(test)]
mod ip_cache_tests {
    //! IP_BLACKLIST_CACHE_V1 測試（R-34）。尚未編譯與執行。
    use super::*;

    #[test]
    fn cache_returns_value_until_ttl_then_misses() {
        let c = IpBlacklistCache::default();
        let t0 = Instant::now();
        c.put("198.51.100.9", true, c.generation(), t0);
        assert_eq!(c.get("198.51.100.9", t0), Some(true));
        assert_eq!(c.get("198.51.100.9", t0 + IP_BLACKLIST_CACHE_TTL - Duration::from_millis(1)), Some(true));
        assert_eq!(c.get("198.51.100.9", t0 + IP_BLACKLIST_CACHE_TTL + Duration::from_millis(1)), None);
    }

    #[test]
    fn clear_bumps_generation_and_discards_stale_puts() {
        let c = IpBlacklistCache::default();
        let t0 = Instant::now();
        let before = c.generation();
        c.clear();
        // 查詢期間發生過寫入：以舊 generation 回寫的結果必須被捨棄。
        c.put("198.51.100.9", false, before, t0);
        assert_eq!(c.get("198.51.100.9", t0), None);
    }

    #[test]
    fn cache_stays_within_cap() {
        let c = IpBlacklistCache::default();
        let t0 = Instant::now();
        let g = c.generation();
        for i in 0..(IP_BLACKLIST_CACHE_CAP + 50) {
            c.put(&format!("10.{}.{}.{}", i / 65536 % 256, i / 256 % 256, i % 256), false, g, t0);
        }
        assert!(c.lock().entries.len() <= IP_BLACKLIST_CACHE_CAP);
    }

    /// 解除封鎖後下一次查詢立即生效（寫入清空快取），不必等 TTL。
    #[tokio::test]
    async fn unblock_is_visible_immediately_despite_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = AccountsDb::open(dir.path().join("accounts.db")).await.expect("open db");
        db.block_ip("198.51.100.44", "admin", "手動封鎖").await.unwrap();
        assert!(db.is_ip_blacklisted("198.51.100.44").await.unwrap());
        assert!(db.is_ip_blacklisted("198.51.100.44").await.unwrap()); // 第二次走快取
        db.unblock_ip("198.51.100.44").await.unwrap();
        assert!(!db.is_ip_blacklisted("198.51.100.44").await.unwrap());
    }
}
