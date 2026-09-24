//! STOCKGENIE_EMBED_V1：智選股獨立 sidecar（FastAPI @ 127.0.0.1:18765）。
//!
//! 瀏覽器只打 `/admin/stock`（頁面）與 `/admin/stock-api/*`（JSON 代理）。
//! 絕不能掛在頂層 `/api`——那是 iClass 反向代理（`is_tc_proxy_path`）。
//! 頁面含 innerHTML，故安全標頭中介層對這兩條路徑放行（不加
//! `require-trusted-types-for`），由本模組自行套較鬆的 CSP。
//!
//! sidecar 由 `ptero-start.sh` 在主機／容器啟動時拉起並監看；本模組先探測
//! 127.0.0.1:18765，已在就不重開。若部署端還沒換新啟動腳本，才 fallback spawn。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde_json::json;
use tokio::process::Command;
use tokio::sync::Mutex;

use super::{
    admin_login_redirect, must_change_password_redirect, same_origin_request, unauthorized_response,
    AdminQuery, AdminWebState,
};

const STOCK_SIDECAR_HOST: &str = "127.0.0.1";
const STOCK_SIDECAR_PORT: u16 = 18765;
const STOCK_PAGE_HTML: &str = include_str!("stockgenie/static/index.html");
const STOCK_API_BODY_LIMIT: usize = 256 * 1024;
const STOCK_UPSTREAM_TIMEOUT_SECS: u64 = 180;

static SIDECAR_LOCK: Mutex<()> = Mutex::const_new(());
static SIDECAR_READY: AtomicBool = AtomicBool::new(false);

fn stockgenie_python_candidates() -> Vec<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(env_py) = std::env::var("STOCKGENIE_PYTHON") {
        let trimmed = env_py.trim();
        if !trimmed.is_empty() {
            cands.push(PathBuf::from(trimmed));
        }
    }
    if let Ok(txt) = std::fs::read_to_string("run/stockgenie-python-bin.txt") {
        let trimmed = txt.trim();
        if !trimmed.is_empty() {
            cands.push(PathBuf::from(trimmed));
        }
    }
    let rel = [
        "run/stockgenie-venv/bin/python",
        "run/stockgenie-venv/bin/python3",
    ];
    if let Ok(cwd) = std::env::current_dir() {
        for r in rel {
            cands.push(cwd.join(r));
        }
        if let Ok(txt) = std::fs::read_to_string(cwd.join("run/stockgenie-python-bin.txt")) {
            let trimmed = txt.trim();
            if !trimmed.is_empty() {
                cands.push(cwd.join(trimmed));
                cands.push(PathBuf::from(trimmed));
            }
        }
    }
    for r in rel {
        cands.push(PathBuf::from(r));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            cands.push(parent.join("run/stockgenie-venv/bin/python"));
            if let Some(root) = parent.parent().and_then(|p| p.parent()) {
                cands.push(root.join("run/stockgenie-venv/bin/python"));
            }
        }
    }
    cands
}

fn resolve_stockgenie_python() -> Option<PathBuf> {
    for cand in stockgenie_python_candidates() {
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn bootstrap_script_path() -> Option<PathBuf> {
    let dir = stockgenie_dir()?;
    let script = dir.join("bootstrap.sh");
    if script.is_file() {
        Some(script)
    } else {
        None
    }
}

fn stockgenie_dir() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    cands.push(PathBuf::from("src/adapters/admin_web/stockgenie"));
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.join("src/adapters/admin_web/stockgenie"));
        cands.push(cwd.join("stockgenie"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            cands.push(parent.join("src/adapters/admin_web/stockgenie"));
            cands.push(parent.join("stockgenie"));
        }
    }
    cands.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/adapters/admin_web/stockgenie"));
    for cand in cands {
        if cand.join("app.py").is_file() {
            return Some(cand);
        }
    }
    None
}

