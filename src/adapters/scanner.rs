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
use crate::adapters::requests::{
    NumberCodeInput, NumberCodeSender, QrCodeSender, RadarPointInput, RadarPointSender,
};
use crate::api::rollcall::{RollcallProgress, AUTO_SUBMIT_PERCENT};
use crate::api::ApiClient;
use crate::text_utils::truncate_chars;

const DEFAULT_SCANNER_PATH: &str = "/scanner";
const DEFAULT_NUMBER_PATH: &str = "/number";
// RADAR_INPUT_PAGE_V1（fix71）：雷達點名輸入頁（輸入座標）。
const DEFAULT_RADAR_PATH: &str = "/radar";
const MAX_QR_DATA_LEN: usize = 16 * 1024;
/// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：同一個點名的班上進度最多每幾秒真的讀一次。
/// 監控流程每個帳號都會問一次，靠這層快取把「13 個帳號」壓成「每 10 秒 1 次」。
const CLASS_PROGRESS_TTL: Duration = Duration::from_secs(10);
/// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1：快取最多留這麼久（點名換場就自然清掉）。
const CLASS_PROGRESS_CACHE_TTL: Duration = Duration::from_secs(600);

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

/// RADAR_INPUT_PAGE_V1（fix71）：一個等待「輸入座標」的雷達點名。
#[derive(Clone)]
struct PendingRadarAccount {
    tx: RadarPointSender,
}

struct PendingRadarEntry {
    token: String,
    expires_at: Instant,
    accounts: HashMap<String, PendingRadarAccount>,
    completed_accounts: HashSet<String>,
    all_accounts: HashSet<String>,
    /// 最後一次從輸入頁收到的座標（頁面重新整理後仍看得到自己剛送出的值）。
    submitted_point: Option<RadarPointPayload>,
    /// 最後一次失敗原因（雷達多半是「距離不足」，頁面會直接顯示距離）。
    last_error: Option<String>,
    /// 監控流程目前正在嘗試的座標清單（頁面可一鍵選用）。
    configured_coords: Vec<[f64; 2]>,
    /// 這一輪是哪一堂課的點名（頁面與面板即時狀態都顯示這個）。
    course_name: String,
    /// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：最近一次讀到的「班上點名進度」
    /// （頁面會顯示，並說明超過 15% 系統就會自己送）。
    class_progress: Option<RollcallProgress>,
}

/// Shared registry for scanner links and pending QR waits.
#[derive(Clone)]
pub struct QrScannerRegistry {
    public_base_url: String,
    extra_public_base_urls: Vec<String>,
    scanner_path: String,
    number_path: String,
    radar_path: String,
    inner: Arc<Mutex<HashMap<PendingQrKey, PendingQrEntry>>>,
    number_inner: Arc<Mutex<HashMap<PendingNumberKey, PendingNumberEntry>>>,
    /// RADAR_INPUT_PAGE_V1（fix71）：等待輸入座標的雷達點名（key 同數字點名）。
    radar_inner: Arc<Mutex<HashMap<PendingNumberKey, PendingRadarEntry>>>,
    /// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：班上點名進度快取（key 同數字點名），
    /// 值＝(讀取時間, 進度)。多帳號共用同一份，避免變成 13 倍讀取。
    progress_cache: Arc<Mutex<HashMap<PendingNumberKey, (Instant, Option<RollcallProgress>)>>>,
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
            radar_path: DEFAULT_RADAR_PATH.to_string(),
            inner: Arc::new(Mutex::new(HashMap::new())),
            number_inner: Arc::new(Mutex::new(HashMap::new())),
            radar_inner: Arc::new(Mutex::new(HashMap::new())),
            progress_cache: Arc::new(Mutex::new(HashMap::new())),
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

        let mut pending_accounts = entry.accounts.keys().cloned().collect::<Vec<_>>();
        pending_accounts.sort();
        let mut completed_accounts = entry.completed_accounts.iter().cloned().collect::<Vec<_>>();
        completed_accounts.sort();
        let mut all_accounts = entry.all_accounts.iter().cloned().collect::<Vec<_>>();
        all_accounts.sort();

