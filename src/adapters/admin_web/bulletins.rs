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

// BULLETIN_TRANSLATE_V5: 公告翻譯，統一使用 Google（gtx）；
// 錯誤訊息帶回失敗原因，方便診斷。
#[derive(Deserialize)]
pub(super) struct BulletinTranslateRequest {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tl: String,
}

// BULLETIN_TRANSLATE_V3
// 問題：公告內容是 iClass 回傳的 HTML 片段（例如 `<p>…</p>`）。舊流程把整段
// HTML 直接丟給翻譯 API，回來的字串仍帶標籤（部分服務還會把標籤轉成
// `&lt;p&gt;` 實體），前端再以 textContent 呈現，畫面上就出現字面的
// `<p>…</p>`。
// 解法：翻譯「前」把來源轉純文字，翻譯「後」再清一次（服務可能自行加回
// 標籤或實體），兩道都做，任一端有殘留都不會外流到 UI。

/// 需要轉成換行的區塊級標籤。
const BLOCK_LEVEL_TAGS: &[&str] = &[
    "br", "p", "div", "li", "tr", "ul", "ol", "table", "section", "article", "blockquote", "pre",
    "h1", "h2", "h3", "h4", "h5", "h6", "hr",
];

/// 去除 HTML 標籤：區塊標籤轉換行，`script`/`style` 連同內容整段丟棄。
/// 只有 `<` 後面接字母或 `/` 才視為標籤，避免把「a < b」這種文字吃掉。
fn strip_html_tags(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            let ch = input[i..].chars().next().unwrap_or('\u{fffd}');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        if lower[i..].starts_with("<!--") {
            match lower[i..].find("-->") {
                Some(off) => i += off + 3,
                None => i = bytes.len(),
            }
            continue;
        }

        // `<` 之後必須是字母、`/` 或 `!`，否則當一般字元。
        let next = bytes.get(i + 1).copied();
        let looks_like_tag = matches!(next, Some(b) if b.is_ascii_alphabetic() || b == b'/' || b == b'!');
        if !looks_like_tag {
            out.push('<');
            i += 1;
            continue;
        }

        let Some(rel_end) = lower[i..].find('>') else {
            // 沒有收尾角括號：剩下的當純文字。
            out.push_str(&input[i..]);
            break;
        };
        let tag_end = i + rel_end;
        let inner = &lower[i + 1..tag_end];
        let is_closing = inner.starts_with('/');
        let name: String = inner
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();

        if !is_closing && (name == "script" || name == "style") {
            let close = format!("</{name}");
            match lower[tag_end..].find(&close) {
                Some(off) => {
                    let close_at = tag_end + off;
                    match lower[close_at..].find('>') {
                        Some(gt) => i = close_at + gt + 1,
                        None => i = bytes.len(),
                    }
                }
                None => i = bytes.len(),
            }
            continue;
        }

        if BLOCK_LEVEL_TAGS.contains(&name.as_str()) {
            out.push('\n');
        }
        i = tag_end + 1;
    }

    out
}

/// 解碼常見 HTML 實體（含數值實體）；未知實體原樣保留。
fn decode_html_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        // 實體最長只看 12 bytes（`&#x1F600;` 之類都夠）。
        // 用 char_indices 找 `;`，避免在多位元組字元中間切片而 panic。
        let semicolon = tail
            .char_indices()
            .take_while(|(idx, _)| *idx < 12)
            .find(|(_, ch)| *ch == ';')
            .map(|(idx, _)| idx);
        let Some(end) = semicolon else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded: Option<String> = match entity.to_ascii_lowercase().as_str() {
            "lt" => Some("<".into()),
            "gt" => Some(">".into()),
            "amp" => Some("&".into()),
            "quot" => Some("\"".into()),
            "apos" => Some("'".into()),
            "nbsp" => Some(" ".into()),
            "ndash" => Some("–".into()),
            "mdash" => Some("—".into()),
            "hellip" => Some("…".into()),
            "middot" => Some("·".into()),
            "lsquo" => Some("‘".into()),
            "rsquo" => Some("’".into()),
            "ldquo" => Some("“".into()),
            "rdquo" => Some("”".into()),
            other => {
                if let Some(hex) = other.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .map(|c| c.to_string())
                } else if let Some(dec) = other.strip_prefix('#') {
                    dec.parse::<u32>()
                        .ok()
                        .and_then(char::from_u32)
                        .map(|c| c.to_string())
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(value) => {
                out.push_str(&value);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }

    out.push_str(rest);
    out
}

/// 折疊空白：每行去頭尾空白、連續空白併成一個、最多保留一個空行。
fn collapse_plain_text(input: &str) -> String {
    let normalized = input.replace('\r', "\n");
    let mut lines: Vec<String> = Vec::new();

    for raw_line in normalized.split('\n') {
        let mut line = String::with_capacity(raw_line.len());
        let mut pending_space = false;
        for ch in raw_line.chars() {
            if ch.is_whitespace() {
                pending_space = true;
                continue;
            }
            if pending_space && !line.is_empty() {
                line.push(' ');
            }
            pending_space = false;
            line.push(ch);
        }
        if line.is_empty() {
            if lines.last().is_some_and(|last| !last.is_empty()) {
                lines.push(String::new());
            }
        } else {
            lines.push(line);
        }
    }

    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    lines.join("\n")
}

/// HTML 片段 → 純文字（可安全地用 textContent 呈現）。
/// 先去標籤、再解實體；若解實體後又冒出標籤（`&lt;p&gt;` 這種二次編碼），
/// 再清一輪。
pub(super) fn to_plain_text(input: &str) -> String {
    let stripped = strip_html_tags(input);
    let decoded = decode_html_entities(&stripped);
    let decoded = if decoded.contains('<') {
        strip_html_tags(&decoded)
    } else {
        decoded
    };
    collapse_plain_text(&decoded)
}

fn contains_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        let u = c as u32;
        (0x3040..=0x30FF).contains(&u)
            || (0x3400..=0x9FFF).contains(&u)
            || (0xAC00..=0xD7AF).contains(&u)
    })
}

