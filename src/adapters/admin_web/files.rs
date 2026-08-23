//! Materials file-manager page/API handlers for Admin Web.

use super::*;
use std::process::Stdio;
use std::sync::Weak;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

#[derive(Debug, Deserialize)]
pub(super) struct AdminFilesQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminFilesHistoryQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminFileDeleteRequest {
    path: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminFolderCreateRequest {
    path: String,
}

const MATERIALS_LIST_CACHE_TTL: Duration = Duration::from_secs(3);
const MAX_MULTIPART_FIELDS: usize = 32;
const MAX_UPLOAD_FILES: usize = 16;
const MAX_MATERIAL_FILENAME_BYTES: usize = 255;
const OFFICE_PREVIEW_CONCURRENCY: usize = 2;
const OFFICE_PREVIEW_MAX_PENDING: usize = 64;
const OFFICE_PREVIEW_TIMEOUT: Duration = Duration::from_secs(90);
// Serialize uploads so the 10 GiB quota check cannot be bypassed by two
// simultaneous requests observing the same pre-upload size.
const MATERIAL_UPLOAD_CONCURRENCY: usize = 1;
const FILEBROWSER_USER_PORT: u16 = 26102;
const FILEBROWSER_ADMIN_PORT: u16 = 26103;

static MATERIAL_UPLOAD_SEMAPHORE: once_cell::sync::Lazy<Semaphore> =
    once_cell::sync::Lazy::new(|| Semaphore::new(MATERIAL_UPLOAD_CONCURRENCY));
static OFFICE_PREVIEW_PENDING_SEMAPHORE: once_cell::sync::Lazy<Semaphore> =
    once_cell::sync::Lazy::new(|| Semaphore::new(OFFICE_PREVIEW_MAX_PENDING));
static OFFICE_PREVIEW_SEMAPHORE: once_cell::sync::Lazy<Semaphore> =
    once_cell::sync::Lazy::new(|| Semaphore::new(OFFICE_PREVIEW_CONCURRENCY));
static OFFICE_PREVIEW_LOCKS: once_cell::sync::Lazy<
    tokio::sync::Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>,
> = once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(HashMap::new()));

#[derive(Clone)]
struct MaterialsListSnapshot {
    root: PathBuf,
    updated_at: Instant,
    files: Vec<MaterialFileEntry>,
    folders: Vec<String>,
}

fn session_materials_root(session: &AdminSession) -> PathBuf {
    let root = materials_root();
    if session.is_admin {
        root
    } else {
        // Keep regular Dashboard users inside the shared教材 directory, in
        // line with the Filebrowser `/教材` scope.
        root.join("教材")
    }
}

static MATERIALS_LIST_CACHE: once_cell::sync::Lazy<
    tokio::sync::Mutex<Option<MaterialsListSnapshot>>,
> = once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(None));

async fn invalidate_materials_list_cache() {
    *MATERIALS_LIST_CACHE.lock().await = None;
}

async fn cached_materials_list(
    root: PathBuf,
) -> std::result::Result<(Vec<MaterialFileEntry>, Vec<String>), Response> {
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) => return Err(material_path_error(&error, "materials folder not found")),
    };
    {
        let cache = MATERIALS_LIST_CACHE.lock().await;
        if let Some(snapshot) = cache.as_ref() {
            if snapshot.root == root && snapshot.updated_at.elapsed() <= MATERIALS_LIST_CACHE_TTL {
                return Ok((snapshot.files.clone(), snapshot.folders.clone()));
            }
        }
    }

    // CLOUD_LIST_CACHE_SINGLE_SCAN_V1: scan files and folders in one blocking
    // traversal, then reuse the result for short bursts from the files page.
    let scan_root = root.clone();
    let scan = tokio::task::spawn_blocking(move || list_material_tree(&scan_root))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("教材列表背景掃描失敗：{e}") })),
            )
                .into_response()
        })?
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("讀取教材列表失敗：{e}") })),
            )
                .into_response()
        })?;

    let snapshot = MaterialsListSnapshot {
        root,
        updated_at: Instant::now(),
        files: scan.0.clone(),
        folders: scan.1.clone(),
    };
    *MATERIALS_LIST_CACHE.lock().await = Some(snapshot);
    Ok(scan)
}

const MATERIAL_STREAM_CHUNK_SIZE: usize = 64 * 1024;

async fn stream_material_file_response(
    path: PathBuf,
    disposition_prefix: &str,
    cache_control: Option<&str>,
    read_error_label: &str,
    build_error_label: &str,
) -> Response {
    // CLOUD_FILE_STREAMING_V1: avoid loading downloads/previews into a single
    // Vec<u8>. Stream 64KB chunks from tokio::fs::File so large PDFs/videos do
    // not spike memory usage per request.
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("{read_error_label}：{e}") })),
            )
                .into_response();
        }
    };
    let content_len = file.metadata().await.ok().map(|meta| meta.len());
    let content_type = material_content_type(&path);
    let file_name = path.file_name().and_then(|v| v.to_str()).unwrap_or("file");
    let disposition = format!(
        "{}; filename=\"{}\"",
        disposition_prefix,
        sanitize_header_filename(file_name)
    );

    let stream = futures::stream::try_unfold(file, |mut file| async move {
        let mut chunk = vec![0u8; MATERIAL_STREAM_CHUNK_SIZE];
        let read = file.read(&mut chunk).await?;
        if read == 0 {
            Ok::<Option<(Vec<u8>, tokio::fs::File)>, std::io::Error>(None)
        } else {
            chunk.truncate(read);
            Ok::<Option<(Vec<u8>, tokio::fs::File)>, std::io::Error>(Some((chunk, file)))
        }
    });

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_DISPOSITION, disposition);
    if let Some(cache_control) = cache_control {
        builder = builder.header(header::CACHE_CONTROL, cache_control);
    }
    if let Some(content_len) = content_len {
        builder = builder.header(header::CONTENT_LENGTH, content_len.to_string());
    }

    builder
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_else(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("{build_error_label}：{e}") })),
            )
                .into_response()
        })
}