fn stock_page_html() -> String {
    let runtime = [
        "src/adapters/admin_web/stockgenie/static/index.html",
        "stockgenie/static/index.html",
    ];
    for path in runtime {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    STOCK_PAGE_HTML.to_string()
}

fn sidecar_origin() -> String {
    format!("http://{STOCK_SIDECAR_HOST}:{STOCK_SIDECAR_PORT}")
}

fn is_safe_stock_api_rest(rest: &str) -> bool {
    if rest.is_empty() || rest.len() > 240 {
        return false;
    }
    if rest.contains("..") || rest.contains("//") || rest.starts_with('/') {
        return false;
    }
    rest.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '%' ))
}

fn drop_token_query(raw: Option<&str>) -> String {
    let Some(raw) = raw.filter(|v| !v.is_empty()) else {
        return String::new();
    };
    let mut kept: Vec<&str> = Vec::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let Some(key) = pair.split('=').next() else {
            continue;
        };
        if key.eq_ignore_ascii_case("token") {
            continue;
        }
        kept.push(pair);
    }
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    }
}

fn stock_html_response(html: String) -> Response {
    let mut response = Html(html).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
    // 智選前端用 innerHTML 畫表；不能套面板的 require-trusted-types-for。
    if let Ok(value) = HeaderValue::from_str(
        "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; font-src 'self' data:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'self'",
    ) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

async fn spawn_uvicorn(dir: &std::path::Path, bin: &std::path::Path) -> Result<tokio::process::Child, String> {
    Command::new(bin)
        .args([
            "-m",
            "uvicorn",
            "app:app",
            "--host",
            STOCK_SIDECAR_HOST,
            "--port",
            "18765",
            "--log-level",
            "warning",
        ])
        .current_dir(dir)
        .env("PYTHONUNBUFFERED", "1")
        .env("STOCKGENIE_EMBED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(false)
        .spawn()
        .map_err(|e| format!("無法啟動 {}：{e}", bin.display()))
}

async fn bootstrap_stockgenie_python() -> Result<PathBuf, String> {
    if let Some(existing) = resolve_stockgenie_python() {
        return Ok(existing);
    }
    let script = bootstrap_script_path().ok_or_else(|| {
        "容器沒有 python3，且找不到 stockgenie/bootstrap.sh".to_string()
    })?;
    let _ = std::fs::create_dir_all("logs");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("logs/stockgenie-bootstrap.log");
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .stdin(Stdio::null())
        .kill_on_drop(false);
    match log {
        Ok(file) => match file.try_clone() {
            Ok(clone) => {
                cmd.stdout(Stdio::from(clone)).stderr(Stdio::from(file));
            }
            Err(_) => {
                cmd.stdout(Stdio::from(file)).stderr(Stdio::null());
            }
        },
        Err(_) => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    tracing::info!("stockgenie bootstrap starting");
    let status = cmd
        .status()
        .await
        .map_err(|e| format!("無法執行 bootstrap.sh：{e}"))?;
    if !status.success() {
        return Err(
            "智選 Python 環境安裝失敗（容器沒有系統 python3；詳見 logs/stockgenie-bootstrap.log）"
                .to_string(),
        );
    }
    resolve_stockgenie_python().ok_or_else(|| {
        "bootstrap 完成後仍找不到 run/stockgenie-venv/bin/python".to_string()
    })
}

async fn wait_sidecar_health() -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let url = format!("{}/api/health", sidecar_origin());
    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success() || resp.status().as_u16() == 202,
        Err(_) => false,
    }
}

async fn start_stock_sidecar() -> Result<(), String> {
    // ptero-start 可能已經聽在 18765；先確認，避免搶 port。
    if wait_sidecar_health().await {
        return Ok(());
    }
    let dir = stockgenie_dir().ok_or_else(|| {
        "找不到智選程式（src/adapters/admin_web/stockgenie/app.py）".to_string()
    })?;
    let py = match resolve_stockgenie_python() {
        Some(p) => p,
        None => bootstrap_stockgenie_python().await?,
    };
    let mut child = spawn_uvicorn(&dir, &py).await?;
    for _ in 0..40 {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!("智選服務立即結束（{status}）"));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("無法確認智選行程：{e}")),
        }
        if wait_sidecar_health().await {
            tokio::spawn(async move {
                let _ = child.wait().await;
                SIDECAR_READY.store(false, Ordering::SeqCst);
                tracing::warn!("stockgenie sidecar exited");
            });
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // 行程還在、只是 health 較慢（首次掃描）——仍視為已啟動。
    tokio::spawn(async move {
        let _ = child.wait().await;
        SIDECAR_READY.store(false, Ordering::SeqCst);
        tracing::warn!("stockgenie sidecar exited");
    });
    Ok(())
}

