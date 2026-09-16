//! 課程公告頁（Tronclass bulletins）後端。
//!
//! 透過已登入 dashboard 帳號的 Tronclass session，先取 `/api/my-courses`，
//! 再平行（concurrency 8）抓取每門課的 `/api/courses/{id}/bulletins`，
//! 彙整成單一清單回給前端。公告為課程層級；無公告時回空陣列。

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapters::events::{AdapterAccountTarget, AdapterMessenger, BulletinEvent, OutboundMessage};

use super::{
    admin_login_redirect, check_test_notification_rate, html_page_response,
    must_change_password_redirect, session_token, unauthorized_response, AdminQuery,
    AdminSession, AdminWebState,
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
    // ADMIN_PAGE_TRI_STATE_V1：access 過期但 refresh 有效時由伺服器端輪換，不再閃登入頁。
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/bulletins");
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    // BULLETIN_ATTACHMENT_V1（fix55）：公告附件要在面板內預覽（PDF 用 PDF.js），
    // 沿用教材／作業頁同一支按需載入器（`window.__tronEnsureFileLib`），
    // 不把 ~485KB 的重庫塞進這頁的同步載入路徑。
    let html = super::pages::inject_files_office_assets(&super::pages::inject_theme(PAGE_HTML));
    super::pages::attach_fresh_cookies(
        html_page_response(&headers, super::pages::inject_auth_attrs(&html, &page.session)),
        &page,
        &headers,
    )
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
            // VIEWER_NO_SCHOOL_API_V1：管理員 fallback 只挑能登入學校系統的帳號。
            raw = list.into_iter().find(|a| a.enabled && !a.is_viewer);
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
    // VIEWER_NO_SCHOOL_API_V1：檢視者帳號沒有可用的學校憑證，不代為呼叫學校 API。
    if raw.is_viewer {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "檢視者帳號不會連線學校系統；請選擇一般帳號" })),
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
        // ATOMIC_WRITE_V1
        if let Err(e) = crate::atomic_write::write_atomic(path, s) {
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
        // ATOMIC_WRITE_V1
        if let Err(e) = crate::atomic_write::write_atomic(path, s) {
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
                // FIX: 快取命中時也要帶上 is_admin，否則前端讀不到 body.is_admin，
                // 會把「＋發佈」按鈕誤判為隱藏（用完一次就消失的假象）。
                return (
                    StatusCode::OK,
                    Json(json!({ "items": items, "is_admin": session.is_admin })),
                )
                    .into_response();
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
    // CURRENT_SEMESTER_ONLY_V1：只抓當前學期課程的公告（與作業頁同一條
    // 防線；無法判定學期的課程保留），避免上學期課程的公告洗版。
    let briefs = crate::api::todos::parse_course_briefs(&courses_json);
    let courses: Vec<(u64, String)> = crate::api::todos::current_course_pairs(briefs);

    let raw = fetch_all_bulletins(client, base_url, courses).await;

    let (deleted_set, read_by_me) = {
        let store = BULLETIN_READ_STORE.lock().unwrap_or_else(|p| p.into_inner());
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
            // BULLETIN_ATTACHMENT_V1（fix55）：公告附件。iClass 把附件放在公告的
            // `uploads` 陣列裡（每筆有 id／key／name／size／allow_download），
            // 實測 263861 課程的 Course Policy 公告即為此形狀。
            // 前端只拿得到「名稱／大小／可否下載」，真正的位元組一律由伺服器端用
            // 已保存的 iClass session 代抓（見下方 *_attachment_handler）。
            let attachments: Vec<Value> = b
                .get("uploads")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter(|u| {
                            !u.get("deleted").and_then(Value::as_bool).unwrap_or(false)
                        })
                        .filter_map(|u| {
                            let id = u.get("id").and_then(Value::as_u64)?;
                            let name = u
                                .get("name")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|v| !v.is_empty())
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("附件-{id}"));
                            let size = u.get("size").and_then(Value::as_u64).unwrap_or(0);
                            Some(json!({
                                "id": id,
                                "name": name,
                                "size": size,
                                "size_label": super::utils::format_file_size(size),
                                "allow_download": u
                                    .get("allow_download")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(true),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "key": key,
                "course_name": course_name,
                "attachments": attachments,
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

// BULLETIN_ATTACHMENT_V1（fix55）：公告附件代理端點。
//
// 為什麼一定要經過面板伺服器：附件的實際位址是 `/api/uploads/{id}/preview` 換來的
// 限時簽章 URL（S3／MinIO），需要「已保存的 iClass session」才拿得到；而且 iClass
// 對公告附件沒有穩定可分享的公開網址。所以流程與作業附件（fix43／fix47）完全相同：
//   瀏覽器 → 面板（帶面板 session cookie）→ iClass API（帶保存的 session）
//   → 簽章 URL → 串流回瀏覽器（支援 Range／206 邊下邊播）。
#[derive(Debug, Deserialize)]
pub(super) struct BulletinAttachmentQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    account_id: Option<String>,
    upload_id: u64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    disposition: Option<String>,
}

// BULLETIN_ATTACHMENT_CACHE_V1（fix61）：公告附件的重看成本。
//
// 這個端點每次都要先問一次 iClass（`/api/uploads/{id}/preview` 拿簽章 URL）再從 MinIO
// 串流；PDF 檢視器換頁、同一個人再打開同一則公告的附件，等於整條路重跑。教材側在
// fix52 已經解過同一題（`PREVIEW_REVALIDATE_V1`：ETag ＋ `private, max-age=0,
// must-revalidate`），公告附件照同一套做：
//
//   · ETag 直接用 iClass 的 upload id（附件是唯讀的，換一份就是一組新的 id）——
//     所以「內容沒變」這件事不必問上游就知道，命中 `If-None-Match` 時**連上游都不打**。
//   · `private`：附件要登入才看得到，不能被共享快取留存。
//   · `max-age=0, must-revalidate`：每次仍會問伺服器，但內容沒變只回 304（沒有 body）。
//
// 有 `Range` 的分段請求不做 304（範圍內容不能用整檔標籤代替），與教材側一致。
const BULLETIN_ATTACHMENT_CACHE_CONTROL: &str = "private, max-age=0, must-revalidate";

/// 附件回應的 ETag（含引號的強標籤）。
fn bulletin_attachment_etag(upload_id: u64) -> String {
    format!("\"att-{upload_id:x}\"")
}

/// `If-None-Match` 是否命中這個標籤（支援逗號清單與 `*`），與教材側同一套判定。
fn bulletin_attachment_not_modified(headers: &HeaderMap, tag: &str) -> bool {
    headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|incoming| {
            incoming
                .split(',')
                .any(|candidate| candidate.trim() == tag || candidate.trim() == "*")
        })
        .unwrap_or(false)
}

pub(super) async fn admin_bulletin_attachment_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<BulletinAttachmentQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    let account = match resolve_account(&state, &session, query.account_id.as_deref()).await {
        Ok(account) => account,
        Err(response) => return response,
    };
    let client = match get_client(&state, &account).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    let base_url = account.base_url().to_string();
    // 檔名只當顯示／副檔名推斷用（同樣過安全檢查）；真正來源是 upload id。
    let file_name = query
        .name
        .as_deref()
        .and_then(super::utils::safe_file_name)
        .unwrap_or_else(|| format!("公告附件-{}", query.upload_id));
    let inline = matches!(
        query.disposition.as_deref().map(str::trim),
        Some(value) if value.eq_ignore_ascii_case("inline")
    );

    // BULLETIN_ATTACHMENT_CACHE_V1（fix61）：標籤只由 upload id 決定（附件唯讀），
    // 所以「已經有一份相同的內容」時可以立刻回 304，連 iClass 都不必問。
    let etag = bulletin_attachment_etag(query.upload_id);
    let has_range = headers.contains_key(axum::http::header::RANGE);
    if !has_range && bulletin_attachment_not_modified(&headers, &etag) {
        let tag = HeaderValue::from_str(&etag)
            .unwrap_or_else(|_| HeaderValue::from_static("\"att\""));
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, tag)
            .header(header::CACHE_CONTROL, BULLETIN_ATTACHMENT_CACHE_CONTROL)
            .body(axum::body::Body::empty())
            .unwrap_or_else(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("建立 304 回應失敗：{e}") })),
                )
                    .into_response()
            });
    }

    let upload = json!({ "id": query.upload_id });
    let mut urls = super::materials_import::resolve_download_urls(&client, &base_url, &upload).await;
    // 只剩 `…/download/{key}` 備援、或完全沒有 → 多半是 iClass session 過期
    // （preview 端點回登入頁）。重新登入再解一次，與作業附件同一套處置。
    if !urls.iter().any(|url| !url.contains("/download/")) {
        if let Ok(fresh) = super::homework::iclass_api_after_relogin(&account, &state.accounts_db).await
        {
            let retried = super::materials_import::resolve_download_urls(
                &fresh.client,
                &base_url,
                &upload,
            )
            .await;
            if retried.iter().any(|url| !url.contains("/download/")) || urls.is_empty() {
                urls = retried;
            }
        }
    }
    if urls.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "找不到這個附件（可能已被刪除，或 iClass session 已失效）" })),
        )
            .into_response();
    }

    // RANGE_STREAM_V1：原樣把瀏覽器的 Range 轉給上游，206／Content-Range 照傳回去。
    let range = headers.get(axum::http::header::RANGE).cloned();
    let mut attempt = super::homework::stream_iclass_upload(
        &client,
        &urls,
        &file_name,
        inline,
        range.as_ref(),
    )
    .await;

    // HOMEWORK_FILE_SANITY_V1（fix47）同一條防線：上游回 HTML 登入頁／JSON 錯誤時
    // 不再當成檔案串出去（最常見成因是 session 過期），重登後再試一輪。
    if attempt.response.is_none() && attempt.saw_page {
        if let Ok(fresh) = super::homework::iclass_api_after_relogin(&account, &state.accounts_db).await
        {
            let retried_urls = super::materials_import::resolve_download_urls(
                &fresh.client,
                &base_url,
                &upload,
            )
            .await;
            if !retried_urls.is_empty() {
                tracing::info!(account = %account.id, "公告附件疑似 session 過期：重新登入後重試");
                attempt = super::homework::stream_iclass_upload(
                    &fresh.client,
                    &retried_urls,
                    &file_name,
                    inline,
                    range.as_ref(),
                )
                .await;
            }
        }
    }

    match attempt.response {
        Some(mut response) => {
            // 成功串流時補上 ETag 與可驗證的快取政策：下一次重看同一個附件，
            // 瀏覽器會帶 `If-None-Match` 回來，我們直接回 304（上面那段）——
            // 既不必問 iClass、也不必再從 MinIO 傳一次位元組。
            if let Ok(tag) = HeaderValue::from_str(&etag) {
                response.headers_mut().insert(header::ETAG, tag);
            }
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(BULLETIN_ATTACHMENT_CACHE_CONTROL),
            );
            response
        }
        None => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("讀取附件失敗：{}", attempt.last_error) })),
        )
            .into_response(),
    }
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
    // TEST_NOTIFICATION_RATE_LIMIT_V1
    if let Err(response) =
        check_test_notification_rate(&state, "bulletin", &session.account_id).await
    {
        return response;
    }
    let key = format!("test-{}", now_secs());
    crate::web_push_notifications::notify_bulletin(
        &session.account_id,
        &key,
        "公告測試",
        "這是一則測試公告通知，點一下即可開啟課程公告頁。",
        "",  // 測試通知不帶摘要
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
    // ADMIN_BULLETIN_NOTIFY_V1: 發佈後立即對所有啟用帳號推送 Web Push 與 Discord，
    // 確保「管理員公告」也一定有通知（與課程公告走相同的 AI 摘要邏輯）。
    notify_admin_bulletin(&state, bulletin.clone()).await;
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

// MORNING_DIGEST_V1：最新一輪輪詢的公告標題（per account），供晨間摘要
// 組「最新公告」段落。只留在記憶體——重啟後為空，摘要該段自動略過。
static LATEST_BULLETINS: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
> = once_cell::sync::Lazy::new(|| {
    std::sync::Mutex::new(std::collections::HashMap::new())
});

/// 某帳號最新一輪輪詢看到的前 `n` 則公告標題。
pub(super) fn latest_bulletin_titles(account_id: &str, n: usize) -> Vec<String> {
    let guard = LATEST_BULLETINS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .get(account_id)
        .map(|titles| titles.iter().take(n).cloned().collect())
        .unwrap_or_default()
}
static BULLETIN_NOTIFIED_WARM: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// LOG_DISK_GUARD_V1：已通知公告 key 的上限。以前只增不減，config/bulletin_notified.json
/// 會隨學期無限長大。key 依時間順序 push，只保留最新 N 筆即可（舊公告早已不在
/// iClass 列表中，不會再被判成「新公告」重複推播）。
const BULLETIN_NOTIFIED_MAX_KEYS: usize = 3000;

fn cap_notified_keys(keys: &mut Vec<String>) {
    if keys.len() > BULLETIN_NOTIFIED_MAX_KEYS {
        let drop = keys.len() - BULLETIN_NOTIFIED_MAX_KEYS;
        keys.drain(..drop);
    }
}

fn save_notified(keys: &[String]) {
    let path = bulletin_notified_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string(&BulletinNotifiedStore {
        keys: keys.to_vec(),
    }) {
        // ATOMIC_WRITE_V1：半截檔會讓已通知清單歸零 → 重啟後所有公告重複推播。
        if let Err(e) = crate::atomic_write::write_atomic(path, s) {
            tracing::warn!("無法寫入 bulletin_notified.json：{e}");
        }
    }
}

pub(super) fn start_bulletin_notifier(
    state: AdminWebState,
    messenger: Option<Arc<dyn AdapterMessenger>>,
) {
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
        *BULLETIN_NOTIFIED.lock().unwrap_or_else(|p| p.into_inner()) = keys;
        BULLETIN_NOTIFIED_WARM.store(existed, std::sync::atomic::Ordering::SeqCst);

        tokio::spawn(async move {
            // BULLETIN_POLL_TAME_V1：間隔與夜間暫停改由 [bulletins] 設定驅動
            //（預設 5 分鐘、00:00–08:00 暫停；每輪重讀設定）。
            // 夜間跳過輪詢；08:00 恢復後第一輪補抓，夜間累積的新公告
            // 一次推送——不漏通知，只是晚幾小時。已通知清單持久化於磁碟，
            // 重啟亦不會重複推送。
            use chrono::Timelike;
            let mut night_logged = false;
            loop {
                let (interval_secs, night_pause, night_start, night_end) =
                    match crate::config::try_get_config() {
                        Some(cfg) => (
                            cfg.bulletins.poll_interval_secs.max(60),
                            cfg.bulletins.night_pause,
                            cfg.bulletins.night_pause_start.clone(),
                            cfg.bulletins.night_pause_end.clone(),
                        ),
                        None => (
                            300u64,
                            true,
                            "00:00".to_string(),
                            "08:00".to_string(),
                        ),
                    };
                tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                // DateTime 無「當日分鐘數」捷徑，以 hour()*60+minute() 組出。
                let now = chrono::Local::now();
                let minute_of_day = now.hour() * 60 + now.minute();
                if night_pause
                    && crate::homework_reminders::night_window_active(
                        &night_start,
                        &night_end,
                        minute_of_day,
                    )
                {
                    if !night_logged {
                        tracing::info!(
                            start = %night_start,
                            end = %night_end,
                            interval_secs,
                            "夜間時段：暫停公告輪詢（恢復後第一輪補抓推送）"
                        );
                        night_logged = true;
                    }
                    // UPDATE_TIMES_PAGE_V1：夜間跳過也記一筆，頁面不會顯示成「卡住」。
                    crate::update_times::record_global(
                        crate::update_times::KIND_BULLETIN_POLL,
                        true,
                        "夜間暫停，略過本輪",
                    );
                    continue;
                }
                night_logged = false;
                notify_check(&state, messenger.clone()).await;
            }
        });
    });
}

