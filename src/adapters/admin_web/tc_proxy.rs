//! TRONCLASS_PROXY_JUMP_V1：iClass 反向代理與「免登入直達」入口。
//!
//! 使用者點作業卡片的「在 iClass 開啟」→ GET /admin/tc-go 記住代理帳號
//!（httpOnly cookie）並 302 到 `/tc/course/{course_id}/learning-activity/full-screen#{item_id}`；
//! 之後該分頁的所有請求（頁面本體、`/static` 資源、`/api` XHR）都由本模組
//! 轉發到 iClass，學校 session 由伺服器端帳號出面——使用者完全不必登入學校。
//!
//! 已實測（2026-09-14，ession 由使用者提供做一次性驗證）：
//! - 作業 URL：`/course/{course_id}/learning-activity/full-screen#/{item_id}`，
//!   `item_id` 即 `/api/courses/{id}/homework-activities` 回應的 `id`。
//! - SPA 資源為根絕對路徑（`/static/...`），頁面無 `<base>` 標籤，
//!   因此頂層 `/static`、`/api`、`/course`、`/uploads` 一併掛代理即可。
//!
//! 安全：
//! - 鑑權（TC_PUBLIC_STATIC_V1／TC_REFERER_GRANT_V1，fix35）：`/static/*` 是
//!   iClass 的編譯前端資產，實測免 cookie 即可從 iclass.tku.edu.tw 取得
//!   （登入頁自己也用它們）——代理層直接放行，不需面板鑑權也不需帳號。
//!   其餘路徑要求有效面板 session；cookie 失效時 fallback：URL 一次性
//!   token → tc_proxy_ticket 票券 cookie → Referer 內的頁面授權 token。
//! - 非 admin session 只能代理「自己」的帳號（tc_proxy_account cookie 驗證）。
//! - 上游回應的 Set-Cookie 一律剝除：iClass 的同名 `session` cookie 若種到
//!   面板網域，會覆蓋面板登入。
//! - 上游 30x 的 Location 若指向 iClass 網域，改寫為相對路徑（瀏覽器留在
//!   代理內循環），reqwest 不自動跟隨轉址。

use super::*;
use axum::response::Redirect;

/// 代理帳號選擇 cookie（httpOnly；由 /admin/tc-go 設定，Path=/ 讓
/// /tc、/static、/api、/course、/uploads 都帶得到）。
const TC_ACCOUNT_COOKIE: &str = "tc_proxy_account";
const TC_ACCOUNT_COOKIE_MAX_AGE_SECS: u64 = 8 * 60 * 60;
/// TC_NAV_TOKEN_V1（fix34）：短效票券 cookie——實測使用者瀏覽器對 /tc/ 頂層
/// 導航不帶任何 cookie，但 /tc/ 頁面的子資源請求（/static、/api）正常帶。
/// 導航用 URL 內的一次性 token 鑑權；子資源在票券效期（120 秒）內用它。
const TC_TICKET_COOKIE: &str = "tc_proxy_ticket";
/// 轉發 body 上限（未來供作業上傳等場景）。
pub(super) const TC_PROXY_BODY_LIMIT: usize = 32 * 1024 * 1024;
/// 請求轉發到上游的 header 白名單（原始 Cookie 一律不轉發）。
const FORWARD_REQ_HEADERS: &[&str] = &[
    "content-type",
    "accept",
    "accept-language",
    "range",
    "x-requested-with",
];
/// 回應不轉發給瀏覽器的 header（set-cookie 必剝，見模組註解）。
const DROP_RESP_HEADERS: &[&str] = &[
    "set-cookie",
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
];

// TC_PROXY_TLS_FIX_V1（fix24）：本專案 reqwest 是 default-features=false，
// Client::builder() 必須明確 use_rustls_tls()，否則 build() 回 Err。舊版在這裡
// expect() 直接 panic——axum 沒掛 CatchPanicLayer，連線瞬斷，Cloudflare
// 合成 502 錯誤頁（症狀：只有 /tc/ 路由 502，其他功能正常）。
// 治根因之外再加兩層防禦：① build 失敗改存 None（請求時回錯誤頁，不再 panic）
// ② 沿用 api::build_http_client 同款行動版 UA——上游對此 UA 行為已知。
static TC_HTTP: once_cell::sync::Lazy<Option<reqwest::Client>> =
    once_cell::sync::Lazy::new(|| {
        let mut default_headers = reqwest::header::HeaderMap::new();
        // TC_PROXY_TLS_FIX_V1（fix25）：from_static 直接回 HeaderValue（非 Result；
        // 回 Result 的是 from_str），照 api::build_http_client 的原樣賦值。
        default_headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148 Tronclass/5.0",
            ),
        );
        reqwest::Client::builder()
            .default_headers(default_headers)
            .timeout(std::time::Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .use_rustls_tls()
            .build()
            .ok()
    });

// TC_PROXY_RELOGIN_V1（fix26）：session 失效時由伺服器自動重登（AuthClient
// 帳密流程），重登成功存回 DB 並重試原請求一次。**使用者永遠不該在代理分頁
// 輸入學校帳密**：代理必剝上游 Set-Cookie（防覆蓋面板登入），瀏覽器登入
// 永遠拿不到 session——在代理內登入會無限循環（fix25 部署後的實測症狀）。
// 節流：每帳號 60 秒內最多重登一次（13 帳號同時失效也不會狂打學校端）。
static RELOGIN_THROTTLE: once_cell::sync::Lazy<
    tokio::sync::Mutex<HashMap<String, Instant>>,
