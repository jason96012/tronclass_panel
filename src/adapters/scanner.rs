//! Local QR scanner state and submission routing.
//!
//! The scanner page submits one QR payload back to this process. This registry
//! fans that payload out to every account currently waiting for the same
//! provider + rollcall_id.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{Query, State},
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};
use url::Url;

/// SCANNER_TOKEN_256_V1（fix128）：掃碼 token 與面板 session 同級——256-bit
/// 作業系統亂數（64 hex）。拼圖上原有的 URL 傳遞結構不變，統計枚舉空間。
fn scanner_secure_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("SCANNER_TOKEN_256_V1: OS RNG unavailable");
    hex::encode(buf)
}

use crate::adapters::admin_web::{self, AdminWebState, HSTS_HEADER_VALUE};
use crate::adapters::requests::{
    NumberCodeInput, NumberCodeSender, QrCodeSender, RadarPointInput, RadarPointSender,
};
use crate::api::rollcall::{RollcallProgress, AUTO_SUBMIT_PERCENT};
use crate::api::ApiClient;
use crate::text_utils::truncate_chars;
use crate::rollcalls::qr_watchdog::{QrHealth, QrPhase, QR_REPLAY_MAX_AGE};

mod qr_resume;
pub use qr_resume::QrRecoveryStatus;

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
    // QR_RECOVERY_V1：僅在仍等待的帳號上保留錯誤，成功移除帳號時一起清除。
    last_error: Option<String>,
    /// QR_ORDERED_LANE_V1：此帳號的有序送出佇列（只有 QR 使用）。
    lane: Arc<std::sync::Mutex<QrLane>>,
}

impl PendingAccount {
    fn new(tx: QrCodeSender) -> Self {
        Self {
            tx,
            last_error: None,
            lane: Arc::new(std::sync::Mutex::new(QrLane::default())),
        }
    }
}