        let mut result = ScannerStatusResult {
            total_count: all_accounts.len(),
            completed_count: completed_accounts.len(),
            pending_count: pending_accounts.len(),
            completed_accounts,
            pending_accounts,
            all_accounts,
            last_error: None,
            expires_in_secs: secs_until(entry.expires_at, now),
            submitted_code: None,
            class_progress: None,
            auto_submit_percent: AUTO_SUBMIT_PERCENT,
        };
        // CLASS_COUNT_ON_PAGES_V1：先把這一格的鎖放掉再讀快取（兩個 mutex 不重疊持有）。
        drop(inner);
        result.class_progress = self.cached_class_progress(provider, query.rollcall_id).await;
        Ok(result)
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
        // CLASS_COUNT_ON_PAGES_V1：先把這一格的鎖放掉再讀快取，頁面才有「目前班上已點人數」。
        drop(inner);
        status.class_progress = self.cached_class_progress(provider, query.rollcall_id).await;
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

    // ── RADAR_INPUT_PAGE_V1（fix71）：雷達點名輸入頁 ─────────────────────────
    //
    // 雷達簽到要的是「人在哪裡」，而這個值只有使用者知道（或由手機 GPS 提供）。
    // 這裡刻意把註冊／狀態／送出做成與數字點名頁一模一樣的四個步驟，讓前端頁面
    // 與監控流程都能共用同一套心智模型：
    //   register（監控端登記）→ 頁面 status 輪詢 → 頁面 submit 座標 → 監控端取用。

    /// 登記某個帳號正在等待雷達座標輸入。同一個 (provider, rollcall_id) 會共用同一個 token，
    /// 所以監控流程每一輪重試都呼叫它時，使用者手上的網址不會一直變。
    pub async fn register_radar_pending(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        tx: RadarPointSender,
        timeout: Duration,
        configured_coords: &[[f64; 2]],
        course_name: &str,
    ) -> Result<RadarPointLink, ScannerError> {
        let now = Instant::now();
        let expires_at = now + timeout;
        let key = PendingNumberKey::new(provider, rollcall_id);
        let (token, account_count, submitted_point) = {
            let mut inner = self.radar_inner.lock().await;
            cleanup_expired_radar_locked(&mut inner, now);

            let entry = inner.entry(key).or_insert_with(|| PendingRadarEntry {
                token: Uuid::new_v4().simple().to_string(),
                expires_at,
                accounts: HashMap::new(),
                completed_accounts: HashSet::new(),
                all_accounts: HashSet::new(),
                submitted_point: None,
                last_error: None,
                configured_coords: Vec::new(),
                course_name: String::new(),
                class_progress: None,
            });

            entry.expires_at = entry.expires_at.max(expires_at);
            // 座標清單以「監控流程這一輪實際要用的」為準（面板設定可能隨時改）。
            if !configured_coords.is_empty() {
                entry.configured_coords = configured_coords.to_vec();
            }
            // 課程名稱只作為顯示用；空字串代表呼叫端沒提供（維持既有值）。
            let course_name = course_name.trim();
            if !course_name.is_empty() {
                entry.course_name = truncate_chars(course_name, 120).to_string();
            }
            entry.all_accounts.insert(account_id.to_string());
            entry
                .accounts
                .insert(account_id.to_string(), PendingRadarAccount { tx: tx.clone() });
            (
                entry.token.clone(),
                entry.accounts.len(),
                entry.submitted_point.clone(),
            )
        };

        // 監控流程比使用者晚一步登記（例如網頁開著但這一輪才輪到這個帳號）：
        // 已經收到的座標直接補送，不必讓使用者再按一次。
        if let Some(point) = submitted_point.as_ref() {
            if let Err(e) = tx.send(point.to_input()).await {
                warn!(account_id = %account_id, error = %e, "已提交的雷達座標補送失敗");
            }
        }

        let input_url = self.build_radar_url(provider, rollcall_id, account_id, &token)?;
        debug!(
            provider = %provider,
            rollcall_id = rollcall_id,
            account_id = %account_id,
            account_count = account_count,
            "已註冊雷達點名等待項目"
        );

        Ok(RadarPointLink {
            input_url,
            token: token.to_string(),
        })
    }

