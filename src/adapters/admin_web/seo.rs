//! 公開介紹頁與 SEO（SEO_LANDING_V1，fix60）
//!
//! 需求原話：「我希望能在google打關鍵字就能搜尋到我的網站」。
//!
//! 為什麼需要這一支模組：整站（`/admin`、`/pub`、`/tc` 反向代理）都在登入牆後面，
//! Google 只看得到登入頁，任何關鍵字都不會有排名。搜尋引擎能收錄的只有「公開內容」，
//! 因此這裡提供三件事：
//!
//! 1. `GET /`：未登入時回公開介紹頁（`static/landing.html`），已登入時照舊 302 到
//!    `/admin`——對既有使用者完全無感。
//! 2. `GET /robots.txt`：明確「只允許首頁」被檢索，其餘路徑（含面板、分享連結、
//!    iClass 反向代理）全部不給爬；並附上 Sitemap 位置。
//! 3. `GET /sitemap.xml`：只列公開介紹頁（本站唯一該被收錄的網址）。
//! 4. INDEXNOW_V1（fix105）：`GET /{key}.txt` 由 `indexnow.rs` 提供（Bing 的所有權驗證檔），
//!    並在啟動後主動把首頁提交給 Bing（見該檔說明）。
//!
//! 另外面板與私有路徑的 `X-Robots-Tag: noindex` 由 `mod.rs` 的中間件統一補上——
//! robots.txt 只是「請不要爬」，`noindex` 才是「不要收錄」，兩者併用才乾淨。
//!
//! 不想被收錄的路徑請維持在 `super::path_is_private_for_search` 這一份清單裡。

use super::*;

/// 公開介紹頁（單檔、自帶樣式、無外部資源：不必等 CDN，也符合本站 CSP）。
pub(super) const LANDING_HTML: &str = include_str!("static/landing.html");

/// 沒有 `Host` 標頭（或標頭不合法）時的後備網域。
///
/// 只用於產生 canonical／og:url／sitemap 的絕對網址；真正上線的請求一定帶 `Host`，
/// 所以就算網域換了，只要請求是打到新網域，canonical 就會自動跟著換（不必改這裡）。
const DEFAULT_PUBLIC_HOST: &str = "dashboard.jasoncloudserver.me";

/// `Host` 標頭的合理長度上限（DNS 名稱 253 字元，另加埠號）。
const HOST_HEADER_MAX_CHARS: usize = 260;

/// 搜尋引擎提供的站台驗證 meta（使用者沒有 DNS 控制權，只能用 HTML 標記法）。
///
/// 值由環境變數提供，於啟動後第一次請求時讀取一次並快取：
///   `GOOGLE_SITE_VERIFICATION=<Search Console 給的值>`
///   `BING_SITE_VERIFICATION=<Bing Webmaster 給的值>`（可選）
///
/// 一律先過 `sanitize_verification_token`（只留 `[A-Za-z0-9_-]`），
/// 避免環境變數被寫入 HTML 屬性而注入其他標籤。
fn verification_meta_tags() -> &'static str {
    static TAGS: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
        let mut out = String::new();
        for (env_key, meta_name) in [
            ("GOOGLE_SITE_VERIFICATION", "google-site-verification"),
            ("BING_SITE_VERIFICATION", "msvalidate.01"),
        ] {
            let Ok(raw) = std::env::var(env_key) else {
                continue;
            };
            let cleaned = sanitize_verification_token(&raw);
            if cleaned.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "<meta name=\"{meta_name}\" content=\"{cleaned}\">\n"
            ));
        }
        out
    });
    TAGS.as_str()
}

/// 驗證值只允許英數與 `-` `_`（Google／Bing 給的都是這種字元），長度上限 200。
fn sanitize_verification_token(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(200)
        .collect()
}