struct PendingQrEntry {
    token: String,
    expires_at: Instant,
    used: bool,
    submitted_qr_data: Option<String>,
    submitted_qr_at: Option<Instant>,
    submission_revision: u64,
    // QR_STALL_WATCHDOG_V1：ledger 不隨 receiver 替換而歸零。
    watchdogs: HashMap<String, QrHealth>,
    resume: qr_resume::QrResumeState,
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
    qr_wakes: Arc<Mutex<HashMap<(String, String), qr_resume::QrWake>>>,
    qr_resume_signal: tokio::sync::watch::Sender<u64>,
    number_inner: Arc<Mutex<HashMap<PendingNumberKey, PendingNumberEntry>>>,
    /// RADAR_INPUT_PAGE_V1（fix71）：等待輸入座標的雷達點名（key 同數字點名）。
    radar_inner: Arc<Mutex<HashMap<PendingNumberKey, PendingRadarEntry>>>,
    /// AUTO_SUBMIT_BY_CLASS_PROGRESS_V1（fix72）：班上點名進度快取（key 同數字點名），
    /// 值＝(讀取時間, 進度)。多帳號共用同一份，避免變成 13 倍讀取。
    progress_cache: Arc<Mutex<HashMap<PendingNumberKey, (Instant, Option<RollcallProgress>, Option<ApiClient>)>>>,
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
            qr_wakes: Arc::new(Mutex::new(HashMap::new())),
            qr_resume_signal: tokio::sync::watch::channel(0).0,
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
                token: scanner_secure_token(),
                expires_at,
                used: false,
                submitted_qr_data: None,
                submitted_qr_at: None,
                submission_revision: 0,
                watchdogs: HashMap::new(),
                resume: qr_resume::QrResumeState::default(),
                accounts: HashMap::new(),
                completed_accounts: HashSet::new(),
                all_accounts: HashSet::new(),
            });

            if entry.used && entry.submitted_qr_data.is_none() {
                entry.token = scanner_secure_token();
                entry.used = false;
                entry.accounts.clear();
                entry.completed_accounts.clear();
                entry.all_accounts.clear();
                entry.watchdogs.clear();
                entry.resume = qr_resume::QrResumeState::default();
            }
            // A camera-open recovery never extends the original link lifetime.
            if entry.resume.last_request.is_none() { entry.expires_at = entry.expires_at.max(expires_at); }
            entry.all_accounts.insert(account_id.to_string());
            // QR_RECOVERY_V1：重新進入等待代表需要再次確認，不能同時列為已完成。
            entry.completed_accounts.remove(account_id);
            let account = PendingAccount::new(tx);
            let max_age = if entry.resume.last_request.is_some() { Duration::from_secs(3) } else { QR_REPLAY_MAX_AGE };
            let replay = entry.submitted_qr_data.as_ref().filter(|_| {
                entry.submitted_qr_at.is_some_and(|at| now.saturating_duration_since(at) <= max_age)
            }).cloned();
            let health = entry.watchdogs.entry(account_id.to_string()).or_insert_with(|| QrHealth::new(now));
            health.registered(now);
            if let Some(qr_data) = replay.as_ref() {
                health.enqueued(now);
                // 登記時 receiver 尚未開始收取，不能 send().await 等滿通道。
                // 在同一 registry 鎖內排入既有 ordered lane，後續新碼不會超車。
                if !queue_qr_ordered(&account.lane, &account.tx, qr_data.clone(), account_id) {
                    warn!(account_id = %account_id, "已提交 QR data 補送失敗（通道已關閉）");
                }
            }
            entry.accounts.insert(account_id.to_string(), account);

            (
                entry.token.clone(),
                entry.accounts.len(),
                replay,
            )
        };

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

    /// ROLLCALL_FORCE_RESET_V1：強制結束點名時，清掉某帳號在這個點名上的三種登記（QR／數字／雷達）。
    /// 與 `unregister_*` 不同：若該點名已沒有等待中的帳號，同時清掉上一輪已送出的 payload／輸入／座標，
    /// 否則新一輪登記會把舊值再補送給新帳號。已成功的帳號（completed）保留，頁面完成人數不歸零。
    pub async fn reset_account_pending(&self, provider: &str, rollcall_id: u64, account_id: &str) {
        {
            let key = PendingQrKey::new(provider, rollcall_id);
            let mut inner = self.inner.lock().await;
            let mut remove_entry = false;
            if let Some(entry) = inner.get_mut(&key) {
                entry.accounts.remove(account_id);
                entry.watchdogs.remove(account_id);
                entry.resume.requested.remove(account_id);
                entry.resume.templates.remove(account_id);
                entry.resume.errors.remove(account_id);
                if entry.accounts.is_empty() {
                    entry.used = false;
                    entry.submitted_qr_data = None;
                    entry.submitted_qr_at = None;
                    remove_entry = entry.completed_accounts.is_empty();
                }
            }
            if remove_entry {
                inner.remove(&key);
            }
        }
        {
            let key = PendingNumberKey::new(provider, rollcall_id);
            let mut inner = self.number_inner.lock().await;
            let mut remove_entry = false;
            if let Some(entry) = inner.get_mut(&key) {
                entry.accounts.remove(account_id);
                if entry.accounts.is_empty() {
                    entry.submitted_input = None;
                    remove_entry = entry.completed_accounts.is_empty();
                }
            }
            if remove_entry {
                inner.remove(&key);
            }
        }
        {
            let key = PendingNumberKey::new(provider, rollcall_id);
            let mut inner = self.radar_inner.lock().await;
            let mut remove_entry = false;
            if let Some(entry) = inner.get_mut(&key) {
                entry.accounts.remove(account_id);
                if entry.accounts.is_empty() {
                    entry.submitted_point = None;
                    remove_entry = entry.completed_accounts.is_empty();
                }
            }
            if remove_entry {
                inner.remove(&key);
            }
        }
    }

    /// QR_RECOVERY_V1：尚未確認成功時保留輸入通道，讓掃碼頁顯示可恢復原因。
    pub async fn set_qr_error(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        reason: Option<&str>,
    ) {
        let key = PendingQrKey::new(provider, rollcall_id);
        let mut inner = self.inner.lock().await;
        if let Some(account) = inner.get_mut(&key).and_then(|entry| entry.accounts.get_mut(account_id)) {
            account.last_error = reason.map(|text| truncate_chars(text, 240).to_string());
        }
    }

    // QR_STALL_WATCHDOG_V1：短鎖只記錄階段，不跨 await 執行學校請求。
    pub(crate) async fn qr_phase(&self, provider: &str, rollcall_id: u64, account_id: &str, phase: QrPhase) {
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.get_mut(&PendingQrKey::new(provider, rollcall_id)) {
            if entry.accounts.contains_key(account_id) {
                if let Some(health) = entry.watchdogs.get_mut(account_id) { health.enter(phase, Instant::now()); }
            }
        }
    }

    pub(crate) async fn qr_attempt_feedback(&self, provider: &str, rollcall_id: u64, account_id: &str, uncertain: bool) -> bool {
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.get_mut(&PendingQrKey::new(provider, rollcall_id)) {
            if let Some(health) = entry.watchdogs.get_mut(account_id) {
                let now = Instant::now();
                if uncertain {
                    health.uncertain(now);
                    // 此次 PUT/確認已結束，在安全交接點讓 supervisor 先查狀態。
                    health.enter(QrPhase::WaitingInput, now);
                    return health.can_recover(now) && health.stall_reason(now).is_some();
                }
                health.clear_uncertain();
            }
        }
        false
    }

    pub(crate) async fn qr_stall(&self, provider: &str, rollcall_id: u64, account_id: &str) -> Option<(String, bool, u32)> {
        let inner = self.inner.lock().await;
        let entry = inner.get(&PendingQrKey::new(provider, rollcall_id))?;
        if !entry.accounts.contains_key(account_id) { return None; }
        let health = entry.watchdogs.get(account_id)?;
        let now = Instant::now();
        health.stall_reason(now).map(|reason| (reason.to_string(), health.can_recover(now), health.recoveries))
    }

    pub(crate) async fn qr_watchdog_notice(&self, provider: &str, rollcall_id: u64, account_id: &str, notice: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.get_mut(&PendingQrKey::new(provider, rollcall_id)) {
            if let Some(health) = entry.watchdogs.get_mut(account_id) {
                health.notice = Some(truncate_chars(notice, 240).to_string());
            }
        }
    }

    // 呼叫端必須先 drop 舊 worker/input guard，然後才允許新 receiver 註冊。
    pub(crate) async fn begin_qr_recovery(&self, provider: &str, rollcall_id: u64, account_id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        let Some(entry) = inner.get_mut(&PendingQrKey::new(provider, rollcall_id)) else { return false; };
        if !entry.accounts.contains_key(account_id) { return false; }
        let Some(health) = entry.watchdogs.get_mut(account_id) else { return false; };
        let started = health.begin_recovery(Instant::now());
        if started {
            info!(provider = %provider, rollcall_id, account = %account_id,
                recovery_count = health.recoveries, "QR watchdog 啟動局部恢復");
        }
        started
    }

    /// Mark one account as successfully attended for scanner status display.
    pub async fn mark_completed(&self, provider: &str, rollcall_id: u64, account_id: &str) {
        let key = PendingQrKey::new(provider, rollcall_id);
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.get_mut(&key) {
            entry.accounts.remove(account_id);
            entry.all_accounts.insert(account_id.to_string());
            entry.completed_accounts.insert(account_id.to_string());
            entry.resume.errors.remove(account_id);
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

        let last_error = entry.resume.errors.iter().next().map(|(id, error)| format!("{id}：{error}"))
            .or_else(|| if !entry.resume.requested.is_empty() { Some(format!("正在重新核對{}個帳號；尚未確認完成，請保持最新QR在鏡頭內", entry.resume.requested.len())) } else { None })
            .or_else(|| pending_accounts.iter().find_map(|account_id| {
            entry.watchdogs.get(account_id).and_then(|health| health.notice.as_ref())
                .map(|reason| format!("{account_id}：{reason}"))
        }).or_else(|| pending_accounts.iter().find_map(|account_id| {
            entry.accounts.get(account_id).and_then(|account| {
                account.last_error.as_ref().map(|reason| format!("{account_id}：{reason}"))
            })
        })));
        let mut result = ScannerStatusResult {
            qr_recovery: entry.resume.status(),
            total_count: all_accounts.len(),
            completed_count: completed_accounts.len(),
            pending_count: pending_accounts.len(),
            completed_accounts,
            pending_accounts,
            all_accounts,
            last_error,
            expires_in_secs: secs_until(entry.expires_at, now),
            submitted_code: None,
            class_progress: None,
            auto_submit_percent: AUTO_SUBMIT_PERCENT,
        };
        // CLASS_COUNT_ON_PAGES_V1：先把這一格的鎖放掉再讀快取（兩個 mutex 不重疊持有）。
        drop(inner);
        // QR_RECOVERY_V1：狀態輪詢只讀監控流程已更新的班級快取，不等待學校網路。
        // 過期時暫不顯示人數，也不讓每1.5秒的頁面輪詢額外打學校API。
        let progress_key = PendingNumberKey::new(provider, query.rollcall_id);
        let cache = self.progress_cache.lock().await;
        result.class_progress = cache.get(&progress_key)
            .filter(|(at, _, _)| Instant::now().saturating_duration_since(*at) < CLASS_PROGRESS_TTL)
            .and_then(|(_, progress, _)| progress.clone());
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
                token: scanner_secure_token(),
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

    /// 檢查 QR 點名等待群組中所有註冊帳號是否均已完成簽到。
    pub async fn is_qr_group_completed(&self, provider: &str, rollcall_id: u64) -> bool {
        let key = PendingQrKey::new(provider, rollcall_id);
        let inner = self.inner.lock().await;
        if let Some(entry) = inner.get(&key) {
            !entry.all_accounts.is_empty() && entry.accounts.is_empty()
        } else {
            false
        }
    }

    /// 檢查數字點名等待群組中所有註冊帳號是否均已完成簽到。
    pub async fn is_number_group_completed(&self, provider: &str, rollcall_id: u64) -> bool {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let inner = self.number_inner.lock().await;
        if let Some(entry) = inner.get(&key) {
            !entry.all_accounts.is_empty() && entry.accounts.is_empty()
        } else {
            false
        }
    }

    /// H2：本帳號是否已在這個 QR 點名中簽到成功（只看 `completed_accounts`）。
    /// 「群組沒有等待中的帳號」不代表本帳號成功，早退前應以此判斷。
    pub async fn is_qr_account_completed(&self, provider: &str, rollcall_id: u64, account_id: &str) -> bool {
        let key = PendingQrKey::new(provider, rollcall_id);
        let inner = self.inner.lock().await;
        inner
            .get(&key)
            .is_some_and(|entry| entry.completed_accounts.contains(account_id))
    }

    /// H2：本帳號是否已在這個數字點名中簽到成功（只看 `completed_accounts`）。
    pub async fn is_number_account_completed(&self, provider: &str, rollcall_id: u64, account_id: &str) -> bool {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let inner = self.number_inner.lock().await;
        inner
            .get(&key)
            .is_some_and(|entry| entry.completed_accounts.contains(account_id))
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
        let (account_senders, previous_state) = {
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
            // M3：記下舊狀態；一個都沒送到時還原。
            let previous_state = (entry.submitted_input.clone(), entry.last_error.clone());
            entry.submitted_input = Some(input.clone());
            entry.last_error = None;

            let senders = entry
                .accounts
                .iter()
                .map(|(account_id, account)| (account_id.clone(), account.tx.clone()))
                .collect::<Vec<_>>();
            (senders, previous_state)
        };

        let mut delivered_accounts = Vec::new();
        let mut failed_accounts = Vec::new();
        for (account_id, tx) in account_senders {
            if queue_for_account(&tx, input.clone(), &account_id) {
                delivered_accounts.push(account_id);
            } else {
                warn!(account_id = %account_id, "數字點名頁傳送輸入失敗（帳號已結束）");
                failed_accounts.push(account_id);
            }
        }

        if delivered_accounts.is_empty() {
            let mut inner = self.number_inner.lock().await;
            if let Some(entry) = inner.get_mut(&key) {
                entry.submitted_input = previous_state.0;
                entry.last_error = previous_state.1;
            }
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
                token: scanner_secure_token(),
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
            cache.retain(|_, (at, _, _)| now.saturating_duration_since(*at) < CLASS_PROGRESS_CACHE_TTL);
            if let Some((at, progress, stored_api)) = cache.get_mut(&key) {
                *stored_api = Some(api.clone());
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
        cache.insert(key, (Instant::now(), progress.clone(), Some(api.clone())));
        progress
    }

    /// CLASS_COUNT_LIVE_V1：給點名頁的班上進度。
    ///
    /// 頁面每一兩秒輪詢一次狀態。快取仍新鮮（[`CLASS_PROGRESS_TTL`] 內）就只讀快取；
    /// 過期且監控流程已經把該場的 `ApiClient` 留下來時，沿用同一組 session 重讀學校
    /// （瀏覽器仍然不打學校 API，符合「不得從不同 IP 送出」）。
    /// 還沒有任何監控流程讀過 → `None`（頁面顯示讀取中）。
    pub async fn cached_class_progress(
        &self,
        provider: &str,
        rollcall_id: u64,
    ) -> Option<RollcallProgress> {
        let key = PendingNumberKey::new(provider, rollcall_id);
        let api = {
            let cache = self.progress_cache.lock().await;
            cache.get(&key).and_then(|(_, _, api)| api.clone())
        };
        if let Some(api) = api {
            return self.class_progress(&api, provider, rollcall_id).await;
        }
        let now = Instant::now();
        let cache = self.progress_cache.lock().await;
        match cache.get(&key) {
            Some((at, progress, _))
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
            // M1：與 mark_number_completed 一致——完成後移出待點名清單，不再被列為待點名或收到之後的座標。
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
            // RADAR_DEDUCE_TRACE_V1（fix75）：推估過程記在 rollcalls/radar.rs 的模組狀態裡
            //（監控流程與這個 HTTP 服務是同一個行程），這裡照實帶出去給頁面畫。
            probe: crate::rollcalls::radar::probe_trace(provider, query.rollcall_id),
            // RADAR_SCHOOL_BLOCK_V1（fix77）：學校端擋下（429／403）時的停手狀態
            //（停手時間過了自己會消失，不會卡住整場點名）。
            school_block: crate::rollcalls::radar::school_block(provider, query.rollcall_id),
        };
        // CLASS_COUNT_ON_PAGES_V1：這一場的處理流程還沒回報過進度時，退回共用快取
        // （多帳號只會有其中一個帳號的流程在跑，頁面照樣看得到數字）。
        drop(inner);
        // CLASS_COUNT_LIVE_V1：雷達演算期間 entry.class_progress 不會更新；
        // 一律以共用快取（過期會重讀學校）為準，上方人數條才會跟著動。
        if let Some(live) = self.cached_class_progress(provider, query.rollcall_id).await {
            result.class_progress = Some(live);
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
        // H3（RADAR_BROADCAST_UNLOCKED_V1）：鎖內只挑出目標並複製 Sender，送出在鎖外進行。
        // 舊寫法在鎖內 `send().await`，一個滿載的帳號會卡住整個雷達註冊表（其他點名與狀態頁）。
        let plans: Vec<(u64, String, Vec<(String, tokio::sync::mpsc::Sender<RadarPointInput>)>)> = {
            let mut inner = self.radar_inner.lock().await;
            cleanup_expired_radar_locked(&mut inner, now);
            let mut plans = Vec::new();
            for (key, entry) in inner.iter() {
                if key.provider != provider {
                    continue;
                }
                if let Some(id) = rollcall_id {
                    if key.rollcall_id != id {
                        continue;
                    }
                }
                let senders = entry
                    .accounts
                    .iter()
                    .map(|(account_id, account)| (account_id.clone(), account.tx.clone()))
                    .collect::<Vec<_>>();
                plans.push((key.rollcall_id, entry.course_name.clone(), senders));
            }
            plans
        };

        let mut targets: Vec<RadarBroadcastTarget> = Vec::new();
        let mut delivered_total = 0usize;
        let mut delivered_rollcalls: Vec<u64> = Vec::new();
        for (target_rollcall_id, course_name, senders) in plans {
            let mut delivered = 0usize;
            for (account_id, tx) in senders {
                if queue_for_account(&tx, point, &account_id) {
                    delivered += 1;
                } else {
                    warn!(account_id = %account_id, "面板送出雷達座標失敗（帳號已結束）");
                }
            }
            if delivered > 0 {
                delivered_rollcalls.push(target_rollcall_id);
            }
            delivered_total += delivered;
            targets.push(RadarBroadcastTarget {
                rollcall_id: target_rollcall_id,
                course_name,
                delivered_count: delivered,
            });
        }
        targets.sort_by_key(|target| target.rollcall_id);

        // M3：只有真的送到的點名才記為「已送出的座標」。
        if !delivered_rollcalls.is_empty() {
            let mut inner = self.radar_inner.lock().await;
            for target_rollcall_id in delivered_rollcalls {
                if let Some(entry) = inner.get_mut(&PendingNumberKey::new(provider, target_rollcall_id)) {
                    entry.submitted_point = Some(payload.clone());
                    entry.last_error = None;
                }
            }
        }

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
        let (account_senders, previous_state) = {
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
            // M3：記下舊狀態；一個都沒送到時還原。
            let previous_state = (entry.submitted_point.clone(), entry.last_error.clone());
            entry.submitted_point = Some(RadarPointPayload::from_input(&point));
            entry.last_error = None;

            let senders = entry
                .accounts
                .iter()
                .map(|(account_id, account)| (account_id.clone(), account.tx.clone()))
                .collect::<Vec<_>>();
            (senders, previous_state)
        };

        let mut delivered_accounts = Vec::new();
        let mut failed_accounts = Vec::new();
        for (account_id, tx) in account_senders {
            if queue_for_account(&tx, point, &account_id) {
                delivered_accounts.push(account_id);
            } else {
                warn!(account_id = %account_id, "雷達點名頁傳送座標失敗（帳號已結束）");
                failed_accounts.push(account_id);
            }
        }

        if delivered_accounts.is_empty() {
            let mut inner = self.radar_inner.lock().await;
            if let Some(entry) = inner.get_mut(&key) {
                entry.submitted_point = previous_state.0;
                entry.last_error = previous_state.1;
            }
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
        let (account_senders, previous_state, submission_revision) = {
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
            // QR_RECOVERY_V1：連結持有人先完成後，仍可替同場尚未完成者補碼。
            // 不接受陌生／僅被取消的帳號；token、provider、rollcall 邊界保持不變。
            let known_scanner = entry.accounts.contains_key(account_id)
                || entry.completed_accounts.contains(account_id);
            if !known_scanner || entry.accounts.is_empty() {
                return Err(ScannerError::AccountNotPending);
            }

            // 同一個 scanner token 在過期前允許重複送出。
            // 課堂 QR Code 可能會輪換，scanner 頁面會持續掃描並把新的 payload
            // fan-out 給仍在等待的帳號；流程成功的帳號會自行 unregister。
            // M3：記下舊狀態；若下面一個帳號都沒送到，會還原（避免晚到帳號收到從未送達的 payload）。
            let previous_state = (entry.used, entry.submitted_qr_data.clone(), entry.submitted_qr_at);
            entry.submission_revision = entry.submission_revision.wrapping_add(1);
            entry.used = true;
            entry.submitted_qr_data = Some(qr_data.to_string());
            entry.submitted_qr_at = Some(now);
            let senders = entry
                .accounts
                .iter()
                .map(|(account_id, account)| {
                    (account_id.clone(), account.tx.clone(), Arc::clone(&account.lane))
                })
                .collect::<Vec<_>>();
            // 即使 channel 已關閉／滿載，也要記下「確實有人重新掃」供外層偵測。
            for (id, _, _) in &senders {
                if let Some(health) = entry.watchdogs.get_mut(id) { health.enqueued(now); }
            }
            (senders, previous_state, entry.submission_revision)
        };

        // QR_FANOUT_NONBLOCKING_V1：逐一派送時不能在這裡 `send().await`。每個帳號的 QR 佇列只有 4 格，
        // 而帳號處理一筆 payload 前要先等「輪到自己」的群組排程（人數多時可達十幾秒），期間掃碼頁每掃一次
        // 就多一筆。舊寫法遇到滿佇列會卡住這個 HTTP 請求，排在後面的帳號收不到新 payload，掃碼頁逾時重試
        // 又會疊加；症狀就是「QR 仍有效，卻有人一直停在未完成」。現在：有空位立即送；佇列滿時改由背景
        // 任務等待送出，不擋住回應與其他帳號。已知取捨：佇列滿時，背景送出的舊 payload 可能比之後直送的新
        // payload 晚到；帳號端取佇列中最新的一筆，下一次掃碼會自然校正。
        let mut delivered_accounts = Vec::new();
        let mut failed_accounts = Vec::new();
        for (account_id, tx, lane) in account_senders {
            if queue_qr_ordered(&lane, &tx, qr_data.to_string(), &account_id) {
                delivered_accounts.push(account_id);
            } else {
                warn!(account_id = %account_id, "QR scanner callback 傳送失敗（帳號已結束）");
                failed_accounts.push(account_id);
            }
        }

        if delivered_accounts.is_empty() {
            let mut inner = self.inner.lock().await;
            if let Some(entry) = inner.get_mut(&key) {
                // QR_LATEST_ROLLBACK_V1: an older failed delivery cannot overwrite a
                // newer QR or restore a cache explicitly cleared by camera recovery.
                if entry.token == token && entry.submission_revision == submission_revision {
                    entry.used = previous_state.0;
                    entry.submitted_qr_data = previous_state.1;
                    entry.submitted_qr_at = previous_state.2;
                }
            }
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
        // SCANNER_TOKEN_FRAGMENT_V1：token 放 fragment。瀏覽器不會把 `#` 後面
        // 送到伺服器，access log／Referer 都看不到；舊連結 `?token=` 前端仍接受。
        base.query_pairs_mut()
            .clear()
            .append_pair("provider", provider)
            .append_pair("rollcall_id", &rollcall_id.to_string())
            .append_pair("account_id", account_id);
        base.set_fragment(Some(&format!("token={token}")));
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

/// TOOL_PAGE_SECURITY_HEADERS_V1：公開 `/scanner`／`/number`／`/radar` 不走
/// `admin_security_headers_middleware`（兩份路由各自 merge 面板）。原本 HTML
/// 只送 `Cache-Control: no-store`，沒有 nosniff／XFO／HSTS／Permissions-Policy。
///
/// **不加 CSP**：`scanner.html` 有大段 inline script、zxing／wechat／ONNX WASM、
/// `new Worker(..., {type:"module"})`。面板那套 `require-trusted-types-for`
/// 會讓相機頁白屏。相機只在 `/scanner` 開、定位只在 `/radar` 開。
// TOOL_PAGE_CSP_V1（fix135）：ZAP DAST 掃描指出公開工具頁沒有任何 CSP。
// 面板主 CSP 的 require-trusted-types-for 會讓相機/解碼頁整頁白屏（見下方
// TOOL_PAGE_SECURITY_HEADERS_V1 原註），因此改送「工具頁專用 CSP」：
// 只允許同源資源＋必要的 inline script/style＋WASM eval＋Worker blob，
// 但 object-src 'none'、base-uri 'none'、frame-ancestors 'none'、
// connect-src 'self' —— 外部腳本、外嵌、站外傳輸依舊全數攔下。
const TOOL_PAGE_CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self'; worker-src 'self' blob:; child-src 'self' blob:; manifest-src 'self'; font-src 'self' data:; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

pub(crate) fn tool_permissions_policy_for_path(path: &str) -> &'static str {
    const SCANNER: &str = "camera=(self), geolocation=(), microphone=(), payment=(), usb=(), midi=(), bluetooth=(), display-capture=()";
    const RADAR: &str = "camera=(), geolocation=(self), microphone=(), payment=(), usb=(), midi=(), bluetooth=(), display-capture=()";
    const DENY: &str = "camera=(), geolocation=(), microphone=(), payment=(), usb=(), midi=(), bluetooth=(), display-capture=()";
    if path == "/scanner" || path.starts_with("/scanner/") {
        SCANNER
    } else if path == "/radar" || path.starts_with("/radar/") {
        RADAR
    } else {
        DENY
    }
}

// TOOL_RATE_LIMIT_V1：公開掃碼／報數／雷達點名頁與 LINE webhook，是獨立於
// admin_web 的另一個 Axum server（見 `main.rs` 的 `start_webhook_server` /
// `start_scanner_server_with_admin`），不會套到 admin_web 那邊的
// `waf::waf_firewall_middleware`（全站 L7 流量限制）——本來就是兩個分開的
// 中介層堆疊，`waf` 模組本身也沒有 `pub`，這裡沒辦法直接共用它的限速器型別。
// 這裡補一層最基本、只攔「明顯洪水」的防護，刻意不是拿來卡正常教室用量：
// 教室 wifi／電信 NAT 背後常常是幾十個學生共用同一個對外 IP，`/scanner/status`
// 這類狀態輪詢端點本來就是前端每 1.5 秒打一次（見 `scanner.html`），50 人的
// 班級尖峰同時在線輪詢，換算下來這個 IP 一分鐘就有兩千多次請求——門檻訂得
// 跟 admin_web 的 WAF（240 次/分鐘，給的是單一登入帳號用的面板）一樣低，
// 反而會把整班學生一起擋在外面。這裡抓一個遠高於正常教室尖峰用量、但仍能
// 攔住真正惡意洪水／壓力測試等級流量的門檻。
const TOOL_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
const TOOL_RATE_LIMIT_MAX_REQUESTS: usize = 4000;
const TOOL_RATE_LIMIT_MAX_IPS: usize = 20_000;

struct ToolRateLimiter {
    attempts: Mutex<HashMap<String, Vec<Instant>>>,
}

impl ToolRateLimiter {
    fn new() -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
        }
    }

    /// 回傳 `true` 表示放行；`false` 表示這個 IP 在時間窗內已經超過上限。
    async fn check_limit(&self, ip: &str) -> bool {
        let mut map = self.attempts.lock().await;
        let now = Instant::now();
        let window_start = now.checked_sub(TOOL_RATE_LIMIT_WINDOW).unwrap_or(now);

        let entry = map.entry(ip.to_string()).or_default();
        entry.retain(|t| *t > window_start);

        if entry.len() >= TOOL_RATE_LIMIT_MAX_REQUESTS {
            return false;
        }

        entry.push(now);

        // 定期或容量滿時清理過期 IP，避免長時間運作下 HashMap 無限長大。
        if map.len() > TOOL_RATE_LIMIT_MAX_IPS {
            map.retain(|_, v| {
                v.retain(|t| *t > window_start);
                !v.is_empty()
            });
        }
        true
    }
}

static TOOL_RATE_LIMITER: once_cell::sync::Lazy<ToolRateLimiter> =
    once_cell::sync::Lazy::new(ToolRateLimiter::new);

/// TOOL_RATE_LIMIT_V1 中介層：套在 `/scanner`、`/number`、`/radar`、LINE webhook
/// 這些公開路由上（見 `build_scanner_router_with_admin` 與
/// `line::webhook::build_router`），不套用在合併進來的 `admin_web` 路由（那邊已經
/// 有自己的 WAF）。用 [`crate::trusted_ip::ClientIp`] 取得真實來源 IP——跟 WAF
/// 共用同一份信任代理清單設定，行為一致。
pub(crate) async fn tool_rate_limit_middleware(
    crate::trusted_ip::ClientIp(ip): crate::trusted_ip::ClientIp,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !TOOL_RATE_LIMITER.check_limit(&ip).await {
        warn!(
            ip = %ip,
            path = %request.uri().path(),
            "🛡️ 公開掃碼／報數／雷達／LINE webhook 伺服器：單一 IP 短時間請求量過高，暫時擋下"
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "error": "too many requests" })),
        )
            .into_response();
    }
    next.run(request).await
}

pub(crate) async fn tool_security_headers_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path().to_string();
    let permissions_policy = tool_permissions_policy_for_path(&path);
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    // TOOL_PAGE_CSP_V1（fix135）
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(TOOL_PAGE_CSP),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static(permissions_policy),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("strict-transport-security"),
        HeaderValue::from_static(HSTS_HEADER_VALUE),
    );
    response
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
        // QR_ZXING_V1（fix86）：zxing-wasm（C++ 編成 WASM 的解碼器）與它的 wasm 檔。
        .route("/scanner/zxing.js", get(scanner_zxing_js_handler))
        .route("/scanner/zxing_reader.wasm", get(scanner_zxing_wasm_handler))
        // QR_WECHAT_V1（fix87）：微信 QRCode 引擎（OpenCV wechat_qrcode 的 WASM 版）。
        .route("/scanner/wechat/index.mjs", get(scanner_wechat_index_handler))
        .route("/scanner/wechat/wasm.mjs", get(scanner_wechat_wasm_handler))
        .route("/scanner/sr/worker.mjs", get(scanner_sr_worker_handler))
        .route("/scanner/sr/ort.mjs", get(scanner_sr_ort_handler))
        .route("/scanner/sr/ort-wasm-simd.wasm", get(scanner_sr_wasm_handler))
        .route("/scanner/sr/model.onnx", get(scanner_sr_model_handler))
        .route("/scanner/status", get(scanner_status_handler))
        .route("/scanner/submit", post(scanner_submit_handler))
        .route("/scanner/resume", post(scanner_resume_handler))
        .route("/number", get(number_page_handler))
        .route("/number/status", get(number_status_handler))
        .route("/number/submit", post(number_submit_handler))
        // RADAR_INPUT_PAGE_V1（fix71）：雷達點名輸入頁（輸入座標）。
        .route("/radar", get(radar_page_handler))
        .route("/radar/status", get(radar_status_handler))
        .route("/radar/submit", post(radar_submit_handler))
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn(tool_security_headers_middleware))
        // SCANNER_PAGE_COMPRESSION_V1：scanner.html／number.html／radar.html 都是
        // 整份內嵌 script/CSS 的純文字頁面（scanner.html 本身就有 ~260KB），
        // brotli/gzip 可以把這種文字壓到原本的一到兩成；admin_web 的
        // dashboard.html（同樣 ~260KB）已經證實這招有效（見
        // `admin_web::build_admin_router` 裡 CompressionLayer 的註解）。這裡補上
        // 同一招——掃碼點名的手機連線常常是教室 wifi／行動網路，省下來的位元組
        // 直接換成「開頁到能掃」的秒數。status/submit 的 JSON 回應也一併受惠。
        .layer(
            tower_http::compression::CompressionLayer::new()
                .br(true)
                .gzip(true)
                .deflate(true),
        )
        // SCANNER_NO_COMPRESSION_BINARY_V1：上面那層壓縮預設「什麼都壓」，但
        // zxing／微信／AI 超解析度用的 wasm、onnx 資產本來就是幾 MB 等級的已編譯
        // 二進位（不像 HTML/JS 那種純文字好壓），每個請求重新跑一次 brotli/gzip
        // 省下的位元組有限，CPU 卻要多花好幾百毫秒；教室尖峰時間（一整班同時點開
        // 「深度掃描」）反而會變成新的效能瓶頸——跟 admin_web 的
        // `STREAM_NO_COMPRESSION_V1`（教材檔案）是同一個道理。沿用同一招：在壓縮層
        // 外面（更晚加的 `.layer()` 會包在外層，先看到請求）把這幾條路徑的
        // Accept-Encoding 拔掉，壓縮層協商不到編碼就會原樣放行位元組。
        .layer(axum::middleware::from_fn(
            no_compression_for_scanner_binary_assets_middleware,
        ))
        // TOOL_RATE_LIMIT_V1：刻意放最外層（最後加的 `.layer()` 最先看到請求），
        // 真正洪水等級的流量在壓縮、標頭處理之前就擋掉。
        .layer(axum::middleware::from_fn(tool_rate_limit_middleware))
        .with_state(ScannerHttpState::new(scanner));

    if let Some(admin_web) = admin_web {
        app = app.merge(admin_web::build_admin_router(admin_web));
    }

    app
}