    /// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：讀「班上點名進度」。
    ///
    /// 同一個點名每 [`CLASS_PROGRESS_TTL`] 秒最多真的讀一次，多帳號共用同一份結果
    /// （讀不到時也把 `None` 放進快取，避免讀不到就一直打學校 API）。
    /// 這是「超過 15% 就自動送出」的唯一資料來源；讀不到時呼叫端一律維持原行為。
    pub async fn class_progress(
        &self,
        api: &ApiClient,
        provider: &str,
        rollcall_id: u64,
    ) -> Option<RollcallProgress> {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let now = Instant::now();
        {
            let mut cache = self.progress_cache.lock().await;
            cache.retain(|_, (at, _)| now.saturating_duration_since(*at) < CLASS_PROGRESS_CACHE_TTL);
            if let Some((at, progress)) = cache.get(&key) {
                if now.saturating_duration_since(*at) < CLASS_PROGRESS_TTL {
                    return progress.clone();
                }
            }
        }

        let progress = match api.get_rollcall_progress(rollcall_id).await {
            Ok(progress) => progress,
            Err(e) => {
                debug!(
                    rollcall_id = rollcall_id,
                    error = %e,
                    "讀取班上點名進度失敗（本次略過，維持原本行為）"
                );
                None
            }
        };

        let mut cache = self.progress_cache.lock().await;
        cache.insert(key, (Instant::now(), progress.clone()));
        progress
    }

    /// CLASS_COUNT_ON_PAGES_V1：**只讀快取、不打學校 API** 的班上進度。
    ///
    /// 點名頁（數字／QR）每一兩秒就輪詢一次狀態，不可能每次都去打學校；
    /// 監控流程本來就每 [`CLASS_PROGRESS_TTL`] 秒會讀一次（見 [`Self::class_progress`]），
    /// 這裡只是把那份結果拿出來給頁面顯示「目前班上已點人數」。
    /// 沒讀過、或放太久（超過 [`CLASS_PROGRESS_CACHE_TTL`]）就回 `None`（頁面顯示讀取中）。
    pub async fn cached_class_progress(
        &self,
        provider: &str,
        rollcall_id: u64,
    ) -> Option<RollcallProgress> {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let now = Instant::now();
        let cache = self.progress_cache.lock().await;
        match cache.get(&key) {
            Some((at, progress))
                if now.saturating_duration_since(*at) < CLASS_PROGRESS_CACHE_TTL =>
            {
                progress.clone()
            }
            _ => None,
        }
    }

    /// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：把最新的班上進度記在等待中的雷達點名上，
    /// 讓點名頁面顯示「班上已點到幾分之幾、超過 15% 系統會自己送」。
    pub async fn set_radar_class_progress(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        progress: Option<RollcallProgress>,
    ) {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let mut inner = self.radar_inner.lock().await;
        if let Some(entry) = inner.get_mut(&key) {
            if entry.all_accounts.contains(account_id) || entry.accounts.contains_key(account_id) {
                entry.class_progress = progress;
            }
        }
    }

    /// 標記某個帳號的雷達簽到已完成（輸入頁顯示「已點到」，與數字點名頁一致）。
    pub async fn mark_radar_completed(&self, provider: &str, rollcall_id: u64, account_id: &str) {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let mut inner = self.radar_inner.lock().await;
        if let Some(entry) = inner.get_mut(&key) {
            entry.all_accounts.insert(account_id.to_string());
            entry.completed_accounts.insert(account_id.to_string());
            entry.last_error = None;
            debug!(
                provider = %provider,
                rollcall_id = rollcall_id,
                account_id = %account_id,
                completed = entry.completed_accounts.len(),
                total = entry.all_accounts.len(),
                "雷達點名頁標記帳號已完成簽到"
            );
        }
    }

