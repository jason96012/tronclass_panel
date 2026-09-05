use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use miette::{IntoDiagnostic, Result, WrapErr};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::{cookie::Jar, header, Client, Url};
use tracing::{debug, info};

use crate::account::AccountConfig;

use super::super::{require_cookie, verify_session_with_client, AuthError, AuthSession};
use super::AuthFlow;

const LOGIN_PATH: &str = "/login?next=/user/index";
const HTML_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
const IMAGE_ACCEPT: &str = "image/avif,image/webp,image/apng,image/*,*/*;q=0.8";
const AJAX_ACCEPT: &str = "text/plain, */*; q=0.01";
const ACCEPT_LANGUAGE: &str = "zh-TW,zh;q=0.9,en;q=0.8";

// ─── SSO HTML 解析 regex（REGEX_STATIC_V1）────────────────────────────────────
//
// TKU SSO 登入流程的每個步驟（NEAI 入口偵測、表單解析、每個 input 欄位的
// 屬性萃取、每個 HTML redirect 追蹤）都會呼叫這些解析函式；重新認證與
// 課表爬蟲重用 `sso_login_to_service` 時更是整套再跑一次。原本每次呼叫都
// `Regex::new` 重新編譯，改為 Lazy 編譯一次終身重用。

/// NEAI 登入入口 URL（`https://sso…/NEAI/login{eb|rwd}.jsp?myurl=`）。
static NAM_ENTRY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)(https?://[^"']+/NEAI/login(?:eb|rwd)\.jsp\?myurl=)"#)
        .expect("valid regex")
});
/// `<form …>…</form>` 區塊（capture 1 = form tag、capture 2 = 表單內容）。
static FORM_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?is)(<form\b[^>]*>)(.*?)</form>"#).expect("valid regex"));
/// 單一 `<input …>` tag。
static INPUT_TAG_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?is)<input\b[^>]*>"#).expect("valid regex"));
/// tag 內的屬性（capture 1 = 屬性名；值為 capture 2 雙引號 / 3 單引號 / 4 無引號）。
static ATTR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)\b([a-zA-Z_:][-a-zA-Z0-9_:.]*)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+))"#)
        .expect("valid regex")
});
/// `window.location = "…"` / `location.href='…'` 等 JS 指派式轉址。
static REDIRECT_LOCATION_ASSIGN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)(?:window|top|parent)?\.?location(?:\.href)?\s*=\s*["']([^"']+)["']"#)
        .expect("valid regex")
});
/// `location.replace("…")` / `location.assign('…')` 等 JS 呼叫式轉址。
static REDIRECT_LOCATION_CALL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)location\.(?:replace|assign)\s*\(\s*["']([^"']+)["']\s*\)"#)
        .expect("valid regex")
});
/// `<meta http-equiv="refresh" content="…;url=…">` 轉址。
static REDIRECT_META_REFRESH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?is)<meta\b[^>]*http-equiv\s*=\s*["']?refresh["']?[^>]*content\s*=\s*["'][^"']*url\s*=\s*([^"'>\s]+)"#,
    )
    .expect("valid regex")
});

/// LOGIN_FAIL_DIAG_V2：登入失敗時萃取頁面上的錯誤提示（alert 訊息優先）。
static ALERT_MSG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)alert\(\s*['"]([^'"]{2,160})['"]\s*\)"#)
        .expect("valid regex")
});

pub fn create_flow() -> Box<dyn AuthFlow> {
    Box::new(TkuAuthFlow)
}

struct TkuAuthFlow;

#[derive(Debug, Clone)]
struct LoginForm {
    action_url: Url,
    fields: BTreeMap<String, String>,
    username_field: String,
    password_field: String,
    vidcode_field: Option<String>,
}

