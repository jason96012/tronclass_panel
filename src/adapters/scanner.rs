//! Local QR scanner state and submission routing.
//!
//! The scanner page submits one QR payload back to this process. This registry
//! fans that payload out to every account currently waiting for the same
//! provider + rollcall_id.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};
use url::Url;
use uuid::Uuid;

use crate::adapters::admin_web::{self, AdminWebState};
use crate::adapters::requests::{NumberCodeInput, NumberCodeSender, QrCodeSender};

const DEFAULT_SCANNER_PATH: &str = "/scanner";
const DEFAULT_NUMBER_PATH: &str = "/number";
const MAX_QR_DATA_LEN: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PendingQrKey {
    provider: String,
    rollcall_id: u64,
}

impl PendingQrKey {
    fn new(provider: impl Into<String>, rollcall_id: u64) -> Self {
        Self {
            provider: provider.into(),
            rollcall_id,
        }
    }
}

#[derive(Clone)]
struct PendingAccount {
    tx: QrCodeSender,
}

struct PendingQrEntry {
    token: String,
    expires_at: Instant,
    used: bool,
    submitted_qr_data: Option<String>,
    /// 尚未簽到成功、仍在等待 QR data 的帳號。
    accounts: HashMap<String, PendingAccount>,
    /// 已回報簽到成功的帳號。
    completed_accounts: HashSet<String>,
    /// 曾加入此 rollcall 等待流程的帳號，用於 scanner 頁面顯示總數。
    all_accounts: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PendingNumberKey {
    provider: String,
    rollcall_id: u64,
}

impl PendingNumberKey {
    fn new(provider: impl Into<String>, rollcall_id: u64) -> Self {
        Self {
            provider: provider.into(),
            rollcall_id,
        }
    }
}

#[derive(Clone)]
struct PendingNumberAccount {
    tx: NumberCodeSender,
}

struct PendingNumberEntry {
    token: String,
    expires_at: Instant,
    accounts: HashMap<String, PendingNumberAccount>,
    completed_accounts: HashSet<String>,
    all_accounts: HashSet<String>,
    submitted_input: Option<NumberCodeInput>,
    last_error: Option<String>,
}

/// Shared registry for scanner links and pending QR waits.
#[derive(Clone)]
pub struct QrScannerRegistry {
    public_base_url: String,
    extra_public_base_urls: Vec<String>,
    scanner_path: String,
    number_path: String,
    inner: Arc<Mutex<HashMap<PendingQrKey, PendingQrEntry>>>,
    number_inner: Arc<Mutex<HashMap<PendingNumberKey, PendingNumberEntry>>>,
}

#[derive(Clone)]
pub struct ScannerHttpState {
    scanner: Arc<QrScannerRegistry>,
}

impl ScannerHttpState {
    pub fn new(scanner: Arc<QrScannerRegistry>) -> Self {
        Self { scanner }
    }
}

impl QrScannerRegistry {
    pub fn new(public_base_url: impl Into<String>) -> Self {
        Self {
            public_base_url: public_base_url.into(),
            extra_public_base_urls: Vec::new(),
            scanner_path: DEFAULT_SCANNER_PATH.to_string(),
            number_path: DEFAULT_NUMBER_PATH.to_string(),
            inner: Arc::new(Mutex::new(HashMap::new())),
            number_inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn public_base_url(&self) -> &str {
        &self.public_base_url
    }

    /// Register one account as waiting for a shared QR scan.
    pub async fn register_pending(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        tx: QrCodeSender,
        timeout: Duration,
    ) -> Result<ScannerLink, ScannerError> {
        let now = Instant::now();
        let expires_at = now + timeout;
        let key = PendingQrKey::new(provider, rollcall_id);
        let (token, account_count, submitted_qr_data) = {
            let mut inner = self.inner.lock().await;
            cleanup_expired_locked(&mut inner, now);

            let entry = inner.entry(key).or_insert_with(|| PendingQrEntry {
                token: Uuid::new_v4().simple().to_string(),
                expires_at,
                used: false,
                submitted_qr_data: None,
                accounts: HashMap::new(),
                completed_accounts: HashSet::new(),
                all_accounts: HashSet::new(),
            });

            if entry.used && entry.submitted_qr_data.is_none() {
                entry.token = Uuid::new_v4().simple().to_string();
                entry.used = false;
                entry.accounts.clear();
                entry.completed_accounts.clear();
                entry.all_accounts.clear();
            }
            entry.expires_at = entry.expires_at.max(expires_at);
            entry.all_accounts.insert(account_id.to_string());
            entry
                .accounts
                .insert(account_id.to_string(), PendingAccount { tx: tx.clone() });

            (
                entry.token.clone(),
                entry.accounts.len(),
                entry.submitted_qr_data.clone(),
            )
        };

        if let Some(qr_data) = submitted_qr_data.as_ref() {
            if let Err(e) = tx.send(qr_data.clone()).await {
                warn!(account_id = %account_id, error = %e, "已提交 QR data 補送失敗");
            }
        }

        let scan_url = self.build_scan_url(provider, rollcall_id, account_id, &token)?;
        debug!(
            provider = %provider,
            rollcall_id = rollcall_id,
            account_id = %account_id,
            account_count = account_count,
            already_submitted = submitted_qr_data.is_some(),
            "已註冊共享 QR scanner 等待項目"
        );

        Ok(ScannerLink {
            scan_url,
            token,
            already_submitted: submitted_qr_data.is_some(),
        })
    }

    /// Remove one account from a pending QR group without marking it successful.
    pub async fn unregister_pending(&self, provider: &str, rollcall_id: u64, account_id: &str) {
        let key = PendingQrKey::new(provider, rollcall_id);
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.get_mut(&key) {
            entry.accounts.remove(account_id);
            if entry.accounts.is_empty() && entry.completed_accounts.is_empty() && !entry.used {
                inner.remove(&key);
            }
        }
    }

    /// Mark one account as successfully attended for scanner status display.
    pub async fn mark_completed(&self, provider: &str, rollcall_id: u64, account_id: &str) {
        let key = PendingQrKey::new(provider, rollcall_id);
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.get_mut(&key) {
            entry.accounts.remove(account_id);
            entry.all_accounts.insert(account_id.to_string());
            entry.completed_accounts.insert(account_id.to_string());
            debug!(
                provider = %provider,
                rollcall_id = rollcall_id,
                account_id = %account_id,
                completed = entry.completed_accounts.len(),
                total = entry.all_accounts.len(),
                "QR scanner 標記帳號已完成簽到"
            );
        }
    }

    /// Return current completion status for the scanner page.
    pub async fn status(
        &self,
        query: ScannerStatusQuery,
    ) -> Result<ScannerStatusResult, ScannerError> {
        let provider = query.provider.trim();
        let account_id = query.account_id.trim();
        let token = query.token.trim();

        if provider.is_empty() || account_id.is_empty() || token.is_empty() {
            return Err(ScannerError::InvalidSubmission("missing required field"));
        }

        let key = PendingQrKey::new(provider, query.rollcall_id);
        let now = Instant::now();
        let mut inner = self.inner.lock().await;
        cleanup_expired_locked(&mut inner, now);
        let entry = inner.get(&key).ok_or(ScannerError::PendingNotFound)?;

        if now > entry.expires_at {
            return Err(ScannerError::Expired);
        }
        if entry.token != token {
            return Err(ScannerError::InvalidToken);
        }
        if !entry.all_accounts.contains(account_id) && !entry.accounts.contains_key(account_id) {
            return Err(ScannerError::AccountNotPending);
        }

        // SEC_SCANNER_MIN_STATUS_V1：與 number_status 共用同一個遮罩化組裝函式。
        let mut status = status_result_from_sets(
            account_id,
            &entry.all_accounts,
            &entry.completed_accounts,
            entry.accounts.keys(),
        );
        status.expires_in_secs = secs_until(entry.expires_at, now);
        Ok(status)
    }

    /// Register one account as waiting for a number code.
    pub async fn register_number_pending(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        tx: NumberCodeSender,
        timeout: Duration,
    ) -> Result<NumberCodeLink, ScannerError> {
        let now = Instant::now();
        let expires_at = now + timeout;
        let key = PendingNumberKey::new(provider, rollcall_id);
        let (token, account_count, submitted_input) = {
            let mut inner = self.number_inner.lock().await;
            cleanup_expired_number_locked(&mut inner, now);

            let entry = inner.entry(key).or_insert_with(|| PendingNumberEntry {
                token: Uuid::new_v4().simple().to_string(),
                expires_at,
                accounts: HashMap::new(),
                completed_accounts: HashSet::new(),
                all_accounts: HashSet::new(),
                submitted_input: None,
                last_error: None,
            });

            entry.expires_at = entry.expires_at.max(expires_at);
            entry.all_accounts.insert(account_id.to_string());
            entry.accounts.insert(
                account_id.to_string(),
                PendingNumberAccount { tx: tx.clone() },
            );
            (
                entry.token.clone(),
                entry.accounts.len(),
                entry.submitted_input.clone(),
            )
        };

        if let Some(input) = submitted_input.as_ref() {
            if let Err(e) = tx.send(input.clone()).await {
                warn!(account_id = %account_id, error = %e, "已提交數字點名輸入補送失敗");
            }
        }

        let input_url = self.build_number_url(provider, rollcall_id, account_id, &token)?;
        debug!(
            provider = %provider,
            rollcall_id = rollcall_id,
            account_id = %account_id,
            account_count = account_count,
            "已註冊數字點名等待項目"
        );

        Ok(NumberCodeLink {
            input_url,
            token: token.to_string(),
        })
    }

    pub async fn unregister_number_pending(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
    ) {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let mut inner = self.number_inner.lock().await;
        if let Some(entry) = inner.get_mut(&key) {
            entry.accounts.remove(account_id);
            if entry.accounts.is_empty() && entry.completed_accounts.is_empty() {
                inner.remove(&key);
            }
        }
    }

    pub async fn mark_number_completed(&self, provider: &str, rollcall_id: u64, account_id: &str) {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let mut inner = self.number_inner.lock().await;
        if let Some(entry) = inner.get_mut(&key) {
            entry.accounts.remove(account_id);
            entry.all_accounts.insert(account_id.to_string());
            entry.completed_accounts.insert(account_id.to_string());
            entry.last_error = None;
            debug!(
                provider = %provider,
                rollcall_id = rollcall_id,
                account_id = %account_id,
                completed = entry.completed_accounts.len(),
                total = entry.all_accounts.len(),
                "數字點名頁標記帳號已完成簽到"
            );
        }
    }

    pub async fn mark_number_failed(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        reason: &str,
    ) {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let mut inner = self.number_inner.lock().await;
        if let Some(entry) = inner.get_mut(&key) {
            entry.all_accounts.insert(account_id.to_string());
            entry.last_error = Some(format!("{}：{}", account_id, reason));
            debug!(
                provider = %provider,
                rollcall_id = rollcall_id,
                account_id = %account_id,
                reason = %reason,
                "數字點名頁記錄簽到失敗原因"
            );
        }
    }

    pub async fn number_status(
        &self,
        query: ScannerStatusQuery,
    ) -> Result<ScannerStatusResult, ScannerError> {
        let provider = query.provider.trim();
        let account_id = query.account_id.trim();
        let token = query.token.trim();

        if provider.is_empty() || account_id.is_empty() || token.is_empty() {
            return Err(ScannerError::InvalidSubmission("missing required field"));
        }

        let key = PendingNumberKey::new(provider, query.rollcall_id);
        let now = Instant::now();
        let mut inner = self.number_inner.lock().await;
        cleanup_expired_number_locked(&mut inner, now);
        let entry = inner.get(&key).ok_or(ScannerError::PendingNotFound)?;

        if now > entry.expires_at {
            return Err(ScannerError::Expired);
        }
        if entry.token != token {
            return Err(ScannerError::InvalidToken);
        }
        if !entry.all_accounts.contains(account_id) && !entry.accounts.contains_key(account_id) {
            return Err(ScannerError::AccountNotPending);
        }

        let mut status = status_result_from_sets(
            account_id,
            &entry.all_accounts,
            &entry.completed_accounts,
            entry.accounts.keys(),
        );
        status.last_error = entry.last_error.clone();
        status.expires_in_secs = secs_until(entry.expires_at, now);
        // NUMBER_PAGE_UX_V1
        status.submitted_code = entry.submitted_input.as_ref().map(|input| match input {
            NumberCodeInput::Code(code) => code.clone(),
            NumberCodeInput::AutoLookup => "auto".to_string(),
        });
        Ok(status)
    }

    pub async fn submit_number(
        &self,
        submission: NumberCodeSubmission,
    ) -> Result<NumberCodeSubmitResult, ScannerError> {
        let provider = submission.provider.trim();
        let account_id = submission.account_id.trim();
        let token = submission.token.trim();

        if provider.is_empty() || account_id.is_empty() || token.is_empty() {
            return Err(ScannerError::InvalidSubmission("missing required field"));
        }

        let input = if submission.auto_lookup.unwrap_or(false) {
            NumberCodeInput::AutoLookup
        } else {
            let code = submission.code.unwrap_or_default().trim().to_string();
            if code.len() != 4 || !code.chars().all(|c| c.is_ascii_digit()) {
                return Err(ScannerError::InvalidNumberCode);
            }
            NumberCodeInput::Code(code)
        };

        let key = PendingNumberKey::new(provider, submission.rollcall_id);
        let account_senders = {
            let now = Instant::now();
            let mut inner = self.number_inner.lock().await;
            cleanup_expired_number_locked(&mut inner, now);
            let entry = inner.get_mut(&key).ok_or(ScannerError::PendingNotFound)?;

            if now > entry.expires_at {
                return Err(ScannerError::Expired);
            }
            if entry.token != token {
                return Err(ScannerError::InvalidToken);
            }
            if !entry.accounts.contains_key(account_id) && !entry.all_accounts.contains(account_id)
            {
                return Err(ScannerError::AccountNotPending);
            }
            entry.submitted_input = Some(input.clone());
            entry.last_error = None;

            entry
                .accounts
                .iter()
                .map(|(account_id, account)| (account_id.clone(), account.tx.clone()))
                .collect::<Vec<_>>()
        };

        let mut delivered_accounts = Vec::new();
        let mut failed_accounts = Vec::new();
        for (account_id, tx) in account_senders {
            match tx.send(input.clone()).await {
                Ok(()) => delivered_accounts.push(account_id),
                Err(e) => {
                    warn!(account_id = %account_id, error = %e, "數字點名頁傳送輸入失敗");
                    failed_accounts.push(account_id);
                }
            }
        }

        if delivered_accounts.is_empty() {
            return Err(ScannerError::NoDeliveredAccounts);
        }

        Ok(NumberCodeSubmitResult {
            delivered_count: delivered_accounts.len(),
            delivered_accounts,
            failed_accounts,
        })
    }

    /// Submit scanned QR data and fan it out to every currently waiting account.
    pub async fn submit(
        &self,
        submission: ScannerSubmission,
    ) -> Result<ScannerSubmitResult, ScannerError> {
        let provider = submission.provider.trim();
        let account_id = submission.account_id.trim();
        let token = submission.token.trim();
        let qr_data = submission.qr_data.trim();

        if provider.is_empty() || account_id.is_empty() || token.is_empty() || qr_data.is_empty() {
            return Err(ScannerError::InvalidSubmission("missing required field"));
        }
        if qr_data.len() > MAX_QR_DATA_LEN {
            return Err(ScannerError::QrDataTooLarge {
                max: MAX_QR_DATA_LEN,
            });
        }

        let key = PendingQrKey::new(provider, submission.rollcall_id);
        let account_senders = {
            let now = Instant::now();
            let mut inner = self.inner.lock().await;
            let entry = inner.get_mut(&key).ok_or(ScannerError::PendingNotFound)?;

            if now > entry.expires_at {
                inner.remove(&key);
                return Err(ScannerError::Expired);
            }
            if entry.token != token {
                return Err(ScannerError::InvalidToken);
            }
            if !entry.accounts.contains_key(account_id) {
                return Err(ScannerError::AccountNotPending);
            }

            // 同一個 scanner token 在過期前允許重複送出。
            // 課堂 QR Code 可能會輪換，scanner 頁面會持續掃描並把新的 payload
            // fan-out 給仍在等待的帳號；流程成功的帳號會自行 unregister。
            entry.used = true;
            entry.submitted_qr_data = Some(qr_data.to_string());
            entry
                .accounts
                .iter()
                .map(|(account_id, account)| (account_id.clone(), account.tx.clone()))
                .collect::<Vec<_>>()
        };

        let mut delivered_accounts = Vec::new();
        let mut failed_accounts = Vec::new();
        for (account_id, tx) in account_senders {
            match tx.send(qr_data.to_string()).await {
                Ok(()) => delivered_accounts.push(account_id),
                Err(e) => {
                    warn!(account_id = %account_id, error = %e, "QR scanner callback 傳送失敗");
                    failed_accounts.push(account_id);
                }
            }
        }

        if delivered_accounts.is_empty() {
            return Err(ScannerError::NoDeliveredAccounts);
        }

        Ok(ScannerSubmitResult {
            delivered_count: delivered_accounts.len(),
            delivered_accounts,
            failed_accounts,
        })
    }

    fn build_scan_url(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        token: &str,
    ) -> Result<String, ScannerError> {
        self.build_tool_url(&self.scanner_path, provider, rollcall_id, account_id, token)
    }

    fn build_number_url(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        token: &str,
    ) -> Result<String, ScannerError> {
        self.build_tool_url(&self.number_path, provider, rollcall_id, account_id, token)
    }

    fn build_tool_url(
        &self,
        path: &str,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        token: &str,
    ) -> Result<String, ScannerError> {
        let mut base =
            Url::parse(&self.public_base_url).map_err(|_| ScannerError::InvalidPublicBaseUrl)?;
        base.set_path(path.trim_start_matches('/'));
        base.query_pairs_mut()
            .clear()
            .append_pair("provider", provider)
            .append_pair("rollcall_id", &rollcall_id.to_string())
            .append_pair("account_id", account_id)
            .append_pair("token", token);
        Ok(base.to_string())
    }

    pub fn with_extra_public_base_urls(mut self, extras: Vec<String>) -> Self {
        self.extra_public_base_urls = extras
            .into_iter()
            .map(|url| url.trim().trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty())
            .collect();
        self
    }

}

/// Tailscale fallback URLs are intentionally disabled. Callers retain this
/// helper for compatibility and receive no alternate route.
pub fn alternate_public_url(_url: &str) -> Option<String> {
    None
}

// SEC_SCANNER_HARDEN_V1（審查 M-05）：公開 /scanner、/number 路由的最小防護——
// (a) 每 IP 固定窗限流（status 輪詢 1.5s/頁，給多分頁餘裕；submit 較嚴）；
// (b) 連續錯誤（400：token 錯／不在等待名單）計「失誤」，超量的 IP 暫時 429，防暴力猜 token；
// (c) 回應統一補安全標頭（Referrer-Policy / nosniff / no-store / CSP / frame-ancestors）。
// 只掛在 scanner 路由本身（在 merge admin router 之前），不影響後台既有標頭策略。
static SCANNER_RATE: once_cell::sync::Lazy<std::sync::Mutex<HashMap<String, (u64, u32)>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

fn scanner_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 固定窗計數：`bucket` 由「類別|ip」組成；回傳 true 代表已超過 `limit`。
fn scanner_rate_hit(bucket: String, limit: u32, window_secs: u64) -> bool {
    let now = scanner_now_secs();
    let Ok(mut map) = SCANNER_RATE.lock() else {
        return false;
    };
    if map.len() > 8192 {
        map.retain(|_, (start, _)| now.saturating_sub(*start) < window_secs);
    }
    let entry = map.entry(bucket).or_insert((now, 0));
    if now.saturating_sub(entry.0) >= window_secs {
        *entry = (now, 1);
        return false;
    }
    entry.1 = entry.1.saturating_add(1);
    entry.1 > limit
}

/// 只讀不加：檢查失誤桶是否已超量。
fn scanner_rate_over(bucket: &str, limit: u32, window_secs: u64) -> bool {
    let now = scanner_now_secs();
    let Ok(map) = SCANNER_RATE.lock() else {
        return false;
    };
    map.get(bucket)
        .is_some_and(|(start, count)| now.saturating_sub(*start) < window_secs && *count > limit)
}

const SCANNER_CSP: &str = "default-src 'self'; \
script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net https://static.cloudflareinsights.com; \
style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; \
connect-src 'self' https://cloudflareinsights.com; frame-src 'self' https://challenges.cloudflare.com; \
frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'";

pub async fn scanner_public_hardening_middleware(
    crate::trusted_ip::ClientIp(ip): crate::trusted_ip::ClientIp,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path().to_string();
    // 只處理 /scanner*、/number*；同一 Router 內的其他路由（例如 LINE webhook path）原樣放行。
    if !(path == "/scanner"
        || path.starts_with("/scanner/")
        || path == "/number"
        || path.starts_with("/number/"))
    {
        return next.run(request).await;
    }
    let is_submit = path.ends_with("/submit");
    let is_status = path.ends_with("/status");
    let is_asset = path.ends_with(".js");

    // 失誤桶（5 分鐘內超過 40 次 400）→ 該 IP 的 status/submit 一律 429。
    let strike_bucket = format!("strike|{ip}");
    if (is_submit || is_status) && scanner_rate_over(&strike_bucket, 40, 300) {
        return scanner_too_many();
    }
    let (class, limit) = if is_submit {
        ("submit", 30)
    } else if is_status {
        ("status", 240)
    } else if is_asset {
        ("asset", 120)
    } else {
        ("page", 60)
    };
    if scanner_rate_hit(format!("{class}|{ip}"), limit, 60) {
        return scanner_too_many();
    }

    let mut response = next.run(request).await;
    if (is_submit || is_status) && response.status() == StatusCode::BAD_REQUEST {
        // 只累計，不因這一次就擋；下一次請求會先看失誤桶。
        let _ = scanner_rate_hit(strike_bucket, u32::MAX, 300);
    }

    let headers = response.headers_mut();
    let set_if_absent = |headers: &mut axum::http::HeaderMap, name: header::HeaderName, value: &'static str| {
        if !headers.contains_key(&name) {
            headers.insert(name, header::HeaderValue::from_static(value));
        }
    };
    set_if_absent(headers, header::REFERRER_POLICY, "no-referrer");
    set_if_absent(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set_if_absent(headers, header::X_FRAME_OPTIONS, "DENY");
    set_if_absent(headers, header::CACHE_CONTROL, "no-store");
    if !is_asset {
        set_if_absent(headers, header::CONTENT_SECURITY_POLICY, SCANNER_CSP);
    }
    response
}

fn scanner_too_many() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "error": "too many requests" })),
    )
        .into_response()
}

