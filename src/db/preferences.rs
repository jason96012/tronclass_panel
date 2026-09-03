//! 通知 / 上課提醒 / 偵測排程偏好，以及上課提醒查詢。

use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::FromRow;

use super::AccountsDb;

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct NotificationPreferenceRow {
    pub account_id: String,
    pub channel: String,
    pub notification_key: String,
    pub enabled: bool,
    pub updated_at: i64,
    pub updated_time: String,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct ClassReminderPreferenceRow {
    pub account_id: String,
    pub discord_enabled: bool,
    pub minutes_before: i64,
    pub updated_at: i64,
    pub updated_time: String,
}

/// Per-account timetable-driven rollcall detection settings.
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct DetectionSchedulePreferenceRow {
    pub account_id: String,
    pub enabled: bool,
    pub minutes_before: i64,
    pub minutes_after: i64,
    pub outside_interval_secs: i64,
    pub updated_at: i64,
    pub updated_time: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct ClassReminderDueRow {
    pub timetable_id: i64,
    pub account_id: String,
    pub weekday: String,
    pub period_start: i64,
    pub period_end: i64,
    pub start_time: String,
    pub end_time: String,
    pub course_name: String,
    pub teacher: String,
    pub classroom: String,
    pub discord_user_id: String,
    pub minutes_before: i64,
}

impl AccountsDb {
    pub async fn list_notification_preferences(
        &self,
        account_id: &str,
        channel: &str,
    ) -> Result<Vec<NotificationPreferenceRow>> {
        let rows = sqlx::query_as::<_, NotificationPreferenceRow>(
            "SELECT account_id, channel, notification_key, enabled, updated_at, updated_time
             FROM notification_preferences
             WHERE account_id = ? AND channel = ?
             ORDER BY notification_key",
        )
        .bind(account_id.trim())
        .bind(channel.trim())
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 通知設定失敗"))?;
        Ok(rows)
    }

    pub async fn set_notification_preference(
        &self,
        account_id: &str,
        channel: &str,
        notification_key: &str,
        enabled: bool,
    ) -> Result<()> {
        let preferences = [(notification_key.trim().to_string(), enabled)];
        self.set_notification_preferences(account_id, channel, &preferences)
            .await
    }

    pub async fn set_notification_preferences(
        &self,
        account_id: &str,
        channel: &str,
        preferences: &[(String, bool)],
    ) -> Result<()> {
        let account_id = account_id.trim();
        let channel = channel.trim();
        if preferences.is_empty() {
            return Ok(());
        }

        // PERFORMANCE_NOTIFICATION_PREF_BATCH_V1:
        // Save all checkbox preferences in a single SQLite transaction. The
        // previous handler upserted each key separately, multiplying write-lock
        // and fsync overhead for one UI save action.
        let now = chrono::Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();
        let mut tx = self
            .pool
            .begin()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("開始 `{account_id}` 通知設定交易失敗"))?;

        for (notification_key, enabled) in preferences {
            sqlx::query(
                "INSERT INTO notification_preferences (account_id, channel, notification_key, enabled, updated_at, updated_time)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(account_id, channel, notification_key) DO UPDATE SET
                   enabled = excluded.enabled,
                   updated_at = excluded.updated_at,
                   updated_time = excluded.updated_time",
            )
            .bind(account_id)
            .bind(channel)
            .bind(notification_key.trim())
            .bind(*enabled)
            .bind(timestamp)
            .bind(&time_label)
            .execute(&mut *tx)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("更新 `{account_id}` 通知設定失敗"))?;
        }

        tx.commit()
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("提交 `{account_id}` 通知設定交易失敗"))?;
        Ok(())
    }

    pub async fn notification_preference_enabled(
        &self,
        account_id: &str,
        channel: &str,
        notification_key: &str,
    ) -> Result<bool> {
        let row = sqlx::query_as::<_, (bool,)>(
            "SELECT enabled FROM notification_preferences
             WHERE account_id = ? AND channel = ? AND notification_key = ?",
        )
        .bind(account_id.trim())
        .bind(channel.trim())
        .bind(notification_key.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 通知設定失敗"))?;
        Ok(row.map(|(enabled,)| enabled).unwrap_or(true))
    }

    pub async fn get_detection_schedule_preference(
        &self,
        account_id: &str,
    ) -> Result<DetectionSchedulePreferenceRow> {
        let account_id = account_id.trim();
        let row = sqlx::query_as::<_, DetectionSchedulePreferenceRow>(
            "SELECT account_id, enabled, minutes_before, minutes_after,
                    outside_interval_secs, updated_at, updated_time
             FROM detection_schedule_preferences
             WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 點名偵測排程失敗"))?;

        Ok(row.unwrap_or_else(|| DetectionSchedulePreferenceRow {
            account_id: account_id.to_string(),
            enabled: true,
            minutes_before: 10,
            minutes_after: 10,
            outside_interval_secs: 300,
            updated_at: 0,
            updated_time: String::new(),
        }))
    }

    pub async fn set_detection_schedule_preference(
        &self,
        account_id: &str,
        enabled: bool,
        minutes_before: i64,
        minutes_after: i64,
        outside_interval_secs: i64,
    ) -> Result<()> {
        let account_id = account_id.trim();
        let minutes_before = minutes_before.clamp(0, 180);
        let minutes_after = minutes_after.clamp(0, 180);
        let outside_interval_secs = outside_interval_secs.clamp(60, 3600);
        let now = chrono::Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();
        sqlx::query(
            "INSERT INTO detection_schedule_preferences
                (account_id, enabled, minutes_before, minutes_after,
                 outside_interval_secs, updated_at, updated_time)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
                enabled = excluded.enabled,
                minutes_before = excluded.minutes_before,
                minutes_after = excluded.minutes_after,
                outside_interval_secs = excluded.outside_interval_secs,
                updated_at = excluded.updated_at,
                updated_time = excluded.updated_time",
        )
        .bind(account_id)
        .bind(enabled)
        .bind(minutes_before)
        .bind(minutes_after)
        .bind(outside_interval_secs)
        .bind(timestamp)
        .bind(&time_label)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("更新 `{account_id}` 點名偵測排程失敗"))?;
        Ok(())
    }

    pub async fn get_class_reminder_preference(
        &self,
        account_id: &str,
    ) -> Result<ClassReminderPreferenceRow> {
        let account_id = account_id.trim();
        let row = sqlx::query_as::<_, ClassReminderPreferenceRow>(
            "SELECT account_id, discord_enabled, minutes_before, updated_at, updated_time
             FROM class_reminder_preferences
             WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 上課提醒設定失敗"))?;

        Ok(row.unwrap_or_else(|| ClassReminderPreferenceRow {
            account_id: account_id.to_string(),
            discord_enabled: true,
            minutes_before: 10,
            updated_at: 0,
            updated_time: String::new(),
        }))
    }

    pub async fn set_class_reminder_preference(
        &self,
        account_id: &str,
        discord_enabled: bool,
        minutes_before: i64,
    ) -> Result<()> {
        let account_id = account_id.trim();
        let minutes_before = minutes_before.clamp(0, 180);
        let now = chrono::Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();
        sqlx::query(
            "INSERT INTO class_reminder_preferences (account_id, discord_enabled, minutes_before, updated_at, updated_time)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
                discord_enabled = excluded.discord_enabled,
                minutes_before = excluded.minutes_before,
                updated_at = excluded.updated_at,
                updated_time = excluded.updated_time",
        )
        .bind(account_id)
        .bind(discord_enabled)
        .bind(minutes_before)
        .bind(timestamp)
        .bind(&time_label)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("更新 `{account_id}` 上課提醒設定失敗"))?;
        Ok(())
    }

    pub async fn list_due_class_reminders_for_weekday(
        &self,
        weekday: &str,
    ) -> Result<Vec<ClassReminderDueRow>> {
        let rows = sqlx::query_as::<_, ClassReminderDueRow>(
            "SELECT
                t.id AS timetable_id,
                t.account_id AS account_id,
                t.weekday AS weekday,
                t.period_start AS period_start,
                t.period_end AS period_end,
                t.start_time AS start_time,
                t.end_time AS end_time,
                t.course_name AS course_name,
                t.teacher AS teacher,
                t.classroom AS classroom,
                a.discord_user_id AS discord_user_id,
                COALESCE(p.minutes_before, 10) AS minutes_before
             FROM timetable_entries t
             JOIN accounts a ON a.id = t.account_id
             LEFT JOIN class_reminder_preferences p ON p.account_id = t.account_id
             WHERE t.weekday = ?
               AND a.enabled = 1
               AND TRIM(a.discord_user_id) <> ''
               AND COALESCE(p.discord_enabled, 1) = 1
               AND TRIM(t.course_name) <> ''
             ORDER BY t.account_id, t.start_time, t.period_start",
        )
        .bind(weekday.trim())
        .fetch_all(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "讀取 `{weekday}` 上課提醒清單失敗；請確認 timetable_entries 課表表格已建立"
            )
        })?;
        Ok(rows)
    }

    pub async fn mark_class_reminder_sent(
        &self,
        account_id: &str,
        reminder_key: &str,
    ) -> Result<bool> {
        let now = chrono::Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();
        let result = sqlx::query(
            "INSERT OR IGNORE INTO class_reminder_sent (account_id, reminder_key, sent_at, sent_time)
             VALUES (?, ?, ?, ?)",
        )
        .bind(account_id.trim())
        .bind(reminder_key.trim())
        .bind(timestamp)
        .bind(&time_label)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("記錄 `{account_id}` 上課提醒已送出失敗"))?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn cleanup_old_class_reminder_sent(&self, older_than_timestamp: i64) -> Result<u64> {
        let result = sqlx::query("DELETE FROM class_reminder_sent WHERE sent_at < ?")
            .bind(older_than_timestamp)
            .execute(&self.pool)
            .await
            .into_diagnostic()
            .wrap_err("清理舊上課提醒紀錄失敗")?;
        Ok(result.rows_affected())
    }

    // ─── 作業截止提醒階梯（HOMEWORK_LADDER_V1）───────────────────────────────

    /// 讀取帳號自訂的作業提醒階梯（小時，由大到小）。
    ///
    /// 回傳 `None` 代表未設定（或儲存值無法解析），呼叫端應退回
    /// [`DEFAULT_HOMEWORK_LADDER_HOURS`]。解析失敗不回報錯誤：壞資料
    /// 只該影響該帳號退回預設，不該讓整輪提醒排程中斷。
    pub async fn get_homework_reminder_ladder_raw(
        &self,
        account_id: &str,
    ) -> Result<Option<Vec<u64>>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT stages_json FROM homework_reminder_preferences WHERE account_id = ?",
        )
        .bind(account_id.trim())
        .fetch_optional(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("讀取 `{account_id}` 作業提醒階梯失敗"))?;

        let Some((stages_json,)) = row else {
            return Ok(None);
        };
        let trimmed = stages_json.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        match serde_json::from_str::<Vec<u64>>(trimmed) {
            Ok(stages) if !stages.is_empty() => Ok(Some(stages)),
            _ => Ok(None),
        }
    }

    /// 讀取「實際生效」的作業提醒階梯：自訂值存在就用自訂，否則退回預設。
    pub async fn get_homework_reminder_ladder(&self, account_id: &str) -> Result<Vec<u64>> {
        Ok(self
            .get_homework_reminder_ladder_raw(account_id)
            .await?
            .unwrap_or_else(|| DEFAULT_HOMEWORK_LADDER_HOURS.to_vec()))
    }

    /// 儲存自訂作業提醒階梯；傳入空切片代表「恢復預設」（儲存空字串）。
    ///
    /// 呼叫端應先用 [`normalize_homework_ladder`] 驗證／正規化；
    /// 這裡另做一次防禦性正規化，確保無論誰呼叫都不會寫入範圍外或重複值。
    pub async fn set_homework_reminder_ladder(
        &self,
        account_id: &str,
        stages: &[u64],
    ) -> Result<Vec<u64>> {
        let normalized = normalize_homework_ladder(stages);
        let stages_json = if normalized.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&normalized)
                .into_diagnostic()
                .wrap_err("序列化作業提醒階梯失敗")?
        };
        let now = chrono::Local::now();
        let timestamp = now.timestamp();
        let time_label = now.format("%Y-%m-%d %H:%M:%S").to_string();
        sqlx::query(
            "INSERT INTO homework_reminder_preferences (account_id, stages_json, updated_at, updated_time)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
                stages_json = excluded.stages_json,
                updated_at = excluded.updated_at,
                updated_time = excluded.updated_time",
        )
        .bind(account_id.trim())
        .bind(&stages_json)
        .bind(timestamp)
        .bind(&time_label)
        .execute(&self.pool)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("儲存 `{account_id}` 作業提醒階梯失敗"))?;
        Ok(normalized)
    }
}