pub(super) async fn record_material_event(
    state: &AdminWebState,
    actor: &str,
    action: &str,
    path: &str,
    folder: &str,
    file_name: &str,
    size: u64,
    result: &str,
    details: &str,
) {
    let _ = state
        .accounts_db
        .record_material_event(MaterialEventInput {
            actor: actor.to_string(),
            action: action.to_string(),
            path: path.to_string(),
            folder: folder.to_string(),
            file_name: file_name.to_string(),
            size: size.min(i64::MAX as u64) as i64,
            result: result.to_string(),
            details: details.to_string(),
        })
        .await;
}

fn material_path_error(error: &std::io::Error, missing_message: &str) -> Response {
    let (status, message) = match error.kind() {
        std::io::ErrorKind::PermissionDenied => (
            StatusCode::FORBIDDEN,
            "invalid materials path",
        ),
        std::io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, missing_message),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "materials storage unavailable",
        ),
    };
    (status, Json(json!({ "error": message }))).into_response()
}

fn secure_existing_material_path_response(
    root: &Path,
    rel: &Path,
    missing_message: &str,
) -> std::result::Result<PathBuf, Response> {
    secure_existing_material_path(root, rel).map_err(|error| material_path_error(&error, missing_message))
}

pub(super) async fn admin_files_page_handler(headers: HeaderMap) -> Response {
    super::html_page_response(&headers, super::pages::inject_theme(ADMIN_FILES_HTML))
}

/// Serve Filebrowser's HTML shell with a same-origin link back to Dashboard.
///
/// Filebrowser is an external Go binary and only supports custom CSS branding.
/// This small GET-only bridge keeps the Pterodactyl deployment self-contained:
/// Caddy sends the initial `/files` document here, while all static assets,
/// APIs, uploads and downloads continue to go directly to Filebrowser.
pub(super) async fn admin_filebrowser_shell_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminFilesQuery>,
    headers: HeaderMap,
) -> Response {
    // The Dashboard session cookie is host-only by design. When Caddy sends
    // the Filebrowser request here, the Caddy-only secret is the proof that
    // `forward_auth` already validated the live Dashboard session.
    let dashboard_session = state.authorize(&query.token).await;
    let trusted_caddy = super::fb_sso::shell_request_secret_valid(&headers);
    if dashboard_session.is_none() && !trusted_caddy {
        return unauthorized_response();
    }
    let is_admin = dashboard_session
        .as_ref()
        .map(|session| session.is_admin)
        .or_else(|| {
            trusted_caddy.then(|| {
                headers
                    .get("x-fb-admin")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.eq_ignore_ascii_case("yes"))
            })
        })
        .unwrap_or(false);

    let backend = headers
        .get("x-fb-backend")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("user")
        .trim()
        .to_ascii_lowercase();
    let port = if backend == "admin" {
        if !is_admin {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "admin Filebrowser backend requires an admin Dashboard session" })),
            )
                .into_response();
        }
        FILEBROWSER_ADMIN_PORT
    } else {
        FILEBROWSER_USER_PORT
    };

    let upstream_url = format!("http://127.0.0.1:{port}/files");
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("建立 Filebrowser shell client 失敗：{error}") })),
            )
                .into_response();
        }
    };

    let upstream = match client
        .get(&upstream_url)
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("讀取 Filebrowser shell 失敗：{error}") })),
            )
                .into_response();
        }
    };

    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/html; charset=utf-8")
        .to_string();
    let body = match upstream.bytes().await {
        Ok(body) => body,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("讀取 Filebrowser shell body 失敗：{error}") })),
            )
                .into_response();
        }
    };

    let is_html = content_type.to_ascii_lowercase().contains("text/html");
    let nonce = is_html.then(|| super::generate_csp_nonce());
    let body = if is_html {
        let body = inject_dashboard_link(body.as_ref());
        let mut text = String::from_utf8_lossy(&body).into_owned();
        inject_filebrowser_trusted_types_assets(&mut text);
        super::add_csp_nonce_to_inline_tags(&text, nonce.as_deref().unwrap_or_default()).into_bytes()
    } else {
        body.to_vec()
    };

    let mut builder = Response::builder()
        .status(status.as_u16())
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, body.len().to_string())
        .header(header::CACHE_CONTROL, "no-store");
    if let Some(nonce) = nonce.as_deref() {
        if let Ok(value) = super::csp_header_value(Some(nonce)).parse::<HeaderValue>() {
            builder = builder.header(header::CONTENT_SECURITY_POLICY, value);
        }
    }
    builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("建立 Filebrowser shell response 失敗：{error}") })),
            )
                .into_response()
        })
}