> = once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(HashMap::new()));

#[derive(Debug, Deserialize)]
pub(super) struct TcGoQuery {
    #[serde(default)]
    pub(super) token: String,
    #[serde(default)]
    pub(super) account_id: Option<String>,
    #[serde(default)]
    pub(super) course_id: Option<i64>,
    #[serde(default)]
    pub(super) item_id: Option<i64>,
    #[serde(default)]
    pub(super) kind: String,
}

/// GET /admin/tc-go：記住代理帳號 → 302 到代理內的 iClass 目標頁。
pub(super) async fn tc_go_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<TcGoQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    // TC_REFRESH_FALLBACK_V1（fix30）：按鈕是頂層導航，改用頁面層同款
    // 「access → refresh 輪換」解析。access 失效（其他分頁剛 refresh 過、
    // rotate 撤銷了所有舊 token；或 12h TTL 到期）但 refresh cookie（7 天）
    // 仍有效時，伺服器端直接換發並把新 cookie 種回，導航繼續不中斷。
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &token).await else {
        // access＋refresh 都無效：送往登入頁（頂層導航回 JSON 對使用者不友善；
        // 登入後回 dashboard，再按一次按鈕即可）。
        return Redirect::to("/admin?next=%2Fadmin%2Fdashboard").into_response();
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(
            Redirect::to("/admin/password?first=1").into_response(),
            &page,
            &headers,
        );
    }
    let session = page.session.clone();
    let account_id = query
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(&session.account_id)
        .to_string();
    if !session.is_admin && account_id != session.account_id {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "只能為自己的帳號開啟 iClass 頁面" })),
        )
            .into_response();
    }
    let Some(raw) = state.accounts_db.get(&account_id).await.ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "找不到指定帳號" })),
        )
            .into_response();
    };
    if !raw.enabled {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "該帳號目前已停用" })),
        )
            .into_response();
    }
    // VIEWER_NO_SCHOOL_API_V1：檢視者帳號沒有學校 session 可代理。
    if raw.is_viewer {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "檢視者帳號沒有 iClass session，無法代理" })),
        )
            .into_response();
    }

    // 目標路徑（皆已實測，見模組註解）：
    // - 作業／學習活動：/course/{cid}/learning-activity/full-screen#{item_id}
    // - 公告：iClass 前端對課程公告只提供列表入口
    //   /bulletin-list/?type=course（無單則深連結；org 公告才有
    //   ?type=org&bulletin_id= 直達，面板目前未抓 org 公告）。
    let kind = query.kind.trim().to_ascii_lowercase();
    let target_path = match kind.as_str() {
        "" | "homework" | "activity" => {
            let (Some(course_id), Some(item_id)) = (query.course_id, query.item_id) else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "course_id 與 item_id 為必填" })),
                )
                    .into_response();
            };
            format!("/course/{course_id}/learning-activity/full-screen#{item_id}")
        }
        "bulletin" => "/bulletin-list/?type=course".to_string(),
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("未支援的 kind：{other}") })),
            )
                .into_response();
        }
    };

    // TC_NAV_TOKEN_V1（fix34）：實測（2026-09-14 log）使用者瀏覽器對 /tc/ 頂層
    // 導航不帶任何 cookie（has_access/has_refresh 全 false）——在轉址 URL 帶
    // 一次性 token（90 秒、用一次即焚）供導航鑑權；另種 120 秒票券 cookie
    // 供 /tc/ 頁面的子資源請求（/static、/api）短時間內重複使用。
    let nav_token = state.mint_tc_nav_token(&account_id, session.is_admin, true);
    let ticket_token = state.mint_tc_nav_token(&account_id, session.is_admin, false);
    let target_with_token = append_query_before_fragment(&target_path, "tc_token", &nav_token);
    let mut response = Redirect::to(&format!("/tc{target_with_token}")).into_response();
    let cookie = format!(
        "{TC_ACCOUNT_COOKIE}={account_id}; Path=/; Max-Age={TC_ACCOUNT_COOKIE_MAX_AGE_SECS}; HttpOnly; SameSite=Lax"
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, value);
    }
    let ticket_ttl = super::TC_NAV_TICKET_TTL_SECS;
    let ticket = format!(
        "{TC_TICKET_COOKIE}={ticket_token}; Path=/; Max-Age={ticket_ttl}; HttpOnly; SameSite=Lax"
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&ticket) {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, value);
    }
    // TC_REFRESH_FALLBACK_V1：本次若做了 refresh 輪換，新 cookie 必須隨 302
    // 種回——否則緊接的 /tc/ 請求帶著「已撤銷的 access＋已被單用消耗的
    // refresh」，會在代理鑑權直接失敗（refresh token 單用，rotate 即作廢舊值）。
    super::pages::attach_fresh_cookies(response, &page, &headers)
}