// ─── 作業提醒階梯常數與正規化（HOMEWORK_LADDER_V1）──────────────────────────

/// 預設作業截止提醒階梯（小時）：24h → 6h → 1h。
pub(crate) const DEFAULT_HOMEWORK_LADDER_HOURS: [u64; 3] = [24, 6, 1];
/// 單一帳號最多自訂幾個階段。
pub(crate) const HOMEWORK_LADDER_MAX_STAGES: usize = 8;
/// 單一階段允許的最小時數（1 小時）。
pub(crate) const HOMEWORK_LADDER_MIN_HOURS: u64 = 1;
/// 單一階段允許的最大時數（7 天）。
pub(crate) const HOMEWORK_LADDER_MAX_HOURS: u64 = 168;

/// 正規化自訂階梯：
/// 1. 丟棄範圍外（< 1 或 > 168 小時）的值；
/// 2. 去重；
/// 3. 由大到小排序（與顯示及评估順序一致）；
/// 4. 超過 [`HOMEWORK_LADDER_MAX_STAGES`] 個時只保留最大的前 N 個。
///
/// 全部被丟棄時回傳空 Vec（語意＝恢復預設）。
pub(crate) fn normalize_homework_ladder(stages: &[u64]) -> Vec<u64> {
    let mut valid: Vec<u64> = stages
        .iter()
        .copied()
        .filter(|&h| (HOMEWORK_LADDER_MIN_HOURS..=HOMEWORK_LADDER_MAX_HOURS).contains(&h))
        .collect();
    valid.sort_unstable_by(|a, b| b.cmp(a)); // 由大到小
    valid.dedup();
    valid.truncate(HOMEWORK_LADDER_MAX_STAGES);
    valid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_keeps_descending_order_and_dedupes() {
        assert_eq!(
            normalize_homework_ladder(&[1, 24, 6]),
            vec![24, 6, 1]
        );
        assert_eq!(
            normalize_homework_ladder(&[6, 6, 24, 24, 1]),
            vec![24, 6, 1]
        );
    }

    #[test]
    fn test_normalize_drops_out_of_range() {
        assert_eq!(normalize_homework_ladder(&[0, 1, 200, 24]), vec![24, 1]);
        // 全部範圍外 → 空（恢復預設）
        assert_eq!(normalize_homework_ladder(&[0, 500]), Vec::<u64>::new());
    }

    #[test]
    fn test_normalize_truncates_to_max_stages() {
        let stages = [1u64, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12];
        let normalized = normalize_homework_ladder(&stages);
        assert_eq!(normalized.len(), HOMEWORK_LADDER_MAX_STAGES);
        // 保留「最大」的前 8 個：12,10,9,8,7,6,5,4
        assert_eq!(normalized, vec![12, 10, 9, 8, 7, 6, 5, 4]);
    }

    #[test]
    fn test_normalize_empty_is_empty() {
        assert_eq!(normalize_homework_ladder(&[]), Vec::<u64>::new());
    }

    #[tokio::test]
    async fn homework_ladder_roundtrip_and_default_fallback() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db = super::super::AccountsDb::open(file.path()).await.unwrap();

        // 未設定 → 生效值為預設
        assert_eq!(
            db.get_homework_reminder_ladder("acct-a").await.unwrap(),
            DEFAULT_HOMEWORK_LADDER_HOURS.to_vec()
        );
        assert_eq!(
            db.get_homework_reminder_ladder_raw("acct-a").await.unwrap(),
            None
        );

        // 儲存自訂階梯（故意給未排序+重複）→ 正規化後回傳與讀回一致
        let saved = db
            .set_homework_reminder_ladder("acct-a", &[1, 72, 24, 24, 6])
            .await
            .unwrap();
        assert_eq!(saved, vec![72, 24, 6, 1]);
        assert_eq!(
            db.get_homework_reminder_ladder("acct-a").await.unwrap(),
            vec![72, 24, 6, 1]
        );

        // 恢復預設（空切片）→ raw 回 None、生效值回預設
        let reset = db
            .set_homework_reminder_ladder("acct-a", &[])
            .await
            .unwrap();
        assert_eq!(reset, Vec::<u64>::new());
        assert_eq!(
            db.get_homework_reminder_ladder("acct-a").await.unwrap(),
            DEFAULT_HOMEWORK_LADDER_HOURS.to_vec()
        );
    }
}
