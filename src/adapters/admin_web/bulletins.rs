//! 課程公告頁（Tronclass bulletins）後端。
//!
//! 透過已登入 dashboard 帳號的 Tronclass session，先取 `/api/my-courses`，
//! 再平行（concurrency 8）抓取每門課的 `/api/courses/{id}/bulletins`，
//! 彙整成單一清單回給前端。公告為課程層級；無公告時回空陣列。

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    admin_login_redirect, html_page_response, must_change_password_redirect, session_token,
    unauthorized_response, AdminQuery, AdminSession, AdminWebState,
};

const PAGE_HTML: &str = include_str!("static/bulletins.html");

const BULLETINS_CONCURRENCY: usize = 8;
const BULLETINS_CACHE_TTL_SECS: u64 = 120;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Deserialize)]
pub(super) struct BulletinsQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
}

pub(super) async fn page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return admin_login_redirect("/admin/bulletins");
    };
    if session.must_change_password {
        return must_change_password_redirect();
    }
    html_page_response(&headers, super::pages::inject_theme(PAGE_HTML))
}

/// 從 dashboard session 解析出可呼叫 Tronclass 的 AccountConfig。
async fn resolve_account(
    state: &AdminWebState,
    session: &AdminSession,
    requested: Option<&str>,
) -> Result<crate::account::AccountConfig, Response> {
    let requested = requested.map(str::trim).filter(|v| !v.is_empty());
    let target = if session.is_admin {
        requested.unwrap_or(&session.account_id)
    } else {
        if let Some(r) = requested {
            if r != session.account_id {
                return Err(unauthorized_response());
            }
        }
        &session.account_id
    };

    let mut raw = state.accounts_db.get(target).await.ok().flatten();
    if raw.is_none() {
        if let Ok(list) = state.accounts_db.find_by_username(target, None).await {
            raw = list.into_iter().next();
        }
    }
    if raw.is_none() && session.is_admin {
        if let Ok(list) = state.accounts_db.list().await {
            raw = list.into_iter().find(|a| a.enabled);
        }
    }
    let raw = raw.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "找不到指定帳號，請至帳號設定確認" })),
        )
            .into_response()
    })?;
    if !raw.enabled {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "該帳號目前已被停用" })),
        )
            .into_response());
    }
    let config = crate::config::try_get_config().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "伺服器設定尚未就緒，請稍後再試" })),
        )
            .into_response()
    })?;
    let provider = config.providers.get(&raw.provider).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("未知的 Provider 設定：{}", raw.provider) })),
        )
            .into_response()
    })?;

    Ok(crate::account::AccountConfig {
        id: raw.id.clone(),
        provider: raw.provider.clone(),
        username: raw.username.clone(),
        password: raw.password.clone(),
        enabled: raw.enabled,
        line_user_id: raw.line_user_id.clone(),
        discord_user_id: raw.discord_user_id.clone(),
        session_cookie: raw.session_cookie.clone(),
        is_admin: raw.is_admin,
        provider_config: provider.clone(),
        request_timeout_secs: provider.api.request_timeout_secs,
    })
}

/// 取得可呼叫 Tronclass 的 client：優先已存 session cookie，認證失敗才重新登入。
async fn get_client(
    state: &AdminWebState,
    account: &crate::account::AccountConfig,
) -> Result<reqwest::Client, Response> {
    if let Some(client) = super::homework::build_client_from_saved_cookie(account) {
        return Ok(client);
    }
    let (auth_client, session) = match crate::auth::AuthClient::new(account).await {
        Ok(v) => v,
        Err(e) => {
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("Tronclass 登入失敗：{e}") })),
            )
                .into_response());
        }
    };
    let _ = state
        .accounts_db
        .set_session_cookie(&account.id, &session.cookie_string)
        .await;
    Ok(auth_client.client)
}

fn pick_str(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(val) = v.get(*k) {
            if let Some(s) = val.as_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            } else if let Some(obj) = val.as_object() {
                for nk in ["name", "title", "text"] {
                    if let Some(n) = obj.get(nk).and_then(|x| x.as_str()) {
                        if !n.is_empty() {
                            return n.to_string();
                        }
                    }
                }
            }
        }
    }
    String::new()
}

