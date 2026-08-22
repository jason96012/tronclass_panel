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

/// Percent-encode everything outside an unreserved ASCII set. Account IDs are
/// often CJK names, and a raw non-ASCII byte makes the whole `Set-Cookie`
/// header invalid, which silently drops EVERY cookie on the response.
fn encode_account_id(account_id: &str) -> String {
    let mut out = String::with_capacity(account_id.len());
    for byte in account_id.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode_account_id(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = encoded.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn sign(account_id: &str, exp: u64) -> Option<String> {
    let secret = secret();
    if secret.is_empty() {
        return None;
    }
    // Sign the raw id, but transport the encoded form.
    let msg = format!("v1|{account_id}|{exp}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(msg.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    Some(format!("v1.{}.{exp}.{sig}", encode_account_id(account_id)))
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
    let account_id = decode_account_id(parts[1])?;
    let exp: u64 = parts[2].parse().ok()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if exp < now || account_id.is_empty() || account_id.len() > 128 {
        return None;
    }
    let expected = sign(&account_id, exp)?;
    if expected != raw {
        return None;
    }
    Some(account_id)
}

fn cookie_value_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookies.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        if name.trim() == COOKIE_NAME {
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
    match cookie.parse::<HeaderValue>() {
        Ok(value) => {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
        Err(e) => {
            // Never fail silently: an invalid Set-Cookie here previously took
            // the session cookie down with it.
            tracing::error!(error = %e, account = %account_id, "fb_sso cookie 無法建立，已略過");
        }
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
