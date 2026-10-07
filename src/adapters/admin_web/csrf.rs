//! CSRF_TOKEN_V1：在既有 `SameSite=Lax` + `Sec-Fetch-Site`/`Origin` 比對
//! （`admin_same_origin_middleware` 內的 `same_origin_request`）之上，疊加一層
//! 顯式、不可被瀏覽器以外的第三方讀到或猜到的 CSRF token。既有防護已足以擋下
//! 絕大多數 CSRF，這層純粹是縱深防禦：若 Origin/Sec-Fetch-Site 標頭因為中介
//! 代理、舊瀏覽器或未來規格變動而不可信，寫入型請求仍會因為缺少這個 token
//! 被擋下。
//!
//! 設計：
//! - token = HMAC-SHA256(行程級密鑰, account_id)。
//!   刻意綁在「帳號」而不是某一顆會輪替的 access/refresh token 上：
//!   `/admin/api/session/refresh` 背景靜默換發新 access token（SESSION_DUAL_
//!   CANDIDATE_V1／三態頁面流程）不會讓目前頁面上已經嵌入的 `data-csrf` 失效
//!   ——否則使用者在同一頁待久一點、背景悄悄刷新過 session 後，下一次按儲存
//!   就會無預警收到 403，需要整頁重新整理才能恢復，是不可接受的體驗回歸。
//!   這個弱化（同帳號在不同裝置登入會共用同一個 token 值）對 CSRF 防護本身
//!   沒有實質影響：攻擊者的跨網域頁面一開始就讀不到這個值，不論它是綁在帳號
//!   還是單一 session 上，威脅模型完全一樣；只有在「要讓某一台裝置的 token
//!   單獨失效」這種其他動機下才有差別，而單一裝置的 session revoke（裝置頁的
//!   「撤銷」、黑名單）本來就已經會讓該裝置的 session cookie 本身失效，所以
//!   這裡不需要重複處理。
//! - 伺服器端渲染已登入頁面時，把這個 token 當成 `data-csrf` 屬性寫進
//!   `<html>`（見 `pages::auth_attrs`）；前端 `session-security.js` 在最早期
//!   把 `window.fetch` 包一層，對所有送往 `/admin/api/*` 的非 GET 請求自動附上
//!   `X-CSRF-Token` 標頭（之後任何子頁或 Dashboard 自己的 fetch 包裝，不論疊
//!   了幾層，呼叫到的都已經是這支被包過的 `window.fetch`）。`/admin/stock-api/*`
//!   刻意不在範圍內——StockGenie 側車是獨立前端，從不載入這支腳本，見
//!   `admin_same_origin_middleware` 上的說明。
//! - `admin_same_origin_middleware` 在收到請求時，先用既有的
//!   `authorize_any_from_cookies` 解析 session（跟各 handler 用的是同一條
//!   路徑，沒有額外的資料庫查詢邏輯）；只有在「解析出一個目前有效的 session」
//!   時才要求這個標頭——找不到有效 session（登入、passkey 登入、session 剛好
//!   過期需要刷新……）一律放行，交由下游 handler 自己判斷回應「未登入」，CSRF
//!   檢查不搶在前面用一個容易誤導的 403 蓋過去。
//! - 行程級密鑰持久化在 `config/csrf_token.secret`（權限 0600，做法與既有
//!   `files.rs` 的 `OFFICE_PREVIEW_SECRET` 完全一致）：重啟沿用舊金鑰，已經
//!   送到瀏覽器裡、嵌在頁面中的 token 不會因為伺服器重啟就全部失效。可用環境
//!   變數 `TRONCLASS_CSRF_TOKEN_SECRET` 覆寫（例如多副本部署需要共用同一把
//!   密鑰時）。
//! - `/admin/api/logout` 固定豁免（`is_exempt_path`）：跟
//!   `reject_blacklisted_ip_middleware` 對登出端點的一貫處理方式相同，確保
//!   使用者在任何情況下都能登出；登出 CSRF 的影響本來就只是把人登出，風險
//!   遠低於其餘寫入端點。

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// 行程級 HMAC 密鑰。優先序：環境變數 → `config/csrf_token.secret` → 隨機產生
/// 並落盤。與 `files.rs` 的 `OFFICE_PREVIEW_SECRET` 同一套做法。
fn csrf_secret() -> &'static [u8] {
    static SECRET: once_cell::sync::Lazy<Vec<u8>> = once_cell::sync::Lazy::new(|| {
        if let Ok(v) = std::env::var("TRONCLASS_CSRF_TOKEN_SECRET") {
            let v = v.trim();
            if !v.is_empty() {
                return v.as_bytes().to_vec();
            }
        }
        let path = std::path::Path::new("config/csrf_token.secret");
        if let Ok(existing) = std::fs::read_to_string(path) {
            let existing = existing.trim().to_string();
            if !existing.is_empty() {
                return existing.into_bytes();
            }
        }
        let mut b = [0u8; 32];
        if let Err(e) = getrandom::getrandom(&mut b) {
            tracing::warn!(error = %e, "getrandom 失敗，CSRF token 金鑰改用 rand 產生");
            b = rand::random::<[u8; 32]>();
        }
        let secret = hex::encode(b);
        if let Err(e) = crate::atomic_write::write_atomic_with_mode(path, &secret, 0o600) {
            tracing::warn!(
                "無法持久化 CSRF token 金鑰，重啟後既有分頁的 token 會失效（重新整理即恢復）：{e}"
            );
        }
        secret.into_bytes()
    });
    SECRET.as_slice()
}

