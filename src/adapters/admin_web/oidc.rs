//! Minimal OpenID Connect provider so Nextcloud user_oidc can log users in
//! with an existing Dashboard session (no second password).

use super::*;
use crate::config::OidcConfig;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct OidcRuntime {
    enabled: bool,
    issuer: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
}

#[derive(Clone)]
struct AuthCode {
    account_id: String,
    name: String,
    redirect_uri: String,
    nonce: String,
    expires: Instant,
}

#[derive(Clone)]
struct AccessTok {
    account_id: String,
    name: String,
    expires: Instant,
}

static OIDC: RwLock<Option<OidcRuntime>> = RwLock::new(None);
static CODES: Mutex<Option<HashMap<String, AuthCode>>> = Mutex::new(None);
static TOKENS: Mutex<Option<HashMap<String, AccessTok>>> = Mutex::new(None);

pub fn configure(cfg: &OidcConfig, fallback_issuer: &str) {
    let mut issuer = cfg.issuer.trim().trim_end_matches('/').to_string();
    if issuer.is_empty() {
        issuer = fallback_issuer.trim().trim_end_matches('/').to_string();
    }
    if let Ok(v) = std::env::var("TRONCLASS_OIDC_ISSUER") {
        if !v.trim().is_empty() {
            issuer = v.trim().trim_end_matches('/').to_string();
        }
    }
    let mut client_id = cfg.client_id.trim().to_string();
    if let Ok(v) = std::env::var("TRONCLASS_OIDC_CLIENT_ID") {
        if !v.trim().is_empty() {
            client_id = v.trim().to_string();
        }
    }
    let mut client_secret = cfg.client_secret.clone();
    if let Ok(v) = std::env::var("TRONCLASS_OIDC_CLIENT_SECRET") {
        if !v.trim().is_empty() {
            client_secret = v;
        }
    }
    let mut redirect_uri = cfg.redirect_uri.trim().to_string();
    if let Ok(v) = std::env::var("TRONCLASS_OIDC_REDIRECT_URI") {
        if !v.trim().is_empty() {
            redirect_uri = v.trim().to_string();
        }
    }
    let enabled = cfg.enabled || std::env::var("TRONCLASS_OIDC_ENABLED").ok().as_deref() == Some("1");
    let ready = enabled && !issuer.is_empty() && !client_id.is_empty() && !client_secret.is_empty();
    *OIDC.write().unwrap() = if ready {
        info!(issuer = %issuer, "OIDC：Dashboard 可為 Nextcloud 免二次登入");
        Some(OidcRuntime {
            enabled: true,
            issuer,
            client_id,
            client_secret,
            redirect_uri,
        })
    } else {
        None
    };
    *CODES.lock().unwrap() = Some(HashMap::new());
    *TOKENS.lock().unwrap() = Some(HashMap::new());
}

fn runtime() -> Option<OidcRuntime> {
    OIDC.read().unwrap().clone()
}

fn b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

fn random_id() -> String {
    b64url(&Sha256::digest(Uuid::new_v4().as_bytes()))[..32].to_string()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sign_jwt(secret: &str, payload: &serde_json::Value) -> String {
    let header = serde_json::json!({"alg":"HS256","typ":"JWT"});
    let h = b64url(header.to_string().as_bytes());
    let p = b64url(payload.to_string().as_bytes());
    let msg = format!("{h}.{p}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac");
    mac.update(msg.as_bytes());
    let sig = mac.finalize().into_bytes();
    format!("{msg}.{}", b64url(&sig))
}

fn redirect_allowed(cfg: &OidcRuntime, uri: &str) -> bool {
    let uri = uri.trim();
    if uri.is_empty() {
        return false;
    }
    if !cfg.redirect_uri.is_empty() {
        if uri == cfg.redirect_uri {
            return true;
        }
        // allow /apps/user_oidc/code on the same origin as configured
        if let (Ok(want), Ok(got)) = (url::Url::parse(&cfg.redirect_uri), url::Url::parse(uri)) {
            if want.origin() == got.origin() && got.path().contains("user_oidc") {
                return true;
            }
        }
    }
    uri.contains("/apps/user_oidc/")
}

fn prune_maps() {
    let now = Instant::now();
    if let Ok(mut g) = CODES.lock() {
        if let Some(m) = g.as_mut() {
            m.retain(|_, v| v.expires > now);
        }
    }
    if let Ok(mut g) = TOKENS.lock() {
        if let Some(m) = g.as_mut() {
            m.retain(|_, v| v.expires > now);
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    nonce: String,
    #[serde(default)]
    token: String,
}

pub async fn discovery_handler() -> Response {
    let Some(cfg) = runtime() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "oidc_disabled" })),
        )
            .into_response();
    };
    let iss = cfg.issuer;
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "issuer": iss,
            "authorization_endpoint": format!("{iss}/admin/oidc/authorize"),
            "token_endpoint": format!("{iss}/admin/oidc/token"),
            "userinfo_endpoint": format!("{iss}/admin/oidc/userinfo"),
            "jwks_uri": format!("{iss}/admin/oidc/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["HS256"],
            "scopes_supported": ["openid","profile","email","groups"],
            "token_endpoint_auth_methods_supported": ["client_secret_post","client_secret_basic"],
            "grant_types_supported": ["authorization_code"],
        })),
    )
        .into_response()
}