// BULLETIN_NOTIFY_SUMMARY_V1
// 新公告推播時，先背景生成一份「精簡但重點不漏」的摘要（與公告頁摘要同一套
// 密鑰輪替），再同時推給 Web Push（手機）與 Discord。同則公告只摘要一次，
// 避免重複呼叫 AI；摘要失敗時退回標題，不影響通知送出。
#[derive(Debug, Clone)]
struct NewBulletin {
    key: String,
    account_id: String,
    line_user_id: String,
    discord_user_id: String,
    course: String,
    title: String,
    content: String,
}

static BULLETIN_SUMMARY_CACHE: once_cell::sync::Lazy<std::sync::Mutex<
    std::collections::HashMap<String, String>,
>> = once_cell::sync::Lazy::new(|| {
    std::sync::Mutex::new(std::collections::HashMap::new())
});

async fn summary_for_bulletin(key: &str, source: &str) -> String {
    if let Some(cached) = BULLETIN_SUMMARY_CACHE.lock().unwrap_or_else(|p| p.into_inner()).get(key).cloned() {
        return cached;
    }
    let summary = super::ai_chat::summarize_bulletin_text(source.to_string())
        .await
        .unwrap_or_default();
    if !summary.is_empty() {
        BULLETIN_SUMMARY_CACHE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.to_string(), summary.clone());
    }
    summary
}

