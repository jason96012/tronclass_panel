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
                        obj.insert("_course_id".into(), json!(id));
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

// BULLETIN_READ_V1: 「已閱讀」每人獨立、全部帳號都讀過則全域刪除。
// 存於 config/bulletin_reads.json（免改 DB schema）。
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct BulletinReadStore {
    #[serde(default)]
    reads: std::collections::HashMap<String, Vec<String>>,
    #[serde(default)]
    deleted: Vec<String>,
}

fn bulletin_reads_file() -> std::path::PathBuf {
    std::path::Path::new("config/bulletin_reads.json").to_path_buf()
}

static BULLETIN_READ_STORE: once_cell::sync::Lazy<
    std::sync::Mutex<BulletinReadStore>,
> = once_cell::sync::Lazy::new(|| {
    let mut store = BulletinReadStore::default();
    if let Ok(text) = std::fs::read_to_string(bulletin_reads_file()) {
        if let Ok(parsed) = serde_json::from_str::<BulletinReadStore>(&text) {
            store = parsed;
        }
    }
    std::sync::Mutex::new(store)
});

fn persist_bulletin_reads(store: &BulletinReadStore) {
    let path = bulletin_reads_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string(store) {
        if let Err(e) = std::fs::write(path, s) {
            tracing::warn!("無法寫入 bulletin_reads.json：{e}");
        }
    }
}

// ADMIN_BULLETIN_V1: 管理員發佈的公告（獨立於 Tronclass 課程公告），對所有使用者顯示。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct AdminBulletin {
    pub id: u64,
    pub title: String,
    pub content: String,
    pub created: i64,
    pub author: String,
}

fn admin_bulletins_file() -> std::path::PathBuf {
    std::path::Path::new("config/admin_bulletins.json").to_path_buf()
}

static ADMIN_BULLETINS: once_cell::sync::Lazy<std::sync::Mutex<Vec<AdminBulletin>>> =
    once_cell::sync::Lazy::new(|| {
        let mut v = Vec::new();
        if let Ok(text) = std::fs::read_to_string(admin_bulletins_file()) {
            if let Ok(list) = serde_json::from_str::<Vec<AdminBulletin>>(&text) {
                v = list;
            }
        }
        std::sync::Mutex::new(v)
    });

fn persist_admin_bulletins(list: &[AdminBulletin]) {
    let path = admin_bulletins_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(list) {
        if let Err(e) = std::fs::write(path, s) {
            tracing::warn!("無法寫入 admin_bulletins.json：{e}");
        }
    }
}