pub fn build_scanner_router(scanner: Arc<QrScannerRegistry>) -> Router {
    build_scanner_router_with_admin(scanner, None)
}

pub fn build_scanner_router_with_admin(
    scanner: Arc<QrScannerRegistry>,
    admin_web: Option<AdminWebState>,
) -> Router {
    let mut app = Router::new()
        .route("/scanner", get(scanner_page_handler))
        .route("/scanner/jsqr.js", get(scanner_jsqr_handler))
        .route("/scanner/status", get(scanner_status_handler))
        .route("/scanner/submit", post(scanner_submit_handler))
        .route("/number", get(number_page_handler))
        .route("/number/status", get(number_status_handler))
        .route("/number/submit", post(number_submit_handler))
        // SEC_TRACE_NO_QUERY_V1：TraceLayer 預設 span 會記完整 URI（含 ?token=…）。
        // 改為只記 method＋path，避免 scanner／number 的 query bearer token 進入應用程式日誌。
        .layer(TraceLayer::new_for_http().make_span_with(|req: &axum::extract::Request| {
            tracing::debug_span!(
                "request",
                method = %req.method(),
                path = %req.uri().path(),
            )
        }))
        // SEC_SCANNER_HARDEN_V1：限流＋安全標頭，僅作用於上述公開路由。
        .layer(axum::middleware::from_fn(scanner_public_hardening_middleware))
        .with_state(ScannerHttpState::new(scanner));

    if let Some(admin_web) = admin_web {
        app = app.merge(admin_web::build_admin_router(admin_web));
    }

    app
}