fn spawn_bulletin_notification(
    item: NewBulletin,
    messenger: Option<Arc<dyn AdapterMessenger>>,
) {
    tokio::spawn(async move {
        let source = if item.content.trim().is_empty() {
            item.title.clone()
        } else {
            format!("{}\n{}", item.title, item.content)
        };
        let summary = summary_for_bulletin(&item.key, &source).await;

        // 手機（Web Push）：body 用摘要，空則退回標題。
        crate::web_push_notifications::notify_bulletin(
            &item.account_id,
            &item.key,
            &item.course,
            &item.title,
            &summary,
        );

        // Discord（及其它可用 adapter）：走通知偏好開關。
        if let Some(m) = messenger {
            let target = AdapterAccountTarget::new(
                item.account_id.clone(),
                item.line_user_id.clone(),
                item.discord_user_id.clone(),
            );
            let event = OutboundMessage::Bulletin(BulletinEvent {
                account: item.account_id.clone(),
                course_name: item.course.clone(),
                title: item.title.clone(),
                summary,
            });
            if let Err(error) = m.push_to_account_or_admin(&target, &event).await {
                tracing::warn!(
                    account = %item.account_id,
                    error = %error,
                    "公告 Discord 通知失敗"
                );
            }
        }
    });
}

// ADMIN_BULLETIN_NOTIFY_V1: 管理員發佈公告後，對所有啟用帳號立即推送 Web Push 與 Discord。
// 與課程公告一致，先產生 AI 摘要（依 key 快取），再逐帳號發送，確保「管理員公告」定有通知。
async fn notify_admin_bulletin(state: &AdminWebState, bulletin: AdminBulletin) {
    let Ok(accounts) = state.accounts_db.list().await else {
        return;
    };
    // 管理員公告是站內訊息，檢視者也要收到（不涉及學校 API）。
    let enabled: Vec<_> = accounts
        .into_iter()
        .filter(|a| a.enabled)
        .map(|a| (a.id.clone(), a.line_user_id.clone(), a.discord_user_id.clone()))
        .collect();
    if enabled.is_empty() {
        return;
    }
    let messenger = state.messenger.clone();
    tokio::spawn(async move {
        let key = format!("admin-{}", bulletin.id);
        let title = bulletin.title.clone();
        let content = bulletin.content.clone();
        let source = if content.trim().is_empty() {
            title.clone()
        } else {
            format!("{}\n{}", title, content)
        };
        let summary = summary_for_bulletin(&key, &source).await;

        for (account_id, line_user_id, discord_user_id) in enabled {
            // 手機（Web Push）：body 用摘要，空則退回標題。
            crate::web_push_notifications::notify_bulletin(
                &account_id,
                &key,
                "管理員公告",
                &title,
                &summary,
            );

            // Discord（及其它可用 adapter）：走通知偏好開關。
            if let Some(m) = messenger.as_ref() {
                let target = AdapterAccountTarget::new(account_id.clone(), line_user_id, discord_user_id);
                let event = OutboundMessage::Bulletin(BulletinEvent {
                    account: account_id.clone(),
                    course_name: "管理員公告".to_string(),
                    title: title.clone(),
                    summary: summary.clone(),
                });
                if let Err(error) = m.push_to_account_or_admin(&target, &event).await {
                    tracing::warn!(account = %account_id, error = %error, "管理員公告 Discord 通知失敗");
                }
            }
        }
    });
}

