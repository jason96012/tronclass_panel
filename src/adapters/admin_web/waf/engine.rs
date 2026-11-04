//! WAF 引擎與設定（WAF_ENGINE_DI_V1，A2 依賴注入）。
//!
//! 所有有狀態元件（遙測、限速、異常、指紋、白名單、私網警告旗標）都是 `WafEngine` 的
//! 實例欄位，由 `AdminWebState` 以 `Arc<WafEngine>` 持有。模組內不再有任何可變 `static`。
//! 測試可用 `WafEngine::new(WafConfig { .. })` 建立彼此獨立的實例，因此可並行執行。
//! 預設值（`WafConfig::default`）與重構前的常數完全相同，行為不變。
//!
//! 設定熱重載不在本模組範圍內（延後至 A4 規則引擎）。

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use axum::http::{HeaderMap, Method, Uri};

use super::rules::WafRules;

/// WAF 可調參數。`Default` 等於重構前的全域常數。
#[derive(Clone, Debug)]
pub struct WafConfig {
    /// 每個 IP（或 IPv6 /64）在 `rate_limit_window` 內的最大請求數。
    pub rate_limit_max_requests: usize,
    pub rate_limit_window: Duration,
    /// 限速表的 IP 數硬上限。
    pub rate_limit_max_ips: usize,
    /// 每個 IP 在 `anomaly_window` 內允許的非 403 4xx 次數（超過即自動封鎖）。
    pub anomaly_max_4xx: usize,
    /// 每個 IP 在 `anomaly_window` 內允許的 401 次數（401 獨立計數，見 `WAF_ANOMALY_MAX_401`）。
    pub anomaly_max_401: usize,
    pub anomaly_window: Duration,
    pub anomaly_max_keys: usize,
    /// 同一指紋在 `fingerprint_window` 內出現於這麼多不同 IP／/64 群組即視為可疑：回 403，並依次數指數延長暫時封鎖（見 `db::waf_fingerprint`；管理員可放行）。
    pub fingerprint_ip_threshold: usize,
    pub fingerprint_window: Duration,
    pub fingerprint_max_ips_per_key: usize,
    pub fingerprint_key_max_bytes: usize,
    pub fingerprint_max_keys: usize,
    /// C5：本體掃描的記憶體預算（單位 MB）。同時掃描的本體大小總和不超過此值。
    pub body_scan_budget_mb: usize,
    /// C5：等待掃描預算的最長時間；逾時回 503（失敗即關閉，不略過掃描）。
    pub body_scan_wait: Duration,
}

impl Default for WafConfig {
    fn default() -> Self {
        Self {
            rate_limit_max_requests: WAF_RATE_LIMIT_MAX_REQUESTS,
            rate_limit_window: WAF_RATE_LIMIT_WINDOW,
            rate_limit_max_ips: WAF_RATE_LIMIT_MAX_IPS,
            anomaly_max_4xx: WAF_ANOMALY_MAX_4XX,
            anomaly_max_401: WAF_ANOMALY_MAX_401,
            anomaly_window: Duration::from_secs(WAF_ANOMALY_WINDOW_SECS),
            anomaly_max_keys: WAF_ANOMALY_MAX_KEYS,
            fingerprint_ip_threshold: WAF_FINGERPRINT_IP_THRESHOLD,
            fingerprint_window: WAF_FINGERPRINT_WINDOW,
            fingerprint_max_ips_per_key: WAF_FINGERPRINT_MAX_IPS_PER_KEY,
            fingerprint_key_max_bytes: WAF_FINGERPRINT_KEY_MAX_BYTES,
            fingerprint_max_keys: WAF_FINGERPRINT_MAX_KEYS,
            body_scan_budget_mb: 128,
            body_scan_wait: Duration::from_secs(10),
        }
    }
}

/// 一個 WAF 實例擁有的全部狀態與檢測流程。
pub struct WafEngine {
    pub(super) telemetry: WafTelemetry,
    pub(super) rate_limiter: WafRateLimiter,
    pub(super) anomaly: WafAnomalyTracker,
    /// 401 獨立計數（與非 401 的 4xx 分開累計）。
    pub(super) anomaly_401: WafAnomalyTracker,
    pub(super) fingerprint: WafFingerprintTracker,
    pub(super) whitelist: WafWhitelist,
    pub(super) detectors: CompositeDetector,
    /// WAF_PRIVATE_PROXY_WARN_V1：私網 peer 警告只記一次。
    pub(super) private_peer_warned: AtomicBool,
    /// C5：本體掃描預算（信號量，單位 MB）與等待上限。
    pub(super) body_scan_budget: Arc<Semaphore>,
    pub(super) body_scan_budget_mb: usize,
    pub(super) body_scan_wait: Duration,
    /// A4：唯讀規則基線（只增不減，在內建檢測器鏈之後執行）。
    pub(super) rules: WafRules,
}

impl WafEngine {
    pub fn new(config: WafConfig) -> Self {
        Self::with_rules(config, WafRules::builtin())
    }

    /// 指定附加規則（A4）。內建規則一律保留。
    pub fn with_rules(config: WafConfig, rules: WafRules) -> Self {
        Self {
            rules,
            body_scan_budget: Arc::new(Semaphore::new(config.body_scan_budget_mb)),
            body_scan_budget_mb: config.body_scan_budget_mb,
            body_scan_wait: config.body_scan_wait,
            telemetry: WafTelemetry::default(),
            rate_limiter: WafRateLimiter::with_limits(
                config.rate_limit_max_requests,
                config.rate_limit_window,
                config.rate_limit_max_ips,
            ),
            anomaly: WafAnomalyTracker::with_limits(
                config.anomaly_max_4xx,
                config.anomaly_window,
                config.anomaly_max_keys,
            ),
            anomaly_401: WafAnomalyTracker::with_limits(
                config.anomaly_max_401,
                config.anomaly_window,
                config.anomaly_max_keys,
            ),
            fingerprint: WafFingerprintTracker::with_limits(
                config.fingerprint_ip_threshold,
                config.fingerprint_window,
                config.fingerprint_max_ips_per_key,
                config.fingerprint_key_max_bytes,
                config.fingerprint_max_keys,
            ),
            whitelist: WafWhitelist::new(),
            detectors: CompositeDetector::standard(),
            private_peer_warned: AtomicBool::new(false),
        }
    }

    /// 執行威脅檢測（計入 total_requests；命中與否由檢測器鏈決定）。
    pub(super) fn inspect_threat(&self, uri: &Uri, method: &Method, headers: &HeaderMap) -> Option<WafThreat> {
        self.telemetry.total_requests.fetch_add(1, Ordering::Relaxed);
        self.detectors
            .inspect(uri, method, headers)
            .or_else(|| self.rules.check(uri, headers))
    }
}

impl Default for WafEngine {
    fn default() -> Self {
        Self::new(WafConfig::default())
    }
}