    /// 記錄某個帳號的雷達簽到失敗原因（多半是「距離不足 X 公尺」，頁面會直接顯示）。
    pub async fn mark_radar_failed(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        reason: &str,
    ) {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let mut inner = self.radar_inner.lock().await;
        if let Some(entry) = inner.get_mut(&key) {
            entry.all_accounts.insert(account_id.to_string());
            entry.last_error = Some(format!("{}：{}", account_id, reason));
            debug!(
                provider = %provider,
                rollcall_id = rollcall_id,
                account_id = %account_id,
                reason = %reason,
                "雷達點名頁記錄簽到失敗原因"
            );
        }
    }

    pub async fn radar_status(
        &self,
        query: ScannerStatusQuery,
    ) -> Result<RadarStatusResult, ScannerError> {
        let provider = query.provider.trim();
        let account_id = query.account_id.trim();
        let token = query.token.trim();

        if provider.is_empty() || account_id.is_empty() || token.is_empty() {
            return Err(ScannerError::InvalidSubmission("missing required field"));
        }

        let key = PendingNumberKey::new(provider, query.rollcall_id);
        let now = Instant::now();
        let mut inner = self.radar_inner.lock().await;
        cleanup_expired_radar_locked(&mut inner, now);
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

        // sender 已關閉＝那個帳號的這一輪已經結束（成功或逾時），不該再顯示成「等待中」。
        let mut pending_accounts = entry
            .accounts
            .iter()
            .filter(|(_, account)| !account.tx.is_closed())
            .map(|(account_id, _)| account_id.clone())
            .collect::<Vec<_>>();
        pending_accounts.sort();
        let mut completed_accounts = entry.completed_accounts.iter().cloned().collect::<Vec<_>>();
        completed_accounts.sort();
        let mut all_accounts = entry.all_accounts.iter().cloned().collect::<Vec<_>>();
        all_accounts.sort();

        let mut result = RadarStatusResult {
            total_count: all_accounts.len(),
            completed_count: completed_accounts.len(),
            pending_count: pending_accounts.len(),
            completed_accounts,
            pending_accounts,
            all_accounts,
            last_error: entry.last_error.clone(),
            expires_in_secs: secs_until(entry.expires_at, now),
            submitted_point: entry.submitted_point.clone(),
            configured_coords: entry.configured_coords.clone(),
            course_name: entry.course_name.clone(),
            class_progress: entry.class_progress.clone(),
            auto_submit_percent: AUTO_SUBMIT_PERCENT,
        };
        // CLASS_COUNT_ON_PAGES_V1：這一場的處理流程還沒回報過進度時，退回共用快取
        // （多帳號只會有其中一個帳號的流程在跑，頁面照樣看得到數字）。
        drop(inner);
        if result.class_progress.is_none() {
            result.class_progress = self.cached_class_progress(provider, query.rollcall_id).await;
        }
        Ok(result)
    }