#[async_trait]
impl AuthFlow for TkuAuthFlow {
    async fn login(
        &self,
        client: &Client,
        cookie_jar: &Arc<Jar>,
        base_url: &str,
        account: &AccountConfig,
    ) -> Result<AuthSession> {
        let login_url = Url::parse(base_url)
            .map_err(|_| AuthError::InvalidUrl(base_url.to_string()))
            .and_then(|base| {
                base.join(LOGIN_PATH)
                    .map_err(|_| AuthError::InvalidUrl(format!("{base_url}{LOGIN_PATH}")))
            })
            .into_diagnostic()
            .wrap_err("TKU: 無效的 Tronclass base_url")?;

        let final_url = sso_login_to_service(client, cookie_jar, &login_url, account).await?;
        debug!(url = %final_url, "TKU: SSO 登入流程完成，驗證 iClass session");

        let user_name = match verify_session_with_client(client, base_url).await {
            Ok(name) => name,
            Err(error) => {
                // LOGIN_FAIL_DIAG_V1：登入失敗時記錄「送出的密碼」形態（不記內容），
                // 當場分辨：密文未解密（key 問題）vs 已解密但帳密被拒（學校端）。
                let envelope = crate::secret_encryption::looks_like_envelope(&account.password);
                tracing::error!(
                    account = %account.id,
                    password_len = account.password.chars().count(),
                    password_is_envelope = envelope,
                    "TKU 登入失敗診斷：password_is_envelope=true 代表密文未被解密（master key 問題）；false 代表已解密成明文，問題在帳密本身或學校端拒絕"
                );
                return Err(error)
                    .into_diagnostic()
                    .wrap_err("TKU: SSO 登入後 session 驗證失敗，請確認帳號密碼是否正確");
            }
        };

        let base_url_parsed = Url::parse(base_url)
            .map_err(|_| AuthError::InvalidUrl(base_url.to_string()))
            .into_diagnostic()?;
        let session_cookie = require_cookie(cookie_jar, &base_url_parsed, "session")
            .into_diagnostic()
            .wrap_err("TKU: 登入後未找到 session cookie")?;

        info!(user_name = %user_name, "TKU: 淡江 SSO 登入成功");
        Ok(AuthSession {
            user_name,
            cookie_string: format!("session={}", session_cookie),
        })
    }
}

