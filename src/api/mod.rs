//! Tronclass API 客戶端連線模組
//!
//! API 將通過此模組進行對外的發送，並發包或接收下級的請求：
//! - `GET  /api/profile`                                      → 取得使用者資料

pub mod profile;
pub mod rollcall;
pub mod todos;

use std::sync::Arc;
use std::time::Duration;

use miette::{IntoDiagnostic, Result, WrapErr};
use reqwest::{
    cookie::Jar,
    header::{self, HeaderMap, HeaderValue},
    Client, StatusCode,
};
use thiserror::Error;

// ─── 錯誤類型 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP 請求失敗：{0}")]
    Http(#[from] reqwest::Error),

    #[error("API 返回錯誤狀態 {status}：{body}")]
    ApiStatus { status: u16, body: String },

    #[error("Session 已過期（HTTP 401/403），需要重新登錄")]
    Unauthorized,

    #[error(transparent)]
    Rollcall(#[from] rollcall::RollcallError),

    #[error("JSON 解析失敗：{0}")]
    Json(String),
}

// ─── API 客戶端 ───────────────────────────────────────────────────────────────
/// Tronclass API 客戶端
pub struct ApiClient {
    /// API 基礎 URL
    pub(crate) base_url: String,
    /// HTTP client（從 Auth 取得已登入的 Session）
    pub(crate) client: Client,
}

impl ApiClient {
    /// 從已認證的 client 建立 API 客戶端
    pub fn new(client: Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into(),
        }
    }

    /// 通用回應處理：反序列化 JSON，遇到 401/403 轉換為 Unauthorized 錯誤
    pub(crate) async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T> {
        let status = resp.status();

        if resp.url().path().contains("/login") {
            return Err(miette::miette!(ApiError::Unauthorized));
        }

        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(miette::miette!(ApiError::Unauthorized));
            }
            _ if !status.is_success() => {
                let body = resp.text().await.unwrap_or_default();
                return Err(miette::miette!(ApiError::ApiStatus {
                    status: status.as_u16(),
                    body,
                }));
            }
            _ => {}
        }

        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let body_bytes = resp
            .bytes()
            .await
            .into_diagnostic()
            .wrap_err("Failed to read response body")?;

        if content_type.contains("text/html")
            || body_bytes
                .iter()
                .copied()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|b| b == b'<')
        {
            return Err(miette::miette!(ApiError::Unauthorized));
        }

        serde_json::from_slice::<T>(&body_bytes).map_err(|e| {
            let body_str = String::from_utf8_lossy(&body_bytes);
            miette::miette!(ApiError::Json(format!(
                "{e}: body was `{}`",
                // REDIRECT_UTF8_SLICE_V1：位元組切片切在中文字中間會 panic，改用字元截斷。
                crate::text_utils::truncate_chars(&body_str, 200)
            )))
        })
    }
}

// ─── HTTP Client ─────────────────────────────────────────────────────────
/// 建立帶有預設 Headers 和 Cookie Jar 的 HTTP client
/// 由認證模組呼叫，建立後交給 `ApiClient` 使用。
pub fn build_http_client(cookie_jar: Arc<Jar>, timeout_secs: u64) -> reqwest::Result<Client> {
    let mut default_headers = HeaderMap::new();

    default_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) \
             Mobile/15E148 Tronclass/5.0",
        ),
    );
    default_headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    default_headers.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-TW,zh;q=0.9,en;q=0.8"),
    );

    Client::builder()
        .cookie_provider(cookie_jar)
        .default_headers(default_headers)
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::limited(10))
        .use_rustls_tls()
        .build()
}

// ─── 輔助函式 ─────────────────────────────────────────────────────────────────

