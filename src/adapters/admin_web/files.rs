//! Materials file-manager page/API handlers for Admin Web.

use super::*;
use hmac::{Hmac, Mac};
use sha2::Sha256;
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
// Serialize uploads so the 10 GiB quota check cannot be bypassed by two
// simultaneous requests observing the same pre-upload size.
const MATERIAL_UPLOAD_CONCURRENCY: usize = 1;

static MATERIAL_UPLOAD_SEMAPHORE: once_cell::sync::Lazy<Semaphore> =
    once_cell::sync::Lazy::new(|| Semaphore::new(MATERIAL_UPLOAD_CONCURRENCY));

#[derive(Clone)]
struct MaterialsListSnapshot {
    root: PathBuf,
    updated_at: Instant,
    files: Vec<MaterialFileEntry>,
    folders: Vec<String>,
}

fn session_materials_root(session: &AdminSession) -> PathBuf {
    let root = materials_root();
    // MATERIALS_EVERYONE_FULL_TREE_V1: 一般人(非 admin)也能看到全部資料夾。
    // 教材統一存於 materials_root()/<分班>/<課程>/<檔名>，管理員與一般帳號共用同一棵
    // 檔案樹；不再把一般帳號窄化到 root/教材，否則他們只看得到空的子目錄。
    // 若要再收緊(例：一般人只能讀、只能看自己分班)，請在各自的 handler 加 is_admin 判斷。
    let _ = session.is_admin;
    root
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

// SNIFF_CONTENT_TYPE_V1: 副檔名與內容不符時（例如「.ppt」其實是 PDF），依真實
// 魔數回 Content-Type，否則 <iframe> 的 PDF 檢視器認不出內容類型→全白。
async fn sniff_content_type(path: &std::path::Path) -> &'static str {
    let mut head = [0u8; 8];
    if let Ok(mut f) = tokio::fs::File::open(path).await {
        if let Ok(n) = f.read(&mut head).await {
            if n >= 4 && &head[0..4] == b"%PDF" {
                return "application/pdf";
            }
        }
    }
    material_content_type(path)
}

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
    // SNIFF_CONTENT_TYPE_V1: 以真實魔數決定 Content-Type（.ppt 實為 PDF 時回 application/pdf）。
    let content_type = sniff_content_type(&path).await;
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
  link.textContent = '← 回到 Dashboard';
  link.title = '回到 Tronclass Dashboard';
  link.setAttribute('aria-label', '回到 Tronclass Dashboard');
  link.setAttribute('rel', 'noopener');
  // Set individual CSS properties instead of cssText so the strict CSP's
  // style-src-attr policy does not block this trusted UI control.
  link.style.position = 'fixed';
  link.style.zIndex = '2147483647';
  // Place the navigation control at the far right of the workspace, below
  // Filebrowser's own toolbar. This avoids both the search field and the
  // action icons, while making the Dashboard return path easy to find.
  link.style.top = '82px';
  link.style.right = '24px';
  link.style.display = 'flex';
  link.style.alignItems = 'center';
  link.style.justifyContent = 'center';
  link.style.minHeight = '38px';
  link.style.boxSizing = 'border-box';
  link.style.padding = '0 14px';
  link.style.borderRadius = '10px';
  link.style.border = '1px solid rgba(37,99,235,.20)';
  link.style.background = '#f8fbff';
  link.style.color = '#1d4ed8';
  link.style.cursor = 'pointer';
  link.style.font = '600 14px/1.2 system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif';
  link.style.textDecoration = 'none';
  link.style.boxShadow = '0 2px 10px rgba(30,64,175,.10)';
  link.style.whiteSpace = 'nowrap';
  link.style.transition = 'background .15s,color .15s,transform .15s,box-shadow .15s';
  link.addEventListener('mouseenter', function () {{
    link.style.background = '#e8f1ff';
    link.style.color = '#1e40af';
    link.style.boxShadow = '0 4px 14px rgba(30,64,175,.18)';
  }});
  link.addEventListener('mouseleave', function () {{
    link.style.background = '#f8fbff';
    link.style.color = '#1d4ed8';
    link.style.boxShadow = '0 2px 10px rgba(30,64,175,.10)';
  }});
  link.addEventListener('mousedown', function () {{ link.style.transform = 'translateY(1px)'; }});
  link.addEventListener('mouseup', function () {{ link.style.transform = 'none'; }});
  document.body.appendChild(link);
  var style = document.createElement('style');
  if (document.currentScript && document.currentScript.nonce) style.setAttribute('nonce', document.currentScript.nonce);
  style.textContent = '@media (min-width:721px) and (max-width:1180px) {{ #tronclass-dashboard-back {{ left:500px !important; right:auto !important; top:12px !important; }} }} @media (max-width:720px) {{ #tronclass-dashboard-back {{ display:none !important; }} #tronclass-dashboard-back.tronclass-dashboard-back-in-sidebar {{ position:static !important; display:flex !important; width:calc(100% - 28px) !important; min-height:40px !important; margin:10px 14px 6px !important; padding:0 12px !important; border-radius:10px !important; }} }}';
  document.head.appendChild(style);

  // On phones, a fixed control overlaps Filebrowser's slide-out drawer. Move
  // the link into that drawer instead, immediately below its "我的檔案" entry.
  // Until the drawer DOM is available the mobile rule above keeps it hidden;
  // showing no button is preferable to covering Filebrowser controls.
  function mountMobileDashboardLink() {{
    if (!window.matchMedia || !window.matchMedia('(max-width: 720px)').matches) {{
      if (link.parentNode !== document.body) document.body.appendChild(link);
      link.classList.remove('tronclass-dashboard-back-in-sidebar');
      return;
    }}
    var nodes = document.querySelectorAll('a,button,li,div,span');
    var row = null;
    for (var i = 0; i < nodes.length; i++) {{
      var node = nodes[i];
      if ((node.textContent || '').trim() !== '我的檔案') continue;
      row = node.closest && node.closest('a,button,li,[role="menuitem"]');
      if (!row) row = node.parentElement;
      if (row && row.parentElement) break;
      row = null;
    }}
    if (!row || !row.parentElement) return;
    var host = row.parentElement;
    if (link.parentNode !== host || link.previousSibling !== row) {{
      host.insertBefore(link, row.nextSibling);
    }}
    link.classList.add('tronclass-dashboard-back-in-sidebar');
  }}
  mountMobileDashboardLink();
  window.addEventListener('resize', mountMobileDashboardLink, {{ passive: true }});
  var mountObserver = new MutationObserver(mountMobileDashboardLink);
  mountObserver.observe(document.body, {{ childList: true, subtree: true }});
  window.setTimeout(function () {{ mountObserver.disconnect(); }}, 8000);
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

