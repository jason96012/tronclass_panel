//! 資料庫 schema 建立與欄位遷移。
//!
//! 從 `open()` 拆出，集中處理所有 `CREATE TABLE IF NOT EXISTS` /
//! `CREATE INDEX IF NOT EXISTS` 與欄位補齊（`ensure_column`）。

use miette::{IntoDiagnostic, Result, WrapErr};
use sqlx::SqlitePool;

use super::{ensure_column, ensure_table_column};

/// 建立（或補齊）所有資料表、索引與欄位遷移。
pub(crate) async fn create_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS accounts (
            id           TEXT PRIMARY KEY NOT NULL,
            provider     TEXT NOT NULL DEFAULT '',
            username     TEXT NOT NULL DEFAULT '',
            password     TEXT NOT NULL DEFAULT '',
            dashboard_password_hash TEXT NOT NULL DEFAULT '',
            enabled      INTEGER NOT NULL DEFAULT 1,
            line_user_id TEXT NOT NULL DEFAULT '',
            discord_user_id TEXT NOT NULL DEFAULT '',
            session_cookie TEXT NOT NULL DEFAULT '',
            is_admin INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 accounts 資料表失敗")?;

    ensure_column(pool, "discord_user_id", "TEXT NOT NULL DEFAULT ''").await?;
    ensure_column(pool, "session_cookie", "TEXT NOT NULL DEFAULT ''").await?;
    ensure_column(pool, "is_admin", "INTEGER NOT NULL DEFAULT 0").await?;
    ensure_column(pool, "dashboard_password_hash", "TEXT NOT NULL DEFAULT ''").await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS material_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL,
            time TEXT NOT NULL,
            actor TEXT NOT NULL DEFAULT '',
            action TEXT NOT NULL DEFAULT '',
            path TEXT NOT NULL DEFAULT '',
            folder TEXT NOT NULL DEFAULT '',
            file_name TEXT NOT NULL DEFAULT '',
            size INTEGER NOT NULL DEFAULT 0,
            size_label TEXT NOT NULL DEFAULT '',
            result TEXT NOT NULL DEFAULT '',
            details TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 material_events 資料表失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_material_events_id ON material_events(id DESC)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 material_events index 失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS activity_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL DEFAULT 0,
            time TEXT NOT NULL DEFAULT '',
            kind TEXT NOT NULL DEFAULT '',
            actor TEXT NOT NULL DEFAULT '',
            action TEXT NOT NULL DEFAULT '',
            account_id TEXT NOT NULL DEFAULT '',
            path TEXT NOT NULL DEFAULT '',
            result TEXT NOT NULL DEFAULT '',
            details TEXT NOT NULL DEFAULT '',
            ip TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 activity_events 資料表失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_activity_events_kind_timestamp ON activity_events(kind, timestamp DESC)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 activity_events kind/timestamp index 失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_activity_events_action_timestamp ON activity_events(action, timestamp DESC)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 activity_events action/timestamp index 失敗")?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_activity_events_kind_id ON activity_events(kind, id DESC)")
        .execute(pool).await.into_diagnostic().wrap_err("初始化 activity_events kind/id index 失敗")?;
    ensure_table_column(pool, "activity_events", "ip", "TEXT NOT NULL DEFAULT ''").await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ip_blacklist (
            ip TEXT PRIMARY KEY NOT NULL,
            blocked_at INTEGER NOT NULL DEFAULT 0,
            blocked_time TEXT NOT NULL DEFAULT '',
            blocked_by TEXT NOT NULL DEFAULT '',
            reason TEXT NOT NULL DEFAULT '',
            failure_count INTEGER NOT NULL DEFAULT 0,
            blocked_account_id TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 ip_blacklist 資料表失敗")?;
    // Migration for installations that created ip_blacklist before failed
    // login counts were recorded with each automatic block.
    ensure_table_column(
        pool,
        "ip_blacklist",
        "failure_count",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    ensure_table_column(
        pool,
        "ip_blacklist",
        "blocked_account_id",
        "TEXT NOT NULL DEFAULT ''",
    )
    .await?;
    // Backfill automatic blocks created before blocked_account_id existed,
    // using the matching security activity record when available.
    let _ = sqlx::query(
        "UPDATE ip_blacklist
         SET blocked_account_id = COALESCE((
           SELECT account_id FROM activity_events
           WHERE action = 'ip_blacklist_auto_block'
             AND path = ip_blacklist.ip
             AND account_id <> ''
           ORDER BY id DESC LIMIT 1
         ), '')
         WHERE blocked_by = 'system:auto-login-failure'
           AND blocked_account_id = ''",
    )
    .execute(pool)
    .await;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_ip_blacklist_blocked_at ON ip_blacklist(blocked_at DESC)")
        .execute(pool)
        .await
        .into_diagnostic()
        .wrap_err("初始化 ip_blacklist index 失敗")?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_accounts_username_provider ON accounts(username, provider, id)")
        .execute(pool).await.into_diagnostic().wrap_err("初始化 accounts username/provider index 失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS passkey_credentials (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id TEXT NOT NULL DEFAULT '',
            credential_id TEXT NOT NULL DEFAULT '',
            credential_json TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL DEFAULT 0,
            last_used_at INTEGER NOT NULL DEFAULT 0,
            UNIQUE(account_id, credential_id)
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 passkey_credentials 資料表失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_passkey_credentials_account ON passkey_credentials(account_id)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 passkey_credentials account index 失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_passkey_credentials_credential ON passkey_credentials(credential_id)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 passkey_credentials credential index 失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS timetable_entries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id TEXT NOT NULL DEFAULT '',
            weekday TEXT NOT NULL DEFAULT '',
            period_start INTEGER NOT NULL DEFAULT 0,
            period_end INTEGER NOT NULL DEFAULT 0,
            start_time TEXT NOT NULL DEFAULT '',
            end_time TEXT NOT NULL DEFAULT '',
            course_name TEXT NOT NULL DEFAULT '',
            teacher TEXT NOT NULL DEFAULT '',
            classroom TEXT NOT NULL DEFAULT '',
            course_code TEXT NOT NULL DEFAULT '',
            source TEXT NOT NULL DEFAULT '',
            updated_at INTEGER NOT NULL DEFAULT 0,
            updated_time TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 timetable_entries 資料表失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_timetable_entries_account_weekday ON timetable_entries(account_id, weekday, period_start)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 timetable_entries account index 失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_timetable_entries_updated ON timetable_entries(updated_at DESC)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 timetable_entries updated index 失敗")?;

    // PERFORMANCE_CLASS_REMINDER_INDEX_V1:
    // The class reminder scheduler filters by weekday, then orders by
    // account/start time. This covering order avoids scanning every account's
    // full timetable as the database grows.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_timetable_entries_weekday_account_start
         ON timetable_entries(weekday, account_id, start_time, period_start)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 timetable_entries weekday/account/start index 失敗")?;

    // TIMETABLE_AUTO_SYNC_V1
    // 課表自動同步（從學校校務系統爬取）的每帳號狀態機。
    //
    // 設計要點：
    // - `enabled` 預設 0 —— 必須由使用者明確開啟，升級既有部署不會突然
    //   開始對學校伺服器發出請求。
    // - `consecutive_failures` 提供指數退避的依據；成功時歸零。
    // - `last_error` 只存清洗過的訊息，不得包含帳密或 cookie。
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS timetable_sync_state (
            account_id TEXT PRIMARY KEY NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 0,
            last_success_at INTEGER NOT NULL DEFAULT 0,
            last_attempt_at INTEGER NOT NULL DEFAULT 0,
            consecutive_failures INTEGER NOT NULL DEFAULT 0,
            last_error TEXT NOT NULL DEFAULT '',
            last_summary TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 timetable_sync_state 資料表失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_timetable_sync_state_enabled
         ON timetable_sync_state(enabled, last_success_at)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 timetable_sync_state enabled index 失敗")?;

    // 舊版資料庫可能已有此表但缺欄位；逐一補齊以維持向前相容。
    ensure_table_column(
        pool,
        "timetable_sync_state",
        "last_summary",
        "TEXT NOT NULL DEFAULT ''",
    )
    .await?;

    // PERFORMANCE_TIMETABLE_AUTO_SOURCE_V1
    // 自動同步只重寫 source='auto' 的列，因此 (account_id, source) 是熱查詢。
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_timetable_entries_account_source
         ON timetable_entries(account_id, source)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 timetable_entries account/source index 失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS timetable_templates (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            description TEXT NOT NULL DEFAULT '',
            entries_json TEXT NOT NULL DEFAULT '[]',
            created_by TEXT NOT NULL DEFAULT '',
            updated_at INTEGER NOT NULL DEFAULT 0,
            updated_time TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 timetable_templates 資料表失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_timetable_templates_updated ON timetable_templates(updated_at DESC)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 timetable_templates updated index 失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS notification_preferences (
            account_id TEXT NOT NULL DEFAULT '',
            channel TEXT NOT NULL DEFAULT '',
            notification_key TEXT NOT NULL DEFAULT '',
            enabled INTEGER NOT NULL DEFAULT 1,
            updated_at INTEGER NOT NULL DEFAULT 0,
            updated_time TEXT NOT NULL DEFAULT '',
            PRIMARY KEY(account_id, channel, notification_key)
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 notification_preferences 資料表失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_notification_preferences_account_channel ON notification_preferences(account_id, channel)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 notification_preferences index 失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS class_reminder_preferences (
            account_id TEXT PRIMARY KEY NOT NULL,
            discord_enabled INTEGER NOT NULL DEFAULT 1,
            minutes_before INTEGER NOT NULL DEFAULT 10,
            updated_at INTEGER NOT NULL DEFAULT 0,
            updated_time TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 class_reminder_preferences 資料表失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS detection_schedule_preferences (
            account_id TEXT PRIMARY KEY NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            minutes_before INTEGER NOT NULL DEFAULT 10,
            minutes_after INTEGER NOT NULL DEFAULT 10,
            outside_interval_secs INTEGER NOT NULL DEFAULT 300,
            updated_at INTEGER NOT NULL DEFAULT 0,
            updated_time TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 detection_schedule_preferences 資料表失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS web_push_settings (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 web_push_settings 資料表失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS web_push_subscriptions (
            endpoint TEXT PRIMARY KEY NOT NULL,
            account_id TEXT NOT NULL DEFAULT '',
            p256dh TEXT NOT NULL DEFAULT '',
            auth TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 web_push_subscriptions 資料表失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_web_push_subscriptions_account
         ON web_push_subscriptions(account_id)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 web_push_subscriptions index 失敗")?;

    // PUSH_STALE_CLEANUP_V1：每個訂閱端點的「連續失敗次數」；成功即歸零，
    // 達門檻（或遇到永久性錯誤）即刪除並通知使用者重新啟用。
    ensure_table_column(
        pool,
        "web_push_subscriptions",
        "consecutive_failures",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS class_reminder_sent (
            account_id TEXT NOT NULL DEFAULT '',
            reminder_key TEXT NOT NULL DEFAULT '',
            sent_at INTEGER NOT NULL DEFAULT 0,
            sent_time TEXT NOT NULL DEFAULT '',
            PRIMARY KEY(account_id, reminder_key)
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 class_reminder_sent 資料表失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_class_reminder_sent_at ON class_reminder_sent(sent_at)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 class_reminder_sent index 失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS dashboard_sessions (
            token TEXT PRIMARY KEY NOT NULL,
            account_id TEXT NOT NULL DEFAULT '',
            is_admin INTEGER NOT NULL DEFAULT 0,
            must_change_password INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL DEFAULT 0,
            expires_at INTEGER NOT NULL DEFAULT 0,
            refresh_token TEXT NOT NULL DEFAULT '',
            refresh_expires_at INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 dashboard_sessions 資料表失敗")?;

    // REFRESH_TOKEN_V1
    // Migrate pre-existing DBs that lack the refresh-token columns.
    ensure_table_column(pool, "dashboard_sessions", "refresh_token", "TEXT NOT NULL DEFAULT ''")
        .await?;
    ensure_table_column(
        pool,
        "dashboard_sessions",
        "refresh_expires_at",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_dashboard_sessions_refresh ON dashboard_sessions(refresh_token)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 dashboard_sessions refresh index 失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_dashboard_sessions_expires_at ON dashboard_sessions(expires_at)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 dashboard_sessions expires_at index 失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_dashboard_sessions_account_id ON dashboard_sessions(account_id)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 dashboard_sessions account_id index 失敗")?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS homework_reminder_sent (
            account_id TEXT NOT NULL DEFAULT '',
            homework_id TEXT NOT NULL DEFAULT '',
            stage TEXT NOT NULL DEFAULT '',
            deadline_timestamp INTEGER NOT NULL DEFAULT 0,
            sent_at INTEGER NOT NULL DEFAULT 0,
            sent_time TEXT NOT NULL DEFAULT '',
            PRIMARY KEY(account_id, homework_id, stage)
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 homework_reminder_sent 資料表失敗")?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_homework_reminder_sent_at ON homework_reminder_sent(sent_at)",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 homework_reminder_sent index 失敗")?;

    // HOMEWORK_USER_COMPLETED_V1
    // 使用者手動標記「已完成」的作業。上游 is_submitted 未必即時反映
    // （例如實體繳交、或學校端尚未更新），讓使用者自行標記以停止
    // 24h/6h/1h 的截止提醒。標記鍵為 (account_id, homework_id)。
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS homework_user_completed (
            account_id  TEXT NOT NULL DEFAULT '',
            homework_id TEXT NOT NULL DEFAULT '',
            completed_at INTEGER NOT NULL DEFAULT 0,
            completed_time TEXT NOT NULL DEFAULT '',
            PRIMARY KEY(account_id, homework_id)
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 homework_user_completed 資料表失敗")?;

    // HOMEWORK_SNAPSHOT_V1
    // 保存每個帳號最近一次成功抓取的作業清單，讓作業頁一開啟就能立即顯示
    // 上次的結果（stale-while-revalidate），不必空等 Tronclass 回應。
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS homework_snapshot (
            account_id  TEXT PRIMARY KEY NOT NULL,
            items_json  TEXT NOT NULL DEFAULT '[]',
            item_count  INTEGER NOT NULL DEFAULT 0,
            fetched_at  INTEGER NOT NULL DEFAULT 0,
            fetched_time TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 homework_snapshot 資料表失敗")?;

    // HOMEWORK_LADDER_V1
    // 每帳號自訂的作業截止提醒階梯（小時陣列，由大到小，如 [72,24,6,1]）。
    // stages_json 為空字串代表「使用預設 24/6/1」（未設定或明確恢復預設）。
    // 提醒去重鍵 `homework_reminder_sent.stage` 沿用 "{h}h" 格式，因此
    // 變更階梯後：新階段會重新發送、移除的階段不再發送，歷史紀錄相容。
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS homework_reminder_preferences (
            account_id TEXT PRIMARY KEY NOT NULL,
            stages_json TEXT NOT NULL DEFAULT '',
            updated_at INTEGER NOT NULL DEFAULT 0,
            updated_time TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(pool)
    .await
    .into_diagnostic()
    .wrap_err("初始化 homework_reminder_preferences 資料表失敗")?;

    Ok(())
}