/// 穩定 key：優先 Tronclass bulletin id，否則用 course+title+time 雜湊。
fn bulletin_key(b: &Value) -> String {
    let course_id = b.get("_course_id").and_then(|v| v.as_u64()).unwrap_or(0);
    if let Some(id) = b
        .get("id")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok())))
    {
        return format!("c{course_id}-b{id}");
    }
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    pick_str(b, &["title", "name", "subject"]).hash(&mut h);
    pick_str(b, &["content", "body", "text", "description"]).hash(&mut h);
    pick_ts(b).hash(&mut h);
    format!("c{course_id}-h{}", h.finish())
}

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

    let (deleted_set, read_by_me) = {
        let store = BULLETIN_READ_STORE.lock().unwrap();
        let me = session.account_id.clone();
        let reads = store
            .reads
            .iter()
            .map(|(k, v)| (k.clone(), v.contains(&me)))
            .collect::<std::collections::HashMap<String, bool>>();
        (store.deleted.clone(), reads)
    };

    let course_items: Vec<Value> = raw
        .into_iter()
        .map(|b| {
            let key = bulletin_key(&b);
            let course_name = b
                .get("_course_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            json!({
                "key": key,
                "course_name": course_name,
                "title": pick_str(&b, &["title", "name", "subject"]),
                "content": pick_str(&b, &["content", "body", "text", "description"]),
                "author": pick_str(&b, &["author", "user", "creator", "instructor", "publisher"]),
                "time": pick_str(&b, &["publish_time", "published_at", "created_at", "create_time", "updated_at", "created"]),
                "pinned": b.get("is_top").or_else(|| b.get("top")).or_else(|| b.get("pinned")).and_then(|v| v.as_bool()).unwrap_or(false),
                "ts": pick_ts(&b),
                "read": read_by_me.get(&key).copied().unwrap_or(false),
            })
        })
        .filter(|it| {
            let k = it.get("key").and_then(|v| v.as_str()).unwrap_or_default();
            !deleted_set.iter().any(|d| d.as_str() == k)
        })
        .collect();

    let admin_items: Vec<Value> = ADMIN_BULLETINS
        .lock()
        .map(|list| {
            list.iter()
                .map(|b| {
                    let key = format!("admin-{}", b.id);
                    json!({
                        "key": key,
                        "course_name": "管理員公告",
                        "title": b.title,
                        "content": b.content,
                        "author": b.author,
                        "time": b.created.to_string(),
                        "pinned": true,
                        "ts": b.created,
                        "read": read_by_me.get(&key).copied().unwrap_or(false),
                        "source": "admin",
                        "id": b.id,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let mut items = admin_items;
    items.extend(course_items);

    if let Ok(mut cache) = BULLETINS_CACHE.lock() {
        *cache = Some((account.id.clone(), now, items.clone()));
    }

    (
        StatusCode::OK,
        Json(json!({ "items": items, "is_admin": session.is_admin })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct BulletinReadRequest {
    #[serde(default)]
    key: String,
}

// BULLETIN_READ_V1: 標記已閱讀（每人獨立）；全部啟用帳號都讀過則全域刪除。
pub(super) async fn read_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(body): Json<BulletinReadRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    let key = body.key.trim().to_string();
    if key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid key" })),
        )
            .into_response();
    }

    let enabled: Vec<String> = state
        .accounts_db
        .list()
        .await
        .map(|l| l.into_iter().filter(|a| a.enabled).map(|a| a.id).collect())
        .unwrap_or_default();

    let mut deleted_now = false;
    if let Ok(mut store) = BULLETIN_READ_STORE.lock() {
        let entry = store.reads.entry(key.clone()).or_insert_with(Vec::new);
        if !entry.contains(&session.account_id) {
            entry.push(session.account_id.clone());
        }
        // 管理員公告由管理員手動管理生命週期，不套用「全讀即刪」。
        if !key.starts_with("admin-")
            && !enabled.is_empty()
            && enabled.iter().all(|id| entry.contains(id))
            && !store.deleted.contains(&key)
        {
            store.deleted.push(key.clone());
            deleted_now = true;
        }
        persist_bulletin_reads(&store);
    }

    (StatusCode::OK, Json(json!({ "ok": true, "deleted": deleted_now }))).into_response()
}

// BULLETIN_PUSH_V1: 公告通知測試（設定頁「測試通知」面板），走真實公告推播路徑。
pub(super) async fn test_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    let key = format!("test-{}", now_secs());
    crate::web_push_notifications::notify_bulletin(
        &session.account_id,
        &key,
        "公告測試",
        "這是一則測試公告通知，點一下即可開啟課程公告頁。",
    );
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminBulletinCreateRequest {
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
}

// ADMIN_BULLETIN_V1: 管理員發佈公告（對所有使用者顯示）。
pub(super) async fn admin_create_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(body): Json<AdminBulletinCreateRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "僅管理員可發佈公告" })),
        )
            .into_response();
    }
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "標題不能為空" })),
        )
            .into_response();
    }
    let now = now_secs() as i64;
    let bulletin = AdminBulletin {
        id: now_secs(),
        title,
        content: body.content.trim().to_string(),
        created: now,
        author: session.account_id.clone(),
    };
    if let Ok(mut list) = ADMIN_BULLETINS.lock() {
        list.insert(0, bulletin.clone());
        persist_admin_bulletins(&list);
    }
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "id": bulletin.id })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminBulletinDeleteRequest {
    #[serde(default)]
    id: u64,
}