/// 可在瀏覽器內直接預覽的副檔名 —— **單一來源**。
///
/// 前端 `static/files.html` 的 `IMAGE_/VIDEO_/AUDIO_/TEXT_/OFFICE_/LEGACY_OFFICE_PREVIEW_EXTS`
/// 六份清單聯集必須與這裡一致；兩邊不同步時會出現「按了預覽卻收到 400」
/// （舊版 gif/bmp/webp 就是這樣壞掉的）。`tools/check_src.py` 會自動比對兩者。
///
/// # NO_LIBREOFFICE_PREVIEW_V1
/// 這裡的每一種格式都不需要伺服器轉檔，因此系統完全不依賴 LibreOffice：
///
/// | 類別 | 格式 | 渲染方式 |
/// |---|---|---|
/// | 圖片 | jpg/png/gif/bmp/webp/avif/ico/svg | 瀏覽器原生 `<img>` |
/// | 文件 | pdf | PDF.js（Firefox 同款，canvas 逐頁） |
/// | 影音 | mp4/webm/mov/m4v、mp3/wav/ogg/m4a/flac | 瀏覽器原生 `<video>`/`<audio>` |
/// | 純文字 | txt/md/log/csv/json/yaml/… | 前端 `<pre>` 或表格（經 `esc()` 跳脫） |
/// | Office 現代 | docx/pptx/xlsx/odf/rtf | 前端 office-view.js 本機解析（單一 container、無 LibreOffice） |
/// | Office 舊二進位 | doc/ppt/xls | 無純 JS 解析器→Office Online（需對外可達）或下載 |
pub(super) const PREVIEWABLE_EXTS: &[&str] = &[
    // 圖片
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "avif", "ico", "svg",
    // 文件
    "pdf",
    // 影片
    "mp4", "webm", "mov", "m4v",
    // 音訊
    "mp3", "wav", "ogg", "m4a", "flac",
    // 純文字 / 資料 / 程式碼
    "txt", "text", "md", "markdown", "log", "csv", "tsv", "json", "xml", "yaml",
    "yml", "ini", "conf", "cfg", "srt", "vtt", "toml", "sql", "sh", "rs", "js",
    "ts", "jsx", "tsx", "css", "py", "java", "c", "h", "cpp", "hpp", "go", "php",
    // HTML 原始碼：只當純文字顯示，見 material_content_type 的說明
    "html", "htm",
    // Internet Shortcut：前端讀取內容的 URL= 後直接跳轉（URL_SHORTCUT_V1）
    "url",
    // Office（前端本機解析）
    "docx", "pptx", "xlsx", "odt", "ods", "odp", "rtf",
    // 舊二進位 Office：僅回原始位元組，前端顯示下載提示
    "doc", "ppt", "xls",
];