fn is_safe_public_share_path(value: &str) -> bool {
    if value.len() > ADMIN_MAX_PATH_BYTES
        || !value.starts_with("/share/")
        || value.chars().any(|character| character.is_control())
    {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    if lower.contains("%2e") || lower.contains("%2f") || lower.contains("%5c") {
        return false;
    }
    let path_only = value.split('?').next().unwrap_or_default();
    !path_only
        .split('/')
        .any(|component| component == "." || component == "..")
        && !path_only.contains('\\')
}

/// Serve a public share page while replacing Filebrowser's built-in login
/// action with the Dashboard login. Public shares intentionally bypass SSO,
/// so this handler authenticates the Caddy hop with the shared secret rather
/// than requiring a Dashboard session.
pub(super) async fn admin_filebrowser_share_shell_handler(headers: HeaderMap) -> Response {
    if !super::fb_sso::shell_request_secret_valid(&headers) {
        return unauthorized_response();
    }

    let share_path = headers
        .get("x-fb-share-path")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !is_safe_public_share_path(share_path) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid public share path" })),
        )
            .into_response();
    }

    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(_) => return unauthorized_response(),
    };
    let upstream_url = format!("http://127.0.0.1:{FILEBROWSER_USER_PORT}{share_path}");
    let upstream = match client
        .get(&upstream_url)
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "public share unavailable" })),
            )
                .into_response();
        }
    };

    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("text/html; charset=utf-8")
        .to_string();
    let body = match upstream.bytes().await {
        Ok(body) => body,
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "public share response unavailable" })),
            )
                .into_response();
        }
    };
    let is_html = content_type.to_ascii_lowercase().contains("text/html");
    let nonce = is_html.then(|| super::generate_csp_nonce());
    let body = if is_html {
        let body = inject_share_login_redirect(body.as_ref());
        let mut text = String::from_utf8_lossy(&body).into_owned();
        inject_filebrowser_trusted_types_assets(&mut text);
        super::add_csp_nonce_to_inline_tags(&text, nonce.as_deref().unwrap_or_default()).into_bytes()
    } else {
        body.to_vec()
    };

    let mut builder = Response::builder()
        .status(status.as_u16())
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, body.len().to_string())
        .header(header::CACHE_CONTROL, "no-store");
    if let Some(nonce) = nonce.as_deref() {
        if let Ok(value) = super::csp_header_value(Some(nonce)).parse::<HeaderValue>() {
            builder = builder.header(header::CONTENT_SECURITY_POLICY, value);
        }
    }
    builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| unauthorized_response())
}

fn inject_share_login_redirect(body: &[u8]) -> Vec<u8> {
    let mut html = String::from_utf8_lossy(body).into_owned();
    if html.contains("tronclass-dashboard-share-login-v1") {
        return html.into_bytes();
    }

    let dashboard_url = std::env::var("TRONCLASS_DASHBOARD_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://dashboard.jasoncloudserver.me/admin".to_string());
    let dashboard_url = serde_json::to_string(&dashboard_url)
        .unwrap_or_else(|_| "\"https://dashboard.jasoncloudserver.me/admin\"".to_string())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let script = format!(
        r#"
<script id="tronclass-dashboard-share-login-script-v1" data-marker="tronclass-dashboard-share-login-v1">
(function () {{
  var dashboard = {dashboard_url};
  function isLoginTarget(value) {{
    var href = String(value || '').trim();
    if (href === '/login' || href.indexOf('/login?') === 0 ||
        href === '/#/login' || href.indexOf('/#/login?') === 0 ||
        href === '#/login' || href.indexOf('#/login?') === 0) return true;
    try {{
      var parsed = new URL(href, location.origin);
      return parsed.origin === location.origin &&
        (parsed.pathname === '/login' || parsed.pathname.indexOf('/login/') === 0);
    }} catch (_) {{ return false; }}
  }}

  function isLoginLocation() {{
    var path = String(location.pathname || '');
    var hash = String(location.hash || '');
    return path === '/login' || path.indexOf('/login/') === 0 ||
      hash === '#/login' || hash.indexOf('#/login?') === 0 ||
      hash.indexOf('#/login/') === 0;
  }}

  function redirectIfLoginRoute() {{
    if (isLoginLocation()) window.location.replace(dashboard);
  }}
  function isLoginControl(element) {{
    if (!element) return false;
    var href = element.getAttribute && element.getAttribute('href');
    var text = (element.textContent || '').trim().toLowerCase();
    var label = ((element.getAttribute && (element.getAttribute('aria-label') || element.getAttribute('title'))) || '').toLowerCase();
    return isLoginTarget(href) || text.indexOf('登入') >= 0 ||
      text.indexOf('login') >= 0 || label.indexOf('登入') >= 0 || label.indexOf('login') >= 0;
  }}
  function loginControlFromEvent(event) {{
    var node = event && event.target;
    while (node && node !== document) {{
      if (node.nodeType === 1 && isLoginControl(node)) return node;
      node = node.parentNode;
    }}
    return null;
  }}
  function redirectToDashboard(event) {{
    var control = loginControlFromEvent(event);
    if (!isLoginControl(control)) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    window.location.replace(dashboard);
  }}
  function patchLoginLinks() {{
    document.querySelectorAll('a,button,[role="button"]').forEach(function (element) {{
      if (!isLoginControl(element)) return;
      if (element.hasAttribute && element.hasAttribute('href')) element.setAttribute('href', dashboard);
      element.setAttribute('title', '前往 Tronclass Dashboard 登入');
    }});
  }}
  document.addEventListener('pointerdown', redirectToDashboard, true);
  document.addEventListener('click', redirectToDashboard, true);
  ['pushState', 'replaceState'].forEach(function (method) {{
    var original = window.history[method];
    if (typeof original !== 'function') return;
    window.history[method] = function () {{
      var result = original.apply(this, arguments);
      setTimeout(redirectIfLoginRoute, 0);
      return result;
    }};
  }});
  window.addEventListener('popstate', redirectIfLoginRoute);
  window.addEventListener('hashchange', redirectIfLoginRoute);
  redirectIfLoginRoute();
  patchLoginLinks();
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', patchLoginLinks);
  setTimeout(patchLoginLinks, 100);
  setTimeout(patchLoginLinks, 500);
  setInterval(redirectIfLoginRoute, 100);
  try {{ new MutationObserver(patchLoginLinks).observe(document.documentElement, {{childList:true, subtree:true}}); }} catch (_) {{}}
}})();
</script>
"#
    );

    let lower = html.to_ascii_lowercase();
    if let Some(index) = lower.rfind("</head>") {
        html.insert_str(index, &script);
    } else if let Some(index) = lower.rfind("</body>") {
        html.insert_str(index, &script);
    } else {
        html.push_str(&script);
    }
    html.into_bytes()
}