/// SCANNER_NO_COMPRESSION_BINARY_V1：這幾條路徑回的是已編譯的 wasm／onnx 二進位，
/// 都是 MB 等級（953KB～10.5MB），即時 brotli/gzip 壓縮省下的位元組有限、CPU 成本卻
/// 不小。`/scanner/sr/ort.mjs`、`/scanner/wechat/index.mjs`、`/scanner/sr/worker.mjs`
/// 是純文字 JS（分別約 145KB／1.8KB／4KB），不在這裡排除，照常給壓縮層處理。
fn is_no_compression_scanner_asset_path(path: &str) -> bool {
    matches!(
        path,
        "/scanner/zxing_reader.wasm"
            | "/scanner/wechat/wasm.mjs"
            | "/scanner/sr/ort-wasm-simd.wasm"
            | "/scanner/sr/model.onnx"
    )
}

async fn no_compression_for_scanner_binary_assets_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !is_no_compression_scanner_asset_path(request.uri().path()) {
        return next.run(request).await;
    }
    // 拿掉 accept-encoding：壓縮層協商不到編碼，就會原樣傳輸位元組（跟
    // admin_web 的 `no_compression_for_file_streams_middleware` 同一招）。
    let (mut parts, body) = request.into_parts();
    parts.headers.remove(header::ACCEPT_ENCODING);
    next.run(axum::extract::Request::from_parts(parts, body)).await
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