async fn notify_check(state: &AdminWebState, messenger: Option<Arc<dyn AdapterMessenger>>) {
    let accounts = match state.accounts_db.list().await {
        Ok(l) => l,
        Err(_) => return,
    };
    // VIEWER_NO_SCHOOL_API_V1：公告輪詢要登入學校系統，檢視者帳號略過。
    let enabled: Vec<_> = accounts.into_iter().filter(|a| a.enabled && !a.is_viewer).collect();
    if enabled.is_empty() {
        crate::update_times::record_global(
            crate::update_times::KIND_BULLETIN_POLL,
            true,
            "沒有啟用的帳號",
        );
        return;
    }
    let Some(config) = crate::config::try_get_config() else {
        return;
    };

    let mut current: Vec<NewBulletin> = Vec::new();
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
        // UPDATE_TIMES_PAGE_V1：每個帳號這一輪的結果（成功＝抓到公告清單；
        // 登入／課程清單失敗則記錯誤，時間頁能看出是哪個帳號卡住）。
        let stamp_fail = |why: &str| {
            crate::update_times::record_account(
                crate::update_times::KIND_BULLETINS,
                &raw.id,
                false,
                why,
            );
        };
        let Ok(client) = get_client(state, &account).await else {
            stamp_fail("Tronclass 登入失敗");
            continue;
        };
        let base = account.base_url().to_string();
        let Ok(resp) = client.get(format!("{base}/api/my-courses")).send().await else {
            stamp_fail("取得課程清單失敗（連線）");
            continue;
        };
        if !resp.status().is_success() {
            stamp_fail(&format!("取得課程清單失敗（HTTP {}）", resp.status().as_u16()));
            continue;
        }
        let Ok(cj) = resp.json::<Value>().await else {
            stamp_fail("解析課程清單失敗");
            continue;
        };
        // CURRENT_SEMESTER_ONLY_V1：同 list_handler，只通知當前學期課程的公告。
        let courses =
            crate::api::todos::current_course_pairs(crate::api::todos::parse_course_briefs(&cj));
        let items = fetch_all_bulletins(client, base, courses).await;
        crate::update_times::record_account(
            crate::update_times::KIND_BULLETINS,
            &raw.id,
            true,
            format!("{} 則", items.len()),
        );
        for b in items {
            let key = bulletin_key(&b);
            let course = b
                .get("_course_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let title = pick_str(&b, &["title", "name", "subject"]);
            let content = pick_str(&b, &["content", "body", "text", "description"]);
            current.push(NewBulletin {
                key,
                account_id: account.id.clone(),
                line_user_id: account.line_user_id.clone(),
                discord_user_id: account.discord_user_id.clone(),
                course,
                title,
                content,
            });
        }
    }

    // UPDATE_TIMES_PAGE_V1：整輪輪詢完成。
    crate::update_times::record_global(
        crate::update_times::KIND_BULLETIN_POLL,
        true,
        format!("{} 則公告", current.len()),
    );

    // MORNING_DIGEST_V1：記錄本輪各帳號的最新公告標題（覆蓋前輪），
    // 供晨間摘要的「最新公告」段落使用。
    {
        let mut latest = LATEST_BULLETINS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        latest.clear();
        for item in &current {
            latest
                .entry(item.account_id.clone())
                .or_default()
                .push(item.title.clone());
        }
    }

    let mut guard = BULLETIN_NOTIFIED.lock().unwrap_or_else(|p| p.into_inner());
    let warm = BULLETIN_NOTIFIED_WARM.load(std::sync::atomic::Ordering::SeqCst);
    let mut keys = guard.clone();
    if !warm {
        for item in &current {
            if !keys.contains(&item.key) {
                keys.push(item.key.clone());
            }
        }
        cap_notified_keys(&mut keys);
        *guard = keys.clone();
        BULLETIN_NOTIFIED_WARM.store(true, std::sync::atomic::Ordering::SeqCst);
        save_notified(&keys);
        return;
    }
    let mut changed = false;
    for item in current {
        if !keys.contains(&item.key) {
            keys.push(item.key.clone());
            changed = true;
            // 背景生成摘要 + 推送到 Web Push 與 Discord，不阻塞輪詢迴圈。
            spawn_bulletin_notification(item, messenger.clone());
        }
    }
    if changed {
        cap_notified_keys(&mut keys);
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

/// BULLETIN_TRANSLATE_V9
/// 單次送出的最大 bytes。取「所有引擎都吃得下」的保守值：
/// MyMemory 匿名端點上限約 500 bytes，是鏈上最嚴格的一環，
/// 因此統一切到 460，任何一段都能丟給任何引擎重試。
const TRANSLATE_CHUNK_BYTES: usize = 460;
/// 片段數上限，避免一則超長公告打出上百個外部請求。
const TRANSLATE_MAX_CHUNKS: usize = 48;
/// 併發翻譯數；太高容易被 Google 擋。
const TRANSLATE_CONCURRENCY: usize = 3;
/// 單段失敗時的重試次數（gtx 偶發 429/5xx）。
const TRANSLATE_RETRIES: usize = 2;
/// 單次上游請求逾時。
const TRANSLATE_REQUEST_TIMEOUT_SECS: u64 = 10;
/// BULLETIN_TRANSLATE_V8
/// 背景工作的整體時間預算。翻譯已不佔用 HTTP 連線，所以可以放寬；
/// 超過就把工作標記為 TRANSLATE_TIMEOUT，不會永遠停在 running。
const TRANSLATE_TOTAL_BUDGET_SECS: u64 = 90;
/// 診斷訊息中保留的上游回應長度。
const TRANSLATE_DETAIL_CHARS: usize = 300;

/// BULLETIN_TRANSLATE_V7
/// 共用單一 HTTP client。舊版每個請求都 `Client::builder().build()`，
/// 會重建連線池與 TLS 設定，在記憶體吃緊的容器裡是不必要的負擔
/// （程序被 OOM 殺掉時，Cloudflare 看到的就是沒有回應的 502）。
static TRANSLATE_CLIENT: once_cell::sync::Lazy<reqwest::Client> =
    once_cell::sync::Lazy::new(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(
                TRANSLATE_REQUEST_TIMEOUT_SECS,
            ))
            .connect_timeout(std::time::Duration::from_secs(5))
            .pool_max_idle_per_host(4)
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) TronclassPanel/1.0")
            .build()
            .unwrap_or_default()
    });

/// BULLETIN_TRANSLATE_V8
/// 這個部署的反向代理（Cloudflare Tunnel）對「久等的請求」會直接回一個
/// 沒有內容的 502 —— 本專案的教材匯入早就踩過同一顆雷，解法寫在
/// `materials_import.rs`：「匯入改為背景 job（立即回 202），避免占住 HTTP
/// request 直到 proxy 逾時 → 502」。
/// 翻譯要連打多段 Google 請求，同樣會佔住連線，因此照相同 pattern 改寫：
/// POST 立即建立背景工作並回 202，前端改用輪詢取進度與結果。
#[derive(Clone)]
struct TranslateJob {
    owner: String,
    running: bool,
    done: bool,
    ok: bool,
    started_at: u64,
    updated_at: u64,
    total: usize,
    finished: usize,
    tl: String,
    translated: String,
    partial: bool,
    note: String,
    error: String,
    code: String,
    trace_id: String,
    engine: String,
    diagnostics: Vec<Value>,
}

impl TranslateJob {
    fn new(owner: &str, total: usize, tl: &str, trace_id: &str) -> Self {
        let now = now_secs();
        Self {
            owner: owner.to_string(),
            running: true,
            done: false,
            ok: false,
            started_at: now,
            updated_at: now,
            total,
            finished: 0,
            tl: tl.to_string(),
            translated: String::new(),
            partial: false,
            note: String::new(),
            error: String::new(),
            code: String::new(),
            trace_id: trace_id.to_string(),
            engine: String::new(),
            diagnostics: Vec::new(),
        }
    }

    fn to_json(&self, job_id: &str) -> Value {
        json!({
            "job_id": job_id,
            "state": if self.done {
                if self.ok { "done" } else { "failed" }
            } else {
                "running"
            },
            "running": self.running,
            "done": self.done,
            "ok": self.ok,
            "finished": self.finished,
            "total": self.total,
            "elapsed_secs": now_secs().saturating_sub(self.started_at),
            "translated": self.translated,
            "tl": self.tl,
            "partial": self.partial,
            "note": if self.note.is_empty() { Value::Null } else { Value::String(self.note.clone()) },
            "error": self.error,
            "code": self.code,
            "trace_id": self.trace_id,
            "engine": self.engine,
            "chunks": self.total,
            "diagnostics": self.diagnostics,
        })
    }
}

/// 工作保留時間（完成後仍可再查詢一次結果）。
const TRANSLATE_JOB_TTL_SECS: u64 = 10 * 60;
/// 最多同時保留的工作數。
const TRANSLATE_JOB_MAX: usize = 32;