fn inject_filebrowser_trusted_types_assets(html: &mut String) {
    if html.contains("trusted-types-init.js") {
        return;
    }
    let assets = concat!(
        "<script src=\"/admin/static/js/dompurify.min.js?v=20260823trustedtypes\"></script>",
        "<script src=\"/admin/static/js/trusted-types-init.js?v=20260823trustedtypes\"></script>"
    );
    let lower = html.to_ascii_lowercase();
    if let Some(index) = lower.rfind("</head>") {
        html.insert_str(index, assets);
    } else {
        html.insert_str(0, assets);
    }
}

fn inject_dashboard_link(body: &[u8]) -> Vec<u8> {
    let mut html = String::from_utf8_lossy(body).into_owned();
    if html.contains("tronclass-dashboard-back-v1") {
        return html.into_bytes();
    }

    let dashboard_url = std::env::var("TRONCLASS_DASHBOARD_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://dashboard.jasoncloudserver.me/admin/dashboard".to_string());
    let dashboard_url = serde_json::to_string(&dashboard_url)
        .unwrap_or_else(|_| "\"https://dashboard.jasoncloudserver.me/admin/dashboard\"".to_string())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let script = format!(
        r#"
<script id="tronclass-dashboard-back-script-v1" data-marker="tronclass-dashboard-back-v1">
(function () {{
  if (document.getElementById('tronclass-dashboard-back')) return;
  var link = document.createElement('a');
  link.id = 'tronclass-dashboard-back';
  link.href = {dashboard_url};
  link.textContent = '回到 Dashboard';
  link.title = '回到 Tronclass Dashboard';
  link.setAttribute('aria-label', '回到 Tronclass Dashboard');
  link.setAttribute('rel', 'noopener');
  // Set individual CSS properties instead of cssText so the strict CSP's
  // style-src-attr policy does not block this trusted UI control.
  link.style.position = 'fixed';
  link.style.zIndex = '2147483647';
  link.style.top = '10px';
  link.style.left = '500px';
  link.style.display = 'flex';
  link.style.alignItems = 'center';
  link.style.justifyContent = 'center';
  link.style.minHeight = '40px';
  link.style.boxSizing = 'border-box';
  link.style.padding = '0 16px';
  link.style.borderRadius = '8px';
  link.style.border = '1px solid rgba(55,71,79,.22)';
  link.style.background = '#ffffff';
  link.style.color = '#37474f';
  link.style.cursor = 'pointer';
  link.style.font = '600 14px/1.2 system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif';
  link.style.textDecoration = 'none';
  link.style.boxShadow = '0 2px 8px rgba(0,0,0,.14)';
  link.style.whiteSpace = 'nowrap';
  link.style.transition = 'background .15s,color .15s,transform .15s';
  link.addEventListener('mouseenter', function () {{
    link.style.background = '#e8f4ff';
    link.style.color = '#1565c0';
  }});
  link.addEventListener('mouseleave', function () {{
    link.style.background = '#ffffff';
    link.style.color = '#37474f';
  }});
  link.addEventListener('mousedown', function () {{ link.style.transform = 'translateY(1px)'; }});
  link.addEventListener('mouseup', function () {{ link.style.transform = 'none'; }});
  document.body.appendChild(link);
  var style = document.createElement('style');
  if (document.currentScript && document.currentScript.nonce) style.setAttribute('nonce', document.currentScript.nonce);
  style.textContent = '@media (max-width: 720px) {{ #tronclass-dashboard-back {{ left:64px !important; top:8px !important; min-height:36px !important; padding:0 10px !important; font-size:12px !important; }} }}';
  document.head.appendChild(style);
}})();
</script>
"#
    );

    if let Some(index) = html.to_ascii_lowercase().rfind("</body>") {
        html.insert_str(index, &script);
    } else {
        html.push_str(&script);
    }
    html.into_bytes()
}

