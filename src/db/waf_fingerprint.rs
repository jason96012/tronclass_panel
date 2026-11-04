//! WAF 指紋封鎖紀錄（指紋＋IP 群組，指數時間封鎖）。
//!
//! 使用者於 2026-11-04 的規則：指紋命中 → 403，並依次數指數延長的時間封鎖；
//! 管理員收到通知，可按「放行」；被放行的組合在 `FP_RELEASE_EXEMPT_SECS`（30 天）內不再因指紋被封鎖。
//! 到期後恢復檢查（R-03，使用者 2026-11-04 決定：加 30 天到期，改變了「放行永久有效」的原規則）。
//!
//! 鍵是 `pair`＝`sha256(指紋)` 前 16 碼 + `|` + IP 群組（IPv4 為單一 IP，IPv6 為 /64）。
//! 只用指紋當鍵會有問題：同一款瀏覽器 UA 可能被很多人共用，一次放行就會讓全部人豁免，
//! 所以放行必須綁定 IP 群組。
//!
//! 時間封鎖與次數的計算在 `waf::fingerprint::next_backoff`，這裡只負責讀寫。

use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::FromRow;

use super::AccountsDb;

/// 放行的有效期（秒）：30 天（R-03）。
pub const FP_RELEASE_EXEMPT_SECS: i64 = 30 * 24 * 60 * 60;

/// 一筆「指紋＋IP 群組」的封鎖狀態。
#[derive(Debug, Clone, FromRow)]
pub struct FingerprintBlockRow {
    pub pair: String,
    pub strikes: i64,
    pub blocked_until: i64,
    pub last_blocked_at: i64,
    pub exempt: i64,
    pub exempt_until: i64,
}

impl FingerprintBlockRow {
    /// 放行是否仍有效：旗標為 1 且尚未到期。到期後回到一般指紋規則。
    pub fn exempt_active(&self, now: i64) -> bool {
        self.exempt != 0 && self.exempt_until > now
    }
}

impl AccountsDb {
    /// 讀取某個組合的封鎖狀態；沒有紀錄回傳 `None`。
    pub async fn get_fingerprint_block(&self, pair: &str) -> Result<Option<FingerprintBlockRow>> {
        sqlx::query_as::<_, FingerprintBlockRow>(
            "SELECT pair, strikes, blocked_until, last_blocked_at, exempt, exempt_until \
             FROM waf_fingerprint_blocks WHERE pair = ?",
        )
        .bind(pair)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("讀取 WAF 指紋封鎖紀錄失敗")
    }

    /// 寫入一次新的封鎖（次數與解除時間由呼叫端算好）。
    /// 不會覆寫 `exempt`：已放行的組合不會再走到這裡。
    pub async fn record_fingerprint_block(
        &self,
        pair: &str,
        ip_group: &str,
        fingerprint_preview: &str,
        strikes: i64,
        blocked_until: i64,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO waf_fingerprint_blocks \
             (pair, ip_group, fingerprint_preview, strikes, blocked_until, last_blocked_at, exempt) \
             VALUES (?, ?, ?, ?, ?, ?, 0) \
             ON CONFLICT(pair) DO UPDATE SET \
               strikes = excluded.strikes, \
               blocked_until = excluded.blocked_until, \
               last_blocked_at = excluded.last_blocked_at",
        )
        .bind(pair)
        .bind(ip_group)
        .bind(fingerprint_preview)
        .bind(strikes)
        .bind(blocked_until)
        .bind(now)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 WAF 指紋封鎖紀錄失敗")?;
        Ok(())
    }

    /// 管理員放行：標記豁免 30 天、清除現有封鎖。回傳是否找到該組合。
    pub async fn release_fingerprint_block(&self, pair: &str, released_by: &str, now: i64) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE waf_fingerprint_blocks \
             SET exempt = 1, exempt_until = ?, blocked_until = 0, released_by = ?, released_at = ? \
             WHERE pair = ?",
        )
        .bind(now + FP_RELEASE_EXEMPT_SECS)
        .bind(released_by)
        .bind(now)
        .bind(pair)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("放行 WAF 指紋封鎖失敗")?;
        Ok(result.rows_affected() > 0)
    }
}