/// 副檔名是否可預覽。傳入值須已轉小寫。
pub(super) fn is_previewable_ext(ext: &str) -> bool {
    PREVIEWABLE_EXTS.contains(&ext)
}

// ─── OFFICE_ONLINE_V1（OneDrive 同款線上檢視）─────────────────────────────
// 舊版二進位 Office（.doc/.ppt/.xls）本機無法解析，改交給 Microsoft Office
// Online 檢視器。檢視器會「從微軟伺服器」抓取 src URL，因此檔案必須有一個
// 公開、有時效、防竄改的 URL。我們用 HMAC-SHA256 簽 {path|exp} 產生
// `/pub/office?path=&exp=&sig=`，過期或竄改即失效。不碰 LibreOffice。
//
// 安全模型：
// - 連結 TTL 僅 15 分鐘，且只簽「特定 path」，不能列舉其他檔案。
// - `/pub/office` 不帶任何 session/cookie 驗證，純靠簽章；黑名單 IP 仍會在外層
//   middleware 被擋。
// - 未設環境變數時用进程內隨機金鑰：重啟後舊連結失效（可接受，TTL 很短）。
const OFFICE_EMBED_TTL_SECS: u64 = 15 * 60;

/// 可交給 Office Online 的副檔名（微軟檢視器支援舊版與 OOXML）。
const OFFICE_ONLINE_EXTS: &[&str] = &["doc", "ppt", "xls", "docx", "pptx", "xlsx"];

static OFFICE_PREVIEW_SECRET: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
    // .ok() 把 Result 轉 Option，才能接 map/filter/unwrap_or_else。
    if let Ok(v) = std::env::var("TRONCLASS_OFFICE_PREVIEW_SECRET") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    // SHARE_SECRET_PERSIST_V1: 沒環境變數時把隨機金鑰「持久化到 config/」，
    // 否則每次部署/重啟會換金鑰，導致先前核發的分享連結（含 Office Online 的
    // src 簽章）全部失效（微軟抓 /pub/office 拿 403 → 「An error occurred」）。
    let path = std::path::Path::new("config/office_preview.secret");
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return existing;
        }
    }
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).expect("getrandom");
    let secret = hex::encode(b);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(path, &secret) {
        tracing::warn!("無法持久化 office preview 金鑰，重啟後舊分享連結將失效：{e}");
    }
    secret
});