/// 代理主 handler：掛在 `/tc/{*path}` 與頂層 `/static`、`/api`、`/course`、`/uploads`。
pub(super) async fn tc_proxy_handler(
    State(state): State<AdminWebState>,
    request: axum::extract::Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let pq = parts
        .uri
        .path_and_query()
        .map(|v| v.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let upstream_raw = strip_tc_prefix(&pq);
    // TC_NAV_TOKEN_V1（fix34）：提取並剝除 tc_token——鑑權用，不轉發上游。
    let (upstream_pq, tc_token) = extract_tc_token(&upstream_raw);
    let body_bytes = match axum::body::to_bytes(body, TC_PROXY_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return tc_error_page(StatusCode::PAYLOAD_TOO_LARGE, "請求內容過大或讀取失敗"),
    };
    tc_proxy_core(
        &state,
        &parts.headers,
        parts.method,
        &upstream_pq,
        body_bytes,
        tc_token.as_deref(),
    )
    .await
}

/// `/tc/xxx` 註冊在同一 handler：剝掉 `/tc` 前綴讓上游收到原始路徑。
fn strip_tc_prefix(pq: &str) -> String {
    if let Some(rest) = pq.strip_prefix("/tc") {
        if rest.is_empty() {
            return "/".to_string();
        }
        if rest.starts_with('/') {
            return rest.to_string();
        }
    }
    pq.to_string()
}

/// TC_NAV_TOKEN_V1（fix34）：在 URL path（可能含 ?query 與 #fragment）附加
/// 一個 query 參數——必須插在 fragment 之前（fragment 不屬於 query 字串）。
/// key/value 皆為 UUID hex /固定字面值，不需 percent-encode。
fn append_query_before_fragment(path: &str, key: &str, value: &str) -> String {
    let (before_frag, frag) = match path.split_once('#') {
        Some((b, f)) => (b, format!("#{f}")),
        None => (path, String::new()),
    };
    let sep = if before_frag.contains('?') { '&' } else { '?' };
    format!("{before_frag}{sep}{key}={value}{frag}")
}

/// TC_NAV_TOKEN_V1：從 path?query 中提取並剝除 tc_token（不轉發給上游），
/// 其餘 query 參數原樣保留。回傳 (清理後的 path?query, token)。
fn extract_tc_token(pq: &str) -> (String, Option<String>) {
    let Some((path, query)) = pq.split_once('?') else {
        return (pq.to_string(), None);
    };
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    let mut token = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if name == "tc_token" {
            if token.is_none() {
                token = Some(value.into_owned());
            }
            continue;
        }
        serializer.append_pair(&name, &value);
    }
    let cleaned = serializer.finish();
    let out = if cleaned.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{cleaned}")
    };
    (out, token)
}

