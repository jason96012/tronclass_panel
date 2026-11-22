//! Nextcloud OIDC 相容殘件：路由仍在，但一律停用。
//! 不依賴 `config.rs` 是否有 `OidcConfig`，也不使用 tokio::sync::RwLock（不能放 static）。

use super::*;

pub fn configure() {}

pub async fn discovery_handler() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "oidc_disabled" })),
    )
        .into_response()
}

pub async fn jwks_handler() -> Response {
    (StatusCode::OK, Json(json!({ "keys": [] }))).into_response()
}

pub async fn authorize_handler() -> Response {
    (StatusCode::NOT_FOUND, "OIDC 已停用（改用 Filebrowser）").into_response()
}

pub async fn token_handler() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "oidc_disabled" })),
    )
        .into_response()
}

pub async fn userinfo_handler() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "oidc_disabled" })),
    )
        .into_response()
}
