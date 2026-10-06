//! Discord 與 Web Push 通知偏好設定 handlers（CHANNEL_PREFS_V1）。

use super::*;
// HOMEWORK_LADDER_V1：作業提醒階梯的限制常數（定義在 db::preferences）。
use crate::db::{
    DEFAULT_HOMEWORK_LADDER_HOURS as DEFAULT_LADDER, HOMEWORK_LADDER_MAX_HOURS as MAX_LADDER_HOURS,
    HOMEWORK_LADDER_MAX_STAGES as MAX_LADDER_STAGES,
    HOMEWORK_LADDER_MIN_HOURS as MIN_LADDER_HOURS,
};

#[derive(Debug, Deserialize)]
pub(super) struct AdminNotificationPreferencesQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminNotificationPreferenceInput {
    key: String,
    enabled: bool,
}

fn default_notification_channel() -> String {
    "discord".to_string()
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminNotificationPreferencesUpdateRequest {
    #[serde(default)]
    account_id: Option<String>,
    /// CHANNEL_PREFS_V1：要更新的渠道（"discord" / "push"）；省略時沿用舊行為＝Discord。
    #[serde(default = "default_notification_channel")]
    channel: String,
    preferences: Vec<AdminNotificationPreferenceInput>,
}

/// CHANNEL_PREFS_V1：通知偏好鍵（Discord 全量；Web Push 顯示其中的子集）。
/// 兩個渠道共用同一份鍵定義，偏好各自存在 notification_preferences
/// 的 channel="discord" / channel="push" 列。
const DISCORD_NOTIFICATION_KEYS: &[(&str, &str, &str)] = &[
    ("system_started", "系統啟動", "服務啟動或帳號監控開始時通知（僅 Discord）"),
    ("rollcall_detected", "偵測到點名", "偵測到新點名時通知"),
    ("number_code", "數字點名", "需要輸入數字點名碼時通知（含催促）"),
    ("qr_code", "QR Code 點名", "需要掃描或送出 QR Code 時通知（含催促）"),
    (
        "rollcall_finished",
        "點名完成結果",
        "點名成功或失敗結果通知",
    ),
    (
        "homework_deadline",
        "作業提醒",
        "作業截止倒數階梯提醒，以及新上架作業通知",
    ),
    ("bulletin", "課程與管理員公告", "新課程公告與管理員公告（含 AI 摘要）"),
    ("monitor_alert", "監控警示", "監控警示與恢復通知（管理員）"),
    ("security_alert", "安全警示", "連續密碼錯誤、異常位置登入等安全警示"),
    (
        "traffic_alert",
        "異常流量",
        "60 秒內嘗試次數超過上限的暫時封鎖通知（管理員）",
    ),
    (
        "geo_access_request",
        "國外存取申請",
        "國外 IP 被「僅限台灣」擋下、按「聯絡管理員」後的申請通知（管理員）",
    ),
    ("status", "狀態回覆", "主動推送狀態摘要通知（僅 Discord）"),
    (
        "control",
        "控制操作",
        "暫停、恢復、Force、Reauth 等控制通知（僅 Discord）",
    ),
    ("other", "其他通知", "其他 Discord 通知（僅 Discord）"),
];

/// CHANNEL_PREFS_V1：Web Push 有實際發送實體的鍵子集；
/// 上課提醒走「上課提醒卡」的獨立開關（class_reminder_preferences），不在此列。
const PUSH_NOTIFICATION_KEYS: &[&str] = &[
    "rollcall_detected",
    "number_code",
    "qr_code",
    "rollcall_finished",
    "homework_deadline",
    "bulletin",
    "monitor_alert",
    "security_alert",
    "traffic_alert",
    "geo_access_request",
];

pub(super) fn resolve_notification_target(
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
                Json(json!({ "error": "請指定 account_id" })),
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

pub(super) async fn admin_notification_preferences_get_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminNotificationPreferencesQuery>,
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
    let target = match resolve_notification_target(&session, query.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    // CHANNEL_PREFS_V1：一次回 Discord 與 Web Push 兩個渠道的偏好。
    let render_preferences = |saved: &std::collections::HashMap<String, bool>,
                              keys: &[(&str, &str, &str)]|
     -> Vec<serde_json::Value> {
        keys.iter()
            .map(|(key, label, description)| {
                json!({
                    "key": key,
                    "label": label,
                    "description": description,
                    "enabled": saved.get(*key).copied().unwrap_or(true),
                })
            })
            .collect()
    };

    let discord_rows = match state
        .accounts_db
        .list_notification_preferences(&target, "discord")
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("notification database error: {e}") })),
            )
                .into_response();
        }
    };
    let mut discord_saved = std::collections::HashMap::new();
    for row in discord_rows {
        discord_saved.insert(row.notification_key, row.enabled);
    }
    let discord_preferences = render_preferences(&discord_saved, DISCORD_NOTIFICATION_KEYS);

    let push_rows = match state
        .accounts_db
        .list_notification_preferences(&target, "push")
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("notification database error: {e}") })),
            )
                .into_response();
        }
    };
    let mut push_saved = std::collections::HashMap::new();
    for row in push_rows {
        push_saved.insert(row.notification_key, row.enabled);
    }
    // Web Push 只列有推送實體的鍵。
    let push_keys: Vec<(&str, &str, &str)> = DISCORD_NOTIFICATION_KEYS
        .iter()
        .filter(|(key, _, _)| PUSH_NOTIFICATION_KEYS.contains(key))
        .copied()
        .collect();
    let push_preferences = render_preferences(&push_saved, &push_keys);

    (
        StatusCode::OK,
        Json(json!({
            "account_id": target,
            "is_admin": session.is_admin,
            "channels": {
                "discord": {
                    "channel": "discord",
                    "preferences": discord_preferences,
                },
                "push": {
                    "channel": "push",
                    "preferences": push_preferences,
                },
            },
        })),
    )
        .into_response()
}