/// TC_NAV_TOKEN_V1：讀 tc_proxy_ticket 短效票券 cookie。
fn tc_ticket_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for pair in cookies.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        if name.trim() == TC_TICKET_COOKIE {
            let value = value.trim();
            if !value.is_empty() && value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// TC_PUBLIC_STATIC_V1（fix35）：/static/* 是 iClass 的編譯前端資產——實測
/// （2026-09-14 curl）不帶任何 cookie 即可從 iclass.tku.edu.tw 取得（登入頁
/// 本身也用它們）——代理層直接放行：不需面板鑑權、不需帳號。這覆蓋「CSS
/// 內部引用的資源」（其 Referer 是 CSS 檔 URL、不含 token）與瀏覽器不帶
/// cookie 的情境；CF 快取這些公開資產亦無安全影響。
fn is_public_tc_asset(pq: &str) -> bool {
    pq == "/static" || pq.starts_with("/static/")
}

/// TC_REFERER_GRANT_V1（fix35）：從 Referer（=發起請求的頁面完整 URL）提取
/// tc_token。實測 iClass 不送 Referrer-Policy header、HTML 也無 meta 標籤 →
/// 瀏覽器預設 strict-origin-when-cross-origin：同源請求的 Referer 帶完整
/// query（含 tc_token）。嚴格同源檢查：referer 的 host 必須等於本請求的
/// Host header（referer 有明寫 port 時 Host 也須同 port）——跨站頁面無法
/// 僞造「帶有效 token 的 referer」，token 本身就是秘密。
fn tc_token_from_referer(headers: &HeaderMap) -> Option<String> {
    let referer = headers.get(axum::http::header::REFERER)?.to_str().ok()?;
    let parsed = url::Url::parse(referer).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let referer_host = parsed.host_str()?.to_ascii_lowercase();
    let own_host = headers
        .get(axum::http::header::HOST)?
        .to_str()
        .ok()?
        .trim()
        .to_ascii_lowercase();
    let (own_name, own_port) = match own_host.split_once(':') {
        Some((name, port)) => (name.to_string(), Some(port.to_string())),
        None => (own_host.clone(), None),
    };
    if referer_host != own_name {
        return None;
    }
    if let Some(port) = parsed.port() {
        let port_matches = match own_port.as_deref() {
            Some(own) => own == port.to_string(),
            None => false,
        };
        if !port_matches {
            return None;
        }
    }
    let query = parsed.query()?;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if name == "tc_token" {
            let value = value.trim();
            if !value.is_empty() && value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// TC_NAV_LOGIN_REDIRECT_V1（fix31）：頂層導航請求（sec-fetch-dest: document）
/// 的鑑權失敗改 302 登入頁；子資源請求保持 401。缺 header 時（老瀏覽器／
/// curl）保守視為非導航，維持錯誤頁。
fn is_tc_navigation_request(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-dest")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("document"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TC_NAV_LOGIN_REDIRECT_V1：導航判定——document 是導航；XHR（empty）、
    /// 缺 header 皆非導航（保守回 401 錯誤頁，避免 fetch 被導去登入頁 HTML）。
    #[test]
    fn tc_navigation_request_detection() {
        let mut headers = HeaderMap::new();
        assert!(
            !is_tc_navigation_request(&headers),
            "缺 sec-fetch-dest 保守視為非導航"
        );
        headers.insert(
            axum::http::HeaderName::from_static("sec-fetch-dest"),
            axum::http::HeaderValue::from_static("document"),
        );
        assert!(is_tc_navigation_request(&headers));
        headers.insert(
            axum::http::HeaderName::from_static("sec-fetch-dest"),
            axum::http::HeaderValue::from_static("empty"),
        );
        assert!(
            !is_tc_navigation_request(&headers),
            "XHR(sec-fetch-dest: empty) 不是導航"
        );
        headers.insert(
            axum::http::HeaderName::from_static("sec-fetch-dest"),
            axum::http::HeaderValue::from_static("DOCUMENT"),
        );
        assert!(
            is_tc_navigation_request(&headers),
            "大小寫不敏感"
        );
    }

    /// TC_NAV_TOKEN_V1（fix34）：token 必須插在 fragment 之前、第一個 ? 之後。
    #[test]
    fn append_query_before_fragment_cases() {
        // 純 path：插 ?tc_token=…。
        assert_eq!(
            append_query_before_fragment("/course/1/full-screen", "tc_token", "abc"),
            "/course/1/full-screen?tc_token=abc"
        );
        // 已有 query（bulletin-list/?type=course）：用 & 接。
        assert_eq!(
            append_query_before_fragment("/bulletin-list/?type=course", "tc_token", "abc"),
            "/bulletin-list/?type=course&tc_token=abc"
        );
        // 有 fragment（作業深連結）：token 在 fragment 之前。
        assert_eq!(
            append_query_before_fragment("/course/263860/learning-activity/full-screen#12345", "tc_token", "abc"),
            "/course/263860/learning-activity/full-screen?tc_token=abc#12345"
        );
        // query＋fragment 同時存在。
        assert_eq!(
            append_query_before_fragment("/bulletin-list/?type=course#top", "tc_token", "abc"),
            "/bulletin-list/?type=course&tc_token=abc#top"
        );
    }

    /// TC_NAV_TOKEN_V1：extract_tc_token 提取＋剝除，且與 append 互相往返。
    #[test]
    fn extract_tc_token_roundtrip_and_strip() {
        // 無 token：原樣返回（含 query、fragment）。
        assert_eq!(
            extract_tc_token("/bulletin-list/?type=course"),
            ("/bulletin-list/?type=course".to_string(), None)
        );
        assert_eq!(
            extract_tc_token("/course/1/full-screen#12345"),
            ("/course/1/full-screen#12345".to_string(), None)
        );
        // 無 query 的 path。
        assert_eq!(
            extract_tc_token("/static/app.css"),
            ("/static/app.css".to_string(), None)
        );
        // append → extract 往返：token 剝除後其餘參數與 fragment 原樣保留。
        for target in [
            "/course/263860/learning-activity/full-screen#12345",
            "/bulletin-list/?type=course",
            "/bulletin-list/?type=course#top",
            "/api/courses/1/homework-activities?page=2",
        ] {
            let with_token = append_query_before_fragment(target, "tc_token", "deadbeef01");
            let (cleaned, token) = extract_tc_token(&with_token);
            assert_eq!(token.as_deref(), Some("deadbeef01"), "token 必須被取出：{with_token}");
            assert_eq!(cleaned, target, "剝除後必須還原原 target：{with_token}");
        }
        // token 在中間：其他參數保留（順序不變）。
        let (cleaned, token) = extract_tc_token("/api/x?tc_token=aa&type=course&b=2");
        assert_eq!(token.as_deref(), Some("aa"));
        assert_eq!(cleaned, "/api/x?type=course&b=2");
        // 重複 tc_token：取第一個、其餘全部剝除。
        let (cleaned, token) = extract_tc_token("/api/x?tc_token=first&tc_token=second&keep=1");
        assert_eq!(token.as_deref(), Some("first"));
        assert_eq!(cleaned, "/api/x?keep=1");
        // 值經 percent-encode 的參數必須原樣保留。
        let (cleaned, token) = extract_tc_token("/api/x?next=%2Fadmin%2Fdash&tc_token=t1");
        assert_eq!(token.as_deref(), Some("t1"));
        assert_eq!(cleaned, "/api/x?next=%2Fadmin%2Fdash");
    }

    /// TC_NAV_TOKEN_V1：票券 cookie 解析——名稱精確匹配、空值／非 hex 拒收。
    #[test]
    fn tc_ticket_cookie_parsing() {
        let mut headers = HeaderMap::new();
        assert!(tc_ticket_from_headers(&headers).is_none(), "無 Cookie header");
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("tc_proxy_account=414850098; other=1"),
        );
        assert!(tc_ticket_from_headers(&headers).is_none(), "沒有票券 cookie");
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("tc_proxy_account=414850098; tc_proxy_ticket=abcdef0123456789; session=x"),
        );
        assert_eq!(
            tc_ticket_from_headers(&headers).as_deref(),
            Some("abcdef0123456789")
        );
        // 空值：視同無票券（consume 會回 None）。
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("tc_proxy_ticket="),
        );
        assert!(tc_ticket_from_headers(&headers).is_none(), "空值拒收");
        // 非 hex 值（避免奇怪字串進 map 查詢）：拒收。
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("tc_proxy_ticket=hello world!"),
        );
        assert!(tc_ticket_from_headers(&headers).is_none(), "非 hex 拒收");
    }

    /// TC_PUBLIC_STATIC_V1（fix35）：僅 /static 家族是公開資產。
    #[test]
    fn public_static_asset_classification() {
        assert!(is_public_tc_asset("/static"));
        assert!(is_public_tc_asset("/static/app.css"));
        assert!(is_public_tc_asset("/static/assets/images/large/fe4b5803bb09db57bab8.png"));
        assert!(is_public_tc_asset("/static/runtime~js/lms-main-63a6d86e.js"));
        // 相似前綴與其他路徑不得誤放行。
        assert!(!is_public_tc_asset("/statics/app.css"));
        assert!(!is_public_tc_asset("/staticx"));
        assert!(!is_public_tc_asset("/api/courses/1/homework-activities"));
        assert!(!is_public_tc_asset("/uploads/file.png"));
        assert!(!is_public_tc_asset("/course/1/learning-activity/full-screen"));
        assert!(!is_public_tc_asset("/favicon.ico"));
        assert!(!is_public_tc_asset("/"));
    }

    /// TC_REFERER_GRANT_V1（fix35）：Referer 解析——嚴格同源、query 帶 token。
    #[test]
    fn tc_token_from_referer_parsing() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("dashboard.example.me"),
        );
        // 無 Referer → None。
        assert!(tc_token_from_referer(&headers).is_none());
        // 同源 + query 帶 token → Some。
        headers.insert(
            axum::http::header::REFERER,
            axum::http::HeaderValue::from_static(
                "https://dashboard.example.me/tc/course/263860/learning-activity/full-screen?tc_token=abc123def456",
            ),
        );
        assert_eq!(
            tc_token_from_referer(&headers).as_deref(),
            Some("abc123def456")
        );
        // 同源、其他 query 參數在前 → 仍取得到 token。
        headers.insert(
            axum::http::header::REFERER,
            axum::http::HeaderValue::from_static(
                "https://dashboard.example.me/tc/bulletin-list/?type=course&tc_token=ff0011",
            ),
        );
        assert_eq!(tc_token_from_referer(&headers).as_deref(), Some("ff0011"));
        // 跨站 referer → None。
        headers.insert(
            axum::http::header::REFERER,
            axum::http::HeaderValue::from_static("https://evil.example.com/tc/x?tc_token=abc123def456"),
        );
        assert!(tc_token_from_referer(&headers).is_none(), "跨站 referer 拒收");
        // 同源但無 query／無 token → None。
        headers.insert(
            axum::http::header::REFERER,
            axum::http::HeaderValue::from_static("https://dashboard.example.me/tc/course/1"),
        );
        assert!(tc_token_from_referer(&headers).is_none());
        // CSS 檔當 referer（CSS 內部引用的資源）→ 無 token，None（走公開路徑）。
        headers.insert(
            axum::http::header::REFERER,
            axum::http::HeaderValue::from_static(
                "https://dashboard.example.me/static/styles-lms-main-073337ae.css",
            ),
        );
        assert!(tc_token_from_referer(&headers).is_none());
        // 非 http(s) scheme → None。
        headers.insert(
            axum::http::header::REFERER,
            axum::http::HeaderValue::from_static("data:text/html?tc_token=abc123def456"),
        );
        assert!(tc_token_from_referer(&headers).is_none());
        // malformed referer → None。
        headers.insert(
            axum::http::header::REFERER,
            axum::http::HeaderValue::from_static("not a url"),
        );
        assert!(tc_token_from_referer(&headers).is_none());
        // host 大小寫不敏感。
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("Dashboard.Example.Me"),
        );
        headers.insert(
            axum::http::header::REFERER,
            axum::http::HeaderValue::from_static("https://dashboard.example.me/tc/x?tc_token=00ff00"),
        );
        assert_eq!(tc_token_from_referer(&headers).as_deref(), Some("00ff00"));
        // referer 明寫 port 但 Host 沒有 → 拒收（同源認定從嚴）。
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("dashboard.example.me"),
        );
        headers.insert(
            axum::http::header::REFERER,
            axum::http::HeaderValue::from_static("https://dashboard.example.me:8443/tc/x?tc_token=00ff00"),
        );
        assert!(tc_token_from_referer(&headers).is_none(), "port 不符拒收");
        // 無 Host header（HTTP/2 :authority 之外的罕見情況）→ None（從嚴）。
        let mut no_host = HeaderMap::new();
        no_host.insert(
            axum::http::header::REFERER,
            axum::http::HeaderValue::from_static("https://dashboard.example.me/tc/x?tc_token=00ff00"),
        );
        assert!(tc_token_from_referer(&no_host).is_none());
    }
}
async fn tc_proxy_core(
    state: &AdminWebState,
    headers: &HeaderMap,
    method: axum::http::Method,
    upstream_pq: &str,
    body: axum::body::Bytes,
    tc_token: Option<&str>,
) -> Response {
    // ── 面板鑑權（TC_NAV_TOKEN_V1：① cookie → ② 一次性 token／短效票券）──
    // TC_REFRESH_FALLBACK_V1（fix30）：access 無效時用 refresh cookie 伺服器端
    // 輪換（與 /admin 頁面的 resolve_page_session 同一機制）。/tc/ 是純導航，
    // 沒有頁面 JS 可以像 /admin/api 的前端那樣在 401 時自動換 token；多分頁
    // 並用時，任一分頁剛好 refresh 過（rotate 會撤銷所有舊 token），本分頁
    // 的導航就會直接撞上「請先登入面板」錯誤頁（2026-09-14 使用者實測）。
    let maybe_page = super::pages::resolve_page_session(state, headers, "").await;
    // TC_PUBLIC_STATIC_V1（fix35）：/static/* 是 iClass 公開編譯資產——不需
    // 面板鑑權（下方仍先試鑑權，成功可沿用帳號的 provider 與上游 cookie）。
    let public_asset = is_public_tc_asset(upstream_pq);
    // TC_NAV_TOKEN_V1（fix34）＋TC_REFERER_GRANT_V1（fix35）：cookie 全滅時
    // （實測該瀏覽器對 /tc/ 頁面家族的導航與子資源都不帶 cookie，連剛種的
    // Path=/ 票券也不送），依序 fallback：① URL 一次性 token（導航；消費後
    // 轉頁面授權）→ ② 120 秒票券 cookie → ③ Referer 內的 token（頁面的
    // 子資源／XHR／站內導航；Referer=頁面完整 URL 含 tc_token，滑動 15 分鐘）。
    let auth: Option<(String, bool)> = match maybe_page.as_ref() {
        Some(page) => Some((page.session.account_id.clone(), page.session.is_admin)),
        None => tc_token
            .and_then(|t| state.consume_tc_nav_token_via_url(t))
            .or_else(|| {
                tc_ticket_from_headers(headers).and_then(|t| state.consume_tc_nav_token(&t))
            })
            .or_else(|| {
                tc_token_from_referer(headers)
                    .and_then(|t| state.consume_tc_nav_token_via_referer(&t))
            }),
    };
    if auth.is_none() && !public_asset {
        // TC_NAV_LOGIN_REDIRECT_V1（fix31）：留下診斷線索——access/refresh
        // cookie 的「存在性」（不記值），搭配 sec-fetch-dest 判別導航或子資源。
        // fix35 加 Referer 欄位：判別瀏覽器是否連 Referer 都不送。
        tracing::warn!(
            has_access_cookie = super::session_cookie_from_headers(headers).is_some(),
            has_refresh_cookie = super::refresh_cookie_from_headers(headers).is_some(),
            has_tc_token = tc_token.is_some(),
            has_ticket_cookie = tc_ticket_from_headers(headers).is_some(),
            has_referer = headers.contains_key(axum::http::header::REFERER),
            referer_had_token = tc_token_from_referer(headers).is_some(),
            is_navigation = is_tc_navigation_request(headers),
            path = %upstream_pq,
            "tc proxy 面板鑑權失敗"
        );
        // 導航請求（HTML 文檔）改送登入頁而非錯誤頁：登入頁會再做一次
        // refresh 輪換（成功＝直接進 dashboard，再按一次按鈕即可；失敗＝
        // 顯示登入表單）。使用者永遠不會卡死在錯誤頁。子資源（XHR／圖片／
        // CSS）維持 401 錯誤頁，避免 fetch 被導去拿登入頁 HTML。
        if is_tc_navigation_request(headers) {
            return Redirect::to("/admin?next=%2Fadmin%2Fdashboard").into_response();
        }
        return tc_error_page(
            StatusCode::UNAUTHORIZED,
            "請先登入面板，再從作業／公告頁的按鈕進入 iClass",
        );
    }
    if let Some(page) = maybe_page.as_ref() {
        if page.session.must_change_password {
            return tc_error_page(
                StatusCode::UNAUTHORIZED,
                "請先回面板完成密碼變更，再使用 iClass 代理",
            );
        }
    }
    // ── 代理帳號：cookie 指定，未指定用登入者本人；公開資產匿名（免帳號）──
    let (account_id, raw) = match auth.as_ref() {
        Some((sess_account_id, sess_is_admin)) => {
            let account_id =
                tc_account_from_headers(headers).unwrap_or_else(|| sess_account_id.clone());
            if !sess_is_admin && account_id != *sess_account_id {
                return tc_error_page(StatusCode::FORBIDDEN, "只能瀏覽自己帳號的 iClass 頁面");
            }
            let Some(raw) = state.accounts_db.get(&account_id).await.ok().flatten() else {
                return tc_error_page(StatusCode::NOT_FOUND, "找不到代理帳號，請回到面板重新開啟");
            };
            if !raw.enabled {
                return tc_error_page(StatusCode::BAD_REQUEST, "該帳號目前已停用");
            }
            // VIEWER_NO_SCHOOL_API_V1：檢視者帳號沒有學校 session 可代理。
            if raw.is_viewer {
                return tc_error_page(StatusCode::BAD_REQUEST, "檢視者帳號沒有 iClass session，無法代理");
            }
            (account_id, Some(raw))
        }
        None => (String::new(), None),
    };
    let mut cookie_value = raw
        .as_ref()
        .map(|r| r.session_cookie.trim().to_string())
        .unwrap_or_default();
    if auth.is_some() && cookie_value.is_empty() {
        return tc_error_page(
            StatusCode::CONFLICT,
            "此帳號尚無 iClass session；請等面板完成一次輪詢同步後再試",
        );
    }
    // ── base_url（比照 homework.rs 的 provider 解析鏈）──
    let Some(config) = crate::config::try_get_config() else {
        return tc_error_page(StatusCode::SERVICE_UNAVAILABLE, "伺服器設定尚未就緒，請稍後再試");
    };
    let provider = match raw.as_ref() {
        Some(raw) => match config.providers.get(&raw.provider) {
            Some(p) => p,
            None => return tc_error_page(StatusCode::INTERNAL_SERVER_ERROR, "此帳號的 provider 設定異常，無法代理"),
        },
        // 匿名公開資產：無帳號可循——用字母序首位 provider（BTreeMap 固定順序；
        // 單一 provider 部署即唯一目標。資產本身公開，選誰都無安全影響）。
        None => match config.providers.values().next() {
            Some(p) => p,
            None => return tc_error_page(StatusCode::SERVICE_UNAVAILABLE, "伺服器設定尚未就緒，請稍後再試"),
        },
    };
    let base = provider.base_url.trim_end_matches('/').to_string();
    let url = format!("{base}{upstream_pq}");

    // ── 轉發 ──
    // 跨 crate 型別（axum/reqwest 的 http 版本）用位元組級轉換，不假設同型別。
    let upstream_method = match reqwest::Method::from_bytes(method.as_str().as_bytes()) {
        Ok(m) => m,
        Err(_) => return tc_error_page(StatusCode::METHOD_NOT_ALLOWED, "不支援的 HTTP 方法"),
    };
    let Some(http) = TC_HTTP.as_ref() else {
        return tc_error_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "代理 HTTP client 初始化失敗，請聯絡管理員",
        );
    };

    let mut upstream = match send_upstream(
        http,
        upstream_method.clone(),
        &url,
        headers,
        &cookie_value,
        &body,
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => {
            warn!(account = %account_id, error = %e, "iClass 代理請求失敗");
            return tc_error_page(StatusCode::BAD_GATEWAY, "iClass 連線失敗，請稍後再試");
        }
    };

    // TC_PROXY_RELOGIN_V1：session 失效（401/403 或被 30x 導向 NEAI/SSO 登入）
    // → 自動重登一次再重試。之後若仍被導向登入頁才回錯誤；單純 401/403
    // （例如權限不足）重試後原樣回傳，不吞掉上游狀態。匿名公開資產（raw=None）
    // 不適用——上游本就免 cookie。
    if let Some(raw_account) = raw.as_ref() {
        if needs_relogin(&upstream) && relogin_allowed(&account_id).await {
            info!(account = %account_id, "代理：iClass session 失效，自動重新登入");
            if let Some(new_cookie) = relogin_and_persist(state, raw_account).await {
                cookie_value = new_cookie;
                upstream = match send_upstream(
                    http,
                    upstream_method,
                    &url,
                    headers,
                    &cookie_value,
                    &body,
                )
                .await
                {
                    Ok(resp) => resp,
                    Err(e) => {
                        warn!(account = %account_id, error = %e, "iClass 代理重試請求失敗");
                        return tc_error_page(StatusCode::BAD_GATEWAY, "iClass 連線失敗，請稍後再試");
                    }
                };
            }
        }
    }
    if is_login_page_redirect(&upstream) {
        return tc_error_page(
            StatusCode::UNAUTHORIZED,
            "iClass session 已失效，且自動重新登入沒有成功；請到面板的帳號設定確認帳號密碼是否正確，稍後再試",
        );
    }

    // ── 回應組裝：剝 Set-Cookie／長度類 header，改寫 Location ──
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response_headers = HeaderMap::new();
    for (name, value) in upstream.headers().iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if DROP_RESP_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if lower == "location" {
            if let Ok(loc) = value.to_str() {
                let rewritten = rewrite_location(loc, &base);
                if let Ok(v) = axum::http::HeaderValue::from_str(&rewritten) {
                    response_headers.append(name, v);
                }
                continue;
            }
        }
        response_headers.append(name, value.clone());
    }
    // TC_NO_STORE_HTML_V1（fix35）：代理的 HTML 一律 no-store——防止瀏覽器用
    // heuristic 快取供應過期的 /tc/ 頁面（其子資源與 XHR 帶著舊狀態打進來，
    // 造成難以歸因的 401 風暴）；CF 也不得快取帳號內容的 HTML。
    let content_type_is_html = response_headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with("text/html"));
    if content_type_is_html {
        response_headers.insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
    }
    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            warn!(account = %account_id, error = %e, "iClass 代理回應讀取失敗");
            return tc_error_page(StatusCode::BAD_GATEWAY, "iClass 回應讀取失敗，請稍後再試");
        }
    };
    let mut response = (status, bytes.to_vec()).into_response();
    *response.headers_mut() = response_headers;
    // TC_REFRESH_FALLBACK_V1：本次導航若有做 refresh 輪換，把新 cookie 種回
    // 瀏覽器（正常成功路徑必附；錯誤頁 early-return 不附——極罕見，代價只是
    // 下次導航需重新登入一次）。TC_NAV_TOKEN_V1：token 鑑權路徑沒有 PageSession
    // （cookie 已失效或不存在），無新 cookie 可附。
    match maybe_page {
        Some(page) => super::pages::attach_fresh_cookies(response, &page, headers),
        None => response,
    }
}