/// ADMIN_ALWAYS_SERVED_V1：沒有 LINE webhook、也沒有掃碼伺服器時（例如未設定 public_base_url），
/// Admin 後台仍必須有 HTTP 監聽；否則管理員連後台都進不去、無法修正帳號問題。
/// 只掛 admin_web 路由（與掃碼伺服器合併時用的是同一個 `build_admin_router`）。
pub async fn start_admin_only_server(admin_web: AdminWebState, bind_addr: &str, port: u16) -> Result<()> {
    let app = admin_web::build_admin_router(admin_web);
    let ip = bind_addr
        .trim()
        .parse::<std::net::IpAddr>()
        .into_diagnostic()
        .wrap_err_with(|| format!("無效的 admin bind address: `{bind_addr}`"))?;
    let addr = std::net::SocketAddr::new(ip, port);

    info!(bind_addr = %bind_addr, port = port, "Admin 後台伺服器（獨立模式）啟動：http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to bind admin server to port {port}"))?;

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .into_diagnostic()
    .wrap_err("Admin server error")
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

/// QR_ZXING_V1（fix86）：本機供應 zxing-wasm（zxing-cpp 編成 WebAssembly 的解碼器）。
///
/// 為什麼要加：實測「手機拍螢幕」情境（4K 擷取、失焦、摩爾紋、手震），
///   jsQR 單框 0/5、zxing-wasm 單框 5/5，而且同一張圖 jsQR 55ms、zxing 4ms。
/// 兩個檔案都由本機供應（不連 CDN）：
///   `line/zxing.js`           —— 官方 iife/reader 建置（已把內建的 jsdelivr 備援改成
///                                永遠指向本機 `/scanner/zxing_reader.wasm`）。
///   `line/zxing_reader.wasm`  —— 官方 3.1.4 產物，sha256
///                                e8af31edb56d0522f4de74495839385ef019ba8bc90d38e5ecb2f18795d86fb2
///                                （與 zxing.js 內宣告的 ZXING_WASM_SHA256 相同）。
async fn scanner_zxing_js_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("line/zxing.js"),
    )
        .into_response()
}