pub(super) async fn admin_files_list_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminFilesQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };

    let root = session_materials_root(&session);
    if let Err(e) = std::fs::create_dir_all(&root) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("建立 materials 目錄失敗：{e}") })),
        )
            .into_response();
    }

    let (files, folders) = match cached_materials_list(root).await {
        Ok(list) => list,
        Err(resp) => return resp,
    };

    (
        StatusCode::OK,
        Json(json!({
            "is_admin": session.is_admin,
            "max_file_size_mb": null,
            "max_file_size_unlimited": true,
            "allowed_extensions": MATERIAL_ALLOWED_EXTENSIONS,
            "folders": folders,
            "files": files,
        })),
    )
        .into_response()
}

pub(super) async fn admin_files_download_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminFilesQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) =
        validate_optional_admin_text(query.path.as_deref(), ADMIN_MAX_PATH_BYTES, "path")
    {
        return response;
    }

    let Some(rel) = query
        .path
        .as_deref()
        .and_then(safe_relative_path)
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid path" })),
        )
            .into_response();
    };

    let root = session_materials_root(&session);
    let full_path = match secure_existing_material_path_response(&root, &rel, "file not found") {
        Ok(path) => path,
        Err(response) => return response,
    };
    if !full_path.is_file() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "file not found" })),
        )
            .into_response();
    }

    stream_material_file_response(
        full_path,
        "attachment",
        None,
        "讀取檔案失敗",
        "建立下載回應失敗",
    )
    .await
}

fn material_total_size_for_upload(root: &std::path::Path) -> std::io::Result<u64> {
    fn walk(path: &std::path::Path, total: &mut u64) -> std::io::Result<()> {
        if !path.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                walk(&entry.path(), total)?;
            } else if file_type.is_file() {
                *total = total.saturating_add(entry.metadata()?.len());
            }
        }
        Ok(())
    }
    let mut total = 0;
    walk(root, &mut total)?;
    Ok(total)
}

pub(super) async fn admin_files_preview_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminFilesQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) =
        validate_optional_admin_text(query.path.as_deref(), ADMIN_MAX_PATH_BYTES, "path")
    {
        return response;
    }

    let Some(rel) = query
        .path
        .as_deref()
        .and_then(safe_relative_path)
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid path" })),
        )
            .into_response();
    };

    let root = session_materials_root(&session);
    let full_path = match secure_existing_material_path_response(&root, &rel, "file not found") {
        Ok(path) => path,
        Err(response) => return response,
    };
    if !full_path.is_file() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "file not found" })),
        )
            .into_response();
    }

    let ext = full_path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let response_path_buf;
    let response_path: &std::path::Path = if matches!(ext.as_str(), "ppt" | "pptx") {
        match ensure_office_pdf_preview(&full_path).await {
            Ok(path) => {
                response_path_buf = path;
                response_path_buf.as_path()
            }
            Err(resp) => return resp,
        }
    } else if matches!(
        ext.as_str(),
        "pdf" | "jpg" | "jpeg" | "png" | "txt" | "md" | "mp4"
    ) {
        full_path.as_path()
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "此檔案格式不支援預覽，請下載查看" })),
        )
            .into_response();
    };

    stream_material_file_response(
        response_path.to_path_buf(),
        "inline",
        Some("no-store"),
        "讀取預覽檔失敗",
        "建立預覽回應失敗",
    )
    .await
}

