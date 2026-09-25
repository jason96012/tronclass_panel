//! RADAR_PANEL_V1（fix70）：雷達點名頁（面板）。
//!
//! 過去雷達設定只能改 `config.toml` 且要重啟；這頁把下列事情搬進面板：
//!
//! - **設定**：座標清單（有序、可增刪上下移）、`accuracy`、`altitude`（每個 provider 一組）
//! - **我的位置**：按鈕取得瀏覽器定位後，可選擇是否「列入雷達考量範圍」
//! - **即時狀態**：每個帳號最後一次雷達送出的結果
//! - **歷史紀錄**：最近的雷達送出紀錄（資料來源＝`rollcall_events`，與出席統計同一份）
//!
//! 權限：**僅管理員**。這是 provider 層級的全域設定（會影響同校所有帳號），
//! 且 `config.toml` 原本就只有維運者能改，因此讀與寫都要求管理員。
//!
//! 手機／電腦兩套版面由 `static/radar.html` 內的 `matchMedia('(max-width:820px)')` 切換
//! （與 `rest_days.html`、`files.html` 等既有頁面同一套慣例）。

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use super::{
    admin_login_redirect, html_page_response, must_change_password_redirect, unauthorized_response,
    validate_optional_admin_text, AdminQuery, AdminSession, AdminWebState,
    ADMIN_MAX_IDENTIFIER_BYTES,
};

const PAGE_HTML: &str = include_str!("static/radar.html");

/// 歷史紀錄最多回傳幾筆（面板桌面版顯示 20 筆、手機版 8 筆，其餘僅作為統計）。
const HISTORY_LIMIT: i64 = 120;
/// 從資料庫一次撈多少筆事件來篩選雷達紀錄（DB 端上限 5000）。
/// 全帳號的其他點名事件會一起被撈出來，所以刻意開大一點，讓雷達紀錄的涵蓋範圍夠長。
const EVENT_SCAN_LIMIT: i64 = 2000;
/// 統計範圍：近 N 天。
const SUMMARY_DAYS: i64 = 90;

#[derive(Debug, Deserialize)]
pub(super) struct RadarQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RadarUpdateRequest {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    coords: Vec<[f64; 2]>,
    #[serde(default = "default_accuracy")]
    accuracy: u32,
    #[serde(default)]
    altitude: i32,
    #[serde(default)]
    use_device_location: bool,
    /// 面板上取得的「我的位置」；`null` ＝ 尚未取得／已清除。
    #[serde(default)]
    device_location: Option<[f64; 2]>,
    #[serde(default)]
    device_label: Option<String>,
    /// `true` ＝ 清除面板覆寫，回到 `config.toml` 的設定。
    #[serde(default)]
    reset: bool,
}

fn default_accuracy() -> u32 {
    crate::radar_settings::clamp_accuracy(35)
}

/// 只允許管理員；非管理員一律 401（與其他設定頁一致）。
async fn require_admin(state: &AdminWebState, token: &str) -> Result<AdminSession, Response> {
    let Some(session) = state.authorize(token).await else {
        return Err(unauthorized_response());
    };
    if !session.is_admin {
        return Err(unauthorized_response());
    }
    Ok(session)
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// 挑出「這次要看哪個 provider」：優先用查詢字串，其次是目前登入帳號所屬的 provider，
/// 最後退回第一個已知 provider。
async fn resolve_provider(
    state: &AdminWebState,
    session: &AdminSession,
    requested: Option<&str>,
) -> Option<String> {
    let known = crate::radar_settings::known_providers().await;
    if let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) {
        if known.iter().any(|name| name.as_str() == requested) {
            return Some(requested.to_string());
        }
        return known.first().cloned();
    }
    if let Ok(Some(raw)) = state.accounts_db.get(&session.account_id).await {
        if known.iter().any(|name| name == &raw.provider) {
            return Some(raw.provider);
        }
    }
    known.first().cloned()
}

pub(super) async fn page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/radar");
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    if !page.session.is_admin {
        // 非管理員：這頁是全域設定，直接導回 Dashboard（前端不會出現入口）。
        return super::pages::attach_fresh_cookies(
            axum::response::Redirect::to("/admin").into_response(),
            &page,
            &headers,
        );
    }
    super::pages::attach_fresh_cookies(
        html_page_response(
            &headers,
            super::pages::inject_auth_attrs(
                &super::pages::inject_theme(PAGE_HTML),
                &page.session,
            ),
        ),
        &page,
        &headers,
    )
}