/// QR_WECHAT_V1（fix87）：微信 QRCode 引擎（OpenCV wechat_qrcode，模型與 wasm 都內嵌在
/// `wasm.mjs` 一個檔案裡，總共約 6MB）。公開 benchmark 的「拍螢幕（monitor）」類命中率 94%，
/// 是我們手上最強的一顆（實測 QR 120px＋失焦只有它解得動）。
/// 這兩個檔案都只在「深度掃描」時才下載（見 scanner.html），平常不會拖慢開頁速度。
async fn scanner_wechat_index_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("line/wechat/index.mjs"),
    )
        .into_response()
}

async fn scanner_wechat_wasm_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("line/wechat/wasm.mjs"),
    )
        .into_response()
}

/// QR_AI_SR_V1（fix93）：AI 超分救援的資產（只在使用者「深度掃描一直解不到」時才會下載）。
///
/// 為什麼要加：實測（見 fix93 報告）「差一點點」的案例（QR 每模組約 3.6px，三個引擎原本全 ❌），
///   用 Swin2SR-lightweight ×2 超分之後 zxing 與微信**都解得出來**；但實拍投影幕照片
///   （訊號已低於可解極限）救不回 —— 所以設計成「最後一擊」而不是每帧跑。
/// 四個檔案都由本機供應（不連 CDN）：
///   `line/sr/worker.mjs`            —— worker 橋接（onnxruntime-web 推論，背景執行緒）
///   `line/sr/ort.mjs`               —— onnxruntime-web 1.17.3 的 WASM-only ESM 建置（單執行緒、
///                                      不需要 SharedArrayBuffer，部署端不必加 COOP/COEP）
///   `line/sr/ort-wasm-simd.wasm`    —— ORT 的 WASM 執行檔（約 10MB）
///   `line/sr/model.onnx`            —— Swin2SR-lightweight ×2（uint8 量化，約 6.8MB；Apache-2.0）
async fn scanner_sr_worker_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("line/sr/worker.mjs"),
    )
        .into_response()
}