async fn office_preview_lock(cache_path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = OFFICE_PREVIEW_LOCKS.lock().await;
    locks.retain(|_, weak| weak.strong_count() > 0);
    if let Some(lock) = locks.get(cache_path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(cache_path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

async fn ensure_office_pdf_preview(source: &Path) -> Result<PathBuf, Response> {
    let cache_dir = materials_preview_cache_root();
    if let Err(e) = tokio::fs::create_dir_all(&cache_dir).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("建立預覽快取失敗：{e}") })),
        )
            .into_response());
    }

    let meta = match tokio::fs::metadata(source).await {
        Ok(meta) => meta,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("讀取原始檔資訊失敗：{e}") })),
            )
                .into_response());
        }
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let stem = source
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("preview")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .take(80)
        .collect::<String>();
    let mut cache_hasher = Sha256::new();
    cache_hasher.update(source.to_string_lossy().as_bytes());
    cache_hasher.update(meta.len().to_le_bytes());
    cache_hasher.update(modified.to_le_bytes());
    let cache_key = hex::encode(cache_hasher.finalize());
    let cache_path = cache_dir.join(format!("{stem}_{}.pdf", &cache_key[..32]));
    if cache_path.is_file() {
        return Ok(cache_path);
    }

    // Bound both converters and same-key waiters. A semaphore by itself limits
    // child processes but otherwise has an unbounded FIFO of waiting requests.
    let _pending_permit = OFFICE_PREVIEW_PENDING_SEMAPHORE
        .try_acquire()
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": "too many pending Office preview requests" })),
            )
                .into_response()
        })?;

    // Requests for the same exact source revision share one conversion. The
    // waiter rechecks the cache after acquiring the key lock.
    let key_lock = office_preview_lock(&cache_path).await;
    let _key_guard = key_lock.lock().await;
    if cache_path.is_file() {
        return Ok(cache_path);
    }

    // Different documents may convert concurrently, but only up to this global
    // process budget. Waiting requests do not spawn LibreOffice processes.
    let _permit = OFFICE_PREVIEW_SEMAPHORE.acquire().await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Office preview service is shutting down" })),
        )
            .into_response()
    })?;
    if cache_path.is_file() {
        return Ok(cache_path);
    }

    let conversion_root =
        cache_dir
            .join(".work")
            .join(format!("{}-{}", &cache_key[..16], Uuid::new_v4().simple()));
    if let Err(e) = tokio::fs::create_dir_all(&conversion_root).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("建立隔離轉檔目錄失敗：{e}") })),
        )
            .into_response());
    }

    let programs = ["/usr/bin/libreoffice", "libreoffice", "soffice"];
    let mut last_error = String::new();
    for (attempt, program) in programs.into_iter().enumerate() {
        let out_dir = conversion_root.join(format!("attempt-{attempt}"));
        let profile_dir = out_dir.join("profile");
        if let Err(e) = tokio::fs::create_dir_all(&profile_dir).await {
            last_error = format!("建立 LibreOffice 隔離目錄失敗：{e}");
            continue;
        }
        let Some(profile_url) = url::Url::from_directory_path(&profile_dir).ok() else {
            last_error = "無法建立 LibreOffice 隔離設定路徑".to_string();
            continue;
        };

        let mut command = tokio::process::Command::new(program);
        command
            .arg("--headless")
            .arg(format!("-env:UserInstallation={profile_url}"))
            .arg("--convert-to")
            .arg("pdf")
            .arg("--outdir")
            .arg(&out_dir)
            .arg(source)
            // Never retain potentially unbounded child output in memory.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Cancellation of this handler must not orphan LibreOffice.
            .kill_on_drop(true);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                last_error = format!("{program}: {e}");
                continue;
            }
        };

        let status = match tokio::time::timeout(OFFICE_PREVIEW_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(e)) => {
                let kill_error = child.start_kill().err();
                let reap_error = child.wait().await.err();
                last_error = format!(
                    "{program}: wait failed: {e}; kill={}; reap={}",
                    kill_error
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "ok".to_string()),
                    reap_error
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "ok".to_string())
                );
                None
            }
            Err(_) => {
                // timeout() cancels only the wait future. Explicitly kill and
                // then wait again so the OS process is always reaped.
                let kill_error = child.start_kill().err();
                let reap_error = child.wait().await.err();
                last_error = match (kill_error, reap_error) {
                    (None, None) => format!("{program}: timeout after 90 seconds"),
                    (kill, reap) => format!(
                        "{program}: timeout; kill={}; reap={}",
                        kill.map(|e| e.to_string())
                            .unwrap_or_else(|| "ok".to_string()),
                        reap.map(|e| e.to_string())
                            .unwrap_or_else(|| "ok".to_string())
                    ),
                };
                None
            }
        };

        if status.as_ref().is_some_and(|status| status.success()) {
            let produced = out_dir.join(format!(
                "{}.pdf",
                source
                    .file_stem()
                    .and_then(|v| v.to_str())
                    .unwrap_or("preview")
            ));
            if produced.is_file() {
                match tokio::fs::rename(&produced, &cache_path).await {
                    Ok(()) => {
                        let _ = tokio::fs::remove_dir_all(&conversion_root).await;
                        return Ok(cache_path);
                    }
                    Err(_) if cache_path.is_file() => {
                        let _ = tokio::fs::remove_dir_all(&conversion_root).await;
                        return Ok(cache_path);
                    }
                    Err(e) => last_error = format!("移動 LibreOffice 預覽輸出失敗：{e}"),
                }
            } else {
                last_error = "LibreOffice 轉檔完成但找不到 PDF 輸出".to_string();
            }
        } else if let Some(status) = status {
            last_error = format!("{program} exit status {status}");
        }
    }

    let _ = tokio::fs::remove_dir_all(&conversion_root).await;
    Err((
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": format!("PPT/PPTX 預覽需要 LibreOffice 轉 PDF，轉檔失敗：{last_error}。可先執行 sudo apt install -y libreoffice")
        })),
    )
        .into_response())
}