/// 執行一次完整的淡江 SSO 登入，把 session 綁定到 `service_url` 所屬的服務。
///
/// TKU_SSO_SERVICE_PARAM_V1
/// 原本這段流程寫死在 `AuthFlow::login` 裡、只服務 iClass（`LOGIN_PATH`）。
/// 校務行政系統（`sso.tku.edu.tw/aissinfo/emis/`）走的是**同一套** NEAI/NAM
/// SSO：未登入時同樣被導向 `NEAI/logineb.jsp?myurl=<原始網址>`。把 service
/// 參數化之後，課表爬蟲可以直接重用這裡，不需要複製一份 18KB 的登入邏輯。
///
/// 回傳登入流程結束後停留的 URL（通常就是 `service_url` 本身）。
pub(crate) async fn sso_login_to_service(
    client: &Client,
    cookie_jar: &Arc<Jar>,
    service_url: &Url,
    account: &AccountConfig,
) -> Result<Url> {
    debug!(url = %service_url, "TKU: GET SSO 服務入口");
    let resp = client
        .get(service_url.clone())
        .header(header::ACCEPT, HTML_ACCEPT)
        .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
        .send()
        .await
        .into_diagnostic()
        .wrap_err("TKU: Failed to GET SSO service entry")?;

    if !resp.status().is_success() {
        return auth_failed(format!("TKU: 登入入口返回非 2xx 狀態：{}", resp.status()));
    }

    let nam_landing_url = resp.url().clone();
    let landing_html = resp
        .text()
        .await
        .into_diagnostic()
        .wrap_err("TKU: Failed to read SSO landing page")?;

    // 若已經有有效 session，學校不會回登入頁，這裡就找不到 NEAI 入口。
    // 這種情況直接視為登入完成，避免無謂地再送一次帳密。
    let Some(neai_entry) = extract_nam_entry_url(&landing_html) else {
        debug!("TKU: 未偵測到 NEAI 登入入口，判定既有 session 仍有效");
        return Ok(nam_landing_url);
    };

    let sso_origin = origin_from_url(&neai_entry).into_diagnostic()?;
    set_iv_jct_cookie(cookie_jar, &sso_origin).into_diagnostic()?;

    let nam_form_url = format!("{}{}", neai_entry, nam_landing_url.as_str());
    debug!(url = %nam_form_url, "TKU: GET NEAI 登入表單");
    let mut form = fetch_login_form(client, service_url, &nam_form_url).await?;

    // 有些 NAM 節點第一次回應會把 jsessionid 放在 form action；重新抓一次可讓
    // cookie jar 固定同一個 session。若第二次仍保留 jsessionid，照樣可送出。
    if form.action_url.as_str().contains(";jsessionid=") {
        debug!(action = %form.action_url, "TKU: form action 含 jsessionid，重新取得表單");
        form = fetch_login_form(client, service_url, &nam_form_url).await?;
    }

    if let Some(vidcode_field) = form.vidcode_field.clone() {
        let vidcode = fetch_nam_vidcode(client, &form.action_url, &sso_origin, &nam_form_url)
            .await
            .wrap_err("TKU: 取得 NEAI 驗證碼 token 失敗")?;
        debug!("TKU: NEAI 驗證碼 token 取得成功");
        form.fields.insert(vidcode_field, vidcode);
    }

    form.fields
        .insert(form.username_field.clone(), account.username.clone());
    form.fields
        .insert(form.password_field.clone(), account.password.clone());

    debug!(action = %form.action_url, "TKU: POST NEAI 登入表單");
    let post_resp = client
        .post(form.action_url.clone())
        .header(header::ACCEPT, HTML_ACCEPT)
        .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
        .header(header::CACHE_CONTROL, "max-age=0")
        .header(header::ORIGIN, &sso_origin)
        .header(header::REFERER, &nam_form_url)
        .header("Upgrade-Insecure-Requests", "1")
        .form(&form.fields)
        .send()
        .await
        .into_diagnostic()
        .wrap_err("TKU: Failed to POST NEAI login form")?;

    let final_url = post_resp.url().clone();
    let body = post_resp
        .text()
        .await
        .into_diagnostic()
        .wrap_err("TKU: Failed to read NEAI login response")?;
    let (final_url, final_html) = follow_html_redirects(client, body, final_url).await?;

    // LOGIN_FAIL_DIAG_V1：SSO 後仍停在登入頁＝帳密被拒絕（或觸發學校端
    // 驗證／鎖定）。舊版這裡不檢查，錯誤會延後到 session 驗證才爆，
    // 產生誤導性的「SSO 登入後 session 驗證失敗」。
    let path = final_url.path();
    if path.contains("logineb") || path.contains("NEAI") {
        // LOGIN_FAIL_DIAG_V2：直接從登入流程「停留的最後頁面」萃取錯誤文字。
        // 不做額外 GET——無 context 的重新請求會讓頁面 JS 初始化失敗，
        // 萃取出「載入登入頁面時發生錯誤」這種自己造成的雜訊。
        let hint = extract_error_hint(&final_html);
        let suffix = if hint.is_empty() {
            String::new()
        } else {
            format!("；學校回應：{hint}")
        };
        return auth_failed(format!(
            "TKU: 帳號或密碼錯誤（登入後仍停在 SSO 登入頁{suffix}）——請先用瀏覽器確認此帳號可以登入，反覆失敗可能已觸發學校端防護"
        ));
    }
    Ok(final_url)
}

async fn fetch_login_form(
    client: &Client,
    lms_login_url: &Url,
    nam_form_url: &str,
) -> Result<LoginForm> {
    let resp = client
        .get(nam_form_url)
        .header(header::ACCEPT, HTML_ACCEPT)
        .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
        .header(header::REFERER, lms_login_url.as_str())
        .header("Upgrade-Insecure-Requests", "1")
        .send()
        .await
        .into_diagnostic()
        .wrap_err("TKU: Failed to GET NEAI login form")?;

    if !resp.status().is_success() {
        return auth_failed(format!(
            "TKU: NEAI 登入表單返回非 2xx 狀態：{}",
            resp.status()
        ));
    }

    let form_url = resp.url().clone();
    let html = resp
        .text()
        .await
        .into_diagnostic()
        .wrap_err("TKU: Failed to read NEAI login form")?;
    extract_login_form(&html, &form_url)
        .into_diagnostic()
        .wrap_err("TKU: 無法解析 NEAI 登入表單")
}

