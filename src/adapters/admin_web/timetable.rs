//! Manual timetable APIs for Admin Web.

use super::*;
use serde::Deserialize;

const MAX_TIMETABLE_ENTRIES: usize = 300;
const MAX_TIMETABLE_LIST_ALL_ENTRIES: usize = 5000;
const MAX_TIMETABLE_TEMPLATES: usize = 64;
const MAX_TIMETABLE_WEEKDAY_BYTES: usize = 32;
const MAX_TIMETABLE_TIME_BYTES: usize = 16;
const MAX_TIMETABLE_COURSE_BYTES: usize = 512;
const MAX_TIMETABLE_TEXT_BYTES: usize = 256;
const MAX_TIMETABLE_CODE_BYTES: usize = 128;
const MAX_TIMETABLE_TEMPLATE_NAME_BYTES: usize = 128;
const MAX_TIMETABLE_TEMPLATE_JSON_BYTES: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
pub(super) struct AdminTimetableQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdminTimetableEntryInput {
    #[serde(default)]
    account_id: Option<String>,
    weekday: String,
    #[serde(default)]
    period_start: Option<i64>,
    #[serde(default)]
    period_end: Option<i64>,
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default)]
    end_time: Option<String>,
    course_name: String,
    #[serde(default)]
    teacher: Option<String>,
    #[serde(default)]
    classroom: Option<String>,
    #[serde(default)]
    course_code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminTimetableImportRequest {
    #[serde(default)]
    account_id: Option<String>,
    entries: Vec<AdminTimetableEntryInput>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminTimetableClearRequest {
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminTimetableTemplateSaveRequest {
    name: String,
    #[serde(default)]
    description: Option<String>,
    entries: Vec<AdminTimetableEntryInput>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminTimetableTemplateApplyRequest {
    name: String,
    #[serde(default)]
    account_id: Option<String>,
}

fn normalize_timetable_weekday(value: &str) -> Option<String> {
    let v = value.trim().to_ascii_lowercase();
    let normalized = match v.as_str() {
        "mon" | "monday" | "1" | "一" | "週一" | "星期一" | "禮拜一" => "mon",
        "tue" | "tues" | "tuesday" | "2" | "二" | "週二" | "星期二" | "禮拜二" => "tue",
        "wed" | "wednesday" | "3" | "三" | "週三" | "星期三" | "禮拜三" => "wed",
        "thu" | "thur" | "thursday" | "4" | "四" | "週四" | "星期四" | "禮拜四" => "thu",
        "fri" | "friday" | "5" | "五" | "週五" | "星期五" | "禮拜五" => "fri",
        "sat" | "saturday" | "6" | "六" | "週六" | "星期六" | "禮拜六" => "sat",
        "sun" | "sunday" | "7" | "日" | "天" | "週日" | "週天" | "星期日" | "星期天" | "禮拜日"
        | "禮拜天" => "sun",
        _ => return None,
    };
    Some(normalized.to_string())
}

fn clean_timetable_text(value: Option<String>) -> String {
    value.unwrap_or_default().trim().to_string()
}

fn validate_timetable_entry(raw: &AdminTimetableEntryInput) -> Result<(), Response> {
    for (value, max_bytes, field) in [
        (
            raw.account_id.as_deref(),
            ADMIN_MAX_IDENTIFIER_BYTES,
            "entry account_id",
        ),
        (
            Some(raw.weekday.as_str()),
            MAX_TIMETABLE_WEEKDAY_BYTES,
            "weekday",
        ),
        (
            raw.start_time.as_deref(),
            MAX_TIMETABLE_TIME_BYTES,
            "start_time",
        ),
        (
            raw.end_time.as_deref(),
            MAX_TIMETABLE_TIME_BYTES,
            "end_time",
        ),
        (
            Some(raw.course_name.as_str()),
            MAX_TIMETABLE_COURSE_BYTES,
            "course_name",
        ),
        (raw.teacher.as_deref(), MAX_TIMETABLE_TEXT_BYTES, "teacher"),
        (
            raw.classroom.as_deref(),
            MAX_TIMETABLE_TEXT_BYTES,
            "classroom",
        ),
        (
            raw.course_code.as_deref(),
            MAX_TIMETABLE_CODE_BYTES,
            "course_code",
        ),
    ] {
        validate_optional_admin_text(value, max_bytes, field)?;
    }
    Ok(())
}

fn resolve_timetable_target(
    session: &AdminSession,
    requested: Option<String>,
) -> Result<String, Response> {
    if session.is_admin {
        let target = requested
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| session.account_id.clone());
        if target == "<token-admin>" {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "管理員 token 請先選擇 account_id" })),
            )
                .into_response());
        }
        Ok(target)
    } else {
        if let Some(requested) = requested
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            if requested != session.account_id {
                return Err(unauthorized_response());
            }
        }
        Ok(session.account_id.clone())
    }
}