async fn send_upstream(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    headers: &HeaderMap,
    cookie: &str,
    body: &[u8],
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = http.request(method, url).header("cookie", cookie.as_bytes());
    for name in FORWARD_REQ_HEADERS {
        if let Some(value) = headers.get(*name) {
            req = req.header(*name, value.as_bytes());
        }
    }
    if !body.is_empty() {
        req = req.body(body.to_vec());
    }
    req.send().await
}

/// 觸發自動重登的寬判定：401/403，或 30x 且 Location 帶登入特徵。
fn needs_relogin(resp: &reqwest::Response) -> bool {
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        return true;
    }
    is_login_page_redirect(resp)
}

/// 「被導向登入頁」的嚴判定：30x 且 Location 含 NEAI/login/CAS/SSO 特徵。
/// （重試後仍命中才算真的登不進去；Location 指向 iClass 以外網域＝SSO 鏈。）
fn is_login_page_redirect(resp: &reqwest::Response) -> bool {
    let status = resp.status().as_u16();
    if !(300..400).contains(&status) {
        return false;
    }
    let Some(loc) = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let lower = loc.to_ascii_lowercase();
    lower.contains("neai") || lower.contains("login") || lower.contains("cas") || lower.contains("sso")
}

/// 每帳號 60 秒內最多重登一次（回 false 表示剛登過，跳過）。
async fn relogin_allowed(account_id: &str) -> bool {
    let mut map = RELOGIN_THROTTLE.lock().await;
    let now = Instant::now();
    map.retain(|_, t| now.duration_since(*t) < Duration::from_secs(60));
    if map.contains_key(account_id) {
        return false;
    }
    map.insert(account_id.to_string(), now);
    true
}