static TRANSLATE_JOBS: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, TranslateJob>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn translate_jobs_prune(map: &mut std::collections::HashMap<String, TranslateJob>) {
    let now = now_secs();
    map.retain(|_, job| now.saturating_sub(job.updated_at) <= TRANSLATE_JOB_TTL_SECS);
    if map.len() > TRANSLATE_JOB_MAX {
        let mut ordered: Vec<(String, u64)> =
            map.iter().map(|(k, v)| (k.clone(), v.updated_at)).collect();
        ordered.sort_by_key(|(_, at)| *at);
        let drop_count = map.len() - TRANSLATE_JOB_MAX;
        for (key, _) in ordered.into_iter().take(drop_count) {
            map.remove(&key);
        }
    }
}

fn translate_job_insert(job_id: &str, job: TranslateJob) {
    if let Ok(mut map) = TRANSLATE_JOBS.lock() {
        translate_jobs_prune(&mut map);
        map.insert(job_id.to_string(), job);
    }
}

fn translate_job_get(job_id: &str) -> Option<TranslateJob> {
    TRANSLATE_JOBS
        .lock()
        .ok()
        .and_then(|map| map.get(job_id).cloned())
}

fn translate_job_update(job_id: &str, apply: impl FnOnce(&mut TranslateJob)) {
    if let Ok(mut map) = TRANSLATE_JOBS.lock() {
        if let Some(job) = map.get_mut(job_id) {
            apply(job);
            job.updated_at = now_secs();
        }
    }
}

/// BULLETIN_TRANSLATE_V9
/// 整段翻譯結果快取。公告內容不會變，重複點「翻譯」不該再打一次上游 ——
/// 既省 MyMemory 的每日配額，也降低再次觸發 Google 反機器人判定的機率。
const TRANSLATE_CACHE_TTL_SECS: u64 = 24 * 60 * 60;
const TRANSLATE_CACHE_MAX: usize = 200;

/// 快取值：(寫入時間, 譯文, 引擎)
static TRANSLATE_CACHE: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, (u64, String, String)>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn translate_cache_key(text: &str, tl: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    tl.hash(&mut hasher);
    text.hash(&mut hasher);
    format!("{tl}-{:x}", hasher.finish())
}

fn translate_cache_get(key: &str) -> Option<(String, String)> {
    let mut map = TRANSLATE_CACHE.lock().ok()?;
    let now = now_secs();
    map.retain(|_, (at, _, _)| now.saturating_sub(*at) <= TRANSLATE_CACHE_TTL_SECS);
    map.get(key)
        .map(|(_, text, engine)| (text.clone(), engine.clone()))
}

fn translate_cache_put(key: &str, text: &str, engine: &str) {
    if text.is_empty() {
        return;
    }
    if let Ok(mut map) = TRANSLATE_CACHE.lock() {
        if map.len() >= TRANSLATE_CACHE_MAX {
            // 丟掉最舊的一筆，維持固定上限（避免長期佔用記憶體）。
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, (at, _, _))| *at)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(key.to_string(), (now_secs(), text.to_string(), engine.to_string()));
    }
}

/// 單段翻譯的結果與診斷資訊。失敗時完整記錄上游狀態碼與回應片段，
/// 讓使用者可以直接把錯誤碼回報出來。
#[derive(Clone)]
struct ChunkOutcome {
    index: usize,
    bytes: usize,
    ok: bool,
    text: String,
    error: String,
    status: Option<u16>,
    detail: String,
    attempts: usize,
    ms: u64,
    /// 實際成功（或最後嘗試）的翻譯引擎。
    engine: String,
}

/// 上游失敗細節。
struct UpstreamError {
    message: String,
    status: Option<u16>,
    detail: String,
}

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

// BULLETIN_TRANSLATE_V9
// 實測結果：translate.googleapis.com 對本機房 IP 回 HTTP 429 +「Sorry…」
// 反機器人攔截頁（Google 的 /sorry/ interstitial），且重試必定再被擋。
// 這是 IP 層級封鎖，不是暫時性限流，只靠重試永遠救不回來。
// 因此改為「引擎鏈」：依序嘗試多個免金鑰端點，任何一個成功就採用；
// 被擋的引擎進入冷卻，短時間內不再浪費請求。
// 順序可用環境變數覆寫：TRONCLASS_TRANSLATE_ENGINES="google,google_dict,mymemory"

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TranslateEngine {
    /// translate.googleapis.com（client=gtx）
    GoogleGtx,
    /// clients5.google.com（client=dict-chrome-ex）—— Chrome 擴充套件用的端點，
    /// 與 gtx 的濫用判定不同，gtx 被擋時常常還通。
    GoogleDict,
    /// MyMemory 匿名 API。非爬蟲端點，機房 IP 也能用，是最後的保底。
    MyMemory,
}

impl TranslateEngine {
    fn key(self) -> &'static str {
        match self {
            Self::GoogleGtx => "google",
            Self::GoogleDict => "google_dict",
            Self::MyMemory => "mymemory",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::GoogleGtx => "Google",
            Self::GoogleDict => "Google(dict)",
            Self::MyMemory => "MyMemory",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "google" | "gtx" | "google_gtx" => Some(Self::GoogleGtx),
            "google_dict" | "dict" | "clients5" => Some(Self::GoogleDict),
            "mymemory" | "mm" => Some(Self::MyMemory),
            _ => None,
        }
    }
}

/// 預設嘗試順序：先 Google（兩種端點），再 MyMemory 保底。
const DEFAULT_ENGINE_ORDER: &[TranslateEngine] = &[
    TranslateEngine::GoogleGtx,
    TranslateEngine::GoogleDict,
    TranslateEngine::MyMemory,
];

fn engine_order() -> Vec<TranslateEngine> {
    match std::env::var("TRONCLASS_TRANSLATE_ENGINES") {
        Ok(raw) if !raw.trim().is_empty() => {
            let list: Vec<TranslateEngine> = raw
                .split(&[',', ' '][..])
                .filter_map(TranslateEngine::parse)
                .collect();
            if list.is_empty() {
                DEFAULT_ENGINE_ORDER.to_vec()
            } else {
                list
            }
        }
        _ => DEFAULT_ENGINE_ORDER.to_vec(),
    }
}

/// 被上游擋下後的冷卻秒數。期間直接跳過該引擎，不浪費往返時間。
const ENGINE_COOLDOWN_SECS: u64 = 10 * 60;

static ENGINE_COOLDOWN: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<&'static str, u64>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn engine_in_cooldown(engine: TranslateEngine) -> bool {
    ENGINE_COOLDOWN
        .lock()
        .ok()
        .and_then(|map| map.get(engine.key()).copied())
        .is_some_and(|until| now_secs() < until)
}

fn engine_start_cooldown(engine: TranslateEngine, status: Option<u16>) {
    // 只有「被擋」才冷卻；一般錯誤（逾時、解析失敗）不該讓引擎整個停用。
    let blocked = matches!(status, Some(403) | Some(429) | Some(503));
    if !blocked {
        return;
    }
    if let Ok(mut map) = ENGINE_COOLDOWN.lock() {
        map.insert(engine.key(), now_secs() + ENGINE_COOLDOWN_SECS);
    }
    tracing::warn!(
        engine = engine.key(),
        status = status.unwrap_or(0),
        cooldown_secs = ENGINE_COOLDOWN_SECS,
        "翻譯引擎被上游封鎖，進入冷卻"
    );
}

fn engine_clear_cooldown(engine: TranslateEngine) {
    if let Ok(mut map) = ENGINE_COOLDOWN.lock() {
        map.remove(engine.key());
    }
}

/// 截斷上游回應，只留前面一小段做診斷（不切壞 UTF-8）。
fn detail_snippet(raw: &str) -> String {
    let cleaned = raw.replace(['\n', '\r'], " ");
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= TRANSLATE_DETAIL_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(TRANSLATE_DETAIL_CHARS).collect();
    out.push('…');
    out
}