async fn scanner_sr_ort_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("line/sr/ort.mjs"),
    )
        .into_response()
}

async fn scanner_sr_wasm_handler() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/wasm"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_bytes!("line/sr/ort-wasm-simd.wasm").as_slice(),
    )
        .into_response()
}

async fn scanner_sr_model_handler() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_bytes!("line/sr/model.onnx").as_slice(),
    )
        .into_response()
}

async fn scanner_zxing_wasm_handler() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/wasm"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_bytes!("line/zxing_reader.wasm").as_slice(),
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
            Json(serde_json::json!({ "error": e.to_string(), "code": e.code() })),
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
            Json(serde_json::json!({ "error": e.to_string(), "code": e.code() })),
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
            Json(serde_json::json!({ "error": e.to_string(), "code": e.code() })),
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
            Json(serde_json::json!({ "error": e.to_string(), "code": e.code() })),
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
            Json(serde_json::json!({ "error": e.to_string(), "code": e.code() })),
        )
            .into_response(),
    }
}

async fn scanner_resume_handler(
    State(state): State<ScannerHttpState>,
    Json(query): Json<ScannerStatusQuery>,
) -> Response {
    match state.scanner.request_qr_resume(query).await {
        Ok(result) => (StatusCode::ACCEPTED, Json(serde_json::json!(result))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e.to_string(), "code": e.code() }))).into_response(),
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
            Json(serde_json::json!({ "error": e.to_string(), "code": e.code() })),
        )
            .into_response(),
    }
}

/// QR_ORDERED_LANE_V1：單一 QR 帳號的有序送出佇列狀態。
///
/// 通道滿載時，新的 payload 不能各自 spawn（那樣會亂序：較舊的碼可能晚到，
/// 讓 handler 拿舊碼去學校送出，學校回過期，掃碼頁顯示錯誤）。
/// 改成：pending 依序排隊，由同一個 pump 任務逐筆送出；pending 非空時，
/// 之後的 payload 一律排在後面，不會搶先。
#[derive(Default)]
struct QrLane {
    /// 等待送出的 payload。隊首是發送中的那一筆，送出成功後才移除。
    pending: VecDeque<String>,
    /// 是否已有 pump 任務在跑（同一帳號只會有一個）。
    pumping: bool,
}

fn lock_lane(lane: &std::sync::Mutex<QrLane>) -> std::sync::MutexGuard<'_, QrLane> {
    lane.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// QR 專用的有序派送。回傳 false 代表帳號通道已關閉（帳號已結束）。
fn queue_qr_ordered(
    lane: &Arc<std::sync::Mutex<QrLane>>,
    tx: &QrCodeSender,
    value: String,
    account_id: &str,
) -> bool {
    if tx.is_closed() { return false; }
    let mut state = lock_lane(lane);
    if state.pending.is_empty() {
        match tx.try_send(value) {
            Ok(()) => return true,
            Err(TrySendError::Full(value)) => state.pending.push_back(value),
            Err(TrySendError::Closed(_)) => return false,
        }
    } else {
        state.pending.push_back(value);
    }
    if !state.pumping {
        state.pumping = true;
        tokio::spawn(qr_lane_pump(Arc::clone(lane), tx.clone(), account_id.to_string()));
    }
    true
}

/// 依序把 pending 送進通道；每筆送出成功後才從隊首移除。
/// 鎖不跨越 await（只在進出時短暫持有）。
async fn qr_lane_pump(lane: Arc<std::sync::Mutex<QrLane>>, tx: QrCodeSender, account_id: String) {
    loop {
        let front = {
            let mut state = lock_lane(&lane);
            let front = state.pending.front().cloned();
            if front.is_none() {
                state.pumping = false;
            }
            front
        };
        let Some(value) = front else {
            return;
        };
        if tx.send(value).await.is_err() {
            let mut state = lock_lane(&lane);
            state.pending.clear();
            state.pumping = false;
            warn!(account_id = %account_id, "QR scanner callback 背景送出失敗（帳號已結束）");
            return;
        }
        lock_lane(&lane).pending.pop_front();
    }
}