pub(super) async fn ensure_stock_sidecar() -> Result<(), String> {
    if SIDECAR_READY.load(Ordering::SeqCst) {
        return Ok(());
    }
    let _guard = SIDECAR_LOCK.lock().await;
    if SIDECAR_READY.load(Ordering::SeqCst) {
        return Ok(());
    }
    if wait_sidecar_health().await {
        SIDECAR_READY.store(true, Ordering::SeqCst);
        return Ok(());
    }
    start_stock_sidecar().await?;
    SIDECAR_READY.store(true, Ordering::SeqCst);
    Ok(())
}

/// GET /admin/stock
pub(super) async fn admin_stock_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/stock");
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(
            must_change_password_redirect(),
            &page,
            &headers,
        );
    }
    tokio::spawn(async {
        if let Err(e) = ensure_stock_sidecar().await {
            tracing::warn!(error = %e, "stockgenie sidecar start failed");
        }
    });
    super::pages::attach_fresh_cookies(stock_html_response(stock_page_html()), &page, &headers)
}

/// ANY /admin/stock-api/{*path} → 127.0.0.1:18765/api/{*path}
pub(super) async fn admin_stock_api_proxy(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Path(rest): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let Some(_session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    let method = request.method().clone();
    let mutating = method == Method::POST
        || method == Method::PUT
        || method == Method::PATCH
        || method == Method::DELETE;
    if mutating && !same_origin_request(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "cross-origin request rejected" })),
        )
            .into_response();
    }
    if !is_safe_stock_api_rest(&rest) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid stock api path" })),
        )
            .into_response();
    }
    if let Err(e) = ensure_stock_sidecar().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": e })),
        )
            .into_response();
    }
    let qs = drop_token_query(request.uri().query());
    let upstream_url = format!("{}/api/{}{}", sidecar_origin(), rest, qs);
    let upstream_method = match reqwest::Method::from_bytes(method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                Json(json!({ "error": "method not allowed" })),
            )
                .into_response();
        }
    };
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned();
    let body = match axum::body::to_bytes(request.into_body(), STOCK_API_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({ "error": "request body too large" })),
            )
                .into_response();
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(STOCK_UPSTREAM_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("http client: {e}") })),
            )
                .into_response();
        }
    };
    let mut builder = client.request(upstream_method, &upstream_url);
    if let Some(ct) = content_type {
        if let Ok(raw) = ct.to_str() {
            builder = builder.header(reqwest::header::CONTENT_TYPE, raw);
        }
    }
    if !body.is_empty() {
        builder = builder.body(body.to_vec());
    }
    let upstream = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            SIDECAR_READY.store(false, Ordering::SeqCst);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("智選服務連線失敗：{e}") })),
            )
                .into_response();
        }
    };
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_ct = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("智選回應讀取失敗：{e}") })),
            )
                .into_response();
        }
    };
    let mut response = (status, Bytes::from(bytes.to_vec())).into_response();
    if let Some(ct) = resp_ct {
        if let Ok(value) = HeaderValue::from_str(&ct) {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }
    response
}