// BULLETIN_TRANSLATE_V5
// 翻譯來源統一為 Google（translate.googleapis.com，client=gtx），不再使用 MyMemory
// —— MyMemory 匿名端點單次上限只有約 500 bytes，舊版直接截斷送出，長公告的譯文
// 永遠停在中途。
// Google gtx 沒有這種硬上限，但走的是 GET，query 百分比編碼後仍受 URL 長度約束，
// 所以照樣分段：依「段落 → 句子 → 硬切」拆成安全長度，記住每段後面的原始分隔
// 字串，逐段翻譯（有限併發、保持順序）後依序拼回，結構與原文完全一致。

/// 單次送出的最大 bytes。中文百分比編碼會膨脹約 3 倍
/// （1200 → 約 3.6 KB query），遠低於常見的 8 KB URL 上限。
const TRANSLATE_CHUNK_BYTES: usize = 1200;
/// 片段數上限，避免一則超長公告打出上百個外部請求。
const TRANSLATE_MAX_CHUNKS: usize = 32;
/// 併發翻譯數；太高容易被 Google 擋。
const TRANSLATE_CONCURRENCY: usize = 3;
/// 單段失敗時的重試次數（gtx 偶發 429/5xx）。
const TRANSLATE_RETRIES: usize = 2;

/// 可以斷句的句尾標點（中英文皆含）。
fn is_sentence_end(ch: char) -> bool {
    matches!(
        ch,
        '.' | '!' | '?' | ';' | ':' | '。' | '！' | '？' | '；' | '：' | '，' | '、'
    )
}

/// 把一行拆成 (片段, 片段後的原始分隔字串)：優先在句尾斷開，
/// 單句仍超過上限時才在字元邊界硬切（絕不切在多位元組字元中間）。
fn split_sentences(line: &str, max_bytes: usize) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        cur.push(ch);
        if is_sentence_end(ch) {
            // 句尾後面的空白歸給「分隔字串」，翻譯時不會被送出去。
            let mut sep = String::new();
            while let Some(next) = chars.peek() {
                if next.is_whitespace() {
                    sep.push(*next);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push((std::mem::take(&mut cur), sep));
            continue;
        }
        if cur.len() >= max_bytes {
            out.push((std::mem::take(&mut cur), String::new()));
        }
    }
    if !cur.is_empty() {
        out.push((cur, String::new()));
    }
    if out.is_empty() {
        out.push((String::new(), String::new()));
    }
    out
}

/// 貪婪合併相鄰片段（含中間的分隔字串），把請求數壓到最少。
fn merge_translation_units(
    units: Vec<(String, String)>,
    max_bytes: usize,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (piece, sep) in units {
        if let Some((last_text, last_sep)) = out.last_mut() {
            if last_text.len() + last_sep.len() + piece.len() <= max_bytes {
                let carried = std::mem::take(last_sep);
                last_text.push_str(&carried);
                last_text.push_str(&piece);
                *last_sep = sep;
                continue;
            }
        }
        out.push((piece, sep));
    }
    out
}