async fn fetch_nam_vidcode(
    client: &Client,
    form_action_url: &Url,
    sso_origin: &str,
    referer: &str,
) -> Result<String> {
    let validate_url = form_action_url
        .join("ImageValidate")
        .map_err(|_| AuthError::InvalidUrl(format!("{form_action_url}/ImageValidate")))
        .into_diagnostic()?;

    client
        .get(validate_url.clone())
        .header(header::ACCEPT, IMAGE_ACCEPT)
        .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
        .header(header::REFERER, referer)
        .send()
        .await
        .into_diagnostic()
        .wrap_err("TKU: Failed to GET ImageValidate")?
        .bytes()
        .await
        .into_diagnostic()
        .wrap_err("TKU: Failed to read ImageValidate image")?;

    post_image_validate(client, &validate_url, sso_origin, referer, "1")
        .await
        .wrap_err("TKU: ImageValidate outType=1 failed")?;
    let code = post_image_validate(client, &validate_url, sso_origin, referer, "2")
        .await
        .wrap_err("TKU: ImageValidate outType=2 failed")?;

    let code = code.trim().to_string();
    if code.is_empty() {
        return auth_failed("TKU: ImageValidate 未回傳驗證碼 token");
    }
    Ok(code)
}

async fn post_image_validate(
    client: &Client,
    validate_url: &Url,
    sso_origin: &str,
    referer: &str,
    out_type: &str,
) -> Result<String> {
    let resp = client
        .post(validate_url.clone())
        .header(header::ACCEPT, AJAX_ACCEPT)
        .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
        .header(header::ORIGIN, sso_origin)
        .header(header::REFERER, referer)
        .header("X-Requested-With", "XMLHttpRequest")
        .form(&[("outType", out_type)])
        .send()
        .await
        .into_diagnostic()
        .wrap_err("TKU: Failed to POST ImageValidate")?;

    if !resp.status().is_success() {
        return auth_failed(format!(
            "TKU: ImageValidate outType={out_type} 返回非 2xx 狀態：{}",
            resp.status()
        ));
    }

    resp.text()
        .await
        .into_diagnostic()
        .wrap_err("TKU: Failed to read ImageValidate response")
}

/// LOGIN_FAIL_DIAG_V2：alert('…') 優先；否則找含關鍵字的文字片段。
fn extract_error_hint(html: &str) -> String {
    if let Some(caps) = ALERT_MSG_RE.captures(html) {
        if let Some(m) = caps.get(1) {
            let text = decode_html_entities(m.as_str()).trim().to_string();
            if !text.is_empty() {
                return crate::text_utils::truncate_chars(&text, 80).to_string();
            }
        }
    }
    // 後備：頁面文字中第一個含關鍵字的短片段（去標籤後掃描）。
    let stripped: String = html
        .replace('<', " ")
        .replace('>', " ");
    for line in stripped.split_whitespace() {
        if ["密碼", "帳號", "錯誤", "失敗", "鎖定", "驗證碼", "錯誤嘗試"]
            .iter()
            .any(|kw| line.contains(kw))
        {
            let text = decode_html_entities(line).trim().to_string();
            if text.chars().count() >= 2 {
                return crate::text_utils::truncate_chars(&text, 80).to_string();
            }
        }
    }
    String::new()
}

async fn follow_html_redirects(
    client: &Client,
    mut html: String,
    mut current_url: Url,
) -> Result<(Url, String)> {
    for _ in 0..10 {
        let Some(next_url) = extract_html_redirect(&html, &current_url).into_diagnostic()? else {
            break;
        };

        debug!(from = %current_url, to = %next_url, "TKU: follow HTML redirect");
        let resp = client
            .get(next_url)
            .header(header::ACCEPT, HTML_ACCEPT)
            .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
            .header(header::REFERER, current_url.as_str())
            .send()
            .await
            .into_diagnostic()
            .wrap_err("TKU: Failed to follow HTML redirect")?;
        current_url = resp.url().clone();
        html = resp
            .text()
            .await
            .into_diagnostic()
            .wrap_err("TKU: Failed to read HTML redirect response")?;
    }

    Ok((current_url, html))
}