fn office_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn office_sig(path: &str, exp: u64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(OFFICE_PREVIEW_SECRET.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(format!("office-preview|{path}|{exp}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn pct(value: &str) -> String {
    // SHARE_LINK_MOBILE_V1: 空格用 %20 而非 '+'。form 編碼的 '+' 在手機訊息
    // App（LINE 等）點按/轉傳時常被轉成空格或截斷，導致 path 與簽章對不上。
    url::form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// SHARE_V1: 常數時間驗證 {path|exp} 的 HMAC 簽章（/pub/office 與 /pub/share 共用）。
fn verify_office_sig(path: &str, exp: u64, sig: &str) -> bool {
    hex::decode(sig)
        .map(|raw| {
            let mut mac = Hmac::<Sha256>::new_from_slice(OFFICE_PREVIEW_SECRET.as_bytes())
                .expect("HMAC accepts any key length");
            mac.update(format!("office-preview|{path}|{exp}").as_bytes());
            mac.verify_slice(&raw).is_ok()
        })
        .unwrap_or(false)
}

/// 已登入後台：為單一檔案核發「限時簽章公開連結」（相對路徑）。
/// 絕對 URL 由前端以 `location.origin` 組出（後台 Host header 在 Pterodactyl/
/// Tunnel 後是容器內部位址，後端猜的 host 微軟抓不到 → 空白）。簽章只綁
/// path+exp，不含 host，故前端組 host 不影響安全。
pub(super) async fn admin_files_office_embed_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminFilesQuery>,
) -> Response {
    let Some(_session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    let Some(rel) = query
        .path
        .as_deref()
        .and_then(safe_relative_path)
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid path" }))).into_response();
    };
    let rel_str = rel.to_string_lossy().into_owned();
    let ext = rel
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !OFFICE_ONLINE_EXTS.contains(&ext.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "此格式不支援線上 Office 檢視" })),
        )
            .into_response();
    }
    let exp = office_now_secs() + OFFICE_EMBED_TTL_SECS;
    let sig = office_sig(&rel_str, exp);
    let public_path = format!("/pub/office?path={}&exp={exp}&sig={sig}", pct(&rel_str));
    (StatusCode::OK, Json(json!({ "public_path": public_path }))).into_response()
}

// SHARE_V1: 分享連結時效 7 天（比 Office 內嵌的 15 分鐘長，適合給人點開預覽）。
const SHARE_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// 已登入後台：核發「免登入、可預覽」的公開分享連結（/pub/share?path=&exp=&sig=）。
/// 簽章只綁 path+exp，時效內任何人點開都能預覽、不能列舉其他檔案。
// SHARE_ADMIN_V1: 已發出分享連結的記錄（檔案型 config/share_links.jsonl，免改 DB schema）。
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub(super) struct ShareLinkRecord {
    pub path: String,
    pub exp: i64,
    pub created: i64,
    #[serde(default)]
    pub revoked: bool,
}

fn share_links_file() -> std::path::PathBuf {
    std::path::Path::new("config/share_links.jsonl").to_path_buf()
}

static SHARE_LINKS: once_cell::sync::Lazy<std::sync::Mutex<Vec<ShareLinkRecord>>> =
    once_cell::sync::Lazy::new(|| {
        let mut v = Vec::new();
        if let Ok(text) = std::fs::read_to_string(share_links_file()) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(rec) = serde_json::from_str::<ShareLinkRecord>(line) {
                    v.push(rec);
                }
            }
        }
        std::sync::Mutex::new(v)
    });

fn persist_share_links(list: &[ShareLinkRecord]) {
    let mut out = String::new();
    for r in list {
        if let Ok(s) = serde_json::to_string(r) {
            out.push_str(&s);
            out.push('\n');
        }
    }
    let path = share_links_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(path, out) {
        tracing::warn!("無法寫入 share_links.jsonl：{e}");
    }
}