/// 由帳號 ID 推導顯式 CSRF token（見本檔開頭的設計說明：刻意綁帳號、不綁單一
/// access/refresh token）。
pub(super) fn token_for_account(account_id: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(csrf_secret()).expect("HMAC-SHA256 accepts any key length");
    mac.update(account_id.trim().as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 哪些 `/admin/api/*` 寫入端點固定豁免——即使解析得出有效 session，也不要求
/// 這個標頭（見本檔開頭「`/admin/api/logout` 固定豁免」說明）。
pub(super) fn is_exempt_path(path: &str) -> bool {
    path == "/admin/api/logout"
}

/// 中介層呼叫入口：請求帶的 `X-CSRF-Token` 標頭是否等於「這個已登入帳號」
/// 應有的 token。比對走 `Mac::verify_slice`，是固定時間比較。
pub(super) fn request_has_valid_token(headers: &axum::http::HeaderMap, account_id: &str) -> bool {
    let Some(provided) = headers.get("x-csrf-token").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Ok(sig) = hex::decode(provided.trim()) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(csrf_secret()) else {
        return false;
    };
    mac.update(account_id.trim().as_bytes());
    mac.verify_slice(&sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips_for_same_account() {
        let token = token_for_account("alice");
        let headers = {
            let mut h = axum::http::HeaderMap::new();
            h.insert(
                "x-csrf-token",
                axum::http::HeaderValue::from_str(&token).unwrap(),
            );
            h
        };
        assert!(request_has_valid_token(&headers, "alice"));
    }

    #[test]
    fn token_rejects_mismatched_account() {
        let token = token_for_account("alice");
        let headers = {
            let mut h = axum::http::HeaderMap::new();
            h.insert(
                "x-csrf-token",
                axum::http::HeaderValue::from_str(&token).unwrap(),
            );
            h
        };
        assert!(!request_has_valid_token(&headers, "bob"));
    }

    #[test]
    fn token_rejects_missing_or_garbage_header() {
        assert!(!request_has_valid_token(&axum::http::HeaderMap::new(), "alice"));
        let headers = {
            let mut h = axum::http::HeaderMap::new();
            h.insert(
                "x-csrf-token",
                axum::http::HeaderValue::from_str("not-hex-at-all").unwrap(),
            );
            h
        };
        assert!(!request_has_valid_token(&headers, "alice"));
    }

    #[test]
    fn stable_across_repeated_calls() {
        // 同一個行程、同一個帳號，多次呼叫必須得到一樣的值（否則前端嵌入的
        // data-csrf 跟中介層重算出來的值永遠對不上）。
        assert_eq!(token_for_account("alice"), token_for_account("alice"));
    }

    #[test]
    fn logout_is_exempt_everything_else_is_not() {
        assert!(is_exempt_path("/admin/api/logout"));
        assert!(!is_exempt_path("/admin/api/login"));
        assert!(!is_exempt_path("/admin/api/action"));
        assert!(!is_exempt_path("/admin/api/passkey/register/start"));
    }
}