fn extract_nam_entry_url(html: &str) -> Option<String> {
    // REGEX_STATIC_V1
    NAM_ENTRY_RE
        .captures(html)
        .and_then(|caps| caps.get(1))
        .map(|m| decode_html_entities(m.as_str()))
}

fn origin_from_url(url: &str) -> std::result::Result<String, AuthError> {
    let parsed = Url::parse(url).map_err(|_| AuthError::InvalidUrl(url.to_string()))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| AuthError::InvalidUrl(url.to_string()))?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    Ok(format!("{}://{}{}", parsed.scheme(), host, port))
}

fn set_iv_jct_cookie(cookie_jar: &Jar, sso_origin: &str) -> std::result::Result<(), AuthError> {
    let origin = Url::parse(&format!("{}/", sso_origin.trim_end_matches('/')))
        .map_err(|_| AuthError::InvalidUrl(sso_origin.to_string()))?;
    cookie_jar.add_cookie_str("IV_JCT=%2FNEAI; Path=/", &origin);
    Ok(())
}

fn extract_login_form(html: &str, page_url: &Url) -> std::result::Result<LoginForm, AuthError> {
    // REGEX_STATIC_V1
    for caps in FORM_RE.captures_iter(html) {
        let form_tag = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let form_body = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
        let form_attrs = parse_attrs(form_tag);

        let mut fields = BTreeMap::new();
        let mut password_field = None;
        let mut vidcode_field = None;
        let mut text_fields = Vec::new();

        for input in INPUT_TAG_RE.find_iter(form_body) {
            let attrs = parse_attrs(input.as_str());
            let Some(name) = attrs.get("name").cloned() else {
                continue;
            };
            let input_type = attrs
                .get("type")
                .map(|v| v.to_ascii_lowercase())
                .unwrap_or_else(|| "text".to_string());
            let value = attrs.get("value").cloned().unwrap_or_default();

            if input_type == "password" {
                password_field = Some(name.clone());
            }
            if matches!(input_type.as_str(), "text" | "email" | "tel" | "") {
                if name.eq_ignore_ascii_case("vidcode")
                    || name.to_ascii_lowercase().contains("captcha")
                {
                    vidcode_field = Some(name.clone());
                } else {
                    text_fields.push(name.clone());
                }
            }
            fields.insert(name, value);
        }

        let Some(password_field) = password_field else {
            continue;
        };
        let username_field = text_fields
            .iter()
            .find(|name| name.to_ascii_lowercase().contains("user"))
            .cloned()
            .or_else(|| text_fields.first().cloned())
            .unwrap_or_else(|| "username".to_string());

        let action_url = if let Some(action) = form_attrs.get("action") {
            page_url
                .join(action)
                .map_err(|_| AuthError::InvalidUrl(action.clone()))?
        } else {
            page_url.clone()
        };

        return Ok(LoginForm {
            action_url,
            fields,
            username_field,
            password_field,
            vidcode_field,
        });
    }

    Err(AuthError::LoginFailed {
        reason: "找不到含 password 欄位的登入表單".to_string(),
    })
}

fn parse_attrs(tag: &str) -> BTreeMap<String, String> {
    // REGEX_STATIC_V1
    let mut attrs = BTreeMap::new();

    for caps in ATTR_RE.captures_iter(tag) {
        let Some(name) = caps.get(1) else {
            continue;
        };
        let value = caps
            .get(2)
            .or_else(|| caps.get(3))
            .or_else(|| caps.get(4))
            .map(|m| decode_html_entities(m.as_str()))
            .unwrap_or_default();
        attrs.insert(name.as_str().to_ascii_lowercase(), value);
    }

    attrs
}