/// QR_FANOUT_NONBLOCKING_V1 / M2：共用的非阻塞派送 helper。
///
/// - `try_send` 成功 → true。
/// - 通道滿載 → clone 一份 Sender 交給背景任務送出，呼叫端不等待（回 true，表示已交給送出管道）。
/// - 通道已關閉（帳號已結束）→ false。
///
/// 不可在註冊表鎖內或 handler 的關鍵路徑上 `send().await`：一個滿載的帳號會把整個註冊表卡住。
fn queue_for_account<T: Send + 'static>(
    tx: &tokio::sync::mpsc::Sender<T>,
    value: T,
    account_id: &str,
) -> bool {
    match tx.try_send(value) {
        Ok(()) => true,
        Err(TrySendError::Full(value)) => {
            let tx = tx.clone();
            let account_id = account_id.to_string();
            tokio::spawn(async move {
                if tx.send(value).await.is_err() {
                    warn!(account_id = %account_id, "背景送出掃碼資料失敗（帳號已結束）");
                }
            });
            true
        }
        Err(TrySendError::Closed(_)) => false,
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
        qr_recovery: QrRecoveryStatus::default(),
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
    /// RADAR_DEDUCE_TRACE_V1（fix75）：系統「怎麼推估教室位置」的過程
    /// （量測圓／兩圓交會候選／最小平方推估點／棋盤格掃描點；沒有紀錄時沒有這個欄位）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<crate::rollcalls::radar::RadarProbeTrace>,
    /// RADAR_SCHOOL_BLOCK_V1（fix77）：學校端正在擋我們（429 限流／403 禁止）時，
    /// 這裡帶出「停手到什麼時候」的資訊（頁面顯示紅字橫幅；沒被擋時沒有這個欄位）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub school_block: Option<crate::rollcalls::radar::RadarSchoolBlock>,
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
    #[serde(flatten)]
    pub qr_recovery: QrRecoveryStatus,
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

impl ScannerError {
    /// 機器可讀的錯誤代碼（HTTP 回應的 `code` 欄位）。掃碼頁據此區分「帳號已不需要掃碼」與真正的錯誤。
    pub fn code(&self) -> &'static str {
        match self {
            ScannerError::InvalidPublicBaseUrl => "invalid_public_base_url",
            ScannerError::InvalidSubmission(_) => "invalid_submission",
            ScannerError::PendingNotFound => "pending_not_found",
            ScannerError::Expired => "expired",
            ScannerError::TokenAlreadyUsed => "token_already_used",
            ScannerError::InvalidToken => "invalid_token",
            ScannerError::AccountNotPending => "account_not_pending",
            ScannerError::QrDataTooLarge { .. } => "qr_data_too_large",
            ScannerError::InvalidNumberCode => "invalid_number_code",
            ScannerError::InvalidRadarPoint => "invalid_radar_point",
            ScannerError::NoDeliveredAccounts => "no_delivered_accounts",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn registry() -> QrScannerRegistry {
        QrScannerRegistry::new("https://scanner.example.test/base")
    }

    #[test]
    fn tool_permissions_policy_is_path_specific() {
        let scanner = tool_permissions_policy_for_path("/scanner");
        assert!(scanner.contains("camera=(self)"));
        assert!(!scanner.contains("geolocation=(self)"));
        assert_eq!(
            tool_permissions_policy_for_path("/scanner/jsqr.js"),
            scanner
        );
        let radar = tool_permissions_policy_for_path("/radar");
        assert!(radar.contains("geolocation=(self)"));
        assert!(!radar.contains("camera=(self)"));
        let number = tool_permissions_policy_for_path("/number");
        assert!(!number.contains("camera=(self)"));
        assert!(!number.contains("geolocation=(self)"));
    }

    #[tokio::test]
    async fn reset_account_pending_clears_stale_submission_so_new_round_does_not_replay() {
        let registry = registry();
        let (tx_a, mut rx_a) = mpsc::channel(4);
        registry
            .register_pending("fju", 42, "acc-a", tx_a, Duration::from_secs(60))
            .await
            .unwrap();
        // 第一輪：掃到的 payload 已送出（used=true、submitted_qr_data 有值）。
        let token = registry.inner.lock().await[&PendingQrKey::new("fju", 42)]
            .token
            .clone();
        registry
            .submit(ScannerSubmission {
                provider: "fju".to_string(),
                rollcall_id: 42,
                account_id: "acc-a".to_string(),
                token: token.clone(),
                qr_data: "old-payload".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(rx_a.recv().await.as_deref(), Some("old-payload"));

        // 強制結束：清掉 acc-a 的登記與舊 payload。
        registry.reset_account_pending("fju", 42, "acc-a").await;

        // 新一輪重新登記：不應再拿到舊 payload。
        let (tx_a2, mut rx_a2) = mpsc::channel(4);
        let link = registry
            .register_pending("fju", 42, "acc-a", tx_a2, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(!link.already_submitted);
        assert!(rx_a2.try_recv().is_err());
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
        assert!(link.scan_url.contains("#token="));
        assert!(!link.scan_url.contains("?token="));
        assert!(!link.scan_url.contains("&token="));
        assert_eq!(link.token.len(), 64); // 32 bytes OS RNG 的 hex 長度（既有 token 策略不變）
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

    /// H2：被移出群組但從未簽到的帳號，不可因為「群組沒人等了」被判成功。
    #[tokio::test]
    async fn qr_account_completed_is_false_for_account_removed_without_signing() {
        let registry = registry();
        let (tx_a, _rx_a) = mpsc::channel(1);
        let (tx_b, _rx_b) = mpsc::channel(1);
        registry
            .register_pending("fju", 42, "acc-a", tx_a, Duration::from_secs(60))
            .await
            .unwrap();
        registry
            .register_pending("fju", 42, "acc-b", tx_b, Duration::from_secs(60))
            .await
            .unwrap();

        // acc-a 因致命錯誤被移出（從未簽到）；acc-b 簽到成功。
        registry.unregister_pending("fju", 42, "acc-a").await;
        registry.mark_completed("fju", 42, "acc-b").await;

        assert!(registry.is_qr_group_completed("fju", 42).await);
        assert!(!registry.is_qr_account_completed("fju", 42, "acc-a").await);
        assert!(registry.is_qr_account_completed("fju", 42, "acc-b").await);
    }

    /// M1：雷達完成後必須從待點名清單移出，並記入已完成。
    #[tokio::test]
    async fn radar_completed_account_is_removed_from_pending_accounts() {
        let registry = registry();
        let (tx_a, _rx_a) = mpsc::channel(1);
        let (tx_b, _rx_b) = mpsc::channel(1);
        registry
            .register_radar_pending("fju", 42, "acc-a", tx_a, Duration::from_secs(60), &[], "課程")
            .await
            .unwrap();
        registry
            .register_radar_pending("fju", 42, "acc-b", tx_b, Duration::from_secs(60), &[], "課程")
            .await
            .unwrap();

        registry.mark_radar_completed("fju", 42, "acc-a").await;

        let inner = registry.radar_inner.lock().await;
        let entry = inner.get(&PendingNumberKey::new("fju", 42)).unwrap();
        assert!(!entry.accounts.contains_key("acc-a"));
        assert!(entry.accounts.contains_key("acc-b"));
        assert!(entry.completed_accounts.contains("acc-a"));
    }

    /// QR_ORDERED_LANE_V1：通道滿載時，payload 仍必須依送出順序到達（不可亂序）。
    #[tokio::test]
    async fn qr_payloads_arrive_in_submission_order_when_channel_is_full() {
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(1);
        let link = registry
            .register_pending("fju", 42, "acc-a", tx, Duration::from_secs(60))
            .await
            .unwrap();

        for data in ["0~100!3~p1!4~42", "0~100!3~p2!4~42", "0~100!3~p3!4~42", "0~100!3~p4!4~42"] {
            registry
                .submit(ScannerSubmission {
                    provider: "fju".to_string(),
                    rollcall_id: 42,
                    account_id: "acc-a".to_string(),
                    token: link.token.clone(),
                    qr_data: data.to_string(),
                })
                .await
                .unwrap();
        }

        for expected in ["0~100!3~p1!4~42", "0~100!3~p2!4~42", "0~100!3~p3!4~42", "0~100!3~p4!4~42"] {
            let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("payload 應在 2 秒內到達")
                .expect("通道不應關閉");
            assert_eq!(got, expected);
        }
    }

    // QR_RECOVERY_V1：完成的連結持有人仍能補碼，但只發給同場 pending 帳號。
    #[tokio::test]
    async fn completed_scanner_owner_can_feed_only_remaining_accounts() {
        let registry = registry();
        let (tx_a, mut rx_a) = mpsc::channel(4);
        let (tx_b, mut rx_b) = mpsc::channel(4);
        let link = registry.register_pending("fju", 42, "a", tx_a, Duration::from_secs(60)).await.unwrap();
        registry.register_pending("fju", 42, "b", tx_b, Duration::from_secs(60)).await.unwrap();
        registry.mark_completed("fju", 42, "a").await;
        let submission = ScannerSubmission {
            provider: "fju".into(), rollcall_id: 42, account_id: "a".into(),
            token: link.token, qr_data: "fresh-qr".into(),
        };
        let mut unknown = submission.clone();
        unknown.account_id = "stranger".into();
        assert_eq!(registry.submit(unknown).await.unwrap_err(), ScannerError::AccountNotPending);
        let mut wrong_token = submission.clone();
        wrong_token.token = "wrong".into();
        assert_eq!(registry.submit(wrong_token).await.unwrap_err(), ScannerError::InvalidToken);
        let result = registry.submit(submission.clone()).await.unwrap();
        assert_eq!(result.delivered_accounts, vec!["b".to_string()]);
        assert_eq!(rx_b.recv().await.as_deref(), Some("fresh-qr"));
        assert!(rx_a.try_recv().is_err());
        registry.mark_completed("fju", 42, "b").await;
        assert_eq!(registry.submit(submission).await.unwrap_err(), ScannerError::AccountNotPending);
    }

    #[tokio::test]
    async fn qr_retry_registration_clears_completed_and_scopes_error_to_pending() {
        let registry = registry();
        let (tx, _rx) = mpsc::channel(4);
        let link = registry.register_pending("fju", 42, "a", tx, Duration::from_secs(60)).await.unwrap();
        registry.mark_completed("fju", 42, "a").await;
        let (tx_retry, _rx_retry) = mpsc::channel(4);
        registry.register_pending("fju", 42, "a", tx_retry, Duration::from_secs(60)).await.unwrap();
        assert!(!registry.is_qr_account_completed("fju", 42, "a").await);
        let query = || ScannerStatusQuery {
            provider: "fju".into(), rollcall_id: 42, account_id: "a".into(), token: link.token.clone(),
        };
        registry.set_qr_error("fju", 42, "a", Some("尚未確認，請續掃")).await;
        let status = registry.status(query()).await.unwrap();
        assert_eq!((status.pending_count, status.completed_count, status.total_count), (1, 0, 1));
        assert!(status.last_error.unwrap().contains("尚未確認"));
        registry.mark_completed("fju", 42, "a").await;
        let status = registry.status(query()).await.unwrap();
        assert_eq!((status.pending_count, status.completed_count), (0, 1));
        assert!(status.last_error.is_none());
    }

    #[tokio::test]
    async fn qr_registration_replay_does_not_wait_for_full_receiver() {
        let registry = registry();
        let (tx_a, mut rx_a) = mpsc::channel(4);
        let link = registry.register_pending("fju", 42, "a", tx_a, Duration::from_secs(60)).await.unwrap();
        registry.submit(ScannerSubmission {
            provider: "fju".into(), rollcall_id: 42, account_id: "a".into(),
            token: link.token.clone(), qr_data: "replay".into(),
        }).await.unwrap();
        assert_eq!(rx_a.recv().await.as_deref(), Some("replay"));
        let (tx_b, mut rx_b) = mpsc::channel(1);
        tx_b.try_send("queued-before-registration".to_string()).unwrap();
        let replay_link = tokio::time::timeout(
            Duration::from_secs(1),
            registry.register_pending("fju", 42, "b", tx_b, Duration::from_secs(60)),
        ).await.expect("登記不能等待滿通道").unwrap();
        assert!(replay_link.already_submitted);
        registry.submit(ScannerSubmission {
            provider: "fju".into(), rollcall_id: 42, account_id: "a".into(),
            token: link.token, qr_data: "newer".into(),
        }).await.unwrap();
        for expected in ["queued-before-registration", "replay", "newer"] {
            let value = tokio::time::timeout(Duration::from_secs(1), rx_b.recv()).await.unwrap();
            assert_eq!(value.as_deref(), Some(expected));
        }
    }

    #[tokio::test]
    async fn watchdog_rebind_preserves_token_other_accounts_and_budget() {
        let registry = registry();
        let (tx_a, rx_a) = mpsc::channel(4);
        let (tx_b, _rx_b) = mpsc::channel(4);
        let link = registry.register_pending("fju", 42, "a", tx_a, Duration::from_secs(60)).await.unwrap();
        registry.register_pending("fju", 42, "b", tx_b, Duration::from_secs(60)).await.unwrap();
        registry.submit(ScannerSubmission {
            provider: "fju".into(), rollcall_id: 42, account_id: "a".into(),
            token: link.token.clone(), qr_data: "fresh".into(),
        }).await.unwrap();
        registry.mark_completed("fju", 42, "b").await;
        drop(rx_a); // supervisor has dropped the previous worker's receiver first
        assert!(registry.begin_qr_recovery("fju", 42, "a").await);
        let (tx_retry, mut rx_retry) = mpsc::channel(4);
        let retry = registry.register_pending("fju", 42, "a", tx_retry, Duration::from_secs(60)).await.unwrap();
        assert_eq!(retry.token, link.token);
        assert_eq!(rx_retry.try_recv().unwrap(), "fresh");
        assert!(registry.is_qr_account_completed("fju", 42, "b").await);
        assert!(!registry.is_qr_account_completed("fju", 42, "a").await);
        assert!(!registry.begin_qr_recovery("fju", 42, "a").await); // cooldown survived rebind
        let status = registry.status(ScannerStatusQuery {
            provider: "fju".into(), rollcall_id: 42, account_id: "a".into(), token: link.token,
        }).await.unwrap();
        assert_eq!((status.pending_count, status.completed_count), (1, 1));
        assert!(status.last_error.unwrap().contains("自動重建"));
    }

    #[tokio::test]
    async fn watchdog_does_not_replay_expired_cached_qr() {
        let registry = registry();
        let (tx, _rx) = mpsc::channel(4);
        let link = registry.register_pending("fju", 42, "a", tx, Duration::from_secs(60)).await.unwrap();
        registry.submit(ScannerSubmission {
            provider: "fju".into(), rollcall_id: 42, account_id: "a".into(),
            token: link.token.clone(), qr_data: "old".into(),
        }).await.unwrap();
        registry.inner.lock().await.get_mut(&PendingQrKey::new("fju", 42)).unwrap()
            .submitted_qr_at = Some(Instant::now() - Duration::from_secs(60));
        let (tx_new, mut rx_new) = mpsc::channel(4);
        let next = registry.register_pending("fju", 42, "a", tx_new, Duration::from_secs(60)).await.unwrap();
        assert_eq!(next.token, link.token);
        assert!(!next.already_submitted);
        assert!(rx_new.try_recv().is_err());
    }

    #[tokio::test]
    async fn watchdog_records_scan_even_when_receiver_is_closed() {
        let registry = registry();
        let (tx, rx) = mpsc::channel(4);
        let link = registry.register_pending("fju", 42, "a", tx, Duration::from_secs(60)).await.unwrap();
        registry.qr_phase("fju", 42, "a", QrPhase::WaitingInput).await;
        drop(rx);
        let result = registry.submit(ScannerSubmission {
            provider: "fju".into(), rollcall_id: 42, account_id: "a".into(),
            token: link.token, qr_data: "fresh".into(),
        }).await;
        assert_eq!(result.unwrap_err(), ScannerError::NoDeliveredAccounts);
        let inner = registry.inner.lock().await;
        let health = &inner[&PendingQrKey::new("fju", 42)].watchdogs["a"];
        assert!(health.stall_reason(Instant::now() + Duration::from_secs(61)).is_some());
    }
}