/// 把整段文字拆成可安全送出的片段清單。
/// 回傳 `(片段, 片段後面的原始分隔字串)`，依序拼回即為完整原文結構。
fn split_for_translation(text: &str, max_bytes: usize) -> Vec<(String, String)> {
    if text.len() <= max_bytes {
        return vec![(text.to_string(), String::new())];
    }

    let mut units: Vec<(String, String)> = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let last_index = lines.len().saturating_sub(1);

    for (index, line) in lines.iter().enumerate() {
        let sep = if index == last_index { "" } else { "\n" };
        if line.len() <= max_bytes {
            units.push(((*line).to_string(), sep.to_string()));
            continue;
        }
        let mut pieces = split_sentences(line, max_bytes);
        if let Some(last_piece) = pieces.last_mut() {
            // 附加而非覆寫：split_sentences 可能已把行尾空白收進 sep，
            // 覆寫會讓拼回去的結果少掉那些字元。
            last_piece.1.push_str(sep);
        }
        units.extend(pieces);
    }

    merge_translation_units(units, max_bytes)
}

/// Google gtx 單段翻譯（唯一來源）。內含重試。
async fn translate_once_google(
    client: &reqwest::Client,
    q: &str,
    sl: &str,
    tl: &str,
) -> Result<String, String> {
    let mut last_error = String::new();
    for attempt in 0..=TRANSLATE_RETRIES {
        if attempt > 0 {
            // 線性退避，避免立刻再撞上同一個 429。
            tokio::time::sleep(std::time::Duration::from_millis(400 * attempt as u64)).await;
        }
        match google_translate_request(client, q, sl, tl).await {
            Ok(value) => return Ok(value),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

async fn google_translate_request(
    client: &reqwest::Client,
    q: &str,
    sl: &str,
    tl: &str,
) -> Result<String, String> {
    let resp = client
        .get("https://translate.googleapis.com/translate_a/single")
        .query(&[
            ("client", "gtx"),
            ("sl", sl),
            ("tl", tl),
            ("dt", "t"),
            ("q", q),
        ])
        .send()
        .await
        .map_err(|e| format!("Google 連線失敗：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Google HTTP {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|_| "Google 解析失敗".to_string())?;
    let mut out = String::new();
    if let Some(arr) = v.get(0).and_then(|x| x.as_array()) {
        for seg in arr {
            if let Some(s) = seg.get(0).and_then(|x| x.as_str()) {
                out.push_str(s);
            }
        }
    }
    let out = to_plain_text(&out);
    if out.is_empty() {
        Err("Google 未回結果".to_string())
    } else {
        Ok(out)
    }
}

pub(super) async fn translate_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(body): Json<BulletinTranslateRequest>,
) -> Response {
    use futures::StreamExt;

    let token = session_token(&headers, &query.token);
    if state.authorize_any(&token).await.is_none() {
        return unauthorized_response();
    }
    // BULLETIN_TRANSLATE_V3：來源一律先轉純文字，翻譯服務就不會拿到／回吐標籤。
    let text = to_plain_text(&body.text);
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

    // BULLETIN_TRANSLATE_V5：分段送 Google，確保長公告完整翻譯、不被截斷。
    let units = split_for_translation(&text, TRANSLATE_CHUNK_BYTES);
    if units.len() > TRANSLATE_MAX_CHUNKS {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "內容過長，無法一次翻譯（請分段）" })),
        )
            .into_response();
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) TronclassPanel/1.0")
        .build()
        .unwrap_or_default();
    // FnOnce-NOT-GENERAL-ENOUGH-FIX
    // 這裡刻意先 clone 成「擁有所有權」的 Vec<String> 再 into_iter()。
    // 若直接 `units.iter().map(|(piece, _)| async move { … })`，閉包參數是
    // `&'a (String, String)`，回傳的 async block 型別會綁死在那個生命週期上，
    // rustc 無法把它推成 higher-ranked 的 `FnOnce`，就會報
    // 「implementation of `FnOnce` is not general enough」。
    let pieces: Vec<String> = units.iter().map(|(piece, _)| piece.clone()).collect();

    // `buffered` 會保持順序，拼回去才不會亂序。
    let results: Vec<Result<String, String>> = futures::stream::iter(pieces.into_iter().map(|piece| {
        let client = client.clone();
        let tl = tl.clone();
        async move {
            if piece.trim().is_empty() {
                return Ok(piece);
            }
            translate_once_google(&client, &piece, sl, &tl).await
        }
    }))
    .buffered(TRANSLATE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut assembled = String::with_capacity(text.len() + 64);
    let mut errors: Vec<String> = Vec::new();
    let mut translated_count = 0usize;

    for ((piece, sep), result) in units.iter().zip(results.into_iter()) {
        match result {
            Ok(value) => {
                if !piece.trim().is_empty() {
                    translated_count += 1;
                }
                assembled.push_str(&value);
            }
            Err(error) => {
                // 單段失敗就保留原文，不讓整篇翻譯化為烏有。
                if errors.len() < 3 {
                    errors.push(error);
                }
                assembled.push_str(piece);
            }
        }
        assembled.push_str(sep);
    }

    if translated_count == 0 {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("翻譯失敗：{}", errors.join("；")) })),
        )
            .into_response();
    }

    let translated = collapse_plain_text(&assembled);
    let partial = !errors.is_empty();
    (
        StatusCode::OK,
        Json(json!({
            "translated": translated,
            "tl": tl,
            "chunks": units.len(),
            "partial": partial,
            "note": if partial {
                Value::String(format!("有 {} 段翻譯失敗，該段保留原文（{}）", errors.len(), errors.join("；")))
            } else {
                Value::Null
            },
        })),
    )
        .into_response()
}