fn timetable_entry_inputs_to_db(
    raw_entries: Vec<AdminTimetableEntryInput>,
    source: &str,
) -> Result<Vec<crate::db::TimetableEntryInput>, Response> {
    if raw_entries.len() > MAX_TIMETABLE_ENTRIES {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("too many timetable entries; max {MAX_TIMETABLE_ENTRIES}")
            })),
        )
            .into_response());
    }
    let mut entries = Vec::with_capacity(raw_entries.len());
    for raw in raw_entries {
        validate_timetable_entry(&raw)?;
        let weekday = match normalize_timetable_weekday(&raw.weekday) {
            Some(weekday) => weekday,
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("invalid weekday: {}", raw.weekday) })),
                )
                    .into_response());
            }
        };
        let course_name = raw.course_name.trim().to_string();
        if course_name.is_empty() {
            continue;
        }
        let start = raw.period_start.unwrap_or(0).clamp(0, 30);
        let end = raw.period_end.unwrap_or(start).clamp(start, 30);
        entries.push(crate::db::TimetableEntryInput {
            weekday,
            period_start: start,
            period_end: end,
            start_time: clean_timetable_text(raw.start_time),
            end_time: clean_timetable_text(raw.end_time),
            course_name,
            teacher: clean_timetable_text(raw.teacher),
            classroom: clean_timetable_text(raw.classroom),
            course_code: clean_timetable_text(raw.course_code),
            source: source.to_string(),
        });
    }
    if entries.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no valid timetable entries" })),
        )
            .into_response());
    }
    Ok(entries)
}