fn record_share_link(path: &str, exp: i64) {
    if let Ok(mut list) = SHARE_LINKS.lock() {
        if list.iter().any(|r| r.path == path && r.exp == exp) {
            return;
        }
        list.push(ShareLinkRecord {
            path: path.to_string(),
            exp,
            created: office_now_secs() as i64,
            revoked: false,
        });
        persist_share_links(&list);
    }
}

fn revoke_share_link(path: &str, exp: i64) -> bool {
    if let Ok(mut list) = SHARE_LINKS.lock() {
        let mut changed = false;
        for r in list.iter_mut() {
            if r.path == path && r.exp == exp && !r.revoked {
                r.revoked = true;
                changed = true;
            }
        }
        if changed {
            persist_share_links(&list);
        }
        changed
    } else {
        false
    }
}

fn is_share_revoked(path: &str, exp: i64) -> bool {
    SHARE_LINKS
        .lock()
        .map(|l| l.iter().any(|r| r.path == path && r.exp == exp && r.revoked))
        .unwrap_or(false)
}

// 管理員：列出所有分享連結（含伺服器端重算的 sig，前端組出完整網址 + QR）。
// SHARE_ADMIN_V1: 以 session_token(headers+query) 認證，支援 HttpOnly cookie。
pub(super) async fn admin_shares_list_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "admin only" }))).into_response();
    }
    let items: Vec<serde_json::Value> = SHARE_LINKS
        .lock()
        .map(|list| {
            list.iter()
                .map(|r| {
                    json!({
                        "path": r.path,
                        "exp": r.exp,
                        "created": r.created,
                        "revoked": r.revoked,
                        "sig": office_sig(&r.path, r.exp as u64),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (StatusCode::OK, Json(json!({ "items": items }))).into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct RevokeShareRequest {
    #[serde(default)]
    path: String,
    #[serde(default)]
    exp: i64,
}

// 管理員：撤銷指定分享連結。
pub(super) async fn admin_shares_revoke_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
    Json(body): Json<RevokeShareRequest>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize(&token).await else {
        return unauthorized_response();
    };
    if !session.is_admin {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "admin only" }))).into_response();
    }
    let changed = revoke_share_link(&body.path, body.exp);
    (StatusCode::OK, Json(json!({ "revoked": changed }))).into_response()
}

pub(super) async fn admin_files_share_link_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminFilesQuery>,
) -> Response {
    let Some(_session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    let Some(rel) = query
        .path
        .as_deref()
        .and_then(safe_relative_path)
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid path" }))).into_response();
    };
    let rel_str = rel.to_string_lossy().into_owned();
    let exp = office_now_secs() + SHARE_TTL_SECS;
    let sig = office_sig(&rel_str, exp);
    record_share_link(&rel_str, exp as i64);
    let public_path = format!("/pub/share?path={}&exp={exp}&sig={sig}", pct(&rel_str));
    (StatusCode::OK, Json(json!({ "public_path": public_path }))).into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct PreviewLogRequest {
    #[serde(default)]
    path: String,
    #[serde(default)]
    ext: String,
    #[serde(default)]
    stage: String,
    #[serde(default)]
    message: String,
}

// PREVIEW_LOG_V1: 前端預覽/解析失敗時呼叫，伺服器以 warn 寫入日誌，
// 讓管理者在 Pterodactyl console 直接看到「哪個檔、哪一步、什麼錯」。
pub(super) async fn admin_files_preview_log_handler(
    State(state): State<AdminWebState>,
    Query(query): Query<AdminQuery>,
    Json(body): Json<PreviewLogRequest>,
) -> Response {
    let Some(_session) = state.authorize(&query.token).await else {
        return unauthorized_response();
    };
    tracing::warn!(
        path = %sanitize_log_field(&body.path, 300),
        ext = %sanitize_log_field(&body.ext, 16),
        stage = %sanitize_log_field(&body.stage, 64),
        message = %sanitize_log_field(&body.message, 500),
        "files preview client error"
    );
    (StatusCode::NO_CONTENT, ()).into_response()
}

// PUB_RATE_LIMIT_V1: 公開端點（/pub/share、/pub/office、/pub/preview-log）每 IP
// 固定窗限流，純防灌爆/DoS；HMAC 簽章本身無法暴力破解。
static PUB_RATE_LIMIT: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, (u64, u32)>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn pub_rate_exceeded(ip: &str, limit: u32, window_secs: u64) -> bool {
    let now = office_now_secs();
    let Ok(mut map) = PUB_RATE_LIMIT.lock() else {
        return false;
    };
    if map.len() > 4096 {
        map.retain(|_, (start, _)| now.saturating_sub(*start) < window_secs);
    }
    let entry = map.entry(ip.to_string()).or_insert((now, 0));
    if now.saturating_sub(entry.0) >= window_secs {
        *entry = (now, 1);
        false
    } else {
        entry.1 += 1;
        entry.1 > limit
    }
}

fn too_many_requests_response() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "error": "too many requests" })),
    )
        .into_response()
}