pub(super) async fn admin_files_upload_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    mut multipart: Multipart,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    let _upload_permit = match MATERIAL_UPLOAD_SEMAPHORE.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": "已有其他檔案正在上傳，請稍後再試" })),
            )
                .into_response();
        }
    };
    let root = session_materials_root(&session);
    if let Err(e) = tokio::fs::create_dir_all(&root).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("建立 materials 目錄失敗：{e}") })),
        )
            .into_response();
    }

    let mut folder = PathBuf::new();
    let mut saved_files = Vec::new();
    let mut current_materials_size: Option<u64> = None;
    let mut field_count = 0usize;
    let mut file_count = 0usize;

    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("讀取 multipart 失敗：{e}") })),
                )
                    .into_response();
            }
        };
        field_count = field_count.saturating_add(1);
        if field_count > MAX_MULTIPART_FIELDS {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("too many multipart fields; max {MAX_MULTIPART_FIELDS}")
                })),
            )
                .into_response();
        }
        let field_name = field.name().unwrap_or_default();
        if let Err(response) = validate_admin_text(field_name, 64, "multipart field name") {
            return response;
        }
        let is_folder = field_name == "folder";
        let is_file = field_name == "file";

        if is_folder {
            // Do not call Field::text(): a malicious metadata field could consume
            // nearly the entire upload allowance in one String.
            let mut bytes = Vec::new();
            loop {
                let chunk = match field.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("讀取資料夾欄位失敗：{e}") })),
                        )
                            .into_response();
                    }
                };
                if bytes.len().saturating_add(chunk.len()) > ADMIN_MAX_PATH_BYTES {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "資料夾路徑過長" })),
                    )
                        .into_response();
                }
                bytes.extend_from_slice(&chunk);
            }
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "資料夾路徑必須是 UTF-8" })),
                    )
                        .into_response();
                }
            };
            folder = match safe_relative_path(&text) {
                Some(path) => path,
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "資料夾路徑不合法" })),
                    )
                        .into_response();
                }
            };
            continue;
        }

        if !is_file {
            continue;
        }
        file_count = file_count.saturating_add(1);
        if file_count > MAX_UPLOAD_FILES {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("too many uploaded files; max {MAX_UPLOAD_FILES}")
                })),
            )
                .into_response();
        }

        let raw_file_name = field.file_name().unwrap_or_default();
        if let Err(response) =
            validate_admin_text(raw_file_name, MAX_MATERIAL_FILENAME_BYTES, "file name")
        {
            return response;
        }
        let Some(file_name) = safe_file_name(raw_file_name) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "檔名不合法" })),
            )
                .into_response();
        };
        if !is_allowed_material_file(&file_name) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("不允許的檔案格式，允許：{}", MATERIAL_ALLOWED_EXTENSIONS.join(", "))
                })),
            )
                .into_response();
        }

        // CLOUD_UPLOAD_STREAMING_V1: write multipart chunks directly to a temp
        // file instead of collecting the full field into memory with bytes().
        let known_materials_size = if let Some(size) = current_materials_size {
            size
        } else {
            let size_root = root.clone();
            let scanned = match tokio::task::spawn_blocking(move || material_total_size(&size_root)).await
            {
                Ok(Ok(size)) => size,
                Ok(Err(_)) | Err(_) => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({ "error": "無法確認教材儲存空間，已停止上傳" })),
                    )
                        .into_response();
                }
            };
            scanned
        };

        let dir = match secure_material_path_for_create(&root, &folder) {
            Ok(path) => path,
            Err(error) => return material_path_error(&error, "folder not found"),
        };
        if tokio::fs::create_dir_all(&dir).await.is_err() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "unable to create materials folder" })),
            )
                .into_response();
        }
        let dest = unique_material_path(&dir, &file_name);
        let temp_path = dir.join(format!(".{}.uploading", Uuid::new_v4().simple()));
        let mut out = match tokio::fs::File::create(&temp_path).await {
            Ok(file) => file,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("建立暫存檔失敗：{e}") })),
                )
                    .into_response();
            }
        };

        let mut written = 0u64;
        loop {
            let chunk = match field.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("讀取上傳檔案失敗：{e}") })),
                    )
                        .into_response();
                }
            };
            let chunk_len = chunk.len() as u64;
            if written.saturating_add(chunk_len) > MATERIALS_MAX_FILE_SIZE as u64 {
                let _ = tokio::fs::remove_file(&temp_path).await;
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({ "error": "單一教材檔案不可超過 50 MiB" })),
                )
                    .into_response();
            }
            if known_materials_size
                .saturating_add(written)
                .saturating_add(chunk_len)
                > MATERIALS_MAX_TOTAL_SIZE as u64
            {
                let _ = tokio::fs::remove_file(&temp_path).await;
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({ "error": "教材總容量不可超過 10GB" })),
                )
                    .into_response();
            }
            if let Err(e) = out.write_all(&chunk).await {
                let _ = tokio::fs::remove_file(&temp_path).await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("寫入暫存檔失敗：{e}") })),
                )
                    .into_response();
            }
            written = written.saturating_add(chunk_len);
        }
        if let Err(e) = out.flush().await {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("寫入暫存檔失敗：{e}") })),
            )
                .into_response();
        }
        drop(out);

        // A plain rename may replace an existing file if another upload won
        // the same filename race. Create the destination with a non-overwriting
        // hard link first, then remove the temporary name.
        if let Err(e) = tokio::fs::hard_link(&temp_path, &dest).await {
            let _ = tokio::fs::remove_file(&temp_path).await;
            let status = if e.kind() == std::io::ErrorKind::AlreadyExists {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            return (
                status,
                Json(json!({ "error": "檔案名稱發生衝突，請重新上傳" })),
            )
                .into_response();
        }
        if let Err(e) = tokio::fs::remove_file(&temp_path).await {
            // The committed destination remains valid; report only a generic
            // server error and never expose the temporary path.
            tracing::warn!(error = %e, "清理教材上傳暫存檔失敗");
        }

        let rel = dest
            .strip_prefix(&root)
            .unwrap_or(&dest)
            .to_string_lossy()
            .replace('\\', "/");
        current_materials_size = Some(known_materials_size.saturating_add(written));
        let folder_label = folder.to_string_lossy().replace('\\', "/");
        record_material_event(
            &state,
            &session.account_id,
            "upload",
            &rel,
            &folder_label,
            &file_name,
            written,
            "success",
            "material_upload_history",
        )
        .await;
        saved_files.push(rel);
    }

    if saved_files.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "沒有收到檔案" })),
        )
            .into_response();
    }

    invalidate_materials_list_cache().await;

    crate::logs::record_audit(
        "web",
        &session.account_id,
        "material_upload",
        None,
        Some(&format!("{} file(s)", saved_files.len())),
        "上傳教材檔案",
    );

    (
        StatusCode::OK,
        Json(json!({ "ok": true, "files": saved_files })),
    )
        .into_response()
}