/// 依序嘗試引擎鏈，任何一個成功就回傳。全部失敗才回報最後一個錯誤。
async fn translate_chunk(
    client: &reqwest::Client,
    index: usize,
    q: &str,
    sl: &str,
    tl: &str,
) -> ChunkOutcome {
    let started = std::time::Instant::now();
    let order = engine_order();
    let mut attempts = 0usize;
    let mut last_engine = order.first().copied().unwrap_or(TranslateEngine::MyMemory);
    let mut last = UpstreamError {
        message: "沒有可用的翻譯引擎".to_string(),
        status: None,
        detail: String::new(),
    };
    let mut skipped: Vec<&'static str> = Vec::new();

    for engine in order {
        if engine_in_cooldown(engine) {
            skipped.push(engine.label());
            continue;
        }
        last_engine = engine;

        // 同一個引擎的暫時性錯誤（逾時、5xx）才重試；被擋就立刻換下一家。
        for attempt in 0..=TRANSLATE_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(300 * attempt as u64)).await;
            }
            attempts += 1;
            match engine_request(client, engine, q, sl, tl).await {
                Ok(value) => {
                    engine_clear_cooldown(engine);
                    return ChunkOutcome {
                        index,
                        bytes: q.len(),
                        ok: true,
                        text: value,
                        error: String::new(),
                        status: Some(200),
                        detail: String::new(),
                        attempts,
                        ms: started.elapsed().as_millis() as u64,
                        engine: engine.label().to_string(),
                    };
                }
                Err(error) => {
                    let blocked = matches!(error.status, Some(403) | Some(429) | Some(503));
                    last = error;
                    if blocked {
                        engine_start_cooldown(engine, last.status);
                        break; // 換下一個引擎
                    }
                }
            }
        }
    }

    let mut message = format!("{}：{}", last_engine.label(), last.message);
    if !skipped.is_empty() {
        message.push_str(&format!("（冷卻中已跳過：{}）", skipped.join("、")));
    }

    ChunkOutcome {
        index,
        bytes: q.len(),
        ok: false,
        text: String::new(),
        error: message,
        status: last.status,
        detail: last.detail,
        attempts,
        ms: started.elapsed().as_millis() as u64,
        engine: last_engine.label().to_string(),
    }
}

async fn engine_request(
    client: &reqwest::Client,
    engine: TranslateEngine,
    q: &str,
    sl: &str,
    tl: &str,
) -> Result<String, UpstreamError> {
    match engine {
        TranslateEngine::GoogleGtx => google_gtx_request(client, q, sl, tl).await,
        TranslateEngine::GoogleDict => google_dict_request(client, q, sl, tl).await,
        TranslateEngine::MyMemory => mymemory_request(client, q, sl, tl).await,
    }
}

/// 共用的「送出 → 取狀態碼與內文」流程。
async fn fetch_text(
    client: &reqwest::Client,
    engine: TranslateEngine,
    request: reqwest::RequestBuilder,
) -> Result<(u16, String), UpstreamError> {
    let _ = client;
    let resp = request.send().await.map_err(|e| UpstreamError {
        message: if e.is_timeout() {
            format!(
                "{} 逾時（>{TRANSLATE_REQUEST_TIMEOUT_SECS}s）",
                engine.label()
            )
        } else {
            format!("{} 連線失敗", engine.label())
        },
        status: None,
        detail: detail_snippet(&e.to_string()),
    })?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok((status, body))
}

fn http_error(engine: TranslateEngine, status: u16, body: &str) -> UpstreamError {
    // Google 的 /sorry/ 攔截頁是 IP 層級封鎖，明確標示出來避免誤判成暫時限流。
    let blocked_page = body.contains("<title>Sorry") || body.contains("/sorry/");
    let message = if blocked_page {
        format!(
            "{} HTTP {status}（Google 反機器人攔截頁：本機 IP 被封鎖，重試無效）",
            engine.label()
        )
    } else {
        format!("{} HTTP {status}", engine.label())
    };
    UpstreamError {
        message,
        status: Some(status),
        detail: detail_snippet(body),
    }
}

/// gtx 回傳格式：`[[["譯文","原文",…],…],…]`
fn extract_gtx_text(v: &Value) -> String {
    let mut out = String::new();
    if let Some(arr) = v.get(0).and_then(|x| x.as_array()) {
        for seg in arr {
            if let Some(s) = seg.get(0).and_then(|x| x.as_str()) {
                out.push_str(s);
            }
        }
    }
    out
}

