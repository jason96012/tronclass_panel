//! REST_DAY_PAGE_V1：「休息日與寒暑假」頁面。
//!
//! 顯示 REST_DAY_NO_POLL_V1 / SEMESTER_BREAK_V1 的實際生效內容：
//! 今天狀態、下一個上課日、內建國定假日、寒暑假區間、config/holidays.json 覆寫，
//! 以及一份逐日月曆。所有資料都來自 `crate::holidays::calendar_snapshot`，
//! 與監控主迴圈、晨間摘要共用同一套判定，不會各說各話。
//!
//! 任何已登入的 Dashboard 帳號都能看（純唯讀，不含個資）。

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Local, NaiveDate};
use serde::Deserialize;
use serde_json::json;

use super::{
    admin_login_redirect, html_page_response, must_change_password_redirect, session_token,
    unauthorized_response, AdminQuery, AdminWebState,
};

const PAGE_HTML: &str = include_str!("static/rest_days.html");

pub(super) async fn page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // ADMIN_PAGE_TRI_STATE_V1：access 過期但 refresh 有效時由伺服器端輪換，不再閃登入頁。
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/rest-days");
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    super::pages::attach_fresh_cookies(
        html_page_response(&headers, super::pages::inject_auth_attrs(&super::pages::inject_theme(PAGE_HTML), &page.session)),
        &page,
        &headers,
    )
}

pub(super) async fn get_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    let today = Local::now().date_naive();
    let mut snapshot = crate::holidays::calendar_snapshot(today);
    if let Some(map) = snapshot.as_object_mut() {
        map.insert(
            "server_time".to_string(),
            serde_json::Value::String(Local::now().format("%Y-%m-%d %H:%M:%S").to_string()),
        );
        // REST_DAY_EDIT_V1：休息日是全站設定（影響所有帳號），只有管理員能改。
        map.insert("can_edit".to_string(), serde_json::Value::Bool(session.is_admin));
    }
    Json(snapshot).into_response()
}

/// REST_DAY_EDIT_V1：頁面上點日期 → 設為不用上課／強制上課／清除。
#[derive(Debug, Deserialize)]
pub(super) struct DayOverrideRequest {
    #[serde(default)]
    date: String,
    /// REST_DAY_RANGE_V1：選填；有值時把 date..=end_date 整段一起套用（含頭尾）。
    #[serde(default)]
    end_date: String,
    /// "rest" | "work" | "clear"
    #[serde(default)]
    mode: String,
}

pub(super) async fn update_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(request): Json<DayOverrideRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "休息日設定會影響所有帳號，僅管理員可修改" })),
        )
            .into_response();
    }
    let Ok(date) = NaiveDate::parse_from_str(request.date.trim(), "%Y-%m-%d") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "日期格式須為 YYYY-MM-DD" })),
        )
            .into_response();
    };
    // REST_DAY_RANGE_V1：end_date 空白＝單日；有值就整段套用，from/to 顛倒會自動對調。
    let end_date = if request.end_date.trim().is_empty() {
        date
    } else {
        match NaiveDate::parse_from_str(request.end_date.trim(), "%Y-%m-%d") {
            Ok(end) => end,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "結束日期格式須為 YYYY-MM-DD" })),
                )
                    .into_response();
            }
        }
    };
    let (range_from, range_to) = if date <= end_date {
        (date, end_date)
    } else {
        (end_date, date)
    };
    let range_days = (range_to - range_from).num_days() + 1;
    if range_days > 366 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "一次最多只能套用 366 天" })),
        )
            .into_response();
    }
    let mode = match request.mode.trim() {
        "rest" => crate::holidays::DayOverride::Rest,
        "work" => crate::holidays::DayOverride::Work,
        "clear" => crate::holidays::DayOverride::Clear,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "mode 須為 rest / work / clear" })),
            )
                .into_response();
        }
    };
    if let Err(error) = crate::holidays::set_day_override_range(range_from, range_to, mode) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": super::public_error("寫入 config/holidays.json 失敗：", &error) })),
        )
            .into_response();
    }

    let today = Local::now().date_naive();
    // 今天被改成「上課」或「清除」時，叫醒所有監控迴圈立刻重新評估（不用等最多 30 分鐘）。
    // 改成「不用上課」則不叫醒——ForcePoll 會繞過休息日判定，叫醒反而多跑一次。
    let touches_today = range_from <= today && today <= range_to;
    if touches_today && mode != crate::holidays::DayOverride::Rest {
        for account in state.accounts.snapshot().iter() {
            account.force_poll_tx.notify_one();
        }
    }
    // /admin/api/status 的 rest_day 欄位有短快取，讓總覽提示立即更新。
    super::status::invalidate_admin_status_cache().await;

    let mode_label = match mode {
        crate::holidays::DayOverride::Rest => "rest",
        crate::holidays::DayOverride::Work => "work",
        crate::holidays::DayOverride::Clear => "clear",
    };
    let range_text = if range_days == 1 {
        range_from.format("%Y-%m-%d").to_string()
    } else {
        format!(
            "{}..{} ({range_days} 天)",
            range_from.format("%Y-%m-%d"),
            range_to.format("%Y-%m-%d")
        )
    };
    crate::logs::record_audit(
        "web",
        &session.account_id,
        "rest_day_override",
        None,
        Some(&format!("{range_text} mode={mode_label}")),
        "更新休息日覆寫（config/holidays.json）",
    );

    let snapshot_reason = crate::holidays::rest_day_reason(range_from);
    let base_message = match mode {
        crate::holidays::DayOverride::Rest => "已設為不用上課，當天不自動輪詢",
        crate::holidays::DayOverride::Work => "已設為強制上課，當天照常自動輪詢",
        crate::holidays::DayOverride::Clear => "已清除自訂，回到內建判定",
    };
    let message = if range_days == 1 {
        base_message.to_string()
    } else {
        format!("{base_message}（共 {range_days} 天）")
    };
    Json(json!({
        "ok": true,
        "date": range_from.format("%Y-%m-%d").to_string(),
        "end_date": range_to.format("%Y-%m-%d").to_string(),
        "days": range_days,
        "reason": snapshot_reason,
        "message": message,
    }))
    .into_response()
}