/// 從 `Host` 標頭取出對外網域（含埠號），不合法時回 `None`。
///
/// 只接受英數、`.`、`-`、`:`（IPv6 不對外服務，直接略過）——避免標頭內容被寫進
/// HTML 屬性（canonical／og:url）或 XML（sitemap）。
fn host_header_value(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::HOST)?.to_str().ok()?.trim();
    if raw.is_empty() || raw.len() > HOST_HEADER_MAX_CHARS {
        return None;
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
    {
        return None;
    }
    // 只留下「host」或「host:port」兩種形狀。
    let mut parts = raw.splitn(2, ':');
    let host = parts.next().unwrap_or_default();
    if host.is_empty() {
        return None;
    }
    if let Some(port) = parts.next() {
        if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    Some(raw.to_string())
}

/// 本站對外 origin（`https://host`），供 canonical／og:url／sitemap 使用。
///
/// 本機開發（localhost／127.0.0.1）用 `http://`，其餘一律 `https://`
/// （正式站由 Cloudflare Tunnel 提供 TLS）。
pub(super) fn request_origin(headers: &HeaderMap) -> String {
    let host = host_header_value(headers).unwrap_or_else(|| DEFAULT_PUBLIC_HOST.to_string());
    let bare = host.split(':').next().unwrap_or_default();
    let scheme = if bare == "localhost" || bare == "127.0.0.1" || bare == "[::1]" {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{host}")
}

/// 正式站的對外 origin（`https://<DEFAULT_PUBLIC_HOST>`）。
///
/// INDEXNOW_V1（fix105）：IndexNow 的提交與金鑰檔都在這個網域底下；
/// 管理員手動提交也用同一個值（可用環境變數 `INDEXNOW_ORIGIN` 覆蓋，見 [`submit_origin`]）。
pub(super) fn default_public_origin() -> String {
    format!("https://{DEFAULT_PUBLIC_HOST}")
}

/// 提交給搜尋引擎時要用的對外 origin。
///
/// **刻意不讀請求的 `Host`**（fix106）：這個值只在管理員按「立即提交給 Bing」時用到，
/// 而 IndexNow 要的是「Bing 實際看到的那個網域」（金鑰檔也在同一個 host）。
/// 從請求推導的話，管理員用內網 IP／區網名稱開面板時就會把錯的網址送出去；
/// 改成固定用正式站網域，必要時用環境變數 `INDEXNOW_ORIGIN` 覆蓋（換網域不必改程式）。
pub(super) fn submit_origin() -> String {
    if let Ok(raw) = std::env::var("INDEXNOW_ORIGIN") {
        let cleaned = raw.trim().trim_end_matches('/').to_string();
        let host = cleaned.split("://").nth(1).unwrap_or_default();
        // 只接受「https ＋ 網域含點」的形狀（避免把 localhost／內網位址送去 IndexNow）。
        if cleaned.starts_with("https://")
            && host.contains('.')
            && !host.starts_with("localhost")
            && !host.starts_with("127.0.0.1")
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
        {
            return cleaned;
        }
        tracing::warn!(
            value = %raw,
            "INDEXNOW_ORIGIN 不是合法的公開 https 網址，改用預設公開網域"
        );
    }
    default_public_origin()
}

/// 組出要送出的介紹頁：換掉 origin 與驗證 meta 兩個佔位符。
fn landing_html(headers: &HeaderMap) -> String {
    LANDING_HTML
        .replace("__ORIGIN__", &request_origin(headers))
        .replace("__VERIFICATION_META__", verification_meta_tags())
}

/// `GET /`：公開介紹頁。
///
/// 回應一律走 `html_page_response()`：統一補雜湊 CSP、資產版號改寫與 ETag／304，
/// 與站內其他頁面同一套管線（不會有「介紹頁少了安全標頭」的破口）。
pub(super) fn landing_page_response(headers: &HeaderMap) -> Response {
    html_page_response(headers, landing_html(headers))
}

/// `GET /robots.txt`
///
/// 策略：**只允許首頁**（`Allow: /$` 的比對比 `Disallow: /` 更精確，因此首頁放行、
/// 其餘全擋），另外放行 favicon 與 PWA 圖示——Google 搜尋結果會抓 favicon，
/// 被 robots 擋掉的話結果只會顯示預設地球圖示。
fn robots_txt(headers: &HeaderMap) -> String {
    let origin = request_origin(headers);
    // ROBOTS_PRECISE_PATHS_V1（fix61）：私有路徑要「逐條、且比 `/` 更長」地列出。
    //
    // 為什麼不能只寫一條 `Disallow: /`：同一個 `User-agent: *` 出現在多個群組時，
    // Google 是把規則**合併**（後面的群組不會覆蓋前面的）；若中間夾進別人的
    // `Allow: /`（例如 Cloudflare 的 Managed robots.txt），就會出現兩條等長又衝突的
    // 規則，而 Google 在衝突時採用**最寬鬆**的一條 → `Allow: /` 勝出、`Disallow: /`
    // 失效（私有路徑開始被爬）。改成 `/admin`、`/pub` 這種較長路徑後，長度勝出，
    // 私有路徑依然被擋；最後一條 `Disallow: /` 保留，作為沒有外來 Allow 時的本體。
    // ROBOTS_NO_BLANKET_DISALLOW_V1（fix105）：這裡刻意「不再」寫一條 `Disallow: /`。
    //
    // 為什麼：Cloudflare 的 Managed robots.txt 會在本檔**前面**插入一組
    // `User-agent: *` ＋ `Allow: /`。兩組合併後 `Allow: /` 與 `Disallow: /` 一樣長、互相衝突：
    // Google 取較寬鬆的那條（首頁可爬，這也是它一直收得到的原因），Bing 的判定則不明確 ——
    // 只要它選了 `Disallow: /`，整站（含首頁）就再也進不了 Bing 索引。
    // 改成「逐條列出所有非公開前綴」後，每條都比 `/` 長、一定勝過外來的 `Allow: /`，
    // 而首頁永遠不會被 blanket 規則誤傷。**新增路由時請同步補上這裡與 `mod.rs` 的
    // `path_is_private_for_search`**（`checks/seo_check.py` 會逐條比對兩份清單）。
    format!(
        r#"# jasonserver — 只有公開介紹頁需要被檢索；其餘路徑都需要登入（見 X-Robots-Tag）。
#
# 本檔沒有 `Disallow: /`（原因見 seo.rs 的 ROBOTS_NO_BLANKET_DISALLOW_V1 註解）：
# 逐條列出所有非公開前綴，長度一定勝過 Cloudflare 在前面插入的 `Allow: /`，
# 而且不會讓首頁被 blanket 規則誤傷（Bing 的判定方式與 Google 不同）。
User-agent: *
Allow: /$
Allow: /favicon.ico
Allow: /favicon.png
Allow: /apple-touch-icon.png
Allow: /admin/pwa/icon-192.png
Allow: /admin/pwa/icon-512.png
Allow: {indexnow_key_path}
Disallow: /admin
Disallow: /pub
Disallow: /api/
Disallow: /tc/
Disallow: /static/
Disallow: /course/
Disallow: /uploads/
Disallow: /bulletin-list
Disallow: /homework
Disallow: /scanner
Disallow: /number
Disallow: /radar
Disallow: /healthz
Disallow: /manifest.webmanifest
Disallow: /service-worker.js
Disallow: /.well-known/

Sitemap: {origin}/sitemap.xml
"#,
        // INDEXNOW_V1：金鑰檔（Bing 驗證所有權時會來抓它）不能被 robots 擋住。
        indexnow_key_path = super::indexnow::key_path(),
    )
}

pub(super) async fn robots_txt_handler(headers: HeaderMap) -> Response {
    let mut response = robots_txt(&headers).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    response
}

/// INDEXNOW_V1（fix105）：搜尋引擎收錄狀態（設定頁「搜尋引擎收錄」卡片顯示用）。
#[derive(Debug, Clone, Serialize)]
pub(super) struct SeoStatus {
    /// Bing Webmaster Tools 的站台驗證 meta 是否已設定（環境變數 `BING_SITE_VERIFICATION`）。
    pub bing_verification: bool,
    /// Google Search Console 的站台驗證 meta 是否已設定（`GOOGLE_SITE_VERIFICATION`）。
    pub google_verification: bool,
    /// IndexNow 金鑰與最近一次提交結果。
    pub indexnow: super::indexnow::IndexNowStatus,
    /// 唯一該被收錄的網址（公開介紹頁）。
    pub landing: String,
    /// sitemap 的完整網址（要提交到搜尋引擎時可直接貼）。
    pub sitemap: String,
}

/// 站台驗證用的環境變數有沒有設（值是空的＝沒設）。
fn verification_env_set(name: &str) -> bool {
    std::env::var(name)
        .map(|raw| !sanitize_verification_token(&raw).is_empty())
        .unwrap_or(false)
}

pub(super) fn seo_status(headers: &HeaderMap) -> SeoStatus {
    let origin = request_origin(headers);
    SeoStatus {
        bing_verification: verification_env_set("BING_SITE_VERIFICATION"),
        google_verification: verification_env_set("GOOGLE_SITE_VERIFICATION"),
        indexnow: super::indexnow::status(&origin),
        landing: format!("{origin}/"),
        sitemap: format!("{origin}/sitemap.xml"),
    }
}

/// `GET /sitemap.xml`
///
/// 本站只有一個公開網址（介紹頁），其餘都在登入牆後，列進去只會產生
/// 「已提交但無法索引」的雜訊。刻意不加 `lastmod`：我們沒有可靠的內容更新時間，
/// 給了不實的日期反而讓 crawler 對 sitemap 失去信任。
fn sitemap_xml(headers: &HeaderMap) -> String {
    let origin = request_origin(headers);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url>
    <loc>{origin}/</loc>
    <changefreq>weekly</changefreq>
    <priority>1.0</priority>
  </url>
</urlset>
"#
    )
}

pub(super) async fn sitemap_xml_handler(headers: HeaderMap) -> Response {
    let mut response = sitemap_xml(&headers).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    response
}