/// dict-chrome-ex 回傳格式：`["譯文"]` 或 `[["譯文","原文"],…]`
fn extract_dict_text(v: &Value) -> String {
    match v {
        Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    Value::String(s) => out.push_str(s),
                    Value::Array(inner) => {
                        if let Some(Value::String(s)) = inner.first() {
                            out.push_str(s);
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

async fn google_gtx_request(
    client: &reqwest::Client,
    q: &str,
    sl: &str,
    tl: &str,
) -> Result<String, UpstreamError> {
    let engine = TranslateEngine::GoogleGtx;
    let request = client
        .get("https://translate.googleapis.com/translate_a/single")
        .query(&[
            ("client", "gtx"),
            ("sl", sl),
            ("tl", tl),
            ("dt", "t"),
            ("q", q),
        ]);
    let (status, body) = fetch_text(client, engine, request).await?;
    if !(200..300).contains(&status) {
        return Err(http_error(engine, status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| UpstreamError {
        message: format!("{} 回應非 JSON", engine.label()),
        status: Some(status),
        detail: format!("{}｜{}", e, detail_snippet(&body)),
    })?;
    let out = to_plain_text(&extract_gtx_text(&v));
    if out.is_empty() {
        Err(UpstreamError {
            message: format!("{} 未回結果", engine.label()),
            status: Some(status),
            detail: detail_snippet(&body),
        })
    } else {
        Ok(out)
    }
}

async fn google_dict_request(
    client: &reqwest::Client,
    q: &str,
    sl: &str,
    tl: &str,
) -> Result<String, UpstreamError> {
    let engine = TranslateEngine::GoogleDict;
    let request = client
        .get("https://clients5.google.com/translate_a/t")
        .query(&[
            ("client", "dict-chrome-ex"),
            ("sl", sl),
            ("tl", tl),
            ("q", q),
        ]);
    let (status, body) = fetch_text(client, engine, request).await?;
    if !(200..300).contains(&status) {
        return Err(http_error(engine, status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| UpstreamError {
        message: format!("{} 回應非 JSON", engine.label()),
        status: Some(status),
        detail: format!("{}｜{}", e, detail_snippet(&body)),
    })?;
    let out = to_plain_text(&extract_dict_text(&v));
    if out.is_empty() {
        Err(UpstreamError {
            message: format!("{} 未回結果", engine.label()),
            status: Some(status),
            detail: detail_snippet(&body),
        })
    } else {
        Ok(out)
    }
}

async fn mymemory_request(
    client: &reqwest::Client,
    q: &str,
    sl: &str,
    tl: &str,
) -> Result<String, UpstreamError> {
    let engine = TranslateEngine::MyMemory;
    // MyMemory 不支援來源自動偵測；上游給了 sl=auto 時，改用「含中日韓字→zh-CN，否則 en」當來源提示。
    let sl_eff: &str = if sl == "auto" {
        if contains_cjk(q) { "zh-CN" } else { "en" }
    } else {
        sl
    };
    let langpair = format!("{sl_eff}|{tl}");
    let request = client
        .get("https://api.mymemory.translated.net/get")
        .query(&[("q", q), ("langpair", langpair.as_str())]);
    let (status, body) = fetch_text(client, engine, request).await?;
    if !(200..300).contains(&status) {
        return Err(http_error(engine, status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| UpstreamError {
        message: format!("{} 回應非 JSON", engine.label()),
        status: Some(status),
        detail: format!("{}｜{}", e, detail_snippet(&body)),
    })?;
    // responseStatus 有時是數字、有時是字串。
    let api_status = v
        .get("responseStatus")
        .and_then(|x| {
            x.as_i64()
                .or_else(|| x.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
        })
        .unwrap_or(-1);
    let out = to_plain_text(
        v.pointer("/responseData/translatedText")
            .and_then(|x| x.as_str())
            .unwrap_or(""),
    );
    if api_status == 200 && !out.is_empty() {
        Ok(out)
    } else {
        Err(UpstreamError {
            // 配額用盡時 MyMemory 會回 429 或在內文提示，一併帶出來。
            message: format!("{} responseStatus={api_status}", engine.label()),
            status: Some(if api_status == 429 { 429 } else { status }),
            detail: detail_snippet(&body),
        })
    }
}

/// 取出 panic payload 的訊息文字。
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "未知 panic（payload 型別無法解析）".to_string()
}

/// BULLETIN_TRANSLATE_V7
/// handler 內若發生 panic，axum/hyper 會直接中斷連線，Cloudflare 只會顯示
/// 一個沒有內容的「502 Bad gateway」，完全查不到原因。這裡把整段處理包在
/// `catch_unwind` 裡，任何 panic 都轉成帶 trace_id 的 JSON 回應。
pub(super) async fn translate_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(body): Json<BulletinTranslateRequest>,
) -> Response {
    use futures::FutureExt;

    let trace_id: String = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect();

    let guarded = std::panic::AssertUnwindSafe(translate_inner(
        state,
        headers,
        query,
        body,
        trace_id.clone(),
    ))
    .catch_unwind()
    .await;

    match guarded {
        Ok(response) => response,
        Err(payload) => {
            let message = panic_payload_message(payload.as_ref());
            tracing::error!(
                trace_id = %trace_id,
                panic = %message,
                "公告翻譯 panic（已攔截，連線未中斷）"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": format!("翻譯處理發生內部錯誤（已攔截 panic）：{message}"),
                    "code": "TRANSLATE_PANIC",
                    "trace_id": trace_id,
                })),
            )
                .into_response()
        }
    }
}

/// BULLETIN_TRANSLATE_V7：自我檢測。直接從本機打一次極小的 Google 翻譯請求，
/// 用來判斷「502 是不是因為本服務根本連不到 translate.googleapis.com」。
/// GET /admin/api/bulletins/translate/selftest（僅管理員）
pub(super) async fn translate_selftest_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return unauthorized_response();
    }

    // BULLETIN_TRANSLATE_V9：逐一實測每個引擎，直接看出誰被擋、誰能用。
    let mut results: Vec<Value> = Vec::new();
    let mut usable: Vec<&'static str> = Vec::new();

    for engine in engine_order() {
        let cooling = engine_in_cooldown(engine);
        let started = std::time::Instant::now();
        let outcome = engine_request(
            &TRANSLATE_CLIENT,
            engine,
            "Curved scores are published on Moodle.",
            "en",
            "zh-TW",
        )
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(text) => {
                engine_clear_cooldown(engine);
                usable.push(engine.label());
                results.push(json!({
                    "engine": engine.key(),
                    "label": engine.label(),
                    "ok": true,
                    "cooling_before": cooling,
                    "elapsed_ms": elapsed_ms,
                    "sample": text,
                }));
            }
            Err(error) => {
                engine_start_cooldown(engine, error.status);
                results.push(json!({
                    "engine": engine.key(),
                    "label": engine.label(),
                    "ok": false,
                    "cooling_before": cooling,
                    "elapsed_ms": elapsed_ms,
                    "status": error.status,
                    "error": error.message,
                    "detail": error.detail,
                }));
            }
        }
    }

    let ok = !usable.is_empty();
    tracing::info!(
        ok,
        usable = %usable.join("、"),
        "公告翻譯自我檢測"
    );

    (
        StatusCode::OK,
        Json(json!({
            "ok": ok,
            "usable": usable,
            "engines": results,
            "request_timeout_secs": TRANSLATE_REQUEST_TIMEOUT_SECS,
            "total_budget_secs": TRANSLATE_TOTAL_BUDGET_SECS,
            "chunk_bytes": TRANSLATE_CHUNK_BYTES,
            "hint": if ok {
                format!("可用引擎：{}。翻譯會自動挑選能用的那一個。", usable.join("、"))
            } else {
                "所有翻譯引擎都連不上或被封鎖。若 Google 顯示 429／Sorry 頁，代表本機 IP 被 Google 擋，需改用其他引擎或出口 IP。".to_string()
            },
        })),
    )
        .into_response()
}

/// 建立背景翻譯工作並立即回 202。實際的多段翻譯在 tokio task 裡跑，
/// HTTP 連線只佔用幾毫秒，反向代理不可能再對它逾時 502。
async fn translate_inner(
    state: AdminWebState,
    headers: HeaderMap,
    query: AdminQuery,
    body: BulletinTranslateRequest,
    trace_id: String,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
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
    // BULLETIN_TRANSLATE_ZH_V2: 所有公告一律翻成繁體中文（zh-TW）。
    // 來源語言交給 Google 自動偵測（sl=auto），這樣中英混雜的公告也會被「完整」翻譯
    // （英文→中文、簡中→繁中）；不會再像固定 sl=zh-CN 那樣把英文留著不翻。
    let sl: &'static str = "auto";
    let tl: String = "zh-TW".to_string();

    // BULLETIN_TRANSLATE_V5：分段送 Google，確保長公告完整翻譯、不被截斷。
    let units = split_for_translation(&text, TRANSLATE_CHUNK_BYTES);
    if units.len() > TRANSLATE_MAX_CHUNKS {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "內容過長，無法一次翻譯（請分段）" })),
        )
            .into_response();
    }

    // 命中快取就直接回結果（前端已支援「直接帶 translated 的同步回應」）。
    let cache_key = translate_cache_key(&text, &tl);
    if let Some((cached, engine)) = translate_cache_get(&cache_key) {
        tracing::info!(trace_id = %trace_id, engine = %engine, "公告翻譯命中快取");
        return (
            StatusCode::OK,
            Json(json!({
                "translated": cached,
                "tl": tl,
                "partial": false,
                "cached": true,
                "engine": engine,
                "trace_id": trace_id,
            })),
        )
            .into_response();
    }

    let total_chunks = units.len();
    let job_id = format!("t{trace_id}");
    translate_job_insert(
        &job_id,
        TranslateJob::new(&session.account_id, total_chunks, &tl, &trace_id),
    );

    tracing::info!(
        trace_id = %trace_id,
        job_id = %job_id,
        chunks = total_chunks,
        bytes = text.len(),
        sl = %sl,
        tl = %tl,
        "公告翻譯開始（背景工作）"
    );

    {
        let job_id = job_id.clone();
        let trace_id = trace_id.clone();
        let tl = tl.clone();
        let cache_key = cache_key.clone();
        tokio::spawn(async move {
            run_translate_job(job_id, units, sl, tl, trace_id, cache_key).await;
        });
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": job_id,
            "state": "running",
            "chunks": total_chunks,
            "total": total_chunks,
            "trace_id": trace_id,
            "poll_path": "/admin/api/bulletins/translate/status",
            "poll_ms": 600,
        })),
    )
        .into_response()
}