pub(super) async fn get_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<RadarQuery>,
) -> Response {
    let session = match require_admin(&state, &query.token).await {
        Ok(session) => session,
        Err(response) => return response,
    };
    if let Err(response) = validate_optional_admin_text(
        query.provider.as_deref(),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "provider",
    ) {
        return response;
    }

    let providers = crate::radar_settings::known_providers().await;
    let Some(provider) = resolve_provider(&state, &session, query.provider.as_deref()).await else {
        return Json(json!({
            "providers": [],
            "provider": null,
            "config": null,
            "accounts": [],
            "history": [],
            "summary": { "total": 0, "success": 0, "failed": 0, "last_success_at": null,
                          "days": SUMMARY_DAYS, "scanned": 0, "scanned_since": null },
            "hint": "設定檔中沒有任何 provider 的雷達設定",
        }))
        .into_response();
    };

    let baseline = crate::radar_settings::baseline(&provider).await;
    let override_ = crate::radar_settings::snapshot(&provider).await;
    let effective = crate::radar_settings::effective(&provider).await;

    let config = json!({
        "provider": provider,
        "coords": override_.as_ref().map(|o| o.coords.clone()).unwrap_or_default(),
        "accuracy": override_.as_ref().map(|o| o.accuracy)
            .or_else(|| baseline.as_ref().map(|b| b.accuracy)).unwrap_or(35),
        "altitude": override_.as_ref().map(|o| o.altitude)
            .or_else(|| baseline.as_ref().map(|b| b.altitude)).unwrap_or(0),
        "use_device_location": override_.as_ref().map(|o| o.use_device_location).unwrap_or(false),
        "device_location": override_.as_ref().and_then(|o| o.device_location),
        "device_label": override_.as_ref().map(|o| o.device_label.clone()).unwrap_or_default(),
        "device_updated_at": override_.as_ref().map(|o| o.device_updated_at).unwrap_or(0),
        "source": if override_.is_some() { "panel" } else { "config" },
        "updated_at": override_.as_ref().map(|o| o.updated_at).unwrap_or(0),
        "baseline_coords": baseline.as_ref().map(|b| b.default_coords.clone()).unwrap_or_default(),
        "baseline_accuracy": baseline.as_ref().map(|b| b.accuracy),
        "baseline_altitude": baseline.as_ref().map(|b| b.altitude),
        "effective_coords": effective.as_ref().map(|e| e.default_coords.clone()).unwrap_or_default(),
        "max_coords": crate::radar_settings::MAX_RADAR_COORDS,
        "accuracy_range": [crate::radar_settings::MIN_RADAR_ACCURACY, crate::radar_settings::MAX_RADAR_ACCURACY],
        "altitude_range": [crate::radar_settings::MIN_RADAR_ALTITUDE, crate::radar_settings::MAX_RADAR_ALTITUDE],
    });

    // ── 紀錄：與出席統計同一份資料（rollcall_events），只挑雷達 ──────────────
    let since = chrono::Local::now().timestamp() - SUMMARY_DAYS * 86_400;
    let events = match state
        .accounts_db
        .list_rollcall_events(None, since, EVENT_SCAN_LIMIT)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("讀取點名紀錄失敗：{error}"),
            )
        }
    };

    let accounts = match state.accounts_db.list().await {
        Ok(rows) => rows,
        Err(error) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("讀取帳號清單失敗：{error}"),
            )
        }
    };
    let provider_accounts: Vec<(String, String)> = accounts
        .into_iter()
        .filter(|account| account.provider == provider)
        .map(|account| (account.id, account.username))
        .collect();

    // 掃描範圍（面板會顯示「實際涵蓋到哪裡」，避免讓人以為統計是空的）。
    let scanned = events.len();
    let scanned_since = events.last().map(|row| row.timestamp);

    let mut radar_events = events
        .into_iter()
        .filter(|row| is_radar_row(&row.attendance_type))
        .collect::<Vec<_>>();

    // 每個帳號最後一次（清單是時間降冪，第一個遇到的就是最後一次）。
    let mut last_by_account: Vec<serde_json::Value> = Vec::new();
    for (account_id, username) in &provider_accounts {
        let last = radar_events.iter().find(|row| &row.account_id == account_id);
        last_by_account.push(json!({
            "account_id": account_id,
            "username": username,
            "last": last.map(|row| json!({
                "timestamp": row.timestamp,
                "course_name": row.course_name,
                "success": row.success != 0,
                "result": row.result,
                "user_name": row.user_name,
            })),
        }));
    }

    let total = radar_events.len();
    let success = radar_events.iter().filter(|row| row.success != 0).count();
    let last_success_at = radar_events
        .iter()
        .find(|row| row.success != 0)
        .map(|row| row.timestamp);

    radar_events.truncate(HISTORY_LIMIT as usize);
    let history: Vec<serde_json::Value> = radar_events
        .iter()
        .map(|row| {
            json!({
                "timestamp": row.timestamp,
                "account_id": row.account_id,
                "user_name": row.user_name,
                "course_name": row.course_name,
                "success": row.success != 0,
                "result": row.result,
                "rollcall_id": row.rollcall_id,
            })
        })
        .collect();

    Json(json!({
        "providers": providers,
        "provider": provider,
        "config": config,
        "accounts": last_by_account,
        "history": history,
        "summary": {
            "total": total,
            "success": success,
            "failed": total.saturating_sub(success),
            "last_success_at": last_success_at,
            "days": SUMMARY_DAYS,
            "scanned": scanned,
            "scanned_since": scanned_since,
        },
    }))
    .into_response()
}

