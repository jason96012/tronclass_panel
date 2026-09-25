//! UPDATE_TIMES_PAGE_V1：「資料更新時間」頁面與 API。
//!
//! 入口：設定 → 其他設定 → 「資料更新時間」。
//!
//! 顯示三類資訊：
//! 1. 全域排程器：公告輪詢、作業預抓、作業截止提醒檢查、課表同步檢查、上課前提醒檢查
//!    （來源：`crate::update_times`，含預期間隔，前端據此判斷「逾期」）。
//! 2. 每個帳號：點名輪詢（`MonitorStatus.last_poll_timestamp`，記憶體）、作業清單
//!    （`homework_snapshot.fetched_at`，SQLite）、課程公告（`update_times`）、
//!    課表自動同步（`timetable_sync_state`，SQLite）、晨間摘要（`update_times`）。
//! 3. 服務本身：本次啟動時間、目前伺服器時間（前端用來算「幾分鐘前」，不吃裝置時鐘誤差）。
//!
//! 權限：與 `/admin/api/status` 一致——管理員／檢視者看全部帳號，一般帳號只看自己；
//! 檢視者看別人時 username 遮罩、錯誤訊息不下發（可能含學號／網址片段）。

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    admin_login_redirect, html_page_response, must_change_password_redirect, session_token,
    unauthorized_response, AdminQuery, AdminWebState,
};
use crate::update_times as ut;

const PAGE_HTML: &str = include_str!("static/update_times.html");

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub(super) async fn page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // ADMIN_PAGE_TRI_STATE_V1：access 過期但 refresh 有效時由伺服器端輪換，不再閃登入頁。
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/update-times");
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    super::pages::attach_fresh_cookies(
        html_page_response(
            &headers,
            super::pages::inject_auth_attrs(&super::pages::inject_theme(PAGE_HTML), &page.session),
        ),
        &page,
        &headers,
    )
}

fn stamp_json(stamp: Option<&ut::UpdateStamp>, hide_error: bool) -> Value {
    let Some(stamp) = stamp else {
        return json!({ "ok_at": 0, "ok_detail": "", "error_at": 0, "error": "" });
    };
    json!({
        "ok_at": stamp.last_ok_at,
        "ok_detail": stamp.last_ok_detail,
        "error_at": stamp.last_error_at,
        "error": if hide_error { String::new() } else { stamp.last_error.clone() },
    })
}