// LOG_SANITIZE_V1: 日誌欄位過濾控制字元（含 \n \r \t）並截斷，防 console 注入/灌爆。
fn sanitize_log_field(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

// SHARE_V1d: 公開分享頁的用戶端回報端點（免登入）。分享頁把「魔數、分支、錯誤」
// POST 進來，後台以 warn 顯示，用於一次性定位預覽問題。不驗證 session。
pub(super) async fn public_share_log_handler(
    ClientIp(ip): ClientIp,
    Json(body): Json<PreviewLogRequest>,
) -> Response {
    if pub_rate_exceeded(&ip, 10, 60) {
        return too_many_requests_response();
    }
    tracing::warn!(
        path = %sanitize_log_field(&body.path, 300),
        ext = %sanitize_log_field(&body.ext, 16),
        stage = %sanitize_log_field(&body.stage, 64),
        message = %sanitize_log_field(&body.message, 500),
        "share page client report"
    );
    (StatusCode::NO_CONTENT, ()).into_response()
}

#[derive(Debug, Deserialize)]
pub(super) struct PublicOfficeQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    exp: Option<u64>,
    #[serde(default)]
    sig: Option<String>,
    // SHARE_V1: dl=1 時改以 attachment 下載（分享頁右上「下載」按鈕），預設 inline 預覽。
    #[serde(default)]
    dl: Option<String>,
    // SHARE_V1b: raw=1 時 /pub/share 直接串流原始檔案（同一簽章驗證），
    // 讓分享頁 fetch 與 Office Online 的 src 都指向「已驗證有效」的同一端點。
    #[serde(default)]
    raw: Option<String>,
}

/// 公開端點：驗證簽章與時效後串流原始檔案（供微軟檢視器抓取）。不驗證 session。
pub(super) async fn public_office_file_handler(
    ClientIp(ip): ClientIp,
    Query(query): Query<PublicOfficeQuery>,
) -> Response {
    if pub_rate_exceeded(&ip, 120, 60) {
        return too_many_requests_response();
    }
    let (Some(path), Some(exp), Some(sig)) =
        (query.path.clone(), query.exp, query.sig.clone())
    else {
        tracing::warn!("pub office request missing params");
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "missing params" }))).into_response();
    };
    tracing::info!(path = %path, exp, "pub office request received");
    if office_now_secs() > exp {
        tracing::warn!(path = %path, exp, "pub office rejected: link expired");
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "link expired" }))).into_response();
    }
    // 常數時間驗證：重新計算 HMAC 並比對簽章位元組。
    let sig_ok = hex::decode(&sig)
        .map(|raw| {
            let mut mac = Hmac::<Sha256>::new_from_slice(OFFICE_PREVIEW_SECRET.as_bytes())
                .expect("HMAC accepts any key length");
            mac.update(format!("office-preview|{path}|{exp}").as_bytes());
            mac.verify_slice(&raw).is_ok()
        })
        .unwrap_or(false);
    if !sig_ok {
        tracing::warn!(path = %path, exp, "pub office rejected: bad signature");
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "bad signature" }))).into_response();
    }
    let Some(rel) = safe_relative_path(&path).filter(|p| !p.as_os_str().is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid path" }))).into_response();
    };
    let full = materials_root().join(&rel);
    if !full.is_file() {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response();
    }
    // SHARE_V1: dl=1 → attachment（下載）；否則 inline（預覽/供檢視器抓取）。
    let disposition = if query.dl.as_deref() == Some("1") { "attachment" } else { "inline" };
    tracing::info!(path = %path, disposition, "pub office streaming file");
    stream_material_file_response(
        full,
        disposition,
        Some("no-store"),
        "讀取預覽檔失敗",
        "建立預覽回應失敗",
    )
    .await
}

