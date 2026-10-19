//! ACCOUNT_SUSPEND_V1：帳號暫時停權（管理 API）。
//!
//! - GET  `/admin/api/suspensions`        ：列出帳號與生效中的停權（管理員或檢視者可讀）。
//! - POST `/admin/api/suspensions/set`    ：停權某帳號（僅管理員）。停權後該帳號所有 session 立即撤銷。
//! - POST `/admin/api/suspensions/clear`  ：提前解除停權（僅管理員）。
//!
//! 停權只擋「進入後台」（登入、session 驗證、refresh 輪換），不影響背景監控。
//! 到期時間一律以伺服器的 Unix 秒判斷；過期後列即失效，無需人工解除。

use super::*;

/// 停權最長 365 天。
const SUSPEND_MAX_SECS: i64 = 365 * 24 * 60 * 60;
/// 到期時間至少要比現在晚這麼久，避免「設了等於沒設」或時鐘誤差造成的瞬間過期。
const SUSPEND_MIN_SECS: i64 = 60;
const SUSPEND_REASON_MAX_CHARS: usize = 200;

/// 預設時長代號 → 秒數。`custom` 不在此表，由 `until` 指定。
fn preset_secs(name: &str) -> Option<i64> {
    match name {
        "1h" => Some(60 * 60),
        "1d" => Some(24 * 60 * 60),
        "3d" => Some(3 * 24 * 60 * 60),
        "7d" => Some(7 * 24 * 60 * 60),
        _ => None,
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn fmt_local(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| unix.to_string())
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminSuspensionSetRequest {
    account_id: String,
    /// `1h`／`1d`／`3d`／`7d`／`custom`
    duration: String,
    /// 自訂到期時間（Unix 秒）。`duration` 為 `custom` 時必填。
    #[serde(default)]
    until: Option<i64>,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminSuspensionClearRequest {
    account_id: String,
}

fn forbidden_admin_only() -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": "admin only" }))).into_response()
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

pub(super) async fn admin_suspensions_list_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin && !session.is_viewer {
        return forbidden_admin_only();
    }
    let now = now_secs();
    let accounts = match state.accounts_db.list().await {
        Ok(accounts) => accounts,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": super::public_error("讀取帳號失敗：", &error) })),
            )
                .into_response();
        }
    };
    let suspensions = match state.accounts_db.account_suspension_list_active(now).await {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": super::public_error("讀取停權列表失敗：", &error) })),
            )
                .into_response();
        }
    };
    // 只回傳必要欄位；密碼、雜湊、session cookie 一律不出現在回應中。
    let accounts: Vec<serde_json::Value> = accounts
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "provider": a.provider,
                "username": a.username,
                "is_admin": a.is_admin,
                "is_viewer": a.is_viewer,
                "enabled": a.enabled,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "now": now,
            "is_admin": session.is_admin,
            "self_account_id": session.account_id,
            "accounts": accounts,
            "suspensions": suspensions,
        })),
    )
        .into_response()
}

pub(super) async fn admin_suspensions_set_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Query(query): Query<AdminQuery>,
    Json(body): Json<AdminSuspensionSetRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return forbidden_admin_only();
    }

    let account_id = body.account_id.trim().to_string();
    if account_id.is_empty() {
        return bad_request("account_id is required");
    }
    if account_id.len() > ADMIN_MAX_IDENTIFIER_BYTES {
        return bad_request("account_id too long");
    }
    if account_id == session.account_id {
        return bad_request("不能停權自己");
    }

    let reason = body.reason.trim().to_string();
    if reason.chars().count() > SUSPEND_REASON_MAX_CHARS {
        return bad_request("停權原因過長（上限 200 字）");
    }

    let now = now_secs();
    let duration = body.duration.trim();
    let until = if duration == "custom" {
        match body.until {
            Some(until) => until,
            None => return bad_request("請指定自訂到期時間"),
        }
    } else {
        match preset_secs(duration) {
            Some(secs) => now + secs,
            None => return bad_request("不支援的停權時長"),
        }
    };
    if until < now + SUSPEND_MIN_SECS {
        return bad_request("到期時間必須晚於現在至少 1 分鐘");
    }
    if until > now + SUSPEND_MAX_SECS {
        return bad_request("最長只能停權 365 天");
    }

    match state.accounts_db.get(&account_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "account not found" })),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": super::public_error("讀取帳號失敗：", &error) })),
            )
                .into_response();
        }
    }

    if let Err(error) = state
        .accounts_db
        .account_suspension_set(&account_id, until, &reason, &session.account_id, now)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": super::public_error("設定停權失敗：", &error) })),
        )
            .into_response();
    }
    let _ = state.accounts_db.account_suspension_prune(now).await;

    // 立即登出：撤銷該帳號所有 DB 與記憶體中的 session。
    state.revoke_sessions_for_account(&account_id).await;
    super::status::invalidate_admin_status_cache().await;

    let until_label = fmt_local(until);
    let detail = if reason.is_empty() {
        format!("Web 管理頁暫時停權至 {until_label}，該帳號所有 session 已撤銷")
    } else {
        format!("Web 管理頁暫時停權至 {until_label}（原因：{reason}），該帳號所有 session 已撤銷")
    };
    crate::logs::record_audit(
        "web",
        &session.account_id,
        "account_suspend",
        Some(&account_id),
        None,
        detail.clone(),
    );
    let _ = state
        .accounts_db
        .record_activity_event(crate::db::ActivityEventInput {
            kind: "security".to_string(),
            actor: session.account_id.clone(),
            action: "account_suspend".to_string(),
            account_id: account_id.clone(),
            path: String::new(),
            result: "success".to_string(),
            details: detail,
            ip: ip.clone(),
            user_agent: String::new(),
        })
        .await;

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "account_id": account_id,
            "until": until,
            "reason": reason,
        })),
    )
        .into_response()
}

pub(super) async fn admin_suspensions_clear_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Query(query): Query<AdminQuery>,
    Json(body): Json<AdminSuspensionClearRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return forbidden_admin_only();
    }
    let account_id = body.account_id.trim().to_string();
    if account_id.is_empty() {
        return bad_request("account_id is required");
    }
    if account_id.len() > ADMIN_MAX_IDENTIFIER_BYTES {
        return bad_request("account_id too long");
    }

    let cleared = match state.accounts_db.account_suspension_clear(&account_id).await {
        Ok(cleared) => cleared,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": super::public_error("解除停權失敗：", &error) })),
            )
                .into_response();
        }
    };
    super::status::invalidate_admin_status_cache().await;

    if cleared {
        let detail = "Web 管理頁提前解除暫時停權".to_string();
        crate::logs::record_audit(
            "web",
            &session.account_id,
            "account_unsuspend",
            Some(&account_id),
            None,
            detail.clone(),
        );
        let _ = state
            .accounts_db
            .record_activity_event(crate::db::ActivityEventInput {
                kind: "security".to_string(),
                actor: session.account_id.clone(),
                action: "account_unsuspend".to_string(),
                account_id: account_id.clone(),
                path: String::new(),
                result: "success".to_string(),
                details: detail,
                ip: ip.clone(),
                user_agent: String::new(),
            })
            .await;
    }

    (
        StatusCode::OK,
        Json(json!({ "ok": true, "account_id": account_id, "cleared": cleared })),
    )
        .into_response()
}