/// 用帳號密碼走 AuthClient 完整登入（會先試既有 cookie，失效自動改帳密），
/// 成功後把新 session cookie 存回 DB。組 AccountConfig 的方式與
/// homework.rs 的 resolved_account 相同。
async fn relogin_and_persist(
    state: &AdminWebState,
    raw: &crate::account::RawAccountConfig,
) -> Option<String> {
    let config = crate::config::try_get_config()?;
    let provider = config.providers.get(&raw.provider)?;
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
    match crate::auth::AuthClient::new(&account).await {
        Ok((_auth_client, session)) => {
            let cookie = session.cookie_string;
            let _ = state
                .accounts_db
                .set_session_cookie(&account.id, &cookie)
                .await;
            info!(account = %account.id, "代理：自動重新登入成功，已更新 session");
            Some(cookie)
        }
        Err(e) => {
            warn!(account = %account.id, error = %e, "代理：自動重新登入失敗");
            None
        }
    }
}

/// 上游 30x 的 Location 若指向 iClass 網域，改寫為相對路徑；
/// 相對路徑與其他網域的 URL 照原樣。
fn rewrite_location(loc: &str, base: &str) -> String {
    if let Some(rest) = loc.strip_prefix(base) {
        if rest.is_empty() {
            return "/".to_string();
        }
        if rest.starts_with('/') {
            return rest.to_string();
        }
    }
    loc.to_string()
}