pub async fn start_scanner_server(
    scanner: Arc<QrScannerRegistry>,
    bind_addr: &str,
    port: u16,
) -> Result<()> {
    start_scanner_server_with_admin(scanner, bind_addr, port, None).await
}

pub async fn start_scanner_server_with_admin(
    scanner: Arc<QrScannerRegistry>,
    bind_addr: &str,
    port: u16,
    admin_web: Option<AdminWebState>,
) -> Result<()> {
    let app = build_scanner_router_with_admin(scanner, admin_web);
    let ip = bind_addr
        .trim()
        .parse::<std::net::IpAddr>()
        .into_diagnostic()
        .wrap_err_with(|| format!("無效的 scanner bind address: `{bind_addr}`"))?;
    let addr = std::net::SocketAddr::new(ip, port);

    info!(bind_addr = %bind_addr, port = port, "QR scanner 伺服器啟動：http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to bind scanner server to port {port}"))?;

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .into_diagnostic()
    .wrap_err("Scanner server error")
}

async fn scanner_jsqr_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("line/jsQR.min.js"),
    )
        .into_response()
}

async fn scanner_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("line/scanner.html")),
    )
        .into_response()
}

async fn number_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("line/number.html")),
    )
        .into_response()
}

async fn scanner_status_handler(
    State(state): State<ScannerHttpState>,
    Query(query): Query<ScannerStatusQuery>,
) -> Response {
    match state.scanner.status(query).await {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn number_status_handler(
    State(state): State<ScannerHttpState>,
    Query(query): Query<ScannerStatusQuery>,
) -> Response {
    match state.scanner.number_status(query).await {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn number_submit_handler(
    State(state): State<ScannerHttpState>,
    Json(submission): Json<NumberCodeSubmission>,
) -> Response {
    match state.scanner.submit_number(submission).await {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn scanner_submit_handler(
    State(state): State<ScannerHttpState>,
    Json(submission): Json<ScannerSubmission>,
) -> Response {
    match state.scanner.submit(submission).await {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

fn cleanup_expired_locked(inner: &mut HashMap<PendingQrKey, PendingQrEntry>, now: Instant) {
    inner.retain(|_, entry| entry.expires_at >= now);
}

fn cleanup_expired_number_locked(
    inner: &mut HashMap<PendingNumberKey, PendingNumberEntry>,
    now: Instant,
) {
    inner.retain(|_, entry| entry.expires_at >= now);
}

/// SEC_SCANNER_MIN_STATUS_V1：把「不是自己」的帳號遮罩成 `410***56`（短 ID 為 `4***`）。
fn mask_account_for(viewer: &str, account: &str) -> String {
    if account == viewer {
        return account.to_string();
    }
    let chars: Vec<char> = account.chars().collect();
    if chars.len() >= 6 {
        let head: String = chars[..3].iter().collect();
        let tail: String = chars[chars.len() - 2..].iter().collect();
        format!("{head}***{tail}")
    } else if let Some(first) = chars.first() {
        format!("{first}***")
    } else {
        "***".to_string()
    }
}

fn status_result_from_sets<'a>(
    viewer: &str,
    all_accounts: &HashSet<String>,
    completed_accounts: &HashSet<String>,
    pending_accounts_iter: impl Iterator<Item = &'a String>,
) -> ScannerStatusResult {
    let mut pending_accounts = pending_accounts_iter
        .map(|a| mask_account_for(viewer, a))
        .collect::<Vec<_>>();
    pending_accounts.sort();
    let mut completed_accounts = completed_accounts
        .iter()
        .map(|a| mask_account_for(viewer, a))
        .collect::<Vec<_>>();
    completed_accounts.sort();

    ScannerStatusResult {
        total_count: all_accounts.len(),
        completed_count: completed_accounts.len(),
        pending_count: pending_accounts.len(),
        completed_accounts,
        pending_accounts,
        last_error: None,
        expires_in_secs: 0,
        submitted_code: None,
    }
}

/// ROLLCALL_INPUT_COUNTDOWN_V1：把 `expires_at` 換算成「還剩幾秒」（已過期回 0）。
fn secs_until(expires_at: Instant, now: Instant) -> u64 {
    expires_at.saturating_duration_since(now).as_secs()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannerLink {
    pub scan_url: String,
    pub token: String,
    pub already_submitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberCodeLink {
    pub input_url: String,
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScannerSubmission {
    pub provider: String,
    pub rollcall_id: u64,
    pub account_id: String,
    pub token: String,
    pub qr_data: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScannerStatusQuery {
    pub provider: String,
    pub rollcall_id: u64,
    pub account_id: String,
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NumberCodeSubmission {
    pub provider: String,
    pub rollcall_id: u64,
    pub account_id: String,
    pub token: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub auto_lookup: Option<bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScannerStatusResult {
    pub total_count: usize,
    pub completed_count: usize,
    pub pending_count: usize,
    /// SEC_SCANNER_MIN_STATUS_V1（審查 M-05）：公開 /status 只回「自己的完整帳號」，
    /// 其他帳號一律遮罩（`410***56`），且不再回傳完整 `all_accounts` 名冊。
    pub completed_accounts: Vec<String>,
    pub pending_accounts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// ROLLCALL_INPUT_COUNTDOWN_V1：距離這個 rollcall 等待輸入逾時還剩幾秒
    /// （以最晚加入的帳號為準）。頁面用它顯示倒數；0 代表已到期。
    pub expires_in_secs: u64,
    /// NUMBER_PAGE_UX_V1：這個 rollcall 最後一次送出的輸入（"auto" 或 4 位數字碼）。
    /// 頁面用它在多裝置／重新整理後顯示「已送出 1234，等待結果」而不是「尚未送出」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScannerSubmitResult {
    pub delivered_count: usize,
    pub delivered_accounts: Vec<String>,
    pub failed_accounts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NumberCodeSubmitResult {
    pub delivered_count: usize,
    pub delivered_accounts: Vec<String>,
    pub failed_accounts: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ScannerError {
    #[error("scanner public_base_url 不是有效 URL")]
    InvalidPublicBaseUrl,
    #[error("scanner submission 欄位不完整：{0}")]
    InvalidSubmission(&'static str),
    #[error("找不到等待中的點名流程，可能已逾時或服務剛重啟")]
    PendingNotFound,
    #[error("QR Code scanner token 已過期")]
    Expired,
    #[error("QR Code scanner token 已使用")]
    TokenAlreadyUsed,
    #[error("QR Code scanner token 不正確")]
    InvalidToken,
    #[error("此帳號沒有等待中的 QR Code 簽到")]
    AccountNotPending,
    #[error("QR Code 資料過大，最大 {max} bytes")]
    QrDataTooLarge { max: usize },
    #[error("數字點名碼必須是 4 位數字")]
    InvalidNumberCode,
    #[error("沒有任何帳號收到資料；等待流程可能已結束，請重新觸發點名檢查")]
    NoDeliveredAccounts,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn registry() -> QrScannerRegistry {
        QrScannerRegistry::new("https://scanner.example.test/base")
    }

    #[tokio::test]
    async fn register_pending_builds_scanner_url() {
        let registry = registry();
        let (tx, _rx) = mpsc::channel(1);

        let link = registry
            .register_pending("fju", 42, "acc1", tx, Duration::from_secs(60))
            .await
            .unwrap();

        assert!(link
            .scan_url
            .starts_with("https://scanner.example.test/scanner?"));
        assert!(link.scan_url.contains("provider=fju"));
        assert!(link.scan_url.contains("rollcall_id=42"));
        assert!(link.scan_url.contains("account_id=acc1"));
        assert!(link.scan_url.contains("token="));
        assert_eq!(link.token.len(), 32);
    }

    #[tokio::test]
    async fn submit_fans_out_to_all_accounts_for_same_rollcall() {
        let registry = registry();
        let (tx_a, mut rx_a) = mpsc::channel(1);
        let (tx_b, mut rx_b) = mpsc::channel(1);

        let link = registry
            .register_pending("fju", 42, "acc-a", tx_a, Duration::from_secs(60))
            .await
            .unwrap();
        registry
            .register_pending("fju", 42, "acc-b", tx_b, Duration::from_secs(60))
            .await
            .unwrap();

        let result = registry
            .submit(ScannerSubmission {
                provider: "fju".to_string(),
                rollcall_id: 42,
                account_id: "acc-a".to_string(),
                token: link.token,
                qr_data: "0~100!3~secret!4~42".to_string(),
            })
            .await
            .unwrap();

        assert_eq!(result.delivered_count, 2);
        assert_eq!(rx_a.recv().await.unwrap(), "0~100!3~secret!4~42");
        assert_eq!(rx_b.recv().await.unwrap(), "0~100!3~secret!4~42");
    }

    #[tokio::test]
    async fn token_reuse_delivers_updated_qr_data() {
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(2);
        let link = registry
            .register_pending("fju", 42, "acc1", tx, Duration::from_secs(60))
            .await
            .unwrap();

        let mut submission = ScannerSubmission {
            provider: "fju".to_string(),
            rollcall_id: 42,
            account_id: "acc1".to_string(),
            token: link.token,
            qr_data: "0~100!3~secret!4~42".to_string(),
        };

        registry.submit(submission.clone()).await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), "0~100!3~secret!4~42");

        submission.qr_data = "0~100!3~new-secret!4~42".to_string();
        registry.submit(submission).await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), "0~100!3~new-secret!4~42");
    }

    #[tokio::test]
    async fn later_account_registration_receives_already_submitted_qr_data() {
        let registry = registry();
        let (tx_a, mut rx_a) = mpsc::channel(1);
        let link = registry
            .register_pending("fju", 42, "acc-a", tx_a, Duration::from_secs(60))
            .await
            .unwrap();

        registry
            .submit(ScannerSubmission {
                provider: "fju".to_string(),
                rollcall_id: 42,
                account_id: "acc-a".to_string(),
                token: link.token,
                qr_data: "0~100!3~secret!4~42".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(rx_a.recv().await.unwrap(), "0~100!3~secret!4~42");

        let (tx_b, mut rx_b) = mpsc::channel(1);
        let link_b = registry
            .register_pending("fju", 42, "acc-b", tx_b, Duration::from_secs(60))
            .await
            .unwrap();

        assert!(link_b.already_submitted);
        assert_eq!(rx_b.recv().await.unwrap(), "0~100!3~secret!4~42");
    }

    #[tokio::test]
    async fn rejects_wrong_rollcall_provider_token_and_expired_token() {
        let registry = registry();
        let (tx, _rx) = mpsc::channel(1);
        let link = registry
            .register_pending("fju", 42, "acc1", tx, Duration::from_millis(20))
            .await
            .unwrap();

        let base = ScannerSubmission {
            provider: "fju".to_string(),
            rollcall_id: 42,
            account_id: "acc1".to_string(),
            token: link.token.clone(),
            qr_data: "0~100!3~secret!4~42".to_string(),
        };

        let mut wrong_rollcall = base.clone();
        wrong_rollcall.rollcall_id = 43;
        assert_eq!(
            registry.submit(wrong_rollcall).await.unwrap_err(),
            ScannerError::PendingNotFound
        );

        let mut wrong_provider = base.clone();
        wrong_provider.provider = "other".to_string();
        assert_eq!(
            registry.submit(wrong_provider).await.unwrap_err(),
            ScannerError::PendingNotFound
        );

        let mut wrong_token = base.clone();
        wrong_token.token = "wrong".to_string();
        assert_eq!(
            registry.submit(wrong_token).await.unwrap_err(),
            ScannerError::InvalidToken
        );

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            registry.submit(base).await.unwrap_err(),
            ScannerError::Expired
        );
    }
}