pub(super) async fn admin_notification_preferences_update_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    body: axum::body::Bytes,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    let req: AdminNotificationPreferencesUpdateRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_request" })),
            )
                .into_response();
        }
    };
    if req.preferences.len() > ADMIN_MAX_NOTIFICATION_PREFERENCES {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "too many notification preferences; max {}",
                    ADMIN_MAX_NOTIFICATION_PREFERENCES
                )
            })),
        )
            .into_response();
    }
    if let Err(response) = validate_optional_admin_text(
        req.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_notification_target(&session, req.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    // CHANNEL_PREFS_V1：依渠道決定允許的鍵（Discord 全量；push 僅子集）。
    let channel = req.channel.trim().to_string();
    let allowed: std::collections::HashSet<&str> = if channel == "push" {
        PUSH_NOTIFICATION_KEYS.iter().copied().collect()
    } else if channel == "discord" {
        DISCORD_NOTIFICATION_KEYS
            .iter()
            .map(|(key, _, _)| *key)
            .collect()
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown notification channel: {channel}") })),
        )
            .into_response();
    };
    let mut preferences = Vec::with_capacity(req.preferences.len());
    let mut seen = std::collections::HashSet::with_capacity(req.preferences.len());
    for pref in req.preferences {
        if let Err(response) =
            validate_admin_text(&pref.key, ADMIN_MAX_ACTION_BYTES, "notification key")
        {
            return response;
        }
        let key = pref.key.trim().to_string();
        if !seen.insert(key.clone()) {
            continue;
        }
        if !allowed.contains(key.as_str()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unknown notification key: {key}") })),
            )
                .into_response();
        }
        preferences.push((key, pref.enabled));
    }
    if let Err(e) = state
        .accounts_db
        .set_notification_preferences(&target, &channel, &preferences)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("save notification preference failed: {e}") })),
        )
            .into_response();
    }
    let channel_label = if channel == "push" {
        "Web Push"
    } else {
        "Discord"
    };
    crate::logs::record_audit(
        "web",
        &session.account_id,
        "notification_preferences",
        Some(&target),
        Some(channel.as_str()),
        &format!("更新 {channel_label} 通知設定"),
    );
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "account_id": target,
            "channel": channel,
        })),
    )
        .into_response()
}

// ─── 作業截止提醒階梯（HOMEWORK_LADDER_V1）──────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkLadderQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminHomeworkLadderUpdateRequest {
    #[serde(default)]
    account_id: Option<String>,
    /// 小時陣列（1～168、最多 8 個）。空陣列＝恢復預設 24/6/1。
    #[serde(default)]
    stages: Vec<u64>,
}

pub(super) async fn admin_homework_ladder_get_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminHomeworkLadderQuery>,
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
    let target = match resolve_notification_target(&session, query.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };
    match state
        .accounts_db
        .get_homework_reminder_ladder_raw(&target)
        .await
    {
        Ok(custom) => {
            let effective =
                custom.clone().unwrap_or_else(|| DEFAULT_LADDER.to_vec());
            (
                StatusCode::OK,
                Json(json!({
                    "account_id": target,
                    "stages": effective,
                    "use_default": custom.is_none(),
                    "default_stages": DEFAULT_LADDER,
                    "min_hours": MIN_LADDER_HOURS,
                    "max_hours": MAX_LADDER_HOURS,
                    "max_stages": MAX_LADDER_STAGES,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("homework ladder database error: {e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_homework_ladder_update_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    body: axum::body::Bytes,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    let req: AdminHomeworkLadderUpdateRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_request" })),
            )
                .into_response();
        }
    };
    if let Err(response) = validate_optional_admin_text(
        req.account_id.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "account_id",
    ) {
        return response;
    }
    let target = match resolve_notification_target(&session, req.account_id) {
        Ok(target) => target,
        Err(resp) => return resp,
    };

    // 驗證：最多 8 個階段、每個 1～168 小時。空陣列視為「恢復預設」。
    if req.stages.len() > MAX_LADDER_STAGES {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("too many stages; max {MAX_LADDER_STAGES}") })),
        )
            .into_response();
    }
    if let Some(&bad) = req
        .stages
        .iter()
        .find(|&&h| !(MIN_LADDER_HOURS..=MAX_LADDER_HOURS).contains(&h))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("stage {bad} out of range {MIN_LADDER_HOURS}..={MAX_LADDER_HOURS} hours") })),
        )
            .into_response();
    }

    match state
        .accounts_db
        .set_homework_reminder_ladder(&target, &req.stages)
        .await
    {
        Ok(saved) => {
            let effective = if saved.is_empty() {
                DEFAULT_LADDER.to_vec()
            } else {
                saved.clone()
            };
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "homework_ladder_settings",
                Some(&target),
                Some(&if saved.is_empty() {
                    "restore-default".to_string()
                } else {
                    serde_json::to_string(&saved).unwrap_or_default()
                }),
                "更新作業提醒階梯設定",
            );
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "account_id": target,
                    "stages": effective,
                    "use_default": saved.is_empty(),
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("save homework ladder failed: {e}") })),
        )
            .into_response(),
    }
}