pub(super) async fn post_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<RadarQuery>,
    Json(request): Json<RadarUpdateRequest>,
) -> Response {
    let _session = match require_admin(&state, &query.token).await {
        Ok(session) => session,
        Err(response) => return response,
    };

    let provider = request.provider.trim().to_string();
    if provider.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "缺少 provider");
    }
    if let Err(response) = validate_optional_admin_text(
        Some(&provider),
        ADMIN_MAX_IDENTIFIER_BYTES,
        "provider",
    ) {
        return response;
    }
    let known = crate::radar_settings::known_providers().await;
    if !known.iter().any(|name| name == &provider) {
        return error_response(StatusCode::BAD_REQUEST, "未知的 provider");
    }

    // ── 回到設定檔預設 ──────────────────────────────────────────────────────
    if request.reset {
        if let Err(error) = state.accounts_db.clear_radar_preference(&provider).await {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("清除雷達設定失敗：{error}"),
            );
        }
        crate::radar_settings::clear(&provider).await;
        return Json(json!({ "ok": true, "reset": true, "provider": provider })).into_response();
    }

    // ── 寫入（先驗證、再落地、最後才更新記憶體）─────────────────────────────
    let coords = crate::radar_settings::sanitize_coords(request.coords.clone());
    if request.coords.len() > crate::radar_settings::MAX_RADAR_COORDS {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "座標最多 {} 組",
                crate::radar_settings::MAX_RADAR_COORDS
            ),
        );
    }
    if coords.len() != request.coords.len() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "座標格式不正確：緯度需在 -90～90、經度在 -180～180，且不要放重複的點",
        );
    }

    let accuracy = crate::radar_settings::clamp_accuracy(request.accuracy);
    let altitude = crate::radar_settings::clamp_altitude(request.altitude);
    let (device_latitude, device_longitude) =
        crate::radar_settings::clamp_device_location(
            request.device_location.map(|[lat, _]| lat),
            request.device_location.map(|[_, lon]| lon),
        );
    if request.device_location.is_some() && device_latitude.is_none() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "「我的位置」座標不正確（緯度 -90～90、經度 -180～180）",
        );
    }
    let device_label = request
        .device_label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("未命名裝置")
        .to_string();
    let device_updated_at = if device_latitude.is_some() {
        chrono::Local::now().timestamp()
    } else {
        0
    };

    if let Err(error) = state
        .accounts_db
        .set_radar_preference(
            &provider,
            &crate::radar_settings::encode_coords(&coords),
            accuracy,
            altitude,
            request.use_device_location && device_latitude.is_some(),
            device_latitude,
            device_longitude,
            &device_label,
            device_updated_at,
        )
        .await
    {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("儲存雷達設定失敗：{error}"),
        );
    }

    let override_ = crate::radar_settings::RadarOverride {
        coords: coords.clone(),
        accuracy,
        altitude,
        use_device_location: request.use_device_location && device_latitude.is_some(),
        device_location: match (device_latitude, device_longitude) {
            (Some(lat), Some(lon)) => Some([lat, lon]),
            _ => None,
        },
        device_label: device_label.clone(),
        device_updated_at,
        updated_at: chrono::Local::now().timestamp(),
    };
    crate::radar_settings::set(&provider, override_.clone()).await;

    let effective = crate::radar_settings::effective(&provider).await;
    Json(json!({
        "ok": true,
        "provider": provider,
        "coords": coords,
        "accuracy": accuracy,
        "altitude": altitude,
        "use_device_location": override_.use_device_location,
        "device_location": override_.device_location,
        "effective_coords": effective.map(|value| value.default_coords).unwrap_or_default(),
    }))
    .into_response()
}

/// `attendance_type` 是不是雷達（目前寫入的是「雷達簽到」；寬鬆比對以免舊資料寫法不同）。
fn is_radar_row(attendance_type: &str) -> bool {
    attendance_type.contains("雷達") || attendance_type.eq_ignore_ascii_case("radar")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radar_rows_are_recognised() {
        assert!(is_radar_row("雷達簽到"));
        assert!(is_radar_row("radar"));
        assert!(is_radar_row("Radar"));
        assert!(!is_radar_row("數字簽到"));
        assert!(!is_radar_row("QR Code 簽到"));
        assert!(!is_radar_row("自主報到"));
    }
}
