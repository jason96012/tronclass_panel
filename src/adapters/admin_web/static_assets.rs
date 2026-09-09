//! 內嵌靜態資源（CSS/JS）的響應 handlers。

use super::*;
use super::pages;

const ADMIN_STATIC_CACHE_CONTROL: &str = "public, max-age=604800, immutable, stale-while-revalidate=86400";

// ───────────────────────────────────────────────────────────────────────────────
// STATIC_BROTLI_PRECOMPRESS_V1
//
// 原本所有回應都交給最外層 tower-http CompressionLayer 即時壓縮（brotli 預設品質偏低，
// 且每個請求都重壓一次同一份 JS/CSS）。靜態資產在一個 process 的生命週期內內容固定
// （include_str! + ?sv=<build id>），所以改成：
//   1. 第一次有支援 br 的客戶端要某個 /admin/static/… 或 /pub/static/… 時，照常回覆
//      （仍由 CompressionLayer 即時壓），同時把本文丟到 spawn_blocking 用 quality 11 壓一份。
//   2. 之後同路徑的請求直接回預壓縮位元組，帶 Content-Encoding: br；CompressionLayer 看到
//      已有 Content-Encoding 會跳過，不會重複壓。
// 排除 theme.css（可由 runtime theme 覆蓋，內容非固定）。不支援 br 的客戶端行為不變。
// 此 middleware 掛在 admin_security_headers_middleware 內側：命中快取的回應一樣會被補上
// CSP／HSTS／nosniff 等安全標頭與靜態 Cache-Control。
// ───────────────────────────────────────────────────────────────────────────────
struct PrecompressedAsset {
    body: axum::body::Bytes,
    content_type: Option<HeaderValue>,
    cache_control: Option<HeaderValue>,
}

enum PrecompressState {
    /// 已有人在背景壓，其他請求先走一般路徑，不重複排工作。
    Pending,
    Ready(Arc<PrecompressedAsset>),
}

static PRECOMPRESSED: once_cell::sync::Lazy<std::sync::RwLock<HashMap<String, PrecompressState>>> =
    once_cell::sync::Lazy::new(|| std::sync::RwLock::new(HashMap::new()));

/// 單一靜態資產本文上限（pdf.worker.min.js 約 1.3 MB；留足夠餘裕）。
const PRECOMPRESS_MAX_BODY: usize = 32 * 1024 * 1024;

fn precompress_eligible_path(path: &str) -> bool {
    (path.starts_with("/admin/static/") || path.starts_with("/pub/static/"))
        && path != "/admin/static/theme.css"
}

/// Accept-Encoding 是否接受 br（含 q 值不為 0）。
fn accepts_brotli(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|item| {
            let mut parts = item.split(';');
            let name = parts.next().map(str::trim).unwrap_or("");
            if !name.eq_ignore_ascii_case("br") {
                return false;
            }
            !parts.any(|p| {
                let p = p.trim();
                p.starts_with("q=") && p[2..].trim().parse::<f32>().map(|q| q <= 0.0).unwrap_or(false)
            })
        })
}

fn brotli_compress(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut input = raw;
    let mut out = Vec::with_capacity(raw.len() / 3 + 64);
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        lgwin: 22,
        ..Default::default()
    };
    brotli::enc::BrotliCompress(&mut input, &mut out, &params)?;
    Ok(out)
}

fn precompressed_response(asset: &PrecompressedAsset) -> Response {
    let mut response = (StatusCode::OK, asset.body.clone()).into_response();
    let headers = response.headers_mut();
    if let Some(ct) = asset.content_type.clone() {
        headers.insert(header::CONTENT_TYPE, ct);
    }
    if let Some(cc) = asset.cache_control.clone() {
        headers.insert(header::CACHE_CONTROL, cc);
    }
    headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    headers.insert(
        HeaderName::from_static("x-precompressed"),
        HeaderValue::from_static("br"),
    );
    response
}

