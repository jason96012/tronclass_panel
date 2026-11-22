//! 作業截止提醒去重、使用者手動完成標記與作業清單快照。

use miette::{IntoDiagnostic, Result, WrapErr};

use super::{AccountsDb, MEANINGFUL_EXTENSION_SECS};

impl AccountsDb {
    // ── 作業截止提醒去重 ──────────────────────────────────────────────────────

    /// 檢查某個作業在指定階段（如 24h, 6h, 1h）是否已發送過提醒。
    /// 若作業截止時間被老師顯著延長，則自動重置歷史並回傳 false，讓倒數重新提醒。
    ///
    /// MEANINGFUL_EXTENSION_ONLY_V1
    /// 只有延期超過 [`MEANINGFUL_EXTENSION_SECS`] 才視為「新的一輪」。
    /// 否則老師只把截止時間往後挪幾分鐘（很常見），會把已發過的階梯紀錄清掉，
    /// 導致使用者在同一個階段收到重複的緊急提醒。
    pub async fn is_homework_reminder_sent(
        &self,
        account_id: &str,
        homework_id: &str,
        stage: &str,
        deadline_timestamp: i64,
    ) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT deadline_timestamp FROM homework_reminder_sent
             WHERE account_id = ? AND homework_id = ? AND stage = ?",
        )
        .bind(account_id.trim())
        .bind(homework_id.trim())
        .bind(stage.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢 homework_reminder_sent 失敗")?;

        match row {
            Some((stored_deadline,)) => {
                // 只有「顯著延期」才重置。微幅順延（例如往後挪 10 分鐘）
                // 不應清掉紀錄，否則同一階段會重複發送提醒。
                if deadline_timestamp.saturating_sub(stored_deadline) >= MEANINGFUL_EXTENSION_SECS {
                    let _ = sqlx::query(
                        "DELETE FROM homework_reminder_sent WHERE account_id = ? AND homework_id = ?",
                    )
                    .bind(account_id.trim())
                    .bind(homework_id.trim())
                    .execute(&self.pool)
                    .await;
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
            None => Ok(false),
        }
    }

    /// 標記某個作業的特定階梯提醒已送出
    pub async fn mark_homework_reminder_sent(
        &self,
        account_id: &str,
        homework_id: &str,
        stage: &str,
        deadline_timestamp: i64,
    ) -> Result<bool> {
        let now = chrono::Local::now();
        let sent_at = now.timestamp();
        let sent_time = now.format("%Y-%m-%d %H:%M:%S").to_string();

        let result = sqlx::query(
            "INSERT OR REPLACE INTO homework_reminder_sent
                (account_id, homework_id, stage, deadline_timestamp, sent_at, sent_time)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(account_id.trim())
        .bind(homework_id.trim())
        .bind(stage.trim())
        .bind(deadline_timestamp)
        .bind(sent_at)
        .bind(sent_time)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 homework_reminder_sent 失敗")?;

        Ok(result.rows_affected() > 0)
    }

    // ── 新作業通知（HOMEWORK_NEW_NOTIFY_V1）────────────────────────────────

    /// 新作業通知的「已評估」紀錄，用於偵測新上架的作業。
    ///
    /// 語意是「這份作業我們看過了」，因此：
    /// * 不在集合裡、又可通知 → 就是新上架的作業；
    /// * 第一次評估某帳號時（見 [`Self::is_homework_new_baseline_ready`]）先整批寫入，
    ///   避免剛部署時把既有作業全部當成新的通知一輪；
    /// * 這個資料表**不做保留期清理**（與 `homework_reminder_sent` 不同）：
    ///   一旦清掉舊紀錄，長跑的作業會被當成新的重複通知。列數等於歷來作業數，很小。
    pub const HOMEWORK_SEEN_BASELINE_ID: &'static str = "baseline";

    /// 取得此帳號「已評估過」的作業 id 集合（不含基準標記）。
    pub async fn list_homework_seen_ids(
        &self,
        account_id: &str,
    ) -> Result<std::collections::HashSet<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT homework_id FROM homework_seen WHERE account_id = ? AND homework_id <> ?",
        )
        .bind(account_id.trim())
        .bind(Self::HOMEWORK_SEEN_BASELINE_ID)
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢已評估作業清單失敗")?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// 標記一批作業為「已評估」。已存在者不重複寫入。
    pub async fn mark_homework_seen(&self, account_id: &str, homework_ids: &[String]) -> Result<()> {
        let now = chrono::Local::now().timestamp();
        for homework_id in homework_ids {
            let homework_id = homework_id.trim();
            if homework_id.is_empty() {
                continue;
            }
            sqlx::query(
                "INSERT OR IGNORE INTO homework_seen (account_id, homework_id, seen_at) VALUES (?, ?, ?)",
            )
            .bind(account_id.trim())
            .bind(homework_id)
            .bind(now)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("寫入已評估作業清單失敗")?;
        }
        Ok(())
    }

    /// 此帳號是否已建立「新作業通知」基準（第一次評估時由
    /// [`Self::mark_homework_new_baseline`] 寫入）。
    pub async fn is_homework_new_baseline_ready(&self, account_id: &str) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM homework_seen WHERE account_id = ? AND homework_id = ? LIMIT 1",
        )
        .bind(account_id.trim())
        .bind(Self::HOMEWORK_SEEN_BASELINE_ID)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢新作業通知基準失敗")?;
        Ok(row.is_some())
    }

    /// 建立此帳號的新作業通知基準。
    pub async fn mark_homework_new_baseline(&self, account_id: &str) -> Result<()> {
        let now = chrono::Local::now().timestamp();
        sqlx::query(
            "INSERT OR REPLACE INTO homework_seen (account_id, homework_id, seen_at) VALUES (?, ?, ?)",
        )
        .bind(account_id.trim())
        .bind(Self::HOMEWORK_SEEN_BASELINE_ID)
        .bind(now)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("建立新作業通知基準失敗")?;
        Ok(())
    }

    // ── 使用者手動標記作業完成（HOMEWORK_USER_COMPLETED_V1） ───────────────

    /// 標記某個作業為「使用者已手動完成」，停止後續截止提醒。
    pub async fn mark_homework_user_completed(
        &self,
        account_id: &str,
        homework_id: &str,
    ) -> Result<bool> {
        let now = chrono::Local::now();
        let result = sqlx::query(
            "INSERT OR REPLACE INTO homework_user_completed
                (account_id, homework_id, completed_at, completed_time)
             VALUES (?, ?, ?, ?)",
        )
        .bind(account_id.trim())
        .bind(homework_id.trim())
        .bind(now.timestamp())
        .bind(now.format("%Y-%m-%d %H:%M:%S").to_string())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 homework_user_completed 失敗")?;
        Ok(result.rows_affected() > 0)
    }

    /// 取消標記某個作業為完成。
    pub async fn unmark_homework_user_completed(
        &self,
        account_id: &str,
        homework_id: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM homework_user_completed WHERE account_id = ? AND homework_id = ?",
        )
        .bind(account_id.trim())
        .bind(homework_id.trim())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("刪除 homework_user_completed 失敗")?;
        Ok(result.rows_affected() > 0)
    }

    /// 取得該帳號所有「使用者手動標記完成」的作業 id 集合。
    pub async fn list_homework_user_completed(
        &self,
        account_id: &str,
    ) -> Result<std::collections::HashSet<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT homework_id FROM homework_user_completed WHERE account_id = ?",
        )
        .bind(account_id.trim())
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢 homework_user_completed 失敗")?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// 該帳號是否已將此作業標記完成。
    pub async fn is_homework_user_completed(
        &self,
        account_id: &str,
        homework_id: &str,
    ) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT 1 FROM homework_user_completed WHERE account_id = ? AND homework_id = ?",
        )
        .bind(account_id.trim())
        .bind(homework_id.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢 homework_user_completed 失敗")?;
        Ok(row.is_some())
    }

    // ── 作業清單快照（開頁秒顯示用） ──────────────────────────────────────────

    /// 讀取帳號最近一次成功抓取的作業清單快照。
    ///
    /// 回傳 `(items_json, fetched_at)`；沒有快照時為 `None`。
    pub async fn get_homework_snapshot(&self, account_id: &str) -> Result<Option<(String, i64)>> {
        let row: Option<(String, i64)> = sqlx::query_as(
            "SELECT items_json, fetched_at FROM homework_snapshot WHERE account_id = ?",
        )
        .bind(account_id.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢 homework_snapshot 失敗")?;
        Ok(row)
    }

    /// UPDATE_TIMES_PAGE_V1：只讀快照的抓取時間（不載入 items_json，避免為了一個
    /// 時間戳把幾十 KB 的清單拉出來）。沒有快照時回 0。
    pub async fn get_homework_snapshot_time(&self, account_id: &str) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT fetched_at FROM homework_snapshot WHERE account_id = ?",
        )
        .bind(account_id.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("查詢 homework_snapshot 時間失敗")?;
        Ok(row.map(|(ts,)| ts).unwrap_or(0))
    }

    /// 寫入／更新帳號的作業清單快照。
    pub async fn save_homework_snapshot(
        &self,
        account_id: &str,
        items_json: &str,
        item_count: i64,
    ) -> Result<()> {
        let now = chrono::Local::now();
        sqlx::query(
            "INSERT INTO homework_snapshot
                (account_id, items_json, item_count, fetched_at, fetched_time)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
                items_json = excluded.items_json,
                item_count = excluded.item_count,
                fetched_at = excluded.fetched_at,
                fetched_time = excluded.fetched_time",
        )
        .bind(account_id.trim())
        .bind(items_json)
        .bind(item_count)
        .bind(now.timestamp())
        .bind(now.format("%Y-%m-%d %H:%M:%S").to_string())
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err("寫入 homework_snapshot 失敗")?;
        Ok(())
    }

    /// 清理 14 天前的歷史作業提醒已送出紀錄
    pub async fn cleanup_old_homework_reminders_sent(
        &self,
        older_than_timestamp: i64,
    ) -> Result<u64> {
        let result = sqlx::query("DELETE FROM homework_reminder_sent WHERE sent_at < ?")
            .bind(older_than_timestamp)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("清理舊作業提醒紀錄失敗")?;
        Ok(result.rows_affected())
    }
}

