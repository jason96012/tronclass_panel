//! 點名出席統計與「由誰送出」查詢（ROLLCALL_ATTENDANCE_V1／fix66）。
//!
//! 兩個唯讀端點：
//! - `GET /admin/api/rollcalls/stats?days=&account_id=`    出席統計（總覽＋每門課＋各送出者）
//! - `GET /admin/api/rollcalls/records?days=&account_id=&limit=`  逐筆送出紀錄（含送出者）
//!
//! 資料來源是 `rollcall_events` 資料表（monitor 每次送出後寫入），不是記憶體歷史，
//! 因此重啟後仍查得到，也不受「最近 50 筆」上限影響。
//!
//! 權限：
//! - 一般帳號：只能查自己的帳號（帶別人的 `account_id` 一律拒絕）。
//! - 管理員：預設查全部帳號，可帶 `account_id` 只看某個帳號。
//! - 檢視者：看得到統計，但別人的帳號／姓名會遮罩（與其他頁面一致）。

use super::*;
use serde::Deserialize;

use crate::db::{
    aggregate_courses, aggregate_senders, rollcall_success_rate, select_display_session_rows,
};

/// 查詢範圍上限（天）。3650 天＝實務上的「全部」，但不會產生無上限的 SQL。
const ROLLCALL_STATS_MAX_DAYS: i64 = 3650;
const ROLLCALL_STATS_DEFAULT_DAYS: i64 = 90;
/// 統計最多讀幾筆原始紀錄再聚合（一人一學期數百筆，留很大餘裕）。
const ROLLCALL_STATS_MAX_ROWS: i64 = 5000;
const ROLLCALL_RECORDS_DEFAULT_LIMIT: i64 = 50;
const ROLLCALL_RECORDS_MAX_LIMIT: i64 = 200;

#[derive(Debug, Deserialize)]
pub(super) struct AdminRollcallQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

/// 解析查詢視窗 → `(days, since)`；`days = 0` 代表全部（只受保留期限制）。
fn rollcall_window(days: Option<i64>) -> (i64, i64) {
    let days = days
        .unwrap_or(ROLLCALL_STATS_DEFAULT_DAYS)
        .clamp(0, ROLLCALL_STATS_MAX_DAYS);
    let now = chrono::Local::now().timestamp();
    let since = if days <= 0 {
        0
    } else {
        now - days * 86_400
    };
    (days, since)
}

/// 決定查詢的帳號範圍：`None` ＝ 全部帳號（只有管理員拿得到）。
fn resolve_rollcall_scope(
    session: &AdminSession,
    requested: Option<&str>,
) -> Result<Option<String>, Response> {
    let requested = requested.map(str::trim).filter(|value| !value.is_empty());
    if session.is_admin {
        // 管理員：帶了就只看那個帳號，沒帶就全部（面板預設要看整體出席）。
        return Ok(requested.map(str::to_string));
    }
    if let Some(requested) = requested {
        if requested != session.account_id {
            return Err(unauthorized_response());
        }
    }
    Ok(Some(session.account_id.clone()))
}

/// 檢視者遮罩：別人的學校帳號／姓名不對外顯示（與 status.rs 同一套規則）。
fn mask_other_accounts(rows: &mut [crate::db::RollcallEventRow], session: &AdminSession) {
    if !(session.is_viewer && !session.is_admin) {
        return;
    }
    for row in rows.iter_mut() {
        if row.account_id != session.account_id {
            row.username = super::status::mask_identifier(&row.username);
            row.user_name = super::status::mask_identifier(&row.user_name);
        }
    }
}

/// 當地今天 00:00 的 unix 秒。台灣無 DST；缺時退回「現在往回對齊整點」。
fn local_today_start() -> i64 {
    use chrono::Timelike;
    let now = chrono::Local::now();
    now.timestamp()
        - i64::from(now.hour()) * 3600
        - i64::from(now.minute()) * 60
        - i64::from(now.second())
}

fn database_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "ok": false, "error": message })),
    )
        .into_response()
}

/// `GET /admin/api/rollcalls/stats`：出席統計。
pub(super) async fn admin_rollcall_stats_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminRollcallQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_optional_admin_text(
        query.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let scope = match resolve_rollcall_scope(&session, query.account_id.as_deref()) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let (days, since) = rollcall_window(query.days);

    let mut rows = match state
        .accounts_db
        .list_rollcall_events(scope.as_deref(), since, ROLLCALL_STATS_MAX_ROWS)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            warn!(%error, "讀取點名出席統計失敗");
            return database_error("讀取點名紀錄失敗，請稍後再試");
        }
    };
    mask_other_accounts(&mut rows, &session);

    // ATT_STATS_ONE_SESSION_V1：每門課只顯示一場；今天已有新場且有上一場時不把今天算進去。
    let today_start = local_today_start();
    let rows = select_display_session_rows(&rows, today_start);

    let total = rows.len() as i64;
    let success = rows.iter().filter(|row| row.is_success()).count() as i64;
    let failed = total - success;
    let courses = aggregate_courses(&rows);
    let senders = aggregate_senders(&rows);
    let last_at = rows.iter().map(|row| row.timestamp).max().unwrap_or(0);

    Json(json!({
        "ok": true,
        "days": days,
        "since": since,
        "scope": scope.clone().unwrap_or_else(|| "all".to_string()),
        "summary": {
            "total": total,
            "success": success,
            "failed": failed,
            "success_rate": rollcall_success_rate(success, total),
            "courses": courses.len(),
            "last_at": last_at,
        },
        "courses": courses,
        "senders": senders,
    }))
    .into_response()
}

/// `GET /admin/api/rollcalls/records`：逐筆送出紀錄（新的在前），含送出者。
pub(super) async fn admin_rollcall_records_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminRollcallQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_optional_admin_text(
        query.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let scope = match resolve_rollcall_scope(&session, query.account_id.as_deref()) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let (days, since) = rollcall_window(query.days);
    let limit = query
        .limit
        .unwrap_or(ROLLCALL_RECORDS_DEFAULT_LIMIT)
        .clamp(1, ROLLCALL_RECORDS_MAX_LIMIT);

    let mut rows = match state
        .accounts_db
        .list_rollcall_events(scope.as_deref(), since, limit)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            warn!(%error, "讀取點名送出紀錄失敗");
            return database_error("讀取點名紀錄失敗，請稍後再試");
        }
    };
    mask_other_accounts(&mut rows, &session);

    let records: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "timestamp": row.timestamp,
                "account_id": row.account_id,
                "username": row.username,
                "user_name": row.user_name,
                "sender": row.sender_label(),
                "rollcall_id": row.rollcall_id,
                "course_name": row.course_name,
                "attendance_type": row.attendance_type,
                "success": row.is_success(),
                "result": row.result,
            })
        })
        .collect();

    Json(json!({
        "ok": true,
        "days": days,
        "since": since,
        "scope": scope.unwrap_or_else(|| "all".to_string()),
        "limit": limit,
        "count": records.len(),
        "records": records,
    }))
    .into_response()
}