pub(super) async fn precompressed_static_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path().to_string();
    if request.method() != Method::GET || !precompress_eligible_path(&path) {
        return next.run(request).await;
    }
    let wants_br = accepts_brotli(request.headers());
    if wants_br {
        let hit = PRECOMPRESSED.read().ok().and_then(|map| match map.get(&path) {
            Some(PrecompressState::Ready(asset)) => Some(Arc::clone(asset)),
            _ => None,
        });
        if let Some(asset) = hit {
            return precompressed_response(&asset);
        }
    }
    let response = next.run(request).await;
    if !wants_br
        || response.status() != StatusCode::OK
        || response.headers().contains_key(header::CONTENT_ENCODING)
    {
        return response;
    }
    // 只讓第一個請求負責排壓縮工作；其餘請求（包含壓縮進行中）照常回覆。
    let claimed = PRECOMPRESSED
        .write()
        .ok()
        .map(|mut map| {
            if map.contains_key(&path) {
                false
            } else {
                map.insert(path.clone(), PrecompressState::Pending);
                true
            }
        })
        .unwrap_or(false);
    if !claimed {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, PRECOMPRESS_MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!(%path, error = %err, "STATIC_BROTLI_PRECOMPRESS_V1: 讀取資產本文失敗，略過預壓縮");
            if let Ok(mut map) = PRECOMPRESSED.write() {
                map.remove(&path);
            }
            return (StatusCode::INTERNAL_SERVER_ERROR, "static asset unavailable").into_response();
        }
    };
    let content_type = parts.headers.get(header::CONTENT_TYPE).cloned();
    let cache_control = parts.headers.get(header::CACHE_CONTROL).cloned();
    let raw = bytes.clone();
    let key = path.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let raw_len = raw.len();
        let compressed = tokio::task::spawn_blocking(move || brotli_compress(&raw)).await;
        match compressed {
            Ok(Ok(out)) => {
                let compressed_len = out.len();
                if let Ok(mut map) = PRECOMPRESSED.write() {
                    map.insert(
                        key.clone(),
                        PrecompressState::Ready(Arc::new(PrecompressedAsset {
                            body: axum::body::Bytes::from(out),
                            content_type,
                            cache_control,
                        })),
                    );
                }
                info!(
                    path = %key,
                    raw_len,
                    compressed_len,
                    elapsed_ms = started.elapsed().as_millis(),
                    "STATIC_BROTLI_PRECOMPRESS_V1: 靜態資產預壓縮完成"
                );
            }
            other => {
                warn!(path = %key, result = ?other.map(|r| r.map(|v| v.len())), "STATIC_BROTLI_PRECOMPRESS_V1: 預壓縮失敗，下次請求再試");
                if let Ok(mut map) = PRECOMPRESSED.write() {
                    map.remove(&key);
                }
            }
        }
    });
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

pub(super) fn admin_static_asset(content_type: &'static str, body: &str) -> Response {
    // ASSET_VERSION_AUTO_V1：JS/CSS 內部互相引用的 /admin/static/... 版號
    // （如 mobile-nav.js 注入 mobile-navigation.css?v=…）也在輸出時統一改寫。
    let body = if content_type.starts_with("text/") || content_type.contains("javascript") {
        super::asset_versions::rewrite_asset_versions(body).into_owned()
    } else {
        body.to_string()
    };
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(ADMIN_STATIC_CACHE_CONTROL),
    );
    response
}

/// CSS_BUNDLE_V1：dashboard 專用 CSS 合併成一個回應（一個請求取代原本 9 個 <link>＋
/// 18 段內聯 <style>）。順序即原 dashboard.html 的層疊順序，不可調換：
///   mobile-navigation → mobile-timetable → account-components → dashboard
///   → dashboard-inline（原 head 內聯 14 段）→ dashboard-mobile → maintenance-components
///   → dashboard-components → dashboard-access → dashboard-late-fixes
///   → dashboard-inline-late（原 body 內聯 4 段，維持「最後、最高優先」）。
/// 各單檔路由仍保留（login.html 仍單獨載入 dashboard.css／dashboard-late-fixes.css）。
static DASHBOARD_BUNDLE_CSS: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
    let parts: [(&str, &str); 11] = [
        ("mobile-navigation.css", pages::MOBILE_NAVIGATION_CSS),
        ("mobile-timetable.css", pages::MOBILE_TIMETABLE_CSS),
        ("account-components.css", pages::ACCOUNT_COMPONENTS_CSS),
        ("dashboard.css", pages::DASHBOARD_MODULE_CSS),
        ("dashboard-inline.css", pages::DASHBOARD_INLINE_CSS),
        ("dashboard-mobile.css", pages::DASHBOARD_MOBILE_CSS),
        ("maintenance-components.css", pages::MAINTENANCE_COMPONENTS_CSS),
        ("dashboard-components.css", pages::DASHBOARD_COMPONENTS_CSS),
        ("dashboard-access.css", pages::DASHBOARD_ACCESS_CSS),
        ("dashboard-late-fixes.css", pages::DASHBOARD_LATE_FIXES_CSS),
        ("dashboard-inline-late.css", pages::DASHBOARD_INLINE_LATE_CSS),
    ];
    let total: usize = parts.iter().map(|(_, css)| css.len() + 64).sum();
    let mut out = String::with_capacity(total);
    out.push_str("/* CSS_BUNDLE_V1: dashboard bundle */\n");
    for (name, css) in parts {
        // 每段都以換行結尾並關掉可能沒關的註解，避免上一段末尾影響下一段解析。
        out.push_str("\n/* ===== ");
        out.push_str(name);
        out.push_str(" ===== */\n");
        out.push_str(css);
        out.push_str("\n/**/\n");
    }
    out
});

