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
}