/// 判斷錯誤字串是否為「需要重新登錄」的認證錯誤。
///
/// AUTH_ERROR_TYPED_V1
/// 舊版以裸 `contains("401")` / `contains("403")` / `contains("session")` /
/// `contains("登入")` 等寬鬆子字串比對，任何含這些片段的無關訊息
/// （課程 ID `40123`、雷達距離 `403.52` 公尺、「使用者未登入」等）都會被
/// 誤判成認證錯誤，觸發不必要的重新登入並中斷進行中的簽到。
///
/// 現在只比對「本 codebase 自己產生」的明確標記：
/// - `ApiError::Unauthorized` 的 Display：`Session 已過期（HTTP 401/403），需要重新登錄`
/// - monitor 的致命錯誤包裹訊息：`致命錯誤`（如「批次簽到中發生致命錯誤」）
/// - 我們自己 format 出來、帶前綴的狀態字串：`HTTP 401` / `HTTP 403` /
///   `狀態 401` / `狀態 403`（數字不再單獨成立）
/// - 明確的英文字眼：`Unauthorized` / `Session expired`
///
/// 若手上是 `miette::Report`，請改用 [`is_auth_error_report`]——它會檢查整條
/// 錯誤鏈，`wrap_err` 的外層訊息（如「取得簽到列表失敗」）不會掩蓋內層
/// 真正的 `ApiError::Unauthorized`。
pub(crate) fn is_auth_error(err: &str) -> bool {
    // 統一轉小寫比對，避免大小寫差異漏接（Unauthorized / unauthorized）。
    let lower = err.to_ascii_lowercase();
    lower.contains("unauthorized")
        || lower.contains("session 已過期")
        || lower.contains("session expired")
        || lower.contains("需要重新登錄")
        || lower.contains("致命錯誤")
        || lower.contains("http 401")
        || lower.contains("http 403")
        || lower.contains("狀態 401")
        || lower.contains("狀態 403")
}