pub(super) async fn admin_dashboard_bundle_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", DASHBOARD_BUNDLE_CSS.as_str())
}

// INLINE_SPLIT_V1：抽出的頁面內聯 script/style。
pub(super) async fn admin_dashboard_head_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_HEAD_JS)
}
pub(super) async fn admin_dashboard_main_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MAIN_JS)
}
pub(super) async fn admin_dashboard_body_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_BODY_JS)
}
pub(super) async fn admin_files_head_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::FILES_HEAD_JS)
}
pub(super) async fn admin_files_main_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::FILES_MAIN_JS)
}
pub(super) async fn admin_files_page_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::FILES_PAGE_CSS)
}
pub(super) async fn admin_timetable_head_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::TIMETABLE_HEAD_JS)
}
pub(super) async fn admin_timetable_main_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::TIMETABLE_MAIN_JS)
}
pub(super) async fn admin_timetable_page_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::TIMETABLE_PAGE_CSS)
}
pub(super) async fn admin_ai_page_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::AI_PAGE_JS)
}
pub(super) async fn admin_ai_page_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::AI_PAGE_CSS)
}

pub(super) async fn admin_theme_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", &pages::theme_css())
}

pub(super) async fn admin_session_security_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::SESSION_SECURITY_JS,
    )
}

pub(super) async fn admin_notifications_page_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::NOTIFICATIONS_PAGE_JS,
    )
}

pub(super) async fn admin_app_settings_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::APP_SETTINGS_JS,
    )
}

pub(super) async fn admin_dashboard_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::DASHBOARD_MODULE_CSS)
}

pub(super) async fn admin_dashboard_late_fixes_css_handler() -> Response {
    admin_static_asset(
        "text/css; charset=utf-8",
        pages::DASHBOARD_LATE_FIXES_CSS,
    )
}

pub(super) async fn admin_dashboard_mobile_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::DASHBOARD_MOBILE_CSS)
}

pub(super) async fn admin_dashboard_components_css_handler() -> Response {
    admin_static_asset(
        "text/css; charset=utf-8",
        pages::DASHBOARD_COMPONENTS_CSS,
    )
}

pub(super) async fn admin_dashboard_access_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::DASHBOARD_ACCESS_CSS)
}

pub(super) async fn admin_dashboard_runtime_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::DASHBOARD_RUNTIME_JS,
    )
}

pub(super) async fn admin_dashboard_modules_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::DASHBOARD_FEATURES_JS,
    )
}

pub(super) async fn admin_mobile_navigation_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::MOBILE_NAVIGATION_CSS)
}

pub(super) async fn admin_mobile_timetable_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::MOBILE_TIMETABLE_CSS)
}

pub(super) async fn admin_account_components_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::ACCOUNT_COMPONENTS_CSS)
}

pub(super) async fn admin_maintenance_components_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::MAINTENANCE_COMPONENTS_CSS)
}

pub(super) async fn admin_maintenance_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::MAINTENANCE_MODULE_JS,
    )
}

pub(super) async fn admin_maintenance_implementation_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::MAINTENANCE_IMPLEMENTATION_JS,
    )
}