/// 從 Cookie header 讀取代理帳號選擇。
fn tc_account_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    let prefix = format!("{TC_ACCOUNT_COOKIE}=");
    for part in cookies.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&prefix) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// 簡易錯誤頁（代理路徑上不依賴面板樣式）。
/// 帶狀態碼：上游失敗才是 502；其餘用 401/403/404 等，避免 Cloudflare
/// 把「請先登入面板」這類可自行解決的訊息蓋成制式 502 頁。
fn tc_error_page(status: StatusCode, message: &str) -> Response {
    let safe = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let html = format!(
        r#"<!doctype html><html lang="zh-Hant"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>iClass 代理</title></head><body style="font-family:system-ui,-apple-system,sans-serif;max-width:520px;margin:15vh auto;padding:0 16px;color:#172033"><h2>⚠️ 無法開啟 iClass 頁面</h2><p style="line-height:1.6">{safe}</p><p><a href="/admin/dashboard" style="color:#2563eb">← 返回面板</a></p></body></html>"#
    );
    (
        status,
        [
            (
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            // TC_NO_STORE_HTML_V1（fix35）：錯誤頁不得被瀏覽器／CF heuristic
            // 快取——否則修好鑑權後舊 401 還從快取吐出來。
            (
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            ),
        ],
        html,
    )
        .into_response()
}