pub async fn jwks_handler() -> Response {
    (StatusCode::OK, Json(json!({ "keys": [] }))).into_response()
}

pub async fn authorize_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    let Some(cfg) = runtime() else {
        return (StatusCode::NOT_FOUND, "OIDC 未啟用").into_response();
    };
    if q.response_type != "code" {
        return (StatusCode::BAD_REQUEST, "response_type 必須是 code").into_response();
    }
    if q.client_id != cfg.client_id {
        return (StatusCode::BAD_REQUEST, "client_id 不符").into_response();
    }
    if !redirect_allowed(&cfg, &q.redirect_uri) {
        return (StatusCode::BAD_REQUEST, "redirect_uri 未允許").into_response();
    }

    let cookie_tok = session_cookie_from_headers(&headers).unwrap_or_default();
    let token = if !cookie_tok.is_empty() {
        cookie_tok
    } else {
        q.token.trim().to_string()
    };
    let Some(session) = state.authorize(&token).await else {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/admin")],
        )
            .into_response();
    };

    prune_maps();
    let code = random_id();
    if let Ok(mut g) = CODES.lock() {
        g.get_or_insert_with(HashMap::new).insert(
            code.clone(),
            AuthCode {
                account_id: session.account_id.clone(),
                name: session.account_id.clone(),
                redirect_uri: q.redirect_uri.clone(),
                nonce: q.nonce.clone(),
                expires: Instant::now() + Duration::from_secs(120),
            },
        );
    }
    let mut dest = url::Url::parse(&q.redirect_uri).unwrap();
    dest.query_pairs_mut().append_pair("code", &code);
    if !q.state.is_empty() {
        dest.query_pairs_mut().append_pair("state", &q.state);
    }
    (
        StatusCode::TEMPORARY_REDIRECT,
        [(header::LOCATION, dest.to_string())],
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct TokenForm {
    #[serde(default)]
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
}

pub async fn token_handler(
    headers: HeaderMap,
    axum::extract::Form(form): axum::extract::Form<TokenForm>,
) -> Response {
    let Some(cfg) = runtime() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "oidc_disabled" })),
        )
            .into_response();
    };
    let mut client_id = form.client_id.clone();
    let mut client_secret = form.client_secret.clone();
    if let Some(basic) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(b64) = basic.strip_prefix("Basic ") {
            if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
                if let Ok(s) = String::from_utf8(raw) {
                    if let Some((u, p)) = s.split_once(':') {
                        if client_id.is_empty() {
                            client_id = u.to_string();
                        }
                        if client_secret.is_empty() {
                            client_secret = p.to_string();
                        }
                    }
                }
            }
        }
    }
    if form.grant_type != "authorization_code" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "unsupported_grant_type" })),
        )
            .into_response();
    }
    if client_id != cfg.client_id || client_secret != cfg.client_secret {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_client" })),
        )
            .into_response();
    }
    prune_maps();
    let rec = {
        let mut g = CODES.lock().unwrap();
        g.get_or_insert_with(HashMap::new).remove(&form.code)
    };
    let Some(rec) = rec else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant" })),
        )
            .into_response();
    };
    if rec.expires <= Instant::now() || rec.redirect_uri != form.redirect_uri {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant" })),
        )
            .into_response();
    }
    let access = random_id();
    if let Ok(mut g) = TOKENS.lock() {
        g.get_or_insert_with(HashMap::new).insert(
            access.clone(),
            AccessTok {
                account_id: rec.account_id.clone(),
                name: rec.name.clone(),
                expires: Instant::now() + Duration::from_secs(3600),
            },
        );
    }
    let now = now_unix();
    let mut claims = json!({
        "iss": cfg.issuer,
        "sub": rec.account_id,
        "aud": cfg.client_id,
        "exp": now + 3600,
        "iat": now,
        "preferred_username": rec.account_id,
        "name": rec.name,
        "email": format!("{}@dashboard.local", rec.account_id),
        "groups": ["classroom"],
    });
    if !rec.nonce.is_empty() {
        claims["nonce"] = json!(rec.nonce);
    }
    let id_token = sign_jwt(&cfg.client_secret, &claims);
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": 3600,
            "id_token": id_token,
            "scope": "openid profile email",
        })),
    )
        .into_response()
}

pub async fn userinfo_handler(headers: HeaderMap) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim()
        .to_string();
    prune_maps();
    let rec = {
        let g = TOKENS.lock().unwrap();
        g.as_ref().and_then(|m| m.get(&bearer).cloned())
    };
    let Some(rec) = rec else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_token" })),
        )
            .into_response();
    };
    if rec.expires <= Instant::now() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "expired_token" })),
        )
            .into_response();
    }
    Json(json!({
        "sub": rec.account_id,
        "preferred_username": rec.account_id,
        "name": rec.name,
        "email": format!("{}@dashboard.local", rec.account_id),
        "groups": ["classroom"],
    }))
    .into_response()
}