pub(super) async fn admin_timetable_ui_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::TIMETABLE_UI_MODULE_JS,
    )
}

pub(super) async fn admin_mock_rollcall_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::MOCK_ROLLCALL_MODULE_JS,
    )
}

pub(super) async fn admin_accounts_module_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::ACCOUNTS_MODULE_JS,
    )
}

pub(super) async fn admin_core_api_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::CORE_API_JS)
}

pub(super) async fn admin_web_push_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::WEB_PUSH_JS)
}
// DASHBOARD_LIVE_EVENTS_V1
pub(super) async fn admin_live_events_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::LIVE_EVENTS_JS)
}

// SUBPAGE_FETCH_SHARED_V1
pub(super) async fn admin_subpage_fetch_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::SUBPAGE_FETCH_JS)
}

// CLIENT_ERROR_REPORT_V1
pub(super) async fn admin_client_log_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::CLIENT_LOG_JS)
}

pub(super) async fn admin_dompurify_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DOMPURIFY_JS)
}

pub(super) async fn admin_trusted_types_init_js_handler() -> Response {
    admin_static_asset(
        "application/javascript; charset=utf-8",
        pages::TRUSTED_TYPES_INIT_JS,
    )
}

pub(super) async fn admin_jszip_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::JSZIP_MIN_JS)
}

pub(super) async fn admin_office_view_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::OFFICE_VIEW_JS)
}

// QR_SHARE_V1: 分享彈窗的 QR code 產生函式庫（離線內建）。
pub(super) async fn admin_qrcode_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::QR_CODE_MIN_JS)
}

// SHARE_ADMIN_V1: 分享連結管理頁前端。
pub(super) async fn admin_shares_admin_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::SHARES_ADMIN_JS)
}

// DASH_ASSET_V1: dashboard 資產化模組。
pub(super) async fn admin_dashboard_particles_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_PARTICLES_JS)
}
pub(super) async fn admin_dashboard_login_card_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_LOGIN_CARD_JS)
}
pub(super) async fn admin_dashboard_live_alerts_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_LIVE_ALERTS_JS)
}
// DASH_ASSET_V2
pub(super) async fn admin_dashboard_permissions_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_PERMISSIONS_JS)
}
pub(super) async fn admin_dashboard_cookie_badge_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_COOKIE_BADGE_JS)
}
pub(super) async fn admin_dashboard_modal_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MODAL_JS)
}
// DASH_ASSET_V3
pub(super) async fn admin_dashboard_header_more_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_HEADER_MORE_JS)
}
pub(super) async fn admin_dashboard_more_buttons_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MORE_BUTTONS_JS)
}
pub(super) async fn admin_dashboard_more_links_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MORE_LINKS_JS)
}
pub(super) async fn admin_dashboard_more_pages_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MORE_PAGES_JS)
}
pub(super) async fn admin_dashboard_notify_purge_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_NOTIFY_PURGE_JS)
}
pub(super) async fn admin_dashboard_desktop_size_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_DESKTOP_SIZE_JS)
}
pub(super) async fn admin_dashboard_header_spacious_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_HEADER_SPACIOUS_JS)
}
pub(super) async fn admin_dashboard_more_actions_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_MORE_ACTIONS_JS)
}
// SHARED_MOBILE_NAV_V1
pub(super) async fn shared_mobile_nav_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::SHARED_MOBILE_NAV_JS)
}
// ONBOARDING_GUIDE_V1
pub(super) async fn admin_dashboard_onboarding_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::DASHBOARD_ONBOARDING_JS)
}

// SHARE_V1: 免登入分享預覽頁的外掛 JS/CSS。
pub(super) async fn admin_share_preview_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::SHARE_PREVIEW_JS)
}
pub(super) async fn admin_share_css_handler() -> Response {
    admin_static_asset("text/css; charset=utf-8", pages::SHARE_CSS)
}

// PDFJS_PREVIEW_V1: PDF.js 主程式與 worker。worker 由 pdf.js 在執行期以
// workerSrc 同源載入（worker-src 'self' 合法）。
pub(super) async fn admin_pdf_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::PDF_MIN_JS)
}

pub(super) async fn admin_pdf_worker_js_handler() -> Response {
    admin_static_asset("application/javascript; charset=utf-8", pages::PDF_WORKER_MIN_JS)
}
