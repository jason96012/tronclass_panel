//! RADAR_INPUT_PAGE_V1（fix71）：雷達點名「輸入座標」的輸入頁。
//!
//! 背景（使用者需求）：數字點名偵測到之後會生出一個帶 token 的輸入頁（`/number?token=…`），
//! 使用者在手機上輸入 4 位數字，送出後 fan-out 給同一個 rollcall 的所有等待帳號。
//! **雷達點名也要有等價的輸入頁**——只是輸入的不是數字，而是「座標」。
//!
//! 因此這條路有兩層：
//!
//! 1. **`/radar?provider=…&rollcall_id=…&account_id=…&token=…`**（見 `crate::adapters::scanner`）：
//!    與數字點名輸入頁一對一對應。點名一發生就註冊並把網址推進 LINE／Discord／Web Push 通知，
//!    頁面輪詢狀態、輸入座標、送出後 fan-out 給同一個 rollcall 的其他等待帳號。
//!    這一層不需要登入——誰點名誰輸入，權限靠 UUID token（與數字點名相同）。
//!
//! 2. **`/admin/radar/input`（這一頁）**：管理員的輸入頁。差別在於它認的是「登入身分」而不是
//!    token，所以可以用在 token 頁還沒被開啟、或需要代理輸入的情境：
//!    - 直接看到目前**正在等待輸入座標**的所有雷達點名（課程、剩餘時間、最後錯誤、已送出座標）
//!    - 輸入一組座標 → 送給指定的那場點名／該 provider 全部等待中的點名 → fan-out 行為與輸入頁完全相同
//!    - 可勾選「同時存進面板設定」，下次同一間教室直接命中（等同在雷達設定頁新增一組座標）
//!
//! 為什麼要第二層：雷達的輸入頁是第一層（token 頁），但使用者明確要求「有一個頁面給我輸入」
//! ——手機掉了、通知被吃掉、或想幫別台機器簽到時，仍然需要一個隨時打得開的入口。
//!
//! 權限：**僅管理員**（與雷達設定頁一致；座標是 provider 層級的全域資訊）。
//! 送出紀錄只留在記憶體（最近 [`HISTORY_MAX`] 筆），重啟即清空——它是操作歷程，不是資料。

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

use crate::adapters::requests::RadarPointInput;
use crate::radar_settings;

use super::{
    admin_login_redirect, html_page_response, must_change_password_redirect, unauthorized_response,
    validate_optional_admin_text, AdminQuery, AdminSession, AdminWebState,
    ADMIN_MAX_IDENTIFIER_BYTES,
};

const PAGE_HTML: &str = include_str!("static/radar_input.html");

/// 送出紀錄保留筆數（只在記憶體）。
const HISTORY_MAX: usize = 40;
/// 面板顯示的座標候選最多幾個（與 `radar_settings::MAX_RADAR_COORDS` 一致）。
const KNOWN_COORDS_MAX: usize = 20;

/// 一次「從這個輸入頁送出座標」的紀錄。
#[derive(Debug, Clone, Serialize)]
pub(super) struct RadarInputRecord {
    pub timestamp: i64,
    pub provider: String,
    pub rollcall_id: u64,
    pub course_name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub from_device: bool,
    pub delivered_count: usize,
    pub added_to_panel: bool,
    pub note: String,
}

/// 送出紀錄（最近在前）。
static HISTORY: Lazy<RwLock<Vec<RadarInputRecord>>> = Lazy::new(|| RwLock::new(Vec::new()));