pub(super) async fn admin_files_delete_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(req): Json<AdminFileDeleteRequest>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_admin_text(&req.path, ADMIN_MAX_PATH_BYTES, "path") {
        return response;
    }
    let Some(rel) = safe_relative_path(&req.path).filter(|p| !p.as_os_str().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid path" })),
        )
            .into_response();
    };
    let root = session_materials_root(&session);
    let path = match secure_existing_material_path_response(&root, &rel, "file not found") {
        Ok(path) => path,
        Err(response) => return response,
    };
    if !path.is_file() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "file not found" })),
        )
            .into_response();
    }

    let event_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let event_file_name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_string();
    let event_folder = Path::new(&req.path)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();

    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            record_material_event(
                &state,
                &session.account_id,
                "delete_file",
                &req.path,
                &event_folder,
                &event_file_name,
                event_size,
                "success",
                "material_delete_file_history",
            )
            .await;
            invalidate_materials_list_cache().await;
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "material_delete",
                None,
                Some(&req.path),
                "刪除教材檔案",
            );
            (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("刪除失敗：{e}") })),
        )
            .into_response(),
    }
}

/// Delete a folder and everything inside it.
///
/// The dashboard has always had a "delete folder" button, but the matching
/// route was never registered, so it failed with a 404. `delete` cannot be
/// reused because it rejects anything that is not a plain file.
pub(super) async fn admin_files_rmdir_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(req): Json<AdminFolderCreateRequest>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    // Both roles may delete folders, but the scoped root below prevents a
    // regular user from reaching anything outside /教材.
    if let Err(response) = validate_admin_text(&req.path, ADMIN_MAX_PATH_BYTES, "path") {
        return response;
    }

    let Some(rel) = safe_relative_path(&req.path).filter(|p| !p.as_os_str().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid path" })),
        )
            .into_response();
    };

    let root = session_materials_root(&session);
    // Resolve and validate the complete path before deleting. This rejects
    // traversal, symlinks and paths that escape the configured materials root.
    let canonical_dir = match secure_existing_material_path_response(&root, &rel, "folder not found") {
        Ok(path) => path,
        Err(response) => return response,
    };
    if !canonical_dir.is_dir() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "folder not found" })),
        )
            .into_response();
    }

    let rel_label = rel.to_string_lossy().replace('\\', "/");
    match tokio::fs::remove_dir_all(&canonical_dir).await {
        Ok(()) => {
            record_material_event(
                &state,
                &session.account_id,
                "delete_folder",
                &rel_label,
                &rel_label,
                "",
                0,
                "success",
                "material_folder_delete_history",
            )
            .await;
            invalidate_materials_list_cache().await;
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "material_folder_delete",
                None,
                Some(&rel_label),
                "刪除教材資料夾",
            );
            (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("刪除資料夾失敗：{e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_files_mkdir_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(req): Json<AdminFolderCreateRequest>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if let Err(response) = validate_admin_text(&req.path, ADMIN_MAX_PATH_BYTES, "path") {
        return response;
    }

    let Some(rel) = safe_relative_path(&req.path).filter(|p| !p.as_os_str().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "資料夾名稱不合法" })),
        )
            .into_response();
    };

    let root = session_materials_root(&session);
    if let Err(error) = tokio::fs::create_dir_all(&root).await {
        return material_path_error(&error, "materials folder unavailable");
    }
    let dir = match secure_material_path_for_create(&root, &rel) {
        Ok(path) => path,
        Err(error) => return material_path_error(&error, "folder not found"),
    };
    match tokio::fs::create_dir_all(&dir).await {
        Ok(()) => {
            let rel_label = rel.to_string_lossy().replace('\\', "/");
            record_material_event(
                &state,
                &session.account_id,
                "create_folder",
                &rel_label,
                &rel_label,
                "",
                0,
                "success",
                "material_folder_create_history",
            )
            .await;
            invalidate_materials_list_cache().await;
            crate::logs::record_audit(
                "web",
                &session.account_id,
                "material_folder_create",
                None,
                Some(&rel_label),
                "新增教材資料夾",
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "path": rel_label })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("建立資料夾失敗：{e}") })),
        )
            .into_response(),
    }
}

pub(super) async fn admin_files_history_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminFilesHistoryQuery>,
) -> Response {
    let Some(session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "讀取教材操作紀錄需要管理員權限" })),
        )
            .into_response();
    }

    let limit = query.limit.unwrap_or(100).clamp(1, 200);
    match state.accounts_db.list_material_events(limit).await {
        // Keep both response keys for the files page and the combined activity page.
        Ok(events) => (
            StatusCode::OK,
            Json(json!({ "events": events.clone(), "items": events })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("讀取教材操作紀錄失敗：{e}") })),
        )
            .into_response(),
    }
}