pub(super) async fn admin_timetable_list_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminTimetableQuery>,
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

    let requested_account_id = query
        .account_id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    let selected_account_id = if session.is_admin {
        requested_account_id.or_else(|| {
            if session.account_id == "<token-admin>" {
                None
            } else {
                Some(session.account_id.clone())
            }
        })
    } else {
        if let Some(requested) = requested_account_id.as_deref() {
            if requested != session.account_id {
                return unauthorized_response();
            }
        }
        Some(session.account_id.clone())
    };

    let result = if let Some(account_id) = selected_account_id.as_deref() {
        state
            .accounts_db
            .list_timetable_entries_for_account(account_id)
            .await
    } else {
        // Only emergency token without account_id falls back to all entries.
        state.accounts_db.list_all_timetable_entries().await
    };

    match result {
        Ok(mut entries) => {
            let max_entries = if selected_account_id.is_some() {
                MAX_TIMETABLE_ENTRIES
            } else {
                MAX_TIMETABLE_LIST_ALL_ENTRIES
            };
            let truncated = entries.len() > max_entries;
            entries.truncate(max_entries);
            (
                StatusCode::OK,
                Json(json!({
                    "entries": entries,
                    "truncated": truncated,
                    "current_account_id": session.account_id,
                    "selected_account_id": selected_account_id,
                    "is_admin": session.is_admin,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("timetable database error: {e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_timetable_import_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(req): Json<AdminTimetableImportRequest>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_optional_admin_text(
        req.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_timetable_target(&session, req.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    if req.entries.len() > MAX_TIMETABLE_ENTRIES {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("too many timetable entries; max {MAX_TIMETABLE_ENTRIES}")
            })),
        )
            .into_response();
    }

    let mut entries = Vec::with_capacity(req.entries.len());
    for raw in req.entries {
        if let Err(response) = validate_timetable_entry(&raw) {
            return response;
        }
        let weekday = match normalize_timetable_weekday(&raw.weekday) {
            Some(weekday) => weekday,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("invalid weekday: {}", raw.weekday) })),
                )
                    .into_response();
            }
        };
        let course_name = raw.course_name.trim().to_string();
        if course_name.is_empty() {
            continue;
        }
        let start = raw.period_start.unwrap_or(0).clamp(0, 30);
        let end = raw.period_end.unwrap_or(start).clamp(start, 30);
        entries.push(crate::db::TimetableEntryInput {
            weekday,
            period_start: start,
            period_end: end,
            start_time: clean_timetable_text(raw.start_time),
            end_time: clean_timetable_text(raw.end_time),
            course_name,
            teacher: clean_timetable_text(raw.teacher),
            classroom: clean_timetable_text(raw.classroom),
            course_code: clean_timetable_text(raw.course_code),
            source: "manual".to_string(),
        });
    }
    if entries.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no valid timetable entries" })),
        )
            .into_response();
    }

    match state
        .accounts_db
        .replace_timetable_entries(&target, entries)
        .await
    {
        Ok(count) => {
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "timetable_manual_import",
                Some(&target),
                Some(&format!("entries={count}")),
                "手動輸入課表",
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "account_id": target, "count": count })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("import timetable failed: {e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_timetable_clear_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(req): Json<AdminTimetableClearRequest>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_optional_admin_text(
        req.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_timetable_target(&session, req.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    match state.accounts_db.clear_timetable_entries(&target).await {
        Ok(count) => {
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "timetable_clear",
                Some(&target),
                Some(&format!("deleted={count}")),
                "清除課表",
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "account_id": target, "deleted": count })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("clear timetable failed: {e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_timetable_templates_list_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(_session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    match state.accounts_db.list_timetable_templates().await {
        Ok(mut templates) => {
            let truncated = templates.len() > MAX_TIMETABLE_TEMPLATES;
            templates.truncate(MAX_TIMETABLE_TEMPLATES);
            (
                StatusCode::OK,
                Json(json!({ "templates": templates, "truncated": truncated })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("template database error: {e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_timetable_template_save_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(req): Json<AdminTimetableTemplateSaveRequest>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }
    if let Err(response) = validate_admin_text(
        &req.name,
        MAX_TIMETABLE_TEMPLATE_NAME_BYTES,
        "template name",
    ) {
        return response;
    }
    if let Err(response) = validate_optional_admin_text(
        req.description.as_deref(),
        ADMIN_MAX_DETAILS_BYTES,
        "template description",
    ) {
        return response;
    }
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "template name is required" })),
        )
            .into_response();
    }
    let entries = match timetable_entry_inputs_to_db(req.entries, "template") {
        Ok(entries) => entries,
        Err(resp) => return resp,
    };
    let entries_json = match serde_json::to_string(
        &entries
            .iter()
            .map(|e| {
                json!({
                    "weekday": e.weekday,
                    "period_start": e.period_start,
                    "period_end": e.period_end,
                    "start_time": e.start_time,
                    "end_time": e.end_time,
                    "course_name": e.course_name,
                    "teacher": e.teacher,
                    "classroom": e.classroom,
                    "course_code": e.course_code,
                })
            })
            .collect::<Vec<_>>(),
    ) {
        Ok(json) => json,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("serialize template failed: {e}") })),
            )
                .into_response();
        }
    };
    if entries_json.len() > MAX_TIMETABLE_TEMPLATE_JSON_BYTES {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "serialized template is too large" })),
        )
            .into_response();
    }
    match state
        .accounts_db
        .save_timetable_template(
            &name,
            req.description.as_deref().unwrap_or(""),
            &entries_json,
            &session.account_id,
        )
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "name": name, "count": entries.len() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("save template failed: {e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_timetable_template_apply_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(req): Json<AdminTimetableTemplateApplyRequest>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_admin_text(
        &req.name,
        MAX_TIMETABLE_TEMPLATE_NAME_BYTES,
        "template name",
    ) {
        return response;
    }
    if let Err(response) = validate_optional_admin_text(
        req.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_timetable_target(&session, req.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    let Some(template) = (match state.accounts_db.get_timetable_template(&req.name).await {
        Ok(template) => template,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("template database error: {e}") })),
            )
                .into_response();
        }
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "template not found" })),
        )
            .into_response();
    };
    if template.entries_json.len() > MAX_TIMETABLE_TEMPLATE_JSON_BYTES {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "stored template is too large" })),
        )
            .into_response();
    }
    let raw_entries =
        match serde_json::from_str::<Vec<AdminTimetableEntryInput>>(&template.entries_json) {
            Ok(entries) => entries,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("template parse failed: {e}") })),
                )
                    .into_response();
            }
        };
    let entries =
        match timetable_entry_inputs_to_db(raw_entries, &format!("template:{}", template.name)) {
            Ok(entries) => entries,
            Err(resp) => return resp,
        };
    match state.accounts_db.replace_timetable_entries(&target, entries).await {
        Ok(count) => (StatusCode::OK, Json(json!({ "ok": true, "account_id": target, "template": template.name, "count": count }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("apply template failed: {e}") })),
        )
            .into_response(),
    }
}
