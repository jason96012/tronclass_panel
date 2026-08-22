//! Filebrowser 免登入票：只有已登入 Dashboard 的瀏覽器才帶得到。
//! Cookie 設在父網域（.jasoncloudserver.me），files. 子網域也讀得到。
//! 直接開 files. 而沒有這張票 → Caddy 轉去「要密碼」的 Filebrowser。

use super::*;
use hmac::{Hmac, Mac};
use sha2::Sha256;

const COOKIE_NAME: &str = "tronclass_fb_sso";
const COOKIE_MAX_AGE: u64 = 8 * 60 * 60;

fn secret() -> String {
    if let Ok(v) = std::env::var("TRONCLASS_FB_SSO_SECRET") {
        let t = v.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    String::new()
}

fn cookie_domain(headers: &HeaderMap) -> Option<String> {
    if let Ok(v) = std::env::var("TRONCLASS_FB_SSO_DOMAIN") {
        let t = v.trim();
        if t.starts_with('.') && t.len() > 2 {
            return Some(t.to_string());
        }
    }
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim();
    if host.ends_with("jasoncloudserver.me") {
        return Some(".jasoncloudserver.me".to_string());
    }
    None
}

fn sign(account_id: &str, exp: u64) -> Option<String> {
    let secret = secret();
    if secret.is_empty() {
        return None;
    }
    let msg = format!("v1|{account_id}|{exp}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(msg.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    Some(format!("v1.{account_id}.{exp}.{sig}"))
}

fn verify(raw: &str) -> Option<String> {
    let secret = secret();
    if secret.is_empty() {
        return None;
    }
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() != 4 || parts[0] != "v1" {
        return None;
    }
    let account_id = parts[1];
    let exp: u64 = parts[2].parse().ok()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if exp < now || account_id.is_empty() || account_id.len() > 128 {
        return None;
    }
    let expected = sign(account_id, exp)?;
    if expected != raw {
        return None;
    }
    Some(account_id.to_string())
}

fn cookie_value_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookies.split(';') {
        let (name, value) = pair.trim().split_once('=')?;
        if name == COOKIE_NAME {
            let v = value.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

pub fn append_fb_sso_cookie(
    mut response: Response,
    account_id: &str,
    headers: &HeaderMap,
) -> Response {
    if account_id.is_empty() || account_id == "<token-admin>" {
        return response;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let Some(token) = sign(account_id, now + COOKIE_MAX_AGE) else {
        return response;
    };
    let domain = cookie_domain(headers)
        .map(|d| format!("; Domain={d}"))
        .unwrap_or_default();
    let secure = if request_uses_https(headers) {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{COOKIE_NAME}={token}; Path=/{domain}; Max-Age={COOKIE_MAX_AGE}; HttpOnly; SameSite=Lax{secure}"
    );
    if let Ok(value) = cookie.parse::<HeaderValue>() {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

pub fn append_expired_fb_sso_cookie(mut response: Response) -> Response {
    for domain in ["", "; Domain=.jasoncloudserver.me"] {
        let cookie = format!(
            "{COOKIE_NAME}=; Path=/{domain}; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
        );
        if let Ok(value) = cookie.parse::<HeaderValue>() {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

/// Caddy forward_auth：永遠 200，用 X-Fb-Authed 告訴要不要免登。
pub async fn fb_sso_check_handler(headers: HeaderMap) -> Response {
    let authed = cookie_value_from_headers(&headers)
        .and_then(|raw| verify(&raw))
        .is_some();
    let flag = if authed { "yes" } else { "no" };
    (
        StatusCode::OK,
        [
            (HeaderName::from_static("x-fb-authed"), flag),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(json!({ "authed": authed })),
    )
        .into_response()
}