#[derive(Debug, Deserialize)]
pub(super) struct RadarInputQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RadarInputRequest {
    #[serde(default)]
    provider: String,
    /// 座標；也可以只給 `coords`（「25.0373, 121.4315」或 Google Maps 貼上的字串）。
    #[serde(default)]
    latitude: f64,
    #[serde(default)]
    longitude: f64,
    #[serde(default)]
    coords: Option<String>,
    /// 定位精確度（公尺；只有裝置定位送來的座標才有）。
    #[serde(default)]
    accuracy: Option<f64>,
    /// 是否由裝置定位取得（顯示用）。
    #[serde(default)]
    from_device: bool,
    /// `waiting`（預設，該 provider 全部等待中的點名）、`rollcall`（只送指定點名）、`panel`（只存設定）。
    #[serde(default = "default_target")]
    target: String,
    #[serde(default)]
    rollcall_id: Option<u64>,
    /// 是否同時把這組座標存進該 provider 的面板設定。
    #[serde(default)]
    add_to_panel: bool,
}

fn default_target() -> String {
    "waiting".to_string()
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

/// 挑出「這次要看哪個 provider」：優先用查詢字串，其次是登入帳號所屬的 provider，最後第一個已知的。
async fn resolve_provider(
    state: &AdminWebState,
    session: &AdminSession,
    requested: Option<&str>,
) -> Option<String> {
    let known = radar_settings::known_providers().await;
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

/// 解析「25.0373, 121.4315」這種字串（允許全形逗號、空白、括號）。
fn parse_coord_text(text: &str) -> Option<(f64, f64)> {
    let cleaned = text
        .replace(['，', '、'], ",")
        .replace(['(', ')', '（', '）'], " ");
    let parts = cleaned
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if parts.len() != 2 {
        return None;
    }
    let latitude = parts[0].parse::<f64>().ok()?;
    let longitude = parts[1].parse::<f64>().ok()?;
    Some((latitude, longitude))
}

pub(super) async fn page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/radar/input");
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    if !page.session.is_admin {
        // 非管理員：與雷達設定頁一致，直接導回 Dashboard（前端不會出現入口）。
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
    Query(query): Query<RadarInputQuery>,
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

    let providers = radar_settings::known_providers().await;
    let Some(provider) = resolve_provider(&state, &session, query.provider.as_deref()).await else {
        return Json(json!({
            "providers": [],
            "provider": null,
            "known_coords": [],
            "baseline_coords": [],
            "effective_coords": [],
            "live": [],
            "history": [],
            "summary": { "waiting_rollcalls": 0, "waiting_accounts": 0, "sent": 0, "last_sent_at": null },
            "hint": "設定檔中沒有任何 provider 的雷達設定",
        }))
        .into_response();
    };

    // 這個 provider 的座標：設定檔基準值、面板覆寫、實際生效（含「我的位置」）。
    let baseline = radar_settings::baseline(&provider).await;
    let override_ = radar_settings::snapshot(&provider).await;
    let effective = radar_settings::effective(&provider).await;

    // 等待中的雷達點名（即時狀態；逾時或已結束的不會出現）。
    let live = match state.scanner.as_ref() {
        Some(registry) => registry.radar_live_entries(&provider).await,
        None => Vec::new(),
    };
    let waiting_accounts = live.iter().map(|entry| entry.pending_count).sum::<usize>();

    let history = {
        let records = HISTORY.read().await;
        records
            .iter()
            .filter(|record| record.provider == provider)
            .take(HISTORY_MAX)
            .cloned()
            .collect::<Vec<_>>()
    };
    let last_sent_at = history.iter().map(|record| record.timestamp).max();

    Json(json!({
        "providers": providers,
        "provider": provider,
        "known_coords": override_
            .as_ref()
            .map(|value| {
                value
                    .coords
                    .iter()
                    .take(KNOWN_COORDS_MAX)
                    .copied()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        "baseline_coords": baseline
            .as_ref()
            .map(|value| value.default_coords.clone())
            .unwrap_or_default(),
        "effective_coords": effective
            .as_ref()
            .map(|value| value.default_coords.clone())
            .unwrap_or_default(),
        "accuracy": override_
            .as_ref()
            .map(|value| value.accuracy)
            .or_else(|| baseline.as_ref().map(|value| value.accuracy))
            .unwrap_or(35),
        "has_panel_override": override_.is_some(),
        "scanner_enabled": state.scanner.is_some(),
        "live": live,
        "history": history,
        "summary": {
            "waiting_rollcalls": live.len(),
            "waiting_accounts": waiting_accounts,
            "sent": history.len(),
            "last_sent_at": last_sent_at,
        },
        "ranges": {
            "accuracy": [radar_settings::MIN_RADAR_ACCURACY, radar_settings::MAX_RADAR_ACCURACY],
            "altitude": [radar_settings::MIN_RADAR_ALTITUDE, radar_settings::MAX_RADAR_ALTITUDE],
            "max_coords": radar_settings::MAX_RADAR_COORDS,
        },
    }))
    .into_response()
}

pub(super) async fn post_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<RadarInputQuery>,
    Json(request): Json<RadarInputRequest>,
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
    let known = radar_settings::known_providers().await;
    if !known.iter().any(|name| name == &provider) {
        return error_response(StatusCode::BAD_REQUEST, "未知的 provider");
    }

    // ── 座標：`coords` 字串優先，其次 latitude／longitude ─────────────────────
    let (latitude, longitude) = match request.coords.as_deref().map(str::trim) {
        Some(text) if !text.is_empty() => match parse_coord_text(text) {
            Some(value) => value,
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "座標字串無法解析（範例：25.037300, 121.431500）",
                )
            }
        },
        _ => (request.latitude, request.longitude),
    };
    if !radar_settings::is_valid_coordinate(latitude, longitude) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "座標不正確：緯度需在 -90～90、經度在 -180～180，且不能是 (0, 0)",
        );
    }

    let point = RadarPointInput {
        latitude,
        longitude,
        accuracy: request.accuracy.filter(|value| value.is_finite() && *value > 0.0),
        from_device: request.from_device,
    };

    let target = request.target.trim().to_ascii_lowercase();
    let target = if target.is_empty() {
        "waiting".to_string()
    } else {
        target
    };
    if !matches!(target.as_str(), "waiting" | "rollcall" | "panel") {
        return error_response(
            StatusCode::BAD_REQUEST,
            "target 只接受 waiting／rollcall／panel",
        );
    }

    // ── 送給等待中的點名（fan-out 行為與 token 輸入頁完全相同）───────────────
    let mut delivered_count = 0usize;
    let mut targets: Vec<serde_json::Value> = Vec::new();
    let mut broadcast_rollcall_id = 0u64;
    if target != "panel" {
        let Some(registry) = state.scanner.as_ref() else {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "scanner 未啟用，無法把座標送給等待中的點名",
            );
        };
        let filter = if target == "rollcall" {
            match request.rollcall_id.filter(|value| *value > 0) {
                Some(value) => Some(value),
                None => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "target=rollcall 時必須指定 rollcall_id",
                    )
                }
            }
        } else {
            None
        };
        let result = registry
            .broadcast_radar_point(&provider, filter, point)
            .await;
        delivered_count = result.delivered_count;
        broadcast_rollcall_id = filter.unwrap_or_default();
        targets = result
            .targets
            .into_iter()
            .map(|item| {
                json!({
                    "rollcall_id": item.rollcall_id,
                    "course_name": item.course_name,
                    "delivered_count": item.delivered_count,
                })
            })
            .collect();

        if delivered_count == 0 && !request.add_to_panel {
            return error_response(
                StatusCode::CONFLICT,
                "目前沒有等待中的雷達點名（可能已結束或逾時），座標沒有送出",
            );
        }
    }

    // ── 同時存進面板設定 ───────────────────────────────────────────────────────
    let mut added_to_panel = false;
    // 兩條路（加入面板設定／只讀既有設定）都會在下面給值，所以這裡不做多餘的初始化。
    let panel_coords: Vec<[f64; 2]>;
    let mut note = String::new();
    if request.add_to_panel {
        let current = radar_settings::snapshot(&provider).await;
        let baseline = radar_settings::baseline(&provider).await;
        let (coords, accuracy, altitude, use_device_location, device_location, device_label, device_updated_at) =
            match current {
                Some(value) => (
                    value.coords,
                    value.accuracy,
                    value.altitude,
                    value.use_device_location,
                    value.device_location,
                    value.device_label,
                    value.device_updated_at,
                ),
                None => match baseline {
                    Some(base) => (
                        base.default_coords,
                        base.accuracy,
                        base.altitude,
                        false,
                        None,
                        String::new(),
                        0,
                    ),
                    None => (
                        Vec::new(),
                        radar_settings::clamp_accuracy(35),
                        radar_settings::clamp_altitude(0),
                        false,
                        None,
                        String::new(),
                        0,
                    ),
                },
            };

        let mut next_coords = coords.clone();
        next_coords.push([latitude, longitude]);
        let next_coords = radar_settings::sanitize_coords(next_coords);
        added_to_panel = next_coords.len() > coords.len();
        if added_to_panel {
            let accuracy = match point.accuracy {
                Some(value) => radar_settings::clamp_accuracy(value.round().clamp(1.0, 20_000.0) as u32),
                None => accuracy,
            };
            let (device_latitude, device_longitude) =
                radar_settings::clamp_device_location(device_location.map(|[lat, _]| lat), device_location.map(|[_, lon]| lon));
            if let Err(error) = state
                .accounts_db
                .set_radar_preference(
                    &provider,
                    &radar_settings::encode_coords(&next_coords),
                    accuracy,
                    altitude,
                    use_device_location,
                    device_latitude,
                    device_longitude,
                    &device_label,
                    device_updated_at,
                )
                .await
            {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &super::public_error("儲存面板座標失敗：", &error),
                );
            }
            radar_settings::set(
                &provider,
                radar_settings::RadarOverride {
                    coords: next_coords.clone(),
                    accuracy,
                    altitude,
                    use_device_location,
                    device_location,
                    device_label,
                    device_updated_at,
                    updated_at: chrono::Local::now().timestamp(),
                },
            )
            .await;
            note = format!(
                "座標已加入面板設定（目前 {} 組），下次同一間教室會直接嘗試",
                next_coords.len()
            );
        } else {
            note = "座標與面板設定中既有的點太接近，未重複加入".to_string();
        }
        panel_coords = next_coords;
    } else {
        panel_coords = radar_settings::snapshot(&provider)
            .await
            .map(|value| value.coords)
            .unwrap_or_default();
    }

    if note.is_empty() {
        if delivered_count > 0 {
            note = format!("已把座標送給 {delivered_count} 個等待中的帳號");
        } else if target == "panel" {
            note = "只更新面板設定，沒有送給任何點名".to_string();
        }
    }

    let record = RadarInputRecord {
        timestamp: chrono::Local::now().timestamp(),
        provider: provider.clone(),
        rollcall_id: broadcast_rollcall_id,
        course_name: targets
            .first()
            .and_then(|value| value.get("course_name"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        latitude,
        longitude,
        from_device: point.from_device,
        delivered_count,
        added_to_panel,
        note: crate::text_utils::truncate_chars(&note, 160).to_string(),
    };
    {
        let mut history = HISTORY.write().await;
        history.insert(0, record.clone());
        history.truncate(HISTORY_MAX);
    }

    Json(json!({
        "ok": true,
        "provider": provider,
        "latitude": latitude,
        "longitude": longitude,
        "from_device": point.from_device,
        "accuracy": point.accuracy,
        "target": target,
        "delivered_count": delivered_count,
        "targets": targets,
        "added_to_panel": added_to_panel,
        "panel_coords": panel_coords,
        "note": note,
        "record": record,
    }))
    .into_response()
}