// SHARE_V1: 免登入「可預覽」分享頁。
// 分享連結 = /pub/share?path=&exp=&sig=，純靠 HMAC 簽章＋時效驗證（與 /pub/office
// 同一套金鑰），不帶任何 session/cookie。頁面本身不串流檔案內容，而是讓瀏覽器
// 再去抓同一簽章的 /pub/office 原始位元組，在前端本機渲染（圖片/影音/PDF/文字/
// Office），因此拿到連結的人「不用登入就能預覽」，且不是直接下載頁。
const SHARE_PAGE: &str = r#"<!doctype html>
<html lang="zh-Hant">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>__TITLE__</title>
<link rel="stylesheet" href="/pub/static/css/share.css?v=sharev6">
<meta id="share-ext" content="__EXTATTR__">
<meta id="share-raw" content="__RAWATTR__">
<meta id="share-office" content="__OFFICEATTR__">
__SCRIPTS__
</head>
<body>
<header><span class="t">__TITLE__</span><a class="dl" href="__DLATTR__">下載</a></header>
<div id="stage"><div class="msg">載入中…</div></div>
<script src="/pub/static/js/pdf.min.js?v=sharev6"></script>
<script src="/pub/static/js/share-preview.js?v=sharev6"></script>
</body>
</html>"#;

