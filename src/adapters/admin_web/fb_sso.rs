//! Filebrowser 免登入票：只有已登入 Dashboard 的瀏覽器才帶得到。
//! Cookie 設在父網域（.jasoncloudserver.me），files. 子網域也讀得到。
//! 直接開 files. 而沒有這張票 → Caddy 轉去「要密碼」的 Filebrowser。

use super::*;
use hmac::{Hmac, Mac};
use sha2::Sha256;

const COOKIE_NAME: &str = "tronclass_fb_sso";
const COOKIE_MAX_AGE: u64 = 8 * 60 * 60;

/// The secret never changes while the process runs, but `verify()` used to
/// re-read and re-allocate it from the environment on every call — and Caddy's
/// forward_auth calls this once per HTTP request, including every static asset.
/// Cache it once.
static FB_SSO_SECRET: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn secret() -> &'static str {
    FB_SSO_SECRET.get_or_init(|| {
        std::env::var("TRONCLASS_FB_SSO_SECRET")
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    })
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

/// v3 tickets carry the role and the live Dashboard session token, so a
/// Filebrowser SSO ticket cannot outlive logout, session expiry or restart.
fn sign_v3(account_id: &str, session_token: &str, exp: u64, is_admin: bool) -> Option<String> {
    let secret = secret();
    if secret.is_empty()
        || session_token.len() != 32
        || !session_token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let role = if is_admin { "a" } else { "u" };
    // Bind the parent-domain ticket to the live in-memory Dashboard session.
    // A signed account id alone would remain valid after logout or restart.
    let msg = format!("v3|{account_id}|{session_token}|{exp}|{role}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(msg.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    Some(format!(
        "v3.{}.{}.{exp}.{role}.{sig}",
        encode_account_id(account_id),
        session_token,
    ))
}

/// Returns (account_id, is_admin, live_dashboard_session_token).
/// Older v1/v2 tickets are intentionally rejected so they cannot survive a
/// Dashboard logout/restart. Users receive a v3 ticket on their next login.
fn verify_session_ticket(raw: &str) -> Option<(String, bool, String)> {
    let secret = secret();
    if secret.is_empty() {
        return None;
    }
    let parts: Vec<&str> = raw.split('.').collect();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let ["v3", enc, session_token, exp, role, _sig] = parts.as_slice() else {
        return None;
    };
    let account_id = decode_account_id(enc)?;
    let exp: u64 = exp.parse().ok()?;
    if exp < now
        || account_id.is_empty()
        || account_id.len() > 128
        || session_token.len() != 32
        || !session_token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let is_admin = match *role {
        "a" => true,
        "u" => false,
        _ => return None,
    };
    if sign_v3(&account_id, session_token, exp, is_admin)? != raw {
        return None;
    }
    Some((account_id, is_admin, session_token.to_string()))
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

/// Validate the secret Caddy adds only after `forward_auth` succeeds. This is
/// needed by the Filebrowser HTML-shell bridge because the Dashboard session
/// cookie is host-only and therefore is not sent to `files.`.
pub(crate) fn shell_request_secret_valid(headers: &HeaderMap) -> bool {
    let expected = secret();
    if expected.is_empty() {
        return false;
    }
    headers
        .get("x-fb-shell-secret")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|provided| provided == expected)
}

pub fn append_fb_sso_cookie(
    mut response: Response,
    account_id: &str,
    is_admin: bool,
    session_token: &str,
    headers: &HeaderMap,
) -> Response {
    if account_id.is_empty() || account_id == "<token-admin>" {
        return response;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let Some(token) = sign_v3(account_id, session_token, now + COOKIE_MAX_AGE, is_admin) else {
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

/// Caddy forward_auth：只有仍存在且未過期的 Dashboard session 才能免登。
pub async fn fb_sso_check_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
) -> Response {
    if ip != "unknown" {
        match state.accounts_db.is_ip_blacklisted(&ip).await {
            Ok(true) => {
                warn!(ip = %ip, "Filebrowser SSO rejected for blacklisted IP");
                return (
                    StatusCode::OK,
                    [
                        (HeaderName::from_static("x-fb-authed"), "no"),
                        (HeaderName::from_static("x-fb-admin"), "no"),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    Json(json!({ "authed": false, "is_admin": false })),
                )
                    .into_response();
            }
            Ok(false) => {}
            Err(error) => warn!(ip = %ip, error = %error, "檢查 Filebrowser SSO 黑名單失敗"),
        }
    }
    let ticket = cookie_value_from_headers(&headers)
        .and_then(|raw| verify_session_ticket(&raw));
    let active_ticket = match ticket {
        Some((account_id, ticket_is_admin, session_token)) => state
            .authorize_any(&session_token)
            .await
            .filter(|session| {
                session.account_id == account_id
                    && session.is_admin == ticket_is_admin
                    && !session.must_change_password
            })
            .map(|_| (account_id, ticket_is_admin)),
        None => None,
    };
    let authed = active_ticket.is_some();
    let is_admin = active_ticket
        .as_ref()
        .map(|(_, is_admin)| *is_admin)
        .unwrap_or(false);
    let flag = if authed { "yes" } else { "no" };
    // X-Fb-Admin lets Caddy route admins to the full-scope instance.
    let admin_flag = if is_admin { "yes" } else { "no" };
    (
        StatusCode::OK,
        [
            (HeaderName::from_static("x-fb-authed"), flag),
            (HeaderName::from_static("x-fb-admin"), admin_flag),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(json!({ "authed": authed, "is_admin": is_admin })),
    )
        .into_response()
}