// BULLETIN_TRANSLATE_V3 的回歸測試：翻譯欄位絕不能出現 HTML 標籤。
#[cfg(test)]
mod translate_plain_text_tests {
    use super::to_plain_text;

    #[test]
    fn strips_paragraph_tags_from_bulletin_html() {
        assert_eq!(
            to_plain_text("<p>Week 18 assignment is now open on iClass.</p>"),
            "Week 18 assignment is now open on iClass."
        );
    }

    #[test]
    fn strips_double_encoded_tags_returned_by_translators() {
        assert_eq!(
            to_plain_text("&lt;p&gt;第18週作業現已在iClass上開放。&lt;/p&gt;"),
            "第18週作業現已在iClass上開放。"
        );
    }

    #[test]
    fn block_tags_and_breaks_become_newlines() {
        // `</p><p>` 之間保留一個空行（段落分隔），`<br>` 只換一行。
        assert_eq!(
            to_plain_text("<p>第一段</p><p>第二段<br>換行</p>"),
            "第一段\n\n第二段\n換行"
        );
    }

    #[test]
    fn drops_script_and_style_content() {
        assert_eq!(
            to_plain_text("<style>p{color:red}</style>hi<script>alert(1)</script>"),
            "hi"
        );
    }

    #[test]
    fn keeps_plain_text_and_bare_angle_brackets() {
        assert_eq!(to_plain_text("a < b and c > d"), "a < b and c > d");
    }

    #[test]
    fn decodes_common_entities_and_collapses_whitespace() {
        assert_eq!(
            to_plain_text("Tom&nbsp;&amp;&nbsp;Jerry\n\n\n  said &quot;hi&quot;  "),
            "Tom & Jerry\n\nsaid \"hi\""
        );
    }

    #[test]
    fn numeric_entities_are_decoded() {
        assert_eq!(to_plain_text("&#39;A&#x42;&#39;"), "'AB'");
    }
}

// BULLETIN_TRANSLATE_V4 的回歸測試：長文必須被拆開，且拼回去要等於原文。
#[cfg(test)]
mod translate_chunking_tests {
    use super::{split_for_translation, TRANSLATE_CHUNK_BYTES};

    fn rejoin(units: &[(String, String)]) -> String {
        units
            .iter()
            .map(|(piece, sep)| format!("{piece}{sep}"))
            .collect::<String>()
    }

    #[test]
    fn short_text_is_a_single_chunk() {
        let units = split_for_translation("Hello world.", TRANSLATE_CHUNK_BYTES);
        assert_eq!(units.len(), 1);
        assert_eq!(rejoin(&units), "Hello world.");
    }

    #[test]
    fn long_text_is_split_and_rejoins_exactly() {
        let paragraph = "Curved scores are published on Moodle. \
The computed curved score appears as curved_calc. \
The curved total may differ due to revisions. ";
        let text = paragraph.repeat(12);
        let units = split_for_translation(&text, TRANSLATE_CHUNK_BYTES);
        assert!(units.len() > 1, "長文必須被拆成多段");
        assert_eq!(rejoin(&units), text, "拼回去必須和原文一模一樣");
    }

    #[test]
    fn every_chunk_stays_under_the_upstream_limit() {
        let text = "第一段落。".repeat(300);
        let units = split_for_translation(&text, TRANSLATE_CHUNK_BYTES);
        for (piece, _) in &units {
            assert!(
                piece.len() <= TRANSLATE_CHUNK_BYTES + 8,
                "片段 {} bytes 超過上限",
                piece.len()
            );
        }
        assert_eq!(rejoin(&units), text);
    }

    #[test]
    fn newlines_and_blank_lines_survive_the_round_trip() {
        let text = format!("標題\n\n{}\n結尾", "很長的內容。".repeat(200));
        let units = split_for_translation(&text, TRANSLATE_CHUNK_BYTES);
        assert_eq!(rejoin(&units), text);
    }

    #[test]
    fn a_single_unbreakable_run_is_hard_split_on_char_boundaries() {
        // 沒有任何標點與空白的超長字串：只能硬切，但不能切壞 UTF-8。
        let text = "數".repeat(1000);
        let units = split_for_translation(&text, TRANSLATE_CHUNK_BYTES);
        assert!(units.len() > 1);
        assert_eq!(rejoin(&units), text);
    }
}