fn extract_html_redirect(
    html: &str,
    base_url: &Url,
) -> std::result::Result<Option<Url>, AuthError> {
    // REGEX_STATIC_V1：三條轉址樣式改為靜態 Lazy，依原優先順序嘗試。
    for re in [
        &REDIRECT_LOCATION_ASSIGN_RE,
        &REDIRECT_LOCATION_CALL_RE,
        &REDIRECT_META_REFRESH_RE,
    ] {
        if let Some(caps) = re.captures(html) {
            if let Some(target) = caps.get(1) {
                let target = decode_html_entities(target.as_str().trim());
                let url = base_url
                    .join(&target)
                    .map_err(|_| AuthError::InvalidUrl(target.clone()))?;
                return Ok(Some(url));
            }
        }
    }

    Ok(None)
}

fn decode_html_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn auth_failed<T>(reason: impl Into<String>) -> Result<T> {
    Err(AuthError::LoginFailed {
        reason: reason.into(),
    })
    .into_diagnostic()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_nam_entry_url_prefers_first_entry() {
        let html = r#"
            <script>
            if (isMobile.any()) {
                window.location.href="https://sso.tku.edu.tw/NEAI/loginrwd.jsp?myurl=" + redirUrl;
            } else {
                window.location.href="https://sso.tku.edu.tw/NEAI/logineb.jsp?myurl=" + redirUrl;
            }
            </script>
        "#;

        assert_eq!(
            extract_nam_entry_url(html).as_deref(),
            Some("https://sso.tku.edu.tw/NEAI/loginrwd.jsp?myurl=")
        );
    }

    #[test]
    fn test_origin_from_url_keeps_port() {
        assert_eq!(
            origin_from_url("https://sso.example.test:8443/NEAI/loginrwd.jsp?myurl=").unwrap(),
            "https://sso.example.test:8443"
        );
    }

    #[test]
    fn test_extract_login_form_parses_tku_fields() {
        let html = r#"
            <form name="eaiForm" method="post" action="/NEAI/login2.do;jsessionid=A?action=EAI">
                <input type="hidden" id="myurl" name="myurl" value="https://sso.tku.edu.tw/auth?a=1&b=2" />
                <input type="hidden" id="ln" name="ln" value="zh_TW" />
                <input type="text" id="username" name="username" value="">
                <input type="password" id="password" name="password">
                <input type="text" id="vidcode" name="vidcode" value="" required>
            </form>
        "#;
        let page_url = Url::parse("https://sso.tku.edu.tw/NEAI/loginrwd.jsp?myurl=x").unwrap();
        let form = extract_login_form(html, &page_url).unwrap();

        assert_eq!(
            form.action_url.as_str(),
            "https://sso.tku.edu.tw/NEAI/login2.do;jsessionid=A?action=EAI"
        );
        assert_eq!(form.username_field, "username");
        assert_eq!(form.password_field, "password");
        assert_eq!(form.vidcode_field.as_deref(), Some("vidcode"));
        assert_eq!(form.fields.get("ln").map(String::as_str), Some("zh_TW"));
        assert_eq!(
            form.fields.get("myurl").map(String::as_str),
            Some("https://sso.tku.edu.tw/auth?a=1&b=2")
        );
    }

    #[test]
    fn test_extract_html_redirect_script_and_relative_url() {
        let base_url = Url::parse("https://sso.tku.edu.tw/NEAI/login2.do").unwrap();
        let html = r#"<script>window.location.href='/auth/realms/TKU?x=1&amp;y=2';</script>"#;
        let redirect = extract_html_redirect(html, &base_url).unwrap().unwrap();
        assert_eq!(
            redirect.as_str(),
            "https://sso.tku.edu.tw/auth/realms/TKU?x=1&y=2"
        );
    }

    #[test]
    fn test_extract_html_redirect_meta_refresh() {
        let base_url = Url::parse("https://iclass.tku.edu.tw/login").unwrap();
        let html = r#"<meta http-equiv="refresh" content="0; url=/user/index">"#;
        let redirect = extract_html_redirect(html, &base_url).unwrap().unwrap();
        assert_eq!(redirect.as_str(), "https://iclass.tku.edu.tw/user/index");
    }
}