fn pick_ts(v: &Value) -> i64 {
    let s = pick_str(
        v,
        &[
            "publish_time",
            "published_at",
            "created_at",
            "create_time",
            "updated_at",
            "updated",
            "created",
            "time",
        ],
    );
    if s.is_empty() {
        return 0;
    }
    if let Ok(n) = s.parse::<f64>() {
        if n > 1e9 {
            return if n > 1e12 { (n / 1000.0) as i64 } else { n as i64 };
        }
    }
    chrono::DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

/// 平行抓取所有課程的 bulletins 並附上課程名稱。
async fn fetch_all_bulletins(
    client: reqwest::Client,
    base_url: String,
    courses: Vec<(u64, String)>,
) -> Vec<Value> {
    use futures::StreamExt;
    let out = futures::stream::iter(courses.into_iter().map(|(id, name)| {
        let client = client.clone();
        let base = base_url.clone();
        async move {
            let url = format!("{base}/api/courses/{id}/bulletins");
            let Ok(resp) = client.get(&url).send().await else {
                return Vec::<Value>::new();
            };
            if !resp.status().is_success() {
                return Vec::<Value>::new();
            }
            let Ok(val) = resp.json::<Value>().await else {
                return Vec::<Value>::new();
            };
            let arr = val
                .get("bulletins")
                .and_then(|b| b.as_array())
                .cloned()
                .unwrap_or_default();
            arr.into_iter()
                .map(|mut b| {
                    if let Some(obj) = b.as_object_mut() {
                        obj.insert("_course_name".into(), json!(name));
                    }
                    b
                })
                .collect::<Vec<_>>()
        }
    }))
    .buffer_unordered(BULLETINS_CONCURRENCY)
    .collect::<Vec<Vec<Value>>>()
    .await;
    out.into_iter().flatten().collect()
}

// BULLETINS_CACHE_V1: 短 TTL 快取，避免重整時重複打 33 門課的 Tronclass。
static BULLETINS_CACHE: once_cell::sync::Lazy<
    std::sync::Mutex<Option<(String, u64, Vec<Value>)>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

pub(super) async fn list_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<BulletinsQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };

    let account = match resolve_account(&state, &session, query.account_id.as_deref()).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };

    let now = now_secs();
    if let Ok(cache) = BULLETINS_CACHE.lock() {
        if let Some((key, at, items)) = cache.as_ref() {
            if *key == account.id && now.saturating_sub(*at) <= BULLETINS_CACHE_TTL_SECS {
                return (StatusCode::OK, Json(json!({ "items": items }))).into_response();
            }
        }
    }

    let client = match get_client(&state, &account).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let base_url = account.base_url().to_string();

    let courses_url = format!("{base_url}/api/my-courses");
    let Ok(resp) = client.get(&courses_url).send().await else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "取得課程清單失敗" })),
        )
            .into_response();
    };
    if !resp.status().is_success() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "取得課程清單失敗（Tronclass 認證可能已失效）" })),
        )
            .into_response();
    }
    let courses_json = match resp.json::<Value>().await {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "解析課程清單失敗" })),
            )
                .into_response()
        }
    };
    let briefs = crate::api::todos::parse_course_briefs(&courses_json);
    let courses: Vec<(u64, String)> = briefs
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect();

    let raw = fetch_all_bulletins(client, base_url, courses).await;

    let items: Vec<Value> = raw
        .into_iter()
        .map(|b| {
            let course_name = b
                .get("_course_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            json!({
                "course_name": course_name,
                "title": pick_str(&b, &["title", "name", "subject"]),
                "content": pick_str(&b, &["content", "body", "text", "description"]),
                "author": pick_str(&b, &["author", "user", "creator", "instructor", "publisher"]),
                "time": pick_str(&b, &["publish_time", "published_at", "created_at", "create_time", "updated_at", "created"]),
                "pinned": b.get("is_top").or_else(|| b.get("top")).or_else(|| b.get("pinned")).and_then(|v| v.as_bool()).unwrap_or(false),
                "ts": pick_ts(&b),
            })
        })
        .collect();

    if let Ok(mut cache) = BULLETINS_CACHE.lock() {
        *cache = Some((account.id.clone(), now, items.clone()));
    }

    (StatusCode::OK, Json(json!({ "items": items }))).into_response()
}
