//! 學分／成績狀態 APIs（CREDITS_EMIS_V1）。
//!
//! 手動同步模式：使用者按下「立即同步」時呼叫 emis（SSO → TMWS080 →
//! TMWC120 → TMWS100），抓回的報告以 JSON 快照存進 DB，之後讀取不再
//! 依賴學校系統連線。與課表同步相同的隔離策略：全新 cookie jar、
//! 逾時控制，失敗不影響既有快照。

use super::*;
use serde::Deserialize;

/// 學分報告抓取逾時（SSO ＋ 情境 ＋ 三個頁面）。
const CREDITS_SCRAPE_TIMEOUT_SECS: u64 = 90;
/// 快照 JSON 大小上限（正常報告數十 KB；防異常頁面撐爆儲存）。
const MAX_CREDIT_REPORT_JSON_BYTES: usize = 512 * 1024;

#[derive(Debug, Deserialize)]
pub(super) struct AdminCreditsQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminCreditsSyncRunRequest {
    #[serde(default)]
    account_id: Option<String>,
}

/// 解析「要查哪個帳號」（與課表 API 相同規則：管理員可指定，一般使用者固定自己）。
fn resolve_credits_target(session: &AdminSession, requested: Option<String>) -> Result<String, Response> {
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

/// 讀出帳號設定（含學校憑證）供同步使用； viewer / 未設定 / 非 TKU 檢查。
async fn resolve_account_for_credits(
    state: &AdminWebState,
    account_id: &str,
) -> Result<crate::account::AccountConfig, Response> {
    let raw_account = match state.accounts_db.get(account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "找不到指定帳號" })),
            )
                .into_response());
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": super::public_error("讀取帳號失敗：", &e) })),
            )
                .into_response());
        }
    };

    if raw_account.is_viewer {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "檢視者帳號不會連線學校系統，無法同步學分資料" })),
        )
            .into_response());
    }

    let Some(config) = crate::config::try_get_config() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "伺服器設定尚未就緒，請稍後再試" })),
        )
            .into_response());
    };

    let Some(provider) = config.providers.get(&raw_account.provider) else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("未知的 Provider 設定：{}", raw_account.provider) })),
        )
            .into_response());
    };

    // 學分查詢目前只實作淡江 emis。
    if provider.kind != crate::config::ProviderKind::TKU {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "學分狀態查詢目前僅支援淡江大學（emis）帳號" })),
        )
            .into_response());
    }

    Ok(crate::account::AccountConfig {
        id: raw_account.id.clone(),
        provider: raw_account.provider.clone(),
        username: raw_account.username.clone(),
        password: raw_account.password.clone(),
        enabled: raw_account.enabled,
        line_user_id: raw_account.line_user_id.clone(),
        discord_user_id: raw_account.discord_user_id.clone(),
        session_cookie: raw_account.session_cookie.clone(),
        is_admin: raw_account.is_admin,
        provider_config: provider.clone(),
        request_timeout_secs: provider.api.request_timeout_secs,
    })
}

/// GET /admin/api/credits —— 讀取已存的學分報告快照。
pub(super) async fn admin_credits_report_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminCreditsQuery>,
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
    let target = match resolve_credits_target(&session, query.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };

    match state.accounts_db.get_credit_report(&target).await {
        Ok(Some((json, fetched_at))) => {
            // 快照應該一定是合法 JSON；防禦性解析，壞了就當沒資料。
            let report: serde_json::Value = match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(account = %target, error = %e, "學分報告快照 JSON 無法解析");
                    serde_json::Value::Null
                }
            };
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "account_id": target,
                    "fetched_at": fetched_at,
                    "report": report,
                })),
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "account_id": target,
                "fetched_at": 0,
                "report": serde_json::Value::Null,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": super::public_error("讀取學分報告失敗：", &e) })),
        )
            .into_response(),
    }
}

/// POST /admin/api/credits/sync/run —— 立即從 emis 抓取學分報告並覆寫快照。
pub(super) async fn admin_credits_sync_run_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminCreditsQuery>,
    body: axum::body::Bytes,
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
    let req: AdminCreditsSyncRunRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => AdminCreditsSyncRunRequest { account_id: None },
    };
    if let Err(response) = validate_optional_admin_text(
        req.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_credits_target(&session, req.account_id.or(query.account_id)) {
        Ok(target) => target,
        Err(resp) => return resp,
    };

    let account = match resolve_account_for_credits(&state, &target).await {
        Ok(account) => account,
        Err(resp) => return resp,
    };

    crate::logs::record_audit(
        "web",
        &session.account_id,
        "credits_sync_manual",
        Some(&target),
        None,
        "手動觸發學分狀態同步",
    );

    // 全新 cookie jar＋獨立 client：與課表同步相同的 session 隔離。
    let cookie_jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
    let client = match crate::api::build_http_client(
        std::sync::Arc::clone(&cookie_jar),
        CREDITS_SCRAPE_TIMEOUT_SECS,
    ) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": super::public_error("建立學分查詢 HTTP client 失敗：", &e) })),
            )
                .into_response();
        }
    };

    let fetch = crate::timetable_source::tku_credits::fetch_credit_report(&client, &cookie_jar, &account);
    let report = match tokio::time::timeout(
        std::time::Duration::from_secs(CREDITS_SCRAPE_TIMEOUT_SECS),
        fetch,
    )
    .await
    {
        Ok(Ok(report)) => report,
        Ok(Err(e)) => {
            // 錯誤訊息皆為自訂文字（已 redact 網址），不含憑證，可回給前端。
            // 沿用課表同步的 NO_5XX_FOR_UPSTREAM_FAILURE_V1：422 而非 5xx。
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": super::public_error("學分查詢失敗：", &e) })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": format!("學分查詢逾時（{CREDITS_SCRAPE_TIMEOUT_SECS}s）") })),
            )
                .into_response();
        }
    };

    // 合理性檢查：三頁全空代表流程或版面有問題，不寫入快照。
    if report.courses.is_empty() && report.required_courses.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "未從校務系統解析到任何學分資料，已保留原快照" })),
        )
            .into_response();
    }

    let fetched_at = chrono::Local::now().timestamp();
    let course_count = report.courses.len();
    let earned_total = report.earned_total;
    let student = report.student.clone();
    let report_json = match serde_json::to_string(&report) {
        Ok(j) => j,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": super::public_error("序列化學分報告失敗：", &e) })),
            )
                .into_response();
        }
    };
    if report_json.len() > MAX_CREDIT_REPORT_JSON_BYTES {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": format!("學分報告過大（{} bytes），不寫入", report_json.len()) })),
        )
            .into_response();
    }

    if let Err(e) = state
        .accounts_db
        .upsert_credit_report(&target, &report_json, fetched_at)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": super::public_error("儲存學分報告失敗：", &e) })),
        )
            .into_response();
    }

    tracing::info!(
        account = %target,
        courses = course_count,
        "學分狀態同步完成：{}（{} 科）",
        student.name,
        course_count
    );

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "account_id": target,
            "fetched_at": fetched_at,
            "courses": course_count,
            "earned_total": earned_total,
            "student": {
                "dept": student.dept,
                "student_no": student.student_no,
                "name": student.name,
            },
        })),
    )
        .into_response()
}
