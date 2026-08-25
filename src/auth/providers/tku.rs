use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use miette::{IntoDiagnostic, Result, WrapErr};
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

        debug!(url = %login_url, "TKU: GET iClass 登入入口");
        let resp = client
            .get(login_url.clone())
            .header(header::ACCEPT, HTML_ACCEPT)
            .header(header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("TKU: Failed to GET iClass login entry")?;

        if !resp.status().is_success() {
            return auth_failed(format!("TKU: 登入入口返回非 2xx 狀態：{}", resp.status()));
        }

        let nam_landing_url = resp.url().clone();
        let landing_html = resp
            .text()
            .await
            .into_diagnostic()
            .wrap_err("TKU: Failed to read SSO landing page")?;

        let neai_entry = extract_nam_entry_url(&landing_html)
            .ok_or_else(|| AuthError::LoginFailed {
                reason: "TKU: 無法從 SSO landing page 解析 NEAI 登入入口".to_string(),
            })
            .into_diagnostic()?;
        let sso_origin = origin_from_url(&neai_entry).into_diagnostic()?;
        set_iv_jct_cookie(cookie_jar, &sso_origin).into_diagnostic()?;

        let nam_form_url = format!("{}{}", neai_entry, nam_landing_url.as_str());
        debug!(url = %nam_form_url, "TKU: GET NEAI 登入表單");
        let mut form = fetch_login_form(client, &login_url, &nam_form_url).await?;

        // 有些 NAM 節點第一次回應會把 jsessionid 放在 form action；重新抓一次可讓
        // cookie jar 固定同一個 session。若第二次仍保留 jsessionid，照樣可送出。
        if form.action_url.as_str().contains(";jsessionid=") {
            debug!(action = %form.action_url, "TKU: form action 含 jsessionid，重新取得表單");
            form = fetch_login_form(client, &login_url, &nam_form_url).await?;
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
        let final_url = follow_html_redirects(client, body, final_url).await?;
        debug!(url = %final_url, "TKU: SSO 登入流程完成，驗證 iClass session");

        let user_name = verify_session_with_client(client, base_url)
            .await
            .into_diagnostic()
            .wrap_err("TKU: SSO 登入後 session 驗證失敗，請確認帳號密碼是否正確")?;

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

async fn follow_html_redirects(
    client: &Client,
    mut html: String,
    mut current_url: Url,
) -> Result<Url> {
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

    Ok(current_url)
}

fn extract_nam_entry_url(html: &str) -> Option<String> {
    let re = Regex::new(r#"(?is)(https?://[^"']+/NEAI/login(?:eb|rwd)\.jsp\?myurl=)"#).ok()?;
    re.captures(html)
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
    let form_re = Regex::new(r#"(?is)(<form\b[^>]*>)(.*?)</form>"#).expect("valid regex");
    let input_re = Regex::new(r#"(?is)<input\b[^>]*>"#).expect("valid regex");

    for caps in form_re.captures_iter(html) {
        let form_tag = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let form_body = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
        let form_attrs = parse_attrs(form_tag);

        let mut fields = BTreeMap::new();
        let mut password_field = None;
        let mut vidcode_field = None;
        let mut text_fields = Vec::new();

        for input in input_re.find_iter(form_body) {
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
    let attr_re = Regex::new(
        r#"(?is)\b([a-zA-Z_:][-a-zA-Z0-9_:.]*)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+))"#,
    )
    .expect("valid regex");
    let mut attrs = BTreeMap::new();

    for caps in attr_re.captures_iter(tag) {
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
    let patterns = [
        r#"(?is)(?:window|top|parent)?\.?location(?:\.href)?\s*=\s*["']([^"']+)["']"#,
        r#"(?is)location\.(?:replace|assign)\s*\(\s*["']([^"']+)["']\s*\)"#,
        r#"(?is)<meta\b[^>]*http-equiv\s*=\s*["']?refresh["']?[^>]*content\s*=\s*["'][^"']*url\s*=\s*([^"'>\s]+)"#,
    ];

    for pattern in patterns {
        let re = Regex::new(pattern).expect("valid regex");
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