    /// RADAR_INPUT_PAGE_V1（fix71）：面板（已登入的管理員）直接把座標送給等待中的點名。
    ///
    /// 與輸入頁的差別：輸入頁認的是 token（誰點名誰輸入），這裡認的是登入身分，
    /// 所以可以在**還沒有人開輸入頁**時就先把座標送進去。`rollcall_id` 給 `None` ＝
    /// 送給該 provider 目前所有等待中的點名。
    pub async fn broadcast_radar_point(
        &self,
        provider: &str,
        rollcall_id: Option<u64>,
        point: RadarPointInput,
    ) -> RadarBroadcastResult {
        let provider = provider.trim();
        let now = Instant::now();
        let payload = RadarPointPayload::from_input(&point);
        let mut inner = self.radar_inner.lock().await;
        cleanup_expired_radar_locked(&mut inner, now);

        let mut targets: Vec<RadarBroadcastTarget> = Vec::new();
        let mut delivered_total = 0usize;
        for (key, entry) in inner.iter_mut() {
            if key.provider != provider {
                continue;
            }
            if let Some(id) = rollcall_id {
                if key.rollcall_id != id {
                    continue;
                }
            }
            let mut delivered = 0usize;
            for (account_id, account) in entry.accounts.iter() {
                match account.tx.send(point).await {
                    Ok(()) => delivered += 1,
                    Err(e) => {
                        warn!(account_id = %account_id, error = %e, "面板送出雷達座標失敗");
                    }
                }
            }
            if delivered > 0 {
                entry.submitted_point = Some(payload.clone());
                entry.last_error = None;
            }
            delivered_total += delivered;
            targets.push(RadarBroadcastTarget {
                rollcall_id: key.rollcall_id,
                course_name: entry.course_name.clone(),
                delivered_count: delivered,
            });
        }
        targets.sort_by_key(|target| target.rollcall_id);

        if delivered_total > 0 {
            info!(
                provider = %provider,
                rollcall_id = ?rollcall_id,
                delivered = delivered_total,
                source = point.source_label(),
                "面板送出雷達座標給等待中的點名"
            );
        }

        RadarBroadcastResult {
            delivered_count: delivered_total,
            targets,
        }
    }

    /// RADAR_INPUT_PAGE_V1（fix71）：目前正在等待輸入座標的雷達點名（面板「即時狀態」用）。
    ///
    /// `provider` 給空字串＝列出全部 provider。已逾時的項目會被清掉（不會顯示殭屍狀態）。
    pub async fn radar_live_entries(&self, provider: &str) -> Vec<RadarLiveEntry> {
        let provider = provider.trim();
        let now = Instant::now();
        let inner = self.radar_inner.lock().await;
        let mut out: Vec<RadarLiveEntry> = inner
            .iter()
            .filter(|(key, entry)| entry.expires_at >= now && (provider.is_empty() || key.provider == provider))
            .map(|(key, entry)| {
                let mut pending_accounts = entry
                    .accounts
                    .iter()
                    .filter(|(_, account)| !account.tx.is_closed())
                    .map(|(account_id, _)| account_id.clone())
                    .collect::<Vec<_>>();
                pending_accounts.sort();
                let mut completed_accounts =
                    entry.completed_accounts.iter().cloned().collect::<Vec<_>>();
                completed_accounts.sort();
                RadarLiveEntry {
                    provider: key.provider.clone(),
                    rollcall_id: key.rollcall_id,
                    course_name: entry.course_name.clone(),
                    total_count: entry.all_accounts.len(),
                    completed_count: completed_accounts.len(),
                    pending_count: pending_accounts.len(),
                    pending_accounts,
                    completed_accounts,
                    submitted_point: entry.submitted_point.clone(),
                    last_error: entry.last_error.clone(),
                    expires_in_secs: secs_until(entry.expires_at, now),
                }
            })
            .collect();
        // 最新的點名排前面（逾時時間越晚＝越新）。
        out.sort_by(|a, b| b.expires_in_secs.cmp(&a.expires_in_secs));
        out
    }