/// 判斷 `miette::Report` 是否為認證錯誤（需要重新登錄）。
///
/// AUTH_ERROR_TYPED_V1
/// 直接比對 `report.to_string()` 只會看到最外層 `wrap_err` 的訊息（例如
/// 「取得簽到列表失敗」），內層真正的 `ApiError::Unauthorized` 會被掩蓋，
/// 導致認證錯誤被當成一般錯誤、遲遲不觸發重新登入。這裡改檢查整條
/// source chain（`Report::chain()`），任一層命中即成立：
///
/// 1. **型別檢查**（最精確）：鏈上任一層就是 `ApiError::Unauthorized`。
/// 2. **字串後備**：鏈上任一層的 Display 命中 [`is_auth_error`] 的明確標記
///    （涵蓋已被包成純字串再包回 Report 的訊息，如 monitor 的致命錯誤）。
pub(crate) fn is_auth_error_report(report: &miette::Report) -> bool {
    report.chain().any(|cause| {
        cause
            .downcast_ref::<ApiError>()
            .is_some_and(|err| matches!(err, ApiError::Unauthorized))
            || is_auth_error(&cause.to_string())
    })
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ApiError 訊息格式 ─────────────────────────────────────────────────────

    #[test]
    fn test_api_error_unauthorized_message() {
        let err = ApiError::Unauthorized;
        assert!(err.to_string().contains("401") || err.to_string().contains("重新登錄"));
    }

    #[test]
    fn test_api_error_api_status_message() {
        let err = ApiError::ApiStatus {
            status: 500,
            body: "Internal Server Error".to_string(),
        };
        assert!(err.to_string().contains("500"));
        assert!(err.to_string().contains("Internal Server Error"));
    }

    #[test]
    fn test_api_error_json_message() {
        let err = ApiError::Json("unexpected token".to_string());
        assert!(err.to_string().contains("unexpected token"));
    }

    // ── build_http_client ────────────────────────────────────────────────────

    #[test]
    fn test_build_http_client_succeeds() {
        let jar = Arc::new(Jar::default());
        assert!(build_http_client(jar, 30).is_ok());
    }

    // ── is_auth_error（AUTH_ERROR_TYPED_V1：精確標記比對）────────────────────

    #[test]
    fn test_is_auth_error_canonical_unauthorized_display() {
        // ApiError::Unauthorized 的實際 Display 必須命中
        assert!(is_auth_error(&ApiError::Unauthorized.to_string()));
    }

    #[test]
    fn test_is_auth_error_explicit_markers() {
        assert!(is_auth_error("Session 已過期（HTTP 401/403），需要重新登錄"));
        assert!(is_auth_error("HTTP 401 Unauthorized"));
        assert!(is_auth_error("HTTP 403: Forbidden"));
        assert!(is_auth_error("API 返回錯誤狀態 401：token 無效"));
        assert!(is_auth_error("API 返回錯誤狀態 403：Forbidden"));
        assert!(is_auth_error("批次簽到中發生致命錯誤（session 可能已過期）"));
        assert!(is_auth_error("🔴 致命錯誤：簽到 session 過期"));
        assert!(is_auth_error("Unauthorized access"));
        assert!(is_auth_error("unauthorized"));
        assert!(is_auth_error("Session expired, please re-login"));
    }

    /// AUTH_ERROR_NUMERIC_FP_V1：舊版裸 `contains("401")` / `contains("403")`
    /// 會把這些無關訊息誤判成認證錯誤，觸發不必要的重新登入。
    #[test]
    fn test_is_auth_error_rejects_bare_numbers() {
        assert!(!is_auth_error("取得課程 40123 失敗"));
        assert!(!is_auth_error("錯誤代碼 4012"));
        assert!(!is_auth_error("最後距離 403.52 公尺"));
        assert!(!is_auth_error("rollcall 1740341 已過期"));
        assert!(!is_auth_error("HTTP 500 Internal Server Error"));
    }

    /// 舊版裸「session／登入／登錄／認證」會把這些無關訊息誤判成認證錯誤。
    #[test]
    fn test_is_auth_error_rejects_bare_login_words() {
        assert!(!is_auth_error("Session management failed"));
        assert!(!is_auth_error("session cookie 已寫入資料庫"));
        assert!(!is_auth_error("使用者尚未登入 LINE"));
        assert!(!is_auth_error("登入頁面載入超時"));
        assert!(!is_auth_error("認證流程完成，不需要重新登入"));
        assert!(!is_auth_error("Network timeout"));
        assert!(!is_auth_error("JSON parse error"));
        assert!(!is_auth_error(""));
    }

    // ── is_auth_error_report（走訪整條錯誤鏈）────────────────────────────────

    #[test]
    fn test_is_auth_error_report_finds_wrapped_unauthorized() {
        // 情境：Unauthorized 被 wrap_err 包住，外層訊息沒有任何認證標記。
        let report = miette::miette!(ApiError::Unauthorized).wrap_err("取得簽到列表失敗");
        // 舊做法（只看 to_string）會漏掉——外層訊息本身不匹配：
        assert!(!is_auth_error("取得簽到列表失敗"));
        // 新的 report 版走完整條鏈，找得到內層原因：
        assert!(is_auth_error_report(&report));
    }

    #[test]
    fn test_is_auth_error_report_rejects_wrapped_transient() {
        let report = miette::miette!("Network timeout").wrap_err("取得簽到列表失敗");
        assert!(!is_auth_error_report(&report));
    }

    #[test]
    fn test_is_auth_error_report_rejects_wrapped_api_status() {
        let report = miette::miette!(ApiError::ApiStatus {
            status: 500,
            body: "Internal Server Error".to_string(),
        })
        .wrap_err("上游錯誤");
        assert!(!is_auth_error_report(&report));
    }

    #[test]
    fn test_is_auth_error_report_bare_unauthorized() {
        // 未被包裝的 Unauthorized（API 方法直接回傳的路徑）。
        let report = miette::miette!(ApiError::Unauthorized);
        assert!(is_auth_error_report(&report));
    }
}