/// 背景執行多段翻譯，逐段更新進度，最後把結果寫回工作狀態。
async fn run_translate_job(
    job_id: String,
    units: Vec<(String, String)>,
    sl: &'static str,
    tl: String,
    trace_id: String,
    cache_key: String,
) {
    use futures::StreamExt;

    let client = TRANSLATE_CLIENT.clone();
    let total = units.len();

    // FnOnce-NOT-GENERAL-ENOUGH-FIX
    // 這裡刻意先 clone 成「擁有所有權」的 Vec 再 into_iter()。
    // 若直接 `units.iter().map(|(piece, _)| async move { … })`，閉包參數是
    // `&'a (String, String)`，回傳的 async block 型別會綁死在那個生命週期上，
    // rustc 無法把它推成 higher-ranked 的 `FnOnce`，就會報
    // 「implementation of `FnOnce` is not general enough」。
    let pieces: Vec<(usize, String)> = units
        .iter()
        .enumerate()
        .map(|(index, (piece, _))| (index, piece.clone()))
        .collect();

    // `buffered` 會保持順序，拼回去才不會亂序。
    let translate_all = futures::stream::iter(pieces.into_iter().map(|(index, piece)| {
        let client = client.clone();
        let tl = tl.clone();
        let job_id = job_id.clone();
        async move {
            let outcome = if piece.trim().is_empty() {
                ChunkOutcome {
                    index,
                    bytes: piece.len(),
                    ok: true,
                    text: piece,
                    error: String::new(),
                    status: None,
                    detail: String::new(),
                    attempts: 0,
                    ms: 0,
                    engine: String::new(),
                }
            } else {
                translate_chunk(&client, index, &piece, sl, &tl).await
            };
            translate_job_update(&job_id, |job| job.finished += 1);
            outcome
        }
    }))
    .buffered(TRANSLATE_CONCURRENCY)
    .collect::<Vec<ChunkOutcome>>();

    // 背景工作仍設上限，避免上游卡死時工作永遠停在 running。
    let outcomes = match tokio::time::timeout(
        std::time::Duration::from_secs(TRANSLATE_TOTAL_BUDGET_SECS),
        translate_all,
    )
    .await
    {
        Ok(list) => list,
        Err(_) => {
            tracing::warn!(
                trace_id = %trace_id,
                job_id = %job_id,
                chunks = total,
                budget_secs = TRANSLATE_TOTAL_BUDGET_SECS,
                "公告翻譯逾時（超過總預算）"
            );
            translate_job_update(&job_id, |job| {
                job.running = false;
                job.done = true;
                job.ok = false;
                job.code = "TRANSLATE_TIMEOUT".to_string();
                job.error = format!(
                    "翻譯逾時：{total} 段在 {TRANSLATE_TOTAL_BUDGET_SECS} 秒內未完成（上游 translate.googleapis.com 無回應或被限流）"
                );
            });
            return;
        }
    };

    let mut assembled = String::new();
    let mut diagnostics: Vec<Value> = Vec::new();
    let mut failures = 0usize;
    let mut translated_count = 0usize;

    for ((piece, sep), outcome) in units.iter().zip(outcomes.iter()) {
        if outcome.ok {
            if !piece.trim().is_empty() {
                translated_count += 1;
            }
            assembled.push_str(&outcome.text);
        } else {
            // 單段失敗就保留原文，不讓整篇翻譯化為烏有。
            failures += 1;
            assembled.push_str(piece);
            tracing::warn!(
                trace_id = %trace_id,
                job_id = %job_id,
                chunk = outcome.index + 1,
                total,
                bytes = outcome.bytes,
                attempts = outcome.attempts,
                elapsed_ms = outcome.ms,
                status = outcome.status.unwrap_or(0),
                error = %outcome.error,
                detail = %outcome.detail,
                "公告翻譯片段失敗"
            );
        }
        assembled.push_str(sep);

        if !outcome.ok || outcome.attempts > 1 {
            diagnostics.push(json!({
                "chunk": outcome.index + 1,
                "total": total,
                "bytes": outcome.bytes,
                "ok": outcome.ok,
                "status": outcome.status,
                "attempts": outcome.attempts,
                "ms": outcome.ms,
                "engine": outcome.engine,
                "error": outcome.error,
                "detail": outcome.detail,
            }));
        }
    }

    if translated_count == 0 {
        let reason = outcomes
            .iter()
            .find(|o| !o.ok)
            .map(|o| {
                format!(
                    "{}（狀態 {}，第 {} 段，重試 {} 次，{} ms）",
                    o.error,
                    o.status.map(|s| s.to_string()).unwrap_or_else(|| "無".into()),
                    o.index + 1,
                    o.attempts,
                    o.ms
                )
            })
            .unwrap_or_else(|| "未知原因".to_string());
        tracing::warn!(
            trace_id = %trace_id,
            job_id = %job_id,
            chunks = total,
            reason = %reason,
            "公告翻譯完全失敗"
        );
        translate_job_update(&job_id, |job| {
            job.running = false;
            job.done = true;
            job.ok = false;
            job.code = "TRANSLATE_UPSTREAM_FAILED".to_string();
            job.error = format!("翻譯失敗：{reason}");
            job.diagnostics = diagnostics;
        });
        return;
    }

    let translated = collapse_plain_text(&assembled);
    let partial = failures > 0;
    // 回報實際成功的引擎（多段可能用到不同引擎，去重後列出）。
    let mut engines: Vec<String> = Vec::new();
    for outcome in outcomes.iter() {
        if outcome.ok && !outcome.engine.is_empty() && !engines.contains(&outcome.engine) {
            engines.push(outcome.engine.clone());
        }
    }
    let engine_label = engines.join("＋");
    if partial {
        tracing::warn!(trace_id = %trace_id, job_id = %job_id, failed = failures, chunks = total, "公告翻譯部分失敗");
    } else {
        tracing::info!(trace_id = %trace_id, job_id = %job_id, chunks = total, "公告翻譯完成");
    }

    // 只有「完全成功」才快取；部分失敗的結果不該被記住。
    if !partial {
        translate_cache_put(&cache_key, &translated, &engine_label);
    }

    translate_job_update(&job_id, |job| {
        job.running = false;
        job.done = true;
        job.ok = true;
        job.engine = engine_label;
        job.translated = translated;
        job.partial = partial;
        job.note = if partial {
            format!("{failures}/{total} 段翻譯失敗，該段保留原文")
        } else {
            String::new()
        };
        job.diagnostics = diagnostics;
    });
}

/// 查詢背景翻譯工作的進度／結果。
/// GET /admin/api/bulletins/translate/status?job=xxxx
pub(super) async fn translate_status_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<TranslateStatusQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    let job_id = query.job.trim();
    if job_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "缺少 job 參數", "code": "TRANSLATE_JOB_MISSING" })),
        )
            .into_response();
    }
    let Some(job) = translate_job_get(job_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "找不到這個翻譯工作（可能已逾時被清除，請重新翻譯）",
                "code": "TRANSLATE_JOB_NOT_FOUND",
            })),
        )
            .into_response();
    };
    // 只有工作的擁有者或管理員可以查詢。
    if job.owner != session.account_id && !session.is_admin {
        return unauthorized_response();
    }

    (StatusCode::OK, Json(job.to_json(job_id))).into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct TranslateStatusQuery {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub job: String,
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