// ADMIN_BULLETIN_V1: 管理員刪除公告。
pub(super) async fn admin_delete_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(body): Json<AdminBulletinDeleteRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "僅管理員可刪除公告" })),
        )
            .into_response();
    }
    if let Ok(mut list) = ADMIN_BULLETINS.lock() {
        let before = list.len();
        list.retain(|b| b.id != body.id);
        if list.len() != before {
            persist_admin_bulletins(&list);
        }
    }
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

// BULLETIN_PUSH_V1: 背景輪詢各啟用帳號的課程公告，有新公告即 Web Push 提醒。
// 已通知的 key 存 config/bulletin_notified.json；**首輪只記錄不發送**，避免啟動灌爆。
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct BulletinNotifiedStore {
    #[serde(default)]
    keys: Vec<String>,
}

fn bulletin_notified_file() -> std::path::PathBuf {
    std::path::Path::new("config/bulletin_notified.json").to_path_buf()
}

static BULLETIN_NOTIFIED: once_cell::sync::Lazy<std::sync::Mutex<Vec<String>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(Vec::new()));
static BULLETIN_NOTIFIED_WARM: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn save_notified(keys: &[String]) {
    let path = bulletin_notified_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string(&BulletinNotifiedStore {
        keys: keys.to_vec(),
    }) {
        let _ = std::fs::write(path, s);
    }
}

pub(super) fn start_bulletin_notifier(state: AdminWebState) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let path = bulletin_notified_file();
        let existed = path.exists();
        let mut keys = Vec::new();
        if existed {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(store) = serde_json::from_str::<BulletinNotifiedStore>(&text) {
                    keys = store.keys;
                }
            }
        }
        *BULLETIN_NOTIFIED.lock().unwrap() = keys;
        BULLETIN_NOTIFIED_WARM.store(existed, std::sync::atomic::Ordering::SeqCst);

        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(120));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                notify_check(&state).await;
            }
        });
    });
}

async fn notify_check(state: &AdminWebState) {
    let accounts = match state.accounts_db.list().await {
        Ok(l) => l,
        Err(_) => return,
    };
    let enabled: Vec<_> = accounts.into_iter().filter(|a| a.enabled).collect();
    if enabled.is_empty() {
        return;
    }
    let Some(config) = crate::config::try_get_config() else {
        return;
    };

    let mut current: Vec<(String, String, String, String)> = Vec::new();
    for raw in enabled {
        let Some(provider) = config.providers.get(&raw.provider) else {
            continue;
        };
        let account = crate::account::AccountConfig {
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
        };
        let Ok(client) = get_client(state, &account).await else {
            continue;
        };
        let base = account.base_url().to_string();
        let Ok(resp) = client.get(format!("{base}/api/my-courses")).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(cj) = resp.json::<Value>().await else {
            continue;
        };
        let courses = crate::api::todos::parse_course_briefs(&cj)
            .into_iter()
            .map(|c| (c.id, c.name))
            .collect();
        let items = fetch_all_bulletins(client, base, courses).await;
        for b in items {
            let key = bulletin_key(&b);
            let course = b
                .get("_course_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let title = pick_str(&b, &["title", "name", "subject"]);
            current.push((key, account.id.clone(), course, title));
        }
    }

    let mut guard = BULLETIN_NOTIFIED.lock().unwrap();
    let warm = BULLETIN_NOTIFIED_WARM.load(std::sync::atomic::Ordering::SeqCst);
    let mut keys = guard.clone();
    if !warm {
        for (k, _, _, _) in &current {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
        *guard = keys.clone();
        BULLETIN_NOTIFIED_WARM.store(true, std::sync::atomic::Ordering::SeqCst);
        save_notified(&keys);
        return;
    }
    let mut changed = false;
    for (key, account, course, title) in current {
        if !keys.contains(&key) {
            keys.push(key.clone());
            changed = true;
            crate::web_push_notifications::notify_bulletin(&account, &key, &course, &title);
        }
    }
    if changed {
        *guard = keys.clone();
        save_notified(&keys);
    }
}

// BULLETIN_TRANSLATE_V2: 公告翻譯。MyMemory（免金鑰、支援 zh-TW）為主、Google gtx 為備；
// 錯誤訊息帶回各服務失敗原因，方便診斷。
#[derive(Deserialize)]
pub(super) struct BulletinTranslateRequest {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tl: String,
}

fn contains_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        let u = c as u32;
        (0x3040..=0x30FF).contains(&u)
            || (0x3400..=0x9FFF).contains(&u)
            || (0xAC00..=0xD7AF).contains(&u)
    })
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = s.to_string();
    while out.len() > max {
        out.pop();
    }
    out.push('…');
    out
}