/// 公開分享預覽頁：驗證簽章與時效後回傳一個免登入的預覽 HTML。不驗證 session。
pub(super) async fn public_share_preview_handler(
    ClientIp(ip): ClientIp,
    Query(query): Query<PublicOfficeQuery>,
) -> Response {
    if pub_rate_exceeded(&ip, 120, 60) {
        return too_many_requests_response();
    }
    let (Some(path), Some(exp), Some(sig)) =
        (query.path.clone(), query.exp, query.sig.clone())
    else {
        return (StatusCode::BAD_REQUEST, "missing params").into_response();
    };
    if office_now_secs() > exp {
        tracing::warn!(path = %path, exp, "pub share rejected: link expired");
        return (StatusCode::FORBIDDEN, "分享連結已過期").into_response();
    }
    if !verify_office_sig(&path, exp, &sig) {
        tracing::warn!(path = %path, exp, "pub share rejected: bad signature");
        return (StatusCode::FORBIDDEN, "分享連結無效").into_response();
    }
    if is_share_revoked(&path, exp as i64) {
        tracing::warn!(path = %path, exp, "pub share rejected: revoked");
        return (StatusCode::FORBIDDEN, "分享連結已撤銷").into_response();
    }
    let Some(rel) = safe_relative_path(&path).filter(|p| !p.as_os_str().is_empty()) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };
    let full = materials_root().join(&rel);
    if !full.is_file() {
        return (StatusCode::NOT_FOUND, "檔案不存在").into_response();
    }
    // SHARE_V1b: raw=1 → 同一簽章驗證後直接串流原始檔（分享頁 fetch 與
    // Office Online 的 src 都用這個），不再依賴 /pub/office，避免兩端點差異。
    if query.raw.as_deref() == Some("1") {
        let disposition = if query.dl.as_deref() == Some("1") { "attachment" } else { "inline" };
        tracing::info!(path = %path, disposition, "pub share streaming raw file");
        return stream_material_file_response(
            full,
            disposition,
            Some("no-store"),
            "讀取分享檔失敗",
            "建立分享回應失敗",
        )
        .await;
    }
    let rel_str = rel.to_string_lossy().into_owned();
    let ext = rel
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let title = rel
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("分享檔案")
        .to_string();
    // 現代格式本機解析：fetch 用與頁面「同一已驗證簽章」的 /pub/share?raw=1。
    let raw_url = format!("/pub/share?path={}&exp={exp}&sig={sig}&raw=1", pct(&rel_str));
    let dl_url = format!("{raw_url}&dl=1");
    // 舊版格式：給 Office Online 一條與 app 內 office-embed「完全相同」的
    // 現簽 15 分鐘 /pub/office 連結（proven 可被微軟抓取）。
    let embed_exp = office_now_secs() + OFFICE_EMBED_TTL_SECS;
    let embed_sig = office_sig(&rel_str, embed_exp);
    let office_src = format!("/pub/office?path={}&exp={embed_exp}&sig={embed_sig}", pct(&rel_str));
    let is_modern_office = ["docx", "pptx", "xlsx", "odt", "ods", "odp", "rtf"].contains(&ext.as_str());
    let scripts = if is_modern_office {
        "<script src=\"/pub/static/js/jszip.min.js?v=sharev6\"></script>\n<script src=\"/pub/static/js/office-view.js?v=sharev6\"></script>"
    } else {
        ""
    };
    let esc = |s: &str| {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    };
    let esc_attr = |s: &str| {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    };
    let page = SHARE_PAGE
        .replace("__TITLE__", &esc(&title))
        .replace("__EXT__", &esc(&ext))
        .replace("__SCRIPTS__", scripts)
        .replace("__EXTATTR__", &esc_attr(&ext))
        .replace("__RAWATTR__", &esc_attr(&raw_url))
        .replace("__OFFICEATTR__", &esc_attr(&office_src))
        .replace("__DLATTR__", &esc_attr(&dl_url));
    tracing::info!(path = %path, ext = %ext, raw_url = %raw_url, office_src = %office_src, "pub share page served");
    let mut res = Response::new(page.into());
    *res.status_mut() = StatusCode::OK;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // SHARE_CSP_V1: 自訂 CSP（不帶 nonce/Trusted Types），允許外掛 'self' 資源與
    // data:/blob: 圖片（office 內嵌圖為 data URI）。middleware 見已有 CSP 就不覆蓋。
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'; object-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:; frame-src 'self' https://view.officeapps.live.com https://*.officeapps.live.com; connect-src 'self'; font-src 'self'; worker-src 'self' blob:",
        ),
    );
    res
}

// SINGLE_CONTAINER_V1: 單一 container、無 LibreOffice、無 Gotenberg。
// Office 預覽改由前端本機解析（office-view.js）；舊版二進位（doc/ppt/xls）
// 無純 JS 解析器，走 Office Online（需對外可達）或下載。
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

    // PREVIEW_LOG_V1: 在後台 console 留下預覽請求紀錄（path/ext/大小），
    // 搭配前端 preview-log 端點，方便從 Pterodactyl 日誌排查預覽問題。
    let size = tokio::fs::metadata(&full_path).await.map(|m| m.len()).unwrap_or(0);
    tracing::info!(path = %rel.display(), ext = %ext, size = size, "files preview requested");

    // NO_LIBREOFFICE_PREVIEW_V1 / PREVIEW_ENGINES_V1
    // 一律串流「原始檔案」位元組，伺服器不呼叫任何外部轉檔程式。
    // 渲染落在瀏覽器外部引擎：PDF→PDF.js；Office→Microsoft Office Online。
    // `inline` 讓瀏覽器把它當成可讀取資源。
    if !is_previewable_ext(&ext) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "此檔案格式不支援預覽，請下載查看" })),
        )
            .into_response();
    }

    stream_material_file_response(
        full_path,
        "inline",
        Some("no-store"),
        "讀取預覽檔失敗",
        "建立預覽回應失敗",
    )
    .await
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