pub(super) async fn api_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    let now = now_unix();
    let (global, per_account) = ut::snapshot();

    // ── 全域排程器（含預期間隔，秒） ──
    let bulletin_interval = crate::config::try_get_config()
        .map(|cfg| cfg.bulletins.poll_interval_secs.max(60))
        .unwrap_or(300);
    let schedulers = json!([
        {
            "kind": ut::KIND_BULLETIN_POLL,
            "label": "課程公告輪詢",
            "hint": "背景檢查各帳號課程公告，有新公告時推播（夜間暫停）。",
            "interval_secs": bulletin_interval,
            "stamp": stamp_json(global.get(ut::KIND_BULLETIN_POLL), false),
        },
        {
            "kind": ut::KIND_HOMEWORK_PREFETCH,
            "label": "作業清單預抓",
            "hint": "背景更新每個帳號的作業清單，讓作業頁一開就是最新（夜間暫停）。",
            "interval_secs": crate::homework_reminders::PREFETCH_INTERVAL_SECS,
            "stamp": stamp_json(global.get(ut::KIND_HOMEWORK_PREFETCH), false),
        },
        {
            "kind": ut::KIND_HOMEWORK_REMINDER_CHECK,
            "label": "作業截止提醒檢查",
            "hint": "檢查即將截止的未繳作業並發送倒數提醒。",
            "interval_secs": crate::homework_reminders::CHECK_INTERVAL_SECS,
            "stamp": stamp_json(global.get(ut::KIND_HOMEWORK_REMINDER_CHECK), false),
        },
        {
            "kind": ut::KIND_TIMETABLE_SYNC_CHECK,
            "label": "課表自動同步檢查",
            "hint": "每 6 小時檢查一次；每個帳號每天最多同步一次。",
            "interval_secs": crate::timetable_sync::TICK_INTERVAL_SECS,
            "stamp": stamp_json(global.get(ut::KIND_TIMETABLE_SYNC_CHECK), false),
        },
        {
            "kind": ut::KIND_CLASS_REMINDER_CHECK,
            "label": "上課前提醒檢查",
            "hint": "每分鐘比對課表與提醒設定，透過 Discord 提醒上課（未啟用 Discord 時不會執行）。",
            "interval_secs": crate::class_reminders::CHECK_INTERVAL_SECS,
            "stamp": stamp_json(global.get(ut::KIND_CLASS_REMINDER_CHECK), false),
        },
    ]);

    // ── 點名輪詢目前是否處於「刻意不輪詢」的狀態（休息日／夜間／Web 設定的休息時間）──
    // 前端據此把「很久沒輪詢」顯示成「休息中」而不是紅色逾期。
    let now_local = chrono::Local::now();
    let poll_pause_reason = if crate::holidays::is_rest_day(now_local.date_naive()) {
        "休息日（週末／國定假日／寒暑假）"
    } else if crate::detection_schedule::is_night_pause(now_local.time()) {
        "夜間停止時段"
    } else if crate::runtime_settings::is_rest_time(now_local).await {
        "Web 設定的休息時間"
    } else {
        ""
    };

    // 晨間摘要的全域開關與時間（帳號個別開關另查）。
    let digest = super::morning_digest::digest_settings();

    // ── 每個帳號 ──
    let visible = super::status::resolve_visible_accounts(&state, &session);
    let mask_sensitive = session.is_viewer && !session.is_admin;
    let mut accounts: Vec<Value> = Vec::with_capacity(visible.len());
    for account in &visible {
        let is_self = account.account_id == session.account_id;
        let hide_error = mask_sensitive && !is_self;
        let (last_poll, running, user_name) = {
            let status = account.monitor_status.lock().await;
            (
                status.last_poll_timestamp.unwrap_or(0),
                status.is_running,
                status.user_name.clone(),
            )
        };
        let running = running || *account.is_running.lock().await;
        let homework_snapshot = state
            .accounts_db
            .get_homework_snapshot_time(&account.account_id)
            .await
            .unwrap_or(0);
        let tt = state
            .accounts_db
            .get_timetable_sync_state(&account.account_id)
            .await
            .ok();
        let stamps = per_account.get(&account.account_id);
        let digest_enabled = digest.enabled
            && state
                .accounts_db
                .notification_preference_enabled(
                    &account.account_id,
                    super::morning_digest::DIGEST_PREF_CHANNEL,
                    super::morning_digest::DIGEST_PREF_KEY,
                )
                .await
                .unwrap_or(true);
        let username = if mask_sensitive && !is_self {
            super::status::mask_identifier(&account.username)
        } else {
            account.username.clone()
        };
        accounts.push(json!({
            "account_id": account.account_id,
            "username": username,
            "user_name": user_name,
            "is_self": is_self,
            "monitor_running": running,
            "rollcall_poll_at": last_poll,
            "rollcall_poll_interval_secs": state.default_poll_interval_secs,
            "homework_snapshot_at": homework_snapshot,
            "homework": stamp_json(stamps.and_then(|m| m.get(ut::KIND_HOMEWORK)), hide_error),
            "bulletins": stamp_json(stamps.and_then(|m| m.get(ut::KIND_BULLETINS)), hide_error),
            "morning_digest": stamp_json(stamps.and_then(|m| m.get(ut::KIND_MORNING_DIGEST)), hide_error),
            "morning_digest_enabled": digest_enabled,
            "morning_digest_time": digest.time,
            "timetable_sync": tt.as_ref().map(|row| json!({
                "enabled": row.enabled != 0,
                "last_success_at": row.last_success_at,
                "last_attempt_at": row.last_attempt_at,
                "consecutive_failures": row.consecutive_failures,
                "last_error": if hide_error { String::new() } else { row.last_error.clone() },
                "last_summary": row.last_summary,
            })).unwrap_or(Value::Null),
        }));
    }

    (
        StatusCode::OK,
        Json(json!({
            "server_time": now,
            "started_at": ut::started_at(),
            "is_admin": session.is_admin,
            "is_viewer": session.is_viewer,
            "poll_pause_reason": poll_pause_reason,
            "schedulers": schedulers,
            "accounts": accounts,
        })),
    )
        .into_response()
}