    /// 輸入頁送出的座標：驗證後 fan-out 給同一個 rollcall 的所有等待帳號。
    pub async fn submit_radar(
        &self,
        submission: RadarPointSubmission,
    ) -> Result<RadarSubmitResult, ScannerError> {
        let provider = submission.provider.trim();
        let account_id = submission.account_id.trim();
        let token = submission.token.trim();

        if provider.is_empty() || account_id.is_empty() || token.is_empty() {
            return Err(ScannerError::InvalidSubmission("missing required field"));
        }

        let point = RadarPointInput {
            latitude: submission.latitude,
            longitude: submission.longitude,
            accuracy: submission.accuracy,
            from_device: submission.from_device.unwrap_or(false),
        };
        if !point.is_valid() {
            return Err(ScannerError::InvalidRadarPoint);
        }

        let key = PendingNumberKey::new(provider, submission.rollcall_id);
        let account_senders = {
            let now = Instant::now();
            let mut inner = self.radar_inner.lock().await;
            cleanup_expired_radar_locked(&mut inner, now);
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
            entry.submitted_point = Some(RadarPointPayload::from_input(&point));
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
            match tx.send(point).await {
                Ok(()) => delivered_accounts.push(account_id),
                Err(e) => {
                    warn!(account_id = %account_id, error = %e, "雷達點名頁傳送座標失敗");
                    failed_accounts.push(account_id);
                }
            }
        }

        if delivered_accounts.is_empty() {
            return Err(ScannerError::NoDeliveredAccounts);
        }

        info!(
            provider = %provider,
            rollcall_id = submission.rollcall_id,
            delivered = delivered_accounts.len(),
            source = point.source_label(),
            "雷達點名頁送出座標"
        );

        Ok(RadarSubmitResult {
            delivered_count: delivered_accounts.len(),
            delivered_accounts,
            failed_accounts,
            point: RadarPointPayload::from_input(&point),
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

    fn build_radar_url(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        token: &str,
    ) -> Result<String, ScannerError> {
        let path = self.radar_path.clone();
        self.build_tool_url(&path, provider, rollcall_id, account_id, token)
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
        // RADAR_INPUT_PAGE_V1（fix71）：雷達點名輸入頁（輸入座標）。
        .route("/radar", get(radar_page_handler))
        .route("/radar/status", get(radar_status_handler))
        .route("/radar/submit", post(radar_submit_handler))
        .layer(TraceLayer::new_for_http())
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

/// RADAR_INPUT_PAGE_V1（fix71）：雷達點名輸入頁。
async fn radar_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("line/radar.html")),
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

async fn radar_status_handler(
    State(state): State<ScannerHttpState>,
    Query(query): Query<ScannerStatusQuery>,
) -> Response {
    match state.scanner.radar_status(query).await {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn radar_submit_handler(
    State(state): State<ScannerHttpState>,
    Json(submission): Json<RadarPointSubmission>,
) -> Response {
    match state.scanner.submit_radar(submission).await {
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

/// RADAR_INPUT_PAGE_V1（fix71）：清掉逾時的雷達等待項目。
fn cleanup_expired_radar_locked(
    inner: &mut HashMap<PendingNumberKey, PendingRadarEntry>,
    now: Instant,
) {
    inner.retain(|_, entry| entry.expires_at >= now);
}

fn status_result_from_sets<'a>(
    all_accounts: &HashSet<String>,
    completed_accounts: &HashSet<String>,
    pending_accounts_iter: impl Iterator<Item = &'a String>,
) -> ScannerStatusResult {
    let mut pending_accounts = pending_accounts_iter.cloned().collect::<Vec<_>>();
    pending_accounts.sort();
    let mut completed_accounts = completed_accounts.iter().cloned().collect::<Vec<_>>();
    completed_accounts.sort();
    let mut all_accounts = all_accounts.iter().cloned().collect::<Vec<_>>();
    all_accounts.sort();

    ScannerStatusResult {
        total_count: all_accounts.len(),
        completed_count: completed_accounts.len(),
        pending_count: pending_accounts.len(),
        completed_accounts,
        pending_accounts,
        all_accounts,
        last_error: None,
        expires_in_secs: 0,
        submitted_code: None,
        class_progress: None,
        auto_submit_percent: AUTO_SUBMIT_PERCENT,
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

/// RADAR_INPUT_PAGE_V1（fix71）：等待輸入座標的雷達點名輸入頁網址。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RadarPointLink {
    pub input_url: String,
    pub token: String,
}

/// 輸入頁送出／狀態回報用的座標（JSON 形狀；`lat`／`lon` 對前端最直覺）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RadarPointPayload {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accuracy: Option<f64>,
    #[serde(default)]
    pub from_device: bool,
}

impl RadarPointPayload {
    pub fn from_input(input: &RadarPointInput) -> Self {
        Self {
            latitude: input.latitude,
            longitude: input.longitude,
            accuracy: input.accuracy,
            from_device: input.from_device,
        }
    }

    pub fn to_input(&self) -> RadarPointInput {
        RadarPointInput {
            latitude: self.latitude,
            longitude: self.longitude,
            accuracy: self.accuracy,
            from_device: self.from_device,
        }
    }
}

/// 面板手動輸入座標的發送結果。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RadarBroadcastResult {
    pub delivered_count: usize,
    pub targets: Vec<RadarBroadcastTarget>,
}

/// 面板手動輸入座標時，單一 rollcall 的發送結果。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RadarBroadcastTarget {
    pub rollcall_id: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub course_name: String,
    pub delivered_count: usize,
}

/// 面板「即時狀態」用：一個正在等待輸入座標的雷達點名。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RadarLiveEntry {
    pub provider: String,
    pub rollcall_id: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub course_name: String,
    pub total_count: usize,
    pub completed_count: usize,
    pub pending_count: usize,
    pub pending_accounts: Vec<String>,
    pub completed_accounts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_point: Option<RadarPointPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RadarPointSubmission {
    pub provider: String,
    pub rollcall_id: u64,
    pub account_id: String,
    pub token: String,
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub accuracy: Option<f64>,
    #[serde(default)]
    pub from_device: Option<bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RadarStatusResult {
    pub total_count: usize,
    pub completed_count: usize,
    pub pending_count: usize,
    pub completed_accounts: Vec<String>,
    pub pending_accounts: Vec<String>,
    pub all_accounts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// 距離等待輸入逾時還剩幾秒（頁面倒數用；0＝已到期）。
    pub expires_in_secs: u64,
    /// 最後一次送出的座標（重新整理／換裝置後仍看得到）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_point: Option<RadarPointPayload>,
    /// 監控流程目前正在嘗試的座標清單（頁面可一鍵選用）。
    pub configured_coords: Vec<[f64; 2]>,
    /// 這一輪是哪一堂課的點名（可空）。
    #[serde(skip_serializing_if = "String::is_empty")]
    pub course_name: String,
    /// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：班上目前的點名進度（讀不到時沒有這個欄位）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_progress: Option<RollcallProgress>,
    /// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1：超過這個百分比系統就會自動送出（頁面文案用）。
    pub auto_submit_percent: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RadarSubmitResult {
    pub delivered_count: usize,
    pub delivered_accounts: Vec<String>,
    pub failed_accounts: Vec<String>,
    pub point: RadarPointPayload,
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
    pub completed_accounts: Vec<String>,
    pub pending_accounts: Vec<String>,
    pub all_accounts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// ROLLCALL_INPUT_COUNTDOWN_V1：距離這個 rollcall 等待輸入逾時還剩幾秒
    /// （以最晚加入的帳號為準）。頁面用它顯示倒數；0 代表已到期。
    pub expires_in_secs: u64,
    /// NUMBER_PAGE_UX_V1：這個 rollcall 最後一次送出的輸入（"auto" 或 4 位數字碼）。
    /// 頁面用它在多裝置／重新整理後顯示「已送出 1234，等待結果」而不是「尚未送出」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_code: Option<String>,
    /// CLASS_COUNT_ON_PAGES_V1：班上目前的點名進度（讀不到時沒有這個欄位）。
    /// 點名頁上方的欄位用它顯示「目前班上已點人數」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_progress: Option<RollcallProgress>,
    /// CLASS_COUNT_ON_PAGES_V1：超過這個百分比系統就會自動送出（頁面文案用）。
    pub auto_submit_percent: u32,
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
    #[error("座標不正確：緯度需在 -90～90、經度在 -180～180，且不能是 0, 0")]
    InvalidRadarPoint,
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