pub(super) async fn translate_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(body): Json<BulletinTranslateRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    if state.authorize_any(&token).await.is_none() {
        return unauthorized_response();
    }
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "沒有可翻譯的內容" })))
            .into_response();
    }
    if text.chars().count() > 5000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "內容過長（上限 5000 字）" })),
        )
            .into_response();
    }
    let cjk = contains_cjk(&text);
    let sl: &str = if cjk { "zh-CN" } else { "en" };
    let tl: String = match body.tl.trim() {
        "en" | "ja" | "ko" | "zh-TW" => body.tl.trim().to_string(),
        _ => {
            if cjk {
                "en".to_string()
            } else {
                "zh-TW".to_string()
            }
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) TronclassPanel/1.0")
        .build()
        .unwrap_or_default();
    let mut errors: Vec<String> = Vec::new();

    // 1) MyMemory（匿名免金鑰；單次上限約 500 bytes，截斷處理）
    let mm_q = truncate_bytes(&text, 480);
    let langpair = format!("{sl}|{tl}");
    match client
        .get("https://api.mymemory.translated.net/get")
        .query(&[("q", mm_q.as_str()), ("langpair", langpair.as_str())])
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                match resp.json::<Value>().await {
                    Ok(v) => {
                        let st = v
                            .get("responseStatus")
                            .and_then(|x| x.as_i64())
                            .unwrap_or(-1);
                        let s = v
                            .pointer("/responseData/translatedText")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if st == 200 && !s.is_empty() {
                            return (
                                StatusCode::OK,
                                Json(json!({ "translated": s, "tl": tl })),
                            )
                                .into_response();
                        }
                        errors.push(format!("MyMemory status={st}"));
                    }
                    Err(e) => errors.push(format!("MyMemory 解析失敗：{e}")),
                }
            } else {
                errors.push(format!("MyMemory HTTP {}", resp.status()));
            }
        }
        Err(e) => errors.push(format!("MyMemory 連線失敗：{e}")),
    }

    // 2) Google gtx 備援
    match client
        .get("https://translate.googleapis.com/translate_a/single")
        .query(&[
            ("client", "gtx"),
            ("sl", sl),
            ("tl", tl.as_str()),
            ("dt", "t"),
            ("q", text.as_str()),
        ])
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().is_success() {
                if let Ok(v) = resp.json::<Value>().await {
                    let mut out = String::new();
                    if let Some(arr) = v.get(0).and_then(|x| x.as_array()) {
                        for seg in arr {
                            if let Some(s) = seg.get(0).and_then(|x| x.as_str()) {
                                out.push_str(s);
                            }
                        }
                    }
                    if !out.trim().is_empty() {
                        return (
                            StatusCode::OK,
                            Json(json!({ "translated": out, "tl": tl })),
                        )
                            .into_response();
                    }
                    errors.push("Google 未回結果".to_string());
                } else {
                    errors.push("Google 解析失敗".to_string());
                }
            } else {
                errors.push(format!("Google HTTP {}", resp.status()));
            }
        }
        Err(e) => errors.push(format!("Google 連線失敗：{e}")),
    }

    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": format!("翻譯失敗：{}", errors.join("；")) })),
    )
        .into_response()
}