/// 建表（由 `schema::create_schema` 呼叫）。
pub(super) async fn create_table(pool: &sqlx::SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS waf_fingerprint_blocks (
            pair TEXT PRIMARY KEY NOT NULL,
            ip_group TEXT NOT NULL DEFAULT '',
            fingerprint_preview TEXT NOT NULL DEFAULT '',
            strikes INTEGER NOT NULL DEFAULT 0,
            blocked_until INTEGER NOT NULL DEFAULT 0,
            last_blocked_at INTEGER NOT NULL DEFAULT 0,
            exempt INTEGER NOT NULL DEFAULT 0,
            released_by TEXT NOT NULL DEFAULT '',
            released_at INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 waf_fingerprint_blocks 資料表失敗")?;

    // R-03：既有資料表補上放行到期欄位（已存在則略過）。
    super::ensure_table_column(
        pool,
        "waf_fingerprint_blocks",
        "exempt_until",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    // 舊版放行沒有到期時間：回填為「放行時間 + 30 天」，之後的啟動不會再改動（只處理 exempt_until = 0 的列）。
    sqlx::query(
        "UPDATE waf_fingerprint_blocks \
         SET exempt_until = released_at + ? \
         WHERE exempt = 1 AND exempt_until = 0",
    )
    .bind(FP_RELEASE_EXEMPT_SECS)
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("回填 waf_fingerprint_blocks.exempt_until 失敗")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_db() -> (tempfile::TempDir, AccountsDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = AccountsDb::open(dir.path().join("accounts.db"))
            .await
            .expect("open db");
        (dir, db)
    }

    #[tokio::test]
    async fn block_strikes_then_release_exempts_for_that_pair() {
        let (_dir, db) = open_db().await;
        let pair = "0123456789abcdef|203.0.113.7";
        assert!(db.get_fingerprint_block(pair).await.unwrap().is_none());

        db.record_fingerprint_block(pair, "203.0.113.7", "UA-X", 1, 1_000, 500)
            .await
            .unwrap();
        let row = db.get_fingerprint_block(pair).await.unwrap().unwrap();
        assert_eq!((row.strikes, row.blocked_until, row.exempt), (1, 1_000, 0));

        // 再次封鎖：次數與解除時間更新。
        db.record_fingerprint_block(pair, "203.0.113.7", "UA-X", 2, 2_000, 1_500)
            .await
            .unwrap();
        let row = db.get_fingerprint_block(pair).await.unwrap().unwrap();
        assert_eq!((row.strikes, row.blocked_until, row.last_blocked_at), (2, 2_000, 1_500));

        // 放行：豁免、清除封鎖時間。
        assert!(db.release_fingerprint_block(pair, "admin", 1_600).await.unwrap());
        let row = db.get_fingerprint_block(pair).await.unwrap().unwrap();
        assert_eq!((row.exempt, row.blocked_until), (1, 0));
        assert_eq!(row.exempt_until, 1_600 + FP_RELEASE_EXEMPT_SECS, "放行 30 天後到期");
        assert!(row.exempt_active(1_601));
        assert!(!row.exempt_active(1_600 + FP_RELEASE_EXEMPT_SECS), "到期當下即不再豁免");

        // 放行後即使再寫入封鎖，也不會覆寫豁免旗標。
        db.record_fingerprint_block(pair, "203.0.113.7", "UA-X", 3, 9_999, 1_700)
            .await
            .unwrap();
        let row = db.get_fingerprint_block(pair).await.unwrap().unwrap();
        assert_eq!(row.exempt, 1, "放行狀態不得被封鎖寫入覆寫");

        // 不存在的組合：放行回傳 false，不建立新紀錄。
        assert!(!db.release_fingerprint_block("ffffffffffffffff|1.2.3.4", "admin", 1).await.unwrap());
        assert!(db.get_fingerprint_block("ffffffffffffffff|1.2.3.4").await.unwrap().is_none());
    }

    /// R-03：放行到期後，同一組合可以再次被指紋封鎖（不是永久豁免）。
    #[tokio::test]
    async fn release_expires_after_thirty_days() {
        let (_dir, db) = open_db().await;
        let pair = "0123456789abcdef|203.0.113.7";
        db.record_fingerprint_block(pair, "203.0.113.7", "UA-X", 1, 1_000, 500).await.unwrap();
        db.release_fingerprint_block(pair, "admin", 600).await.unwrap();
        let row = db.get_fingerprint_block(pair).await.unwrap().unwrap();
        assert!(row.exempt_active(600 + FP_RELEASE_EXEMPT_SECS - 1));
        assert!(!row.exempt_active(600 + FP_RELEASE_EXEMPT_SECS + 1), "到期後不得仍豁免");
    }

    #[tokio::test]
    async fn different_ip_group_with_same_fingerprint_is_not_exempted() {
        let (_dir, db) = open_db().await;
        let released = "0123456789abcdef|203.0.113.7";
        let other = "0123456789abcdef|198.51.100.9";
        db.record_fingerprint_block(released, "203.0.113.7", "UA-X", 1, 1_000, 500).await.unwrap();
        db.record_fingerprint_block(other, "198.51.100.9", "UA-X", 1, 1_000, 500).await.unwrap();
        db.release_fingerprint_block(released, "admin", 600).await.unwrap();
        let row = db.get_fingerprint_block(other).await.unwrap().unwrap();
        assert_eq!(row.exempt, 0, "放行只影響被放行的 IP 群組");
    }
}
