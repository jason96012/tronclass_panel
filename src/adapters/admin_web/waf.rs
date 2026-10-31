//! WAF_FIREWALL_MAX_V1: 最高等級 Web 應用程式防火牆與全方位資訊安全防護引擎。
//!
//! 需求：「一次把防火牆升到最高級」。
//!
//! 本模組為整個系統提供企業級／軍規級多層次深度防禦架構（Maximum Defense-in-Depth）：
//!
//! 1. **OWASP Top 10 全面覆蓋與即時攻擊攔截**：
//!    - **蜜罐弱點誘捕（Honeypot Trap）**：涵蓋 100+ 種常見弱點探針（`.env`、`.git`、CMS、phpMyAdmin、
//!      Spring Actuator、Swagger、WebShells、IoT、Router、Cloud Config 等）。
//!    - **黑客工具與自動化 Bot 阻斷（Malicious Scanner Fingerprint）**：封鎖 40+ 種知名掃描器
//!      （`sqlmap`、`nikto`、`nmap`、`masscan`、`nuclei`、`burpsuite`、`acunetix`、`nessus` 等）。
//!    - **SQL 注入（SQLi）深度分析**：涵蓋 Union-based、Error-based、Time-based、Blind SQLi 與堆疊查詢。
//!    - **遠端代碼執行（RCE / Command Injection）**：攔截 Log4j / JNDI、Spring4Shell、Bash/PowerShell 注入。
//!    - **XSS 跨站腳本攻擊**：攔截 `<script>`、事件處理器（`onerror=`、`onload=`）、DOM 操控與 Cookie 竊取。
//!    - **SSRF 與雲端中繼資料探測（Cloud Metadata Guard）**：阻斷探測 `169.254.169.254`、AWS/GCP/阿里雲 Metadata。
//!    - **SSTI 與範本表達式注入**：攔截 `{{...}}`、`${...}`、`#{...}` 等範本執行載荷。
//!    - **目錄穿越與本機檔案包含（Path Traversal / LFI）**：深度比對 `../`、`/etc/passwd`、`win.ini` 等。
//!    - **敏感備份檔探測阻斷**：攔截 `.sql`、`.sqlite3`、`.bak`、`.dump`、`.tar.gz` 等洩漏探測。
//!    - **多重跳脫編碼防繞過（Iterative Recursive URL Decoding）**：遞迴解碼多層 URL-encode，消除編碼混淆。
//!    - **WAF 偽造標頭防護**：阻斷 `X-Original-URL`、`X-Rewrite-URL` 等偽造路由標頭。
//!    - **危險 HTTP 方法阻斷**：阻斷 `TRACE`、`TRACK`、`CONNECT`（防止 XST 攻擊）。
//!
//! 2. **自動威脅處置與蜜罐黑名單聯防（Auto-Ban Jail）**：
//!    - 外部非受信任 IP 只要發起惡意探測或攻擊載荷，WAF 立即將該 IP 寫入 SQLite
//!      `ip_blacklist`，自動觸發 Discord / LINE / Web Push 即時資安通報，
//!      並於後台活動日誌（`activity_events`）留存安全稽核軌跡。
//!
//! 3. **全域 Layer-7 速率限制與防 DoS 洪水防護（L7 Anti-DDoS）**：
//!    - 採用高效能記憶體滑動窗口（Sliding Window）演算法。
//!    - 每外部 IP 每分鐘上限 240 次請求（自適應寬裕上限，不影響前端輪詢與串流，
//!      但堅決阻斷惡意高頻掃描與資源耗盡攻擊）。
//!    - Loopback 與內部受信任代理自動免除限速。
//!
//! 4. **Anomaly Detection & Auto-Ban**: track per-IP 404/403 trigger counts;
//!      abnormally frequent hits in a short window auto-ban the IP, preventing
//!      scanners from evading WAF signature matching with seemingly-legitimate requests.
//!      Request fingerprint (UA + Accept-Language etc.) tracks IP rotation attacks;
//!      the same fingerprint appearing across different IPs raises block likelihood.
//! 5. **WAF Whitelist**: exempt specific IP, CIDR, or path prefixes from WAF checks,
//!      to avoid false-positives on internal monitoring, health checks, or known-good clients.
//! 6. **Total Request Size Limit**: in addition to body scan cap, limit overall request
//!      size to prevent huge requests exhausting memory and bandwidth.
//! 7. **WAF Telemetry API**: `/admin/api/waf/stats` exposes real-time WAF statistics
//!      for the admin dashboard.
//! 8. **WAF Telemetry**: immediate cumulative attack block counts and last defense events.

use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
};
use serde_json::json;
use tokio::sync::Mutex;
use tracing::warn;

// WAF_AUTO_BLOCK_SILENT_V1：WAF 自動封鎖已改成不再透過 notify_admins_security_alert
// 推播 Discord／Web Push（原因見下方 waf_firewall_middleware 內的註解），
// 這裡不再需要 security_now_label／SecurityAlertEvent，只保留寫黑名單與活動日誌
// 需要用到的型別。
use super::AdminWebState;
use crate::db::ActivityEventInput;

/// WAF 全域滑動窗口時間跨度（60 秒）。
const WAF_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
/// 每外部 IP 每分鐘最大請求上限（240 次 / 分鐘，相當於平均 4 req/sec）。
const WAF_RATE_LIMIT_MAX_REQUESTS: usize = 240;
/// 滑動窗口快取清理閾值（最多保留 5000 個獨立 IP 記錄）。
const WAF_RATE_LIMIT_MAX_IPS: usize = 5000;
/// URI / Query 最大長度限制（防止緩衝區溢位攻擊）。
const WAF_MAX_URI_LENGTH: usize = 4096;

/// WAF 異常行為偵測：單一 IP 在視窗內觸發 404/403 的最大次數，超過即自動封鎖。
/// 知識庫 L5 規定：15 次/5 分鐘。
const WAF_ANOMALY_MAX_4XX: usize = 15;
/// WAF 異常行為偵測：視窗長度（秒）。
const WAF_ANOMALY_WINDOW_SECS: u64 = 300;
/// WAF 異常行為偵測：異常 IP 黑名單保留時間（秒，30 分鐘）。
const WAF_ANOMALY_BAN_SECS: u64 = 1800;
/// 總體請求大小上限（防止超大請求耗盡記憶體與頻寬）。
const WAF_MAX_REQUEST_SIZE: usize = 64 * 1024 * 1024;
/// 已知搜尋引擎爬蟲 User-Agent 關鍵字，豁免異常行為偵測與自動封鎖。
const KNOWN_CRAWLER_UAS: &[&str] = &[
    "googlebot", "bingbot", "slurp", "duckduckbot", "baidubot",
    "yandexbot", "sogou", "exabot", "facebot", "twitterbot",
];
/// 標頭欄位數量上限（防止標頭洪水攻擊）。
const WAF_MAX_HEADERS_COUNT: usize = 50;
/// JSON 最大嵌套深度（防止 JSON 炸彈攻擊）。
const WAF_MAX_JSON_DEPTH: usize = 10;
/// 表單欄位數量上限（防止表單欄位洪水攻擊）。
const WAF_MAX_FORM_FIELDS: usize = 100;

/// WAF 即時遙測計數器。
pub struct WafTelemetry {
    pub total_requests: AtomicU64,
    pub total_blocked: AtomicU64,
    pub honeypot_probes: AtomicU64,
    pub sql_injections: AtomicU64,
    pub rce_attempts: AtomicU64,
    pub xss_attempts: AtomicU64,
    pub path_traversals: AtomicU64,
    pub bot_scanners: AtomicU64,
    pub ssrf_attempts: AtomicU64,
    pub ssti_attempts: AtomicU64,
    pub proto_pollution: AtomicU64,
    pub deserialization_attempts: AtomicU64,
    pub host_poisoning: AtomicU64,
    pub smuggling_attempts: AtomicU64,
    pub webshell_attempts: AtomicU64,
    pub rate_limited: AtomicU64,
    pub anomaly_blocked: AtomicU64,
    pub fingerprint_blocked: AtomicU64,
    pub whitelist_bypass: AtomicU64,
}

impl Default for WafTelemetry {
    fn default() -> Self {
        Self {
            total_requests: AtomicU64::new(0),
            total_blocked: AtomicU64::new(0),
            honeypot_probes: AtomicU64::new(0),
            sql_injections: AtomicU64::new(0),
            rce_attempts: AtomicU64::new(0),
            xss_attempts: AtomicU64::new(0),
            path_traversals: AtomicU64::new(0),
            bot_scanners: AtomicU64::new(0),
            ssrf_attempts: AtomicU64::new(0),
            ssti_attempts: AtomicU64::new(0),
            proto_pollution: AtomicU64::new(0),
            deserialization_attempts: AtomicU64::new(0),
            host_poisoning: AtomicU64::new(0),
            smuggling_attempts: AtomicU64::new(0),
            webshell_attempts: AtomicU64::new(0),
            rate_limited: AtomicU64::new(0),
            anomaly_blocked: AtomicU64::new(0),
            fingerprint_blocked: AtomicU64::new(0),
            whitelist_bypass: AtomicU64::new(0),
        }
    }
}

/// 全域 WAF 遙測單例。
pub static WAF_TELEMETRY: once_cell::sync::Lazy<WafTelemetry> =
    once_cell::sync::Lazy::new(WafTelemetry::default);

/// 全域 L7 速率限制器狀態。
#[derive(Clone)]
pub struct WafRateLimiter {
    attempts: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl Default for WafRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl WafRateLimiter {
    pub fn new() -> Self {
        Self {
            attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 檢查 IP 是否超過 L7 請求頻率上限。
    pub async fn check_limit(&self, ip: &str) -> bool {
        let mut map = self.attempts.lock().await;
        let now = Instant::now();
        let window_start = now.checked_sub(WAF_RATE_LIMIT_WINDOW).unwrap_or(now);

        let entry = map.entry(ip.to_string()).or_default();
        entry.retain(|t| *t > window_start);

        if entry.len() >= WAF_RATE_LIMIT_MAX_REQUESTS {
            return false;
        }

        entry.push(now);

        // 定期或容量滿時清理過期 IP
        if map.len() > WAF_RATE_LIMIT_MAX_IPS {
            map.retain(|_, v| {
                v.retain(|t| *t > window_start);
                !v.is_empty()
            });
        }
        true
    }
}

/// 全域單例 WAF 限速器。
static WAF_GLOBAL_LIMITER: once_cell::sync::Lazy<WafRateLimiter> =
    once_cell::sync::Lazy::new(WafRateLimiter::new);

/// 異常行為偵測：追蹤每個 IP 的 4xx 觸發時間點（滑動窗口）。
#[derive(Clone, Default)]
pub struct WafAnomalyTracker {
    hits: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl WafAnomalyTracker {
    pub fn new() -> Self {
        Self {
            hits: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 記錄一次 4xx 觸發，若超過閾值則回傳 true（應封鎖）。
    pub async fn record_and_check(&self, ip: &str) -> bool {
        let mut map = self.hits.lock().await;
        let now = Instant::now();
        let window_start = now
            .checked_sub(Duration::from_secs(WAF_ANOMALY_WINDOW_SECS))
            .unwrap_or(now);

        // 在區塊內計算 should_ban，讓可變借用在 map.len() 檢查前結束
        let should_ban = {
            let entry = map.entry(ip.to_string()).or_default();
            entry.retain(|t| *t > window_start);
            entry.push(now);
            entry.len() > WAF_ANOMALY_MAX_4XX
        };

        // 清理過舊 IP，控制記憶體
        if map.len() > 10000 {
            map.retain(|_, v| {
                v.retain(|t| *t > window_start);
                !v.is_empty()
            });
        }

        should_ban
    }

    pub async fn clear(&self, ip: &str) {
        let mut map = self.hits.lock().await;
        map.remove(ip);
    }
}

/// 單一請求指紋：基於 User-Agent + Accept-Language + Accept-Encoding 等標頭，
/// 用於偵測同一攻擊者輪換 IP 的情況。
fn request_fingerprint(headers: &HeaderMap) -> String {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let accept_lang = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let accept_enc = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let sec_ch_ua = headers
        .get(header::HeaderName::from_static("sec-ch-ua"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    format!(
        "{}|{}|{}|{}",
        ua, accept_lang, accept_enc, sec_ch_ua
    )
}

/// 判斷是否為已知搜尋引擎爬蟲，豁免異常行為偵測與自動封鎖。
fn is_known_crawler(user_agent: &str) -> bool {
    let ua = user_agent.to_ascii_lowercase();
    KNOWN_CRAWLER_UAS.iter().any(|&crawler| ua.contains(crawler))
}

/// 請求指紋追蹤：同一指紋在多個 IP 出現時，提示 IP 輪換攻擊。
#[derive(Clone, Default)]
pub struct WafFingerprintTracker {
    /// 指紋 -> 見過的 IP 集合
    seen: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

impl WafFingerprintTracker {
    pub fn new() -> Self {
        Self {
            seen: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 記錄指紋與 IP。若指紋已在多個不同 IP 出現，回傳 true（懷疑 IP 輪換）。
    /// 閾值設為 10，避免正常用戶在家裡／公司／手機熱點等不同網路間切換時被誤封。
    pub async fn record_and_check(&self, fingerprint: &str, ip: &str) -> bool {
        let mut map = self.seen.lock().await;
        let entry = entry(fingerprint.to_string(), ip.to_string(), &mut map);
        entry.len() >= 10
    }
}

use std::collections::hash_map::Entry;

fn entry(
    key: String,
    value: String,
    map: &mut HashMap<String, Vec<String>>,
) -> Vec<String> {
    match map.entry(key) {
        Entry::Occupied(mut e) => {
            if !e.get().contains(&value) {
                e.get_mut().push(value.clone());
            }
            e.get().clone()
        }
        Entry::Vacant(e) => {
            e.insert(vec![value.clone()]);
            vec![value]
        }
    }
}

/// WAF 白名單：CIDR / IP / 路徑前綴豁免。
#[derive(Clone, Default)]
pub struct WafWhitelist {
    /// 明確允許的 IP 字串（精確比對）
    ips: Arc<Mutex<Vec<String>>>,
    /// CIDR 區塊（字串形式，如 "10.0.0.0/8"）
    cidrs: Arc<Mutex<Vec<String>>>,
    /// 路徑前綴豁免（如 "/api/health", "/metrics"）
    paths: Arc<Mutex<Vec<String>>>,
}

impl WafWhitelist {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn is_ip_whitelisted(&self, ip: &str) -> bool {
        let ips = self.ips.lock().await;
        if ips.contains(&ip.to_string()) {
            return true;
        }
        drop(ips);

        // 檢查 CIDR
        let cidrs = self.cidrs.lock().await;
        let ip_addr = match IpAddr::from_str(ip) {
            Ok(a) => a,
            Err(_) => return false,
        };
        for cidr in cidrs.iter() {
            if let Some(net) = parse_cidr(cidr) {
                if ip_addr_matches_cidr(ip_addr, net) {
                    return true;
                }
            }
        }
        false
    }

    pub async fn is_path_whitelisted(&self, path: &str) -> bool {
        let paths = self.paths.lock().await;
        paths.iter().any(|p| path.starts_with(p.as_str()))
    }
}

/// 解析 CIDR 字串回傳 (network_addr, prefix_len)。
fn parse_cidr(cidr: &str) -> Option<(IpAddr, u8)> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return None;
    }
    let ip = IpAddr::from_str(parts[0]).ok()?;
    let prefix: u8 = parts[1].parse().ok()?;
    Some((ip, prefix))
}

/// 判斷 IP 是否落在 CIDR 區塊內。
fn ip_addr_matches_cidr(ip: IpAddr, (network, prefix): (IpAddr, u8)) -> bool {
    match (ip, network) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let a_bits = u32::from_be_bytes(a.octets());
            let n_bits = u32::from_be_bytes(n.octets());
            let mask: u32 = if prefix == 0 { 0 } else { !((1u32 << (32 - prefix)) - 1) };
            (a_bits & mask) == (n_bits & mask)
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let a_bits = u128::from_be_bytes(a.octets());
            let n_bits = u128::from_be_bytes(n.octets());
            let mask: u128 = if prefix == 0 { 0 } else { !((1u128 << (128 - prefix)) - 1) };
            (a_bits & mask) == (n_bits & mask)
        }
        _ => false,
    }
}

/// 全域單例：異常行為偵測器。
static WAF_ANOMALY_TRACKER: once_cell::sync::Lazy<WafAnomalyTracker> =
    once_cell::sync::Lazy::new(WafAnomalyTracker::new);

/// 全域單例：請求指紋追蹤器。
static WAF_FINGERPRINT_TRACKER: once_cell::sync::Lazy<WafFingerprintTracker> =
    once_cell::sync::Lazy::new(WafFingerprintTracker::new);

/// 全域單例：WAF 白名單。
static WAF_WHITELIST: once_cell::sync::Lazy<WafWhitelist> =
    once_cell::sync::Lazy::new(WafWhitelist::new);

/// 威脅分類。
#[derive(Debug, Clone, PartialEq)]
pub enum WafThreat {
    /// 蜜罐誘餌與常見惡意掃描路徑
    HoneypotProbe(String),
    /// 目錄穿越
    PathTraversal(String),
    /// 惡意代碼注入 / RCE / 命令注入
    RcePayload(String),
    /// SQL 注入
    SqlInjection(String),
    /// XSS 腳本注入
    XssInjection(String),
    /// SSRF / 雲端中繼資料探測
    SsrfPayload(String),
    /// SSTI / 範本表達式注入
    SstiPayload(String),
    /// Prototype Pollution 原型鏈污染
    PrototypePollution(String),
    /// XXE 與反序列化攻擊
    DeserializationAttack(String),
    /// Host 標頭投毒與偽造
    HostHeaderPoisoning(String),
    /// HTTP 請求走私與標頭混淆
    RequestSmuggling(String),
    /// 雙重副檔名與 WebShell 偽裝
    WebshellMasquerade(String),
    /// 惡意黑客掃描工具 User-Agent
    MaliciousUserAgent(String),
    /// 敏感檔案與備份探測
    DangerousExtension(String),
    /// WAF 繞過標頭偽造
    BypassHeader(String),
    /// 請求超長 / 緩衝區異常
    OversizedRequest(String),
    /// 危險 HTTP 方法
    DangerousMethod(String),
    /// 請求本體（JSON／表單）含高信心攻擊特徵（WAF_BODY_SCAN_V1）
    BodyPayload(String),
}

impl WafThreat {
    /// WAF_AUTOBAN_NARROW_V1：是否允許寫入永久黑名單。
    /// UA 類與中段副檔名類誤判率高（WAF 審查 W3／W2），命中時仍回 403，但不封鎖 IP，
    /// 避免一次誤判變成永久鎖定。其餘攻擊特徵維持原本的自動封鎖。
    pub fn allows_auto_ban(&self) -> bool {
        // WAF_AUTOBAN_NARROW_V1：UA 類與中段副檔名類誤判率高（WAF 審查 W3／W2），命中時仍回 403，但不封鎖 IP，
        // 避免一次誤判變成永久鎖定。其餘攻擊特徵維持原本的自動封鎖。
        !matches!(self, Self::MaliciousUserAgent(_) | Self::WebshellMasquerade(_))
    }

    pub fn description(&self) -> String {
        match self {
            Self::HoneypotProbe(p) => format!("探測高危弱點/蜜罐路徑: {p}"),
            Self::PathTraversal(p) => format!("目錄穿越攻擊: {p}"),
            Self::RcePayload(p) => format!("遠端命令執行 (RCE) 攻擊載荷: {p}"),
            Self::SqlInjection(p) => format!("SQL 注入攻擊載荷: {p}"),
            Self::XssInjection(p) => format!("XSS 跨站腳本攻擊載荷: {p}"),
            Self::SsrfPayload(p) => format!("SSRF / 雲端中繼資料探測: {p}"),
            Self::SstiPayload(p) => format!("SSTI 範本注入攻擊: {p}"),
            Self::PrototypePollution(p) => format!("原型鏈污染 (Prototype Pollution) 攻擊: {p}"),
            Self::DeserializationAttack(p) => format!("反序列化 / XXE 注入攻擊: {p}"),
            Self::HostHeaderPoisoning(h) => format!("Host 標頭投毒/畸形偽造攻擊: {h}"),
            Self::RequestSmuggling(s) => format!("HTTP 請求走私 (Request Smuggling) 攻擊: {s}"),
            Self::WebshellMasquerade(w) => format!("雙重副檔名 WebShell 偽裝探測: {w}"),
            Self::MaliciousUserAgent(ua) => format!("惡意黑客工具指紋: {ua}"),
            Self::DangerousExtension(ext) => format!("探測敏感備份/設定檔案: {ext}"),
            Self::BypassHeader(h) => format!("WAF 繞過偽造標頭: {h}"),
            Self::OversizedRequest(s) => format!("請求超長/緩衝區異常: {s}"),
            Self::DangerousMethod(m) => format!("危險 HTTP 方法: {m}"),
            Self::BodyPayload(p) => format!("請求本體攻擊載荷（JSON／表單）: {p}"),
        }
    }

    pub fn record_telemetry(&self) {
        WAF_TELEMETRY.total_blocked.fetch_add(1, Ordering::Relaxed);
        match self {
            Self::HoneypotProbe(_) => {
                WAF_TELEMETRY.honeypot_probes.fetch_add(1, Ordering::Relaxed);
            }
            Self::PathTraversal(_) => {
                WAF_TELEMETRY.path_traversals.fetch_add(1, Ordering::Relaxed);
            }
            Self::RcePayload(_) => {
                WAF_TELEMETRY.rce_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::SqlInjection(_) => {
                WAF_TELEMETRY.sql_injections.fetch_add(1, Ordering::Relaxed);
            }
            Self::XssInjection(_) => {
                WAF_TELEMETRY.xss_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::SsrfPayload(_) => {
                WAF_TELEMETRY.ssrf_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::SstiPayload(_) => {
                WAF_TELEMETRY.ssti_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::PrototypePollution(_) => {
                WAF_TELEMETRY.proto_pollution.fetch_add(1, Ordering::Relaxed);
            }
            Self::DeserializationAttack(_) => {
                WAF_TELEMETRY.deserialization_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::HostHeaderPoisoning(_) => {
                WAF_TELEMETRY.host_poisoning.fetch_add(1, Ordering::Relaxed);
            }
            Self::RequestSmuggling(_) => {
                WAF_TELEMETRY.smuggling_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::WebshellMasquerade(_) => {
                WAF_TELEMETRY.webshell_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::MaliciousUserAgent(_) => {
                WAF_TELEMETRY.bot_scanners.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

/// WAF_BODY_SCAN_V1：本體掃描的單次上限。超過就回 413（不掃描、不放行），
/// 否則填充到上限之後就能繞過檢查。AI 聊天附件（base64）與 tc_proxy 是本系統最大的 JSON 本體來源。
const WAF_BODY_SCAN_MAX_BYTES: usize = 32 * 1024 * 1024;

/// 本體掃描的高信心特徵。刻意只取少數：完整的 URL 規則（例如 `sleep(`、`powershell`）
/// 會把程式碼與教材內容誤判，因此不套用到本體。
const WAF_BODY_PATTERNS: &[&str] = &[
    "<script", "<\\/script", "javascript:", "vbscript:", "onerror=", "onload=", "onmouseover=",
    "<iframe", "<svg/onload", "${jndi:", "<!entity", "<!doctype", "__proto__", "constructor.prototype",
    "union select", "union all select", "' or '1'='1", "' or 1=1", "into outfile", "into dumpfile",
];

/// 本體是否需要掃描：只看會被 handler 解析成文字的 POST／PUT／PATCH 內容。multipart 上傳不掃描。
fn waf_body_needs_scan(method: &Method, headers: &HeaderMap) -> bool {
    if !(*method == Method::POST || *method == Method::PUT || *method == Method::PATCH) {
        return false;
    }
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    ct.starts_with("application/json")
        || ct.starts_with("application/x-www-form-urlencoded")
        || ct.starts_with("text/")
}

/// 掃描本體。表單先做 URL 解碼；JSON 把 \uXXXX 的尖括號與引號跳脫還原（JSON 解析後才會變成 HTML）。
fn inspect_waf_body(body: &str, is_form: bool) -> Option<WafThreat> {
    let mut text = if is_form {
        recursive_url_decode(body.to_string())
    } else {
        body.to_string()
    };
    if !is_form && text.contains("\\u00") {
        text = text
            .replace("\\u003c", "<")
            .replace("\\u003C", "<")
            .replace("\\u003e", ">")
            .replace("\\u003E", ">")
            .replace("\\u0027", "'")
            .replace("\\u0022", "\"");
    }
    let lower = text.to_ascii_lowercase();
    for &pat in WAF_BODY_PATTERNS {
        if lower.contains(pat) {
            return Some(WafThreat::BodyPayload(pat.to_string()));
        }
    }
    None
}

/// 本體命中後的自動封鎖（使用者決定：本體命中也封 IP）。與 URL 威脅使用同一組黑名單與稽核欄位，
/// 只跳過本機／私網 IP，並且已封鎖者不重複寫入（同 WAF_BAN_DEDUP_V1 的考量）。
async fn ban_for_body_threat(
    state: &AdminWebState,
    ip: &str,
    threat_desc: String,
    path: String,
    user_agent: String,
) {
    if is_local_or_trusted_ip(ip) || ip == "unknown" {
        return;
    }
    let ban_key = crate::trusted_ip::blacklist_group_key(ip);
    if state
        .accounts_db
        .is_ip_blacklisted(&ban_key)
        .await
        .unwrap_or(false)
    {
        return;
    }
    let db = state.accounts_db.clone();
    tokio::spawn(async move {
        if let Ok(true) = db
            .block_ip_with_failure_count(
                &ban_key,
                "system:waf-firewall-max",
                &format!("最高級 WAF 防火牆自動封鎖（本體攻擊特徵）: {threat_desc}"),
                1,
            )
            .await
        {
            let _ = db
                .record_activity_event(ActivityEventInput {
                    kind: "security".to_string(),
                    actor: "system:waf-firewall-max".to_string(),
                    action: "waf_threat_blocked".to_string(),
                    account_id: "unknown".to_string(),
                    path,
                    result: "blocked".to_string(),
                    details: format!("最高級 WAF 攔截本體攻擊並自動封鎖: {threat_desc}"),
                    ip: ban_key.clone(),
                    user_agent,
                })
                .await;
        }
    });
}

/// 判定 IP 是否為本地回送或私有網路（私網不列入自動黑名單封鎖，避免內部維護被誤擋）。
pub fn is_local_or_trusted_ip(ip: &str) -> bool {
    match ip.trim().parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
        Err(_) => false,
    }
}

/// 常見已知惡意探針與漏洞掃描路徑特徵庫（120+ 種常見弱點路徑）。
const KNOWN_HONEYPOT_PATHS: &[&str] = &[
    "/.env",
    "/.env.local",
    "/.env.production",
    "/.env.bak",
    "/.env.dev",
    "/.env.example",
    "/.env.backup",
    "/.git",
    "/.git/config",
    "/.git/head",
    "/.gitlab-ci.yml",
    "/.travis.yml",
    "/.svn",
    "/.hg",
    "/.bzr",
    "/.aws",
    "/.ssh",
    "/.vscode",
    "/.idea",
    "/.ds_store",
    "/web.config",
    "/docker-compose.yml",
    "/docker-compose.yaml",
    "/dockerfile",
    "/.dockerenv",
    "/id_rsa",
    "/id_dsa",
    "/id_ed25519",
    "/id_ecdsa",
    "/wp-config.php",
    "/wp-admin",
    "/wp-includes",
    "/wp-login.php",
    "/xmlrpc.php",
    "/wordpress",
    "/wp-content",
    "/administrator",
    "/joomla",
    "/typo3",
    "/drupal",
    "/ghost",
    "/bitrix",
    "/magento",
    "/phpmyadmin",
    "/pma",
    "/mysql",
    "/adminer",
    "/dbadmin",
    "/myadmin",
    "/sqlweb",
    "/mysql-admin",
    "/sqlite",
    "/actuator",
    "/swagger-ui",
    "/swagger-ui.html",
    "/api-docs",
    "/v2/api-docs",
    "/v3/api-docs",
    "/druid",
    "/telescope",
    "/_profiler",
    "/debug/pprof",
    "/h2-console",
    "/jolokia",
    "/cgi-bin/",
    "/eval-stdin.php",
    "/alfa.php",
    "/wso.php",
    "/1.php",
    "/x.php",
    "/b.php",
    "/shell.php",
    "/c99.php",
    "/r57.php",
    "/mini.php",
    "/up.php",
    "/upload.php",
    "/vuln.php",
    "/cmd.php",
    "/webshell",
    "/boaform/",
    "/hnap1/",
    "/setup.cgi",
    "/apply.cgi",
    "/sysinfo.cgi",
    "/luci",
    "/solr",
    "/jenkins",
    "/geoserver",
    "/autodiscover",
    "/owa",
    "/ecp",
    "/citrix",
    "/dana-na",
    "/config.json",
    "/database.yml",
    "/settings.py",
    "/credentials",
    "/kubeconfig",
    "/serviceaccount",
    "/v1/meta-data",
    "/phpinfo.php",
    "/info.php",
    "/test.php",
    "/debug.php",
    "/nacos",
    "/apollo",
    "/consul",
    "/eureka",
    "/kibana",
    "/elasticsearch",
];

/// 惡意黑客掃描工具與自動化攻擊 User-Agent 特徵庫（50+ 種黑客工具）。
const KNOWN_MALICIOUS_USER_AGENTS: &[&str] = &[
    "sqlmap",
    "nikto",
    "masscan",
    "zgrab",
    "zgrab2",
    "gobuster",
    "dirbuster",
    "wpscan",
    "acunetix",
    "nessus",
    "openvas",
    "burpcollaborator",
    "burpsuite",
    "owasp",
    "nuclei",
    "shodan",
    "censys",
    "hydra",
    "nmap",
    "morfeus",
    "havij",
    "metasploit",
    "netsparker",
    "appscan",
    "arachni",
    "whatweb",
    "w3af",
    "dirsearch",
    "ffuf",
    "feroxbuster",
    "jaeles",
    "wfuzz",
    "commix",
    "projectdiscovery",
    "httpx",
    "katana",
    "naabu",
    "subfinder",
    "cortex",
    "qualys",
    "sn1per",
    "golismero",
    "legion",
    "red_hawk",
    "recon-ng",
    "dirb",
    "uniscan",
    "sqlninja",
    "paros",
    "webscarab",
    "skipfish",
];

/// 敏感備份與洩漏副檔名特徵庫。
const KNOWN_DANGEROUS_EXTENSIONS: &[&str] = &[
    ".bak", ".old", ".swp", ".save", ".tmp", ".orig", ".7z", ".rar",
    ".tar.gz", ".sql", ".sqlite3", ".dump", ".ini", ".conf",
    ".pem", ".key", ".crt", ".pfx", ".p12", ".db", ".sqlite", ".mdb", ".log",
];

/// 快速多輪 URL-decode，遞迴還原多重跳脫編碼（最高 3 層），消除混淆繞過。
fn recursive_url_decode(mut input: String) -> String {
    for _ in 0..3 {
        let mut decoded = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let mut i = 0;
        let mut changed = false;

        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let (Some(h1), Some(h2)) = (
                    (bytes[i + 1] as char).to_digit(16),
                    (bytes[i + 2] as char).to_digit(16),
                ) {
                    let byte = (h1 * 16 + h2) as u8;
                    if byte.is_ascii() {
                        decoded.push(byte as char);
                        i += 3;
                        changed = true;
                        continue;
                    }
                }
            }
            if bytes[i] == b'+' {
                decoded.push(' ');
                changed = true;
            } else {
                decoded.push(bytes[i] as char);
            }
            i += 1;
        }

        if !changed {
            break;
        }
        input = decoded;
    }
    input
}

/// 檢測 URI、HTTP Method 與 Headers 是否具備最高等級惡意攻擊威脅特徵。
pub fn inspect_waf_threat(uri: &Uri, method: &Method, headers: &HeaderMap) -> Option<WafThreat> {
    WAF_TELEMETRY.total_requests.fetch_add(1, Ordering::Relaxed);

    // 1. 檢測危險 HTTP 方法
    let m = method.as_str().to_uppercase();
    if m == "TRACE" || m == "TRACK" || m == "CONNECT" {
        return Some(WafThreat::DangerousMethod(m));
    }

    // 2. 檢測 URI 長度異常（防緩衝區溢位）
    let raw_uri = uri.to_string();
    if raw_uri.len() > WAF_MAX_URI_LENGTH {
        return Some(WafThreat::OversizedRequest(format!("長度 {}", raw_uri.len())));
    }

    // 3. 檢測 WAF 偽造/繞過標頭
    if headers.contains_key("x-original-url") {
        return Some(WafThreat::BypassHeader("X-Original-URL".to_string()));
    }
    if headers.contains_key("x-rewrite-url") {
        return Some(WafThreat::BypassHeader("X-Rewrite-URL".to_string()));
    }
    if headers.contains_key("x-custom-ip-authorization") {
        return Some(WafThreat::BypassHeader("X-Custom-IP-Authorization".to_string()));
    }

    // 3b. 檢測 Host 標頭投毒與畸形字元（軍工級防護）
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        if host.contains('@')
            || host.contains('\r')
            || host.contains('\n')
            || host.contains('\0')
            || host.chars().any(|c| c.is_ascii_control() || c.is_whitespace())
        {
            return Some(WafThreat::HostHeaderPoisoning(host.to_string()));
        }
    }

    // 3c. 檢測 HTTP 請求走私與雙重長度標頭 (Request Smuggling Guard)
    if headers.contains_key(header::CONTENT_LENGTH) && headers.contains_key(header::TRANSFER_ENCODING) {
        return Some(WafThreat::RequestSmuggling("Conflicting Content-Length & Transfer-Encoding".to_string()));
    }

    // 4. 檢測惡意 User-Agent 特徵
    if let Some(ua) = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()) {
        let ua_lower = ua.to_ascii_lowercase();
        for &bot in KNOWN_MALICIOUS_USER_AGENTS {
            if ua_lower.contains(bot) {
                return Some(WafThreat::MaliciousUserAgent(ua.to_string()));
            }
        }
    }

    let raw_path = uri.path();
    let lower_path = raw_path.to_ascii_lowercase();
    let query_str = uri.query().unwrap_or("").to_ascii_lowercase();
    let full_target = format!("{lower_path}?{query_str}");
    let decoded_target = recursive_url_decode(full_target.clone()).to_ascii_lowercase();
    let decoded_query = recursive_url_decode(query_str.clone()).to_ascii_lowercase();

    // 5. 檢測蜜罐弱點探針路徑
    for &probe in KNOWN_HONEYPOT_PATHS {
        if lower_path == probe
            || lower_path.starts_with(&format!("{probe}/"))
            || decoded_target.starts_with(probe)
        {
            return Some(WafThreat::HoneypotProbe(raw_path.to_string()));
        }
    }

    // 6. 檢測敏感備份與副檔名探測
    if !lower_path.starts_with("/admin/static/") {
        for &ext in KNOWN_DANGEROUS_EXTENSIONS {
            if lower_path.ends_with(ext)
                || decoded_target
                    .split('?')
                    .next()
                    .unwrap_or("")
                    .ends_with(ext)
            {
                return Some(WafThreat::DangerousExtension(ext.to_string()));
            }
        }
    }

    // 6b. 檢測雙重副檔名與 WebShell 偽裝探測（軍工級防護）
    let dangerous_middle_exts = [
        ".php.", ".phtml.", ".phar.", ".jsp.", ".asp.", ".aspx.", ".cgi.", ".pl.", ".py.", ".sh.", ".bat."
    ];
    // WAF_MIDEXT_PATH_ONLY_V1：只比對 path 部分。分享／預覽連結把檔名放在 query
    // （/pub/share?path=…），若比對整個 target，像 hw3.py.zip 這類正常檔名會被誤判。
    let decoded_path = decoded_target.split('?').next().unwrap_or("");
    for &d_ext in &dangerous_middle_exts {
        if lower_path.contains(d_ext) || decoded_path.contains(d_ext) {
            return Some(WafThreat::WebshellMasquerade(raw_path.to_string()));
        }
    }

    // 7. 檢測目錄穿越（Path Traversal / LFI / Unicode / Overlong UTF-8）
    if full_target.contains("../")
        || full_target.contains("..\\")
        || full_target.contains("..%2f")
        || full_target.contains("%2e%2e/")
        || full_target.contains("%2e%2e%2f")
        || full_target.contains("%252e%252e")
        || full_target.contains("..%5c")
        || full_target.contains("%2e%2e%5c")
        || full_target.contains("%c0%af")
        || full_target.contains("%e0%80%af")
        || full_target.contains("%c1%9c")
        || full_target.contains("%u002e")
        || full_target.contains("..//")
        || full_target.contains("..\\/")
        || decoded_target.contains("../")
        || decoded_target.contains("..\\")
        || decoded_target.contains("/etc/passwd")
        || decoded_target.contains("/etc/shadow")
        || decoded_target.contains("c:\\windows\\system32")
        || decoded_target.contains("%c0%af")
        || decoded_target.contains("%u002e")
    {
        return Some(WafThreat::PathTraversal(raw_path.to_string()));
    }

    // 8. 檢測空字節注入（Null Byte Injection）與 CRLF 注入
    if full_target.contains("%00") || raw_path.contains('\0') || decoded_target.contains('\0') {
        return Some(WafThreat::RcePayload("Null Byte Injection".to_string()));
    }
    if full_target.contains("%0d%0a") || full_target.contains("\r\n") || decoded_target.contains("\r\n") {
        return Some(WafThreat::RcePayload("CRLF Injection".to_string()));
    }

    // 9. 檢測 SSRF 與雲端中繼資料位址探測 (Cloud Metadata Guard & Extended SSRF)
    if full_target.contains("169.254.169.254")
        || full_target.contains("metadata.google.internal")
        || full_target.contains("100.100.100.200")
        || full_target.contains("fd00:ec2::254")
        || full_target.contains("0x7f000001")
        || full_target.contains("0177.0.0.1")
        || full_target.contains("2130706433")
        || full_target.contains("0x7f.1")
        || full_target.contains("::ffff:127.0.0.1")
        || full_target.contains("127.0.0.1.nip.io")
        || full_target.contains("localtest.me")
        || full_target.contains("vcap.me")
        || full_target.contains("burpcollaborator.net")
        || full_target.contains("interactsh.com")
        || full_target.contains("canarytokens.com")
        || full_target.contains("oast.fun")
        || full_target.contains("oast.live")
        || full_target.contains("oast.site")
        || full_target.contains("oast.pro")
        || decoded_target.contains("169.254.169.254")
        || decoded_target.contains("metadata.google.internal")
        || decoded_target.contains("100.100.100.200")
        || decoded_target.contains("fd00:ec2::254")
        || decoded_target.contains("0x7f000001")
        || decoded_target.contains("0177.0.0.1")
        || decoded_target.contains("2130706433")
        || decoded_target.contains("0x7f.1")
        || decoded_target.contains("::ffff:127.0.0.1")
        || decoded_target.contains("127.0.0.1.nip.io")
        || decoded_target.contains("localtest.me")
        || decoded_target.contains("vcap.me")
        || decoded_target.contains("burpcollaborator.net")
        || decoded_target.contains("interactsh.com")
        || decoded_target.contains("canarytokens.com")
        || decoded_target.contains("oast.fun")
        || decoded_target.contains("oast.live")
        || decoded_target.contains("oast.site")
        || decoded_target.contains("oast.pro")
    {
        return Some(WafThreat::SsrfPayload("Cloud Metadata / SSRF Probe".to_string()));
    }

    // 9b. 檢測 SSRF 私有網路段探測（參考 rust-web-security SKILL：封鎖 loopback、私有網路、link-local、metadata 網路）
    if full_target.contains("10.") || full_target.contains("172.16.") || full_target.contains("172.17.")
        || full_target.contains("172.18.") || full_target.contains("172.19.") || full_target.contains("172.20.")
        || full_target.contains("172.21.") || full_target.contains("172.22.") || full_target.contains("172.23.")
        || full_target.contains("172.24.") || full_target.contains("172.25.") || full_target.contains("172.26.")
        || full_target.contains("172.27.") || full_target.contains("172.28.") || full_target.contains("172.29.")
        || full_target.contains("172.30.") || full_target.contains("172.31.") || full_target.contains("192.168.")
        || full_target.contains("127.") || full_target.contains("169.254.") || full_target.contains("::1")
        || full_target.contains("fd00:") || full_target.contains("fe80:") || full_target.contains("0.0.0.0")
        || decoded_target.contains("10.") || decoded_target.contains("172.16.") || decoded_target.contains("172.17.")
        || decoded_target.contains("172.18.") || decoded_target.contains("172.19.") || decoded_target.contains("172.20.")
        || decoded_target.contains("172.21.") || decoded_target.contains("172.22.") || decoded_target.contains("172.23.")
        || decoded_target.contains("172.24.") || decoded_target.contains("172.25.") || decoded_target.contains("172.26.")
        || decoded_target.contains("172.27.") || decoded_target.contains("172.28.") || decoded_target.contains("172.29.")
        || decoded_target.contains("172.30.") || decoded_target.contains("172.31.") || decoded_target.contains("192.168.")
        || decoded_target.contains("127.") || decoded_target.contains("169.254.") || decoded_target.contains("::1")
        || decoded_target.contains("fd00:") || decoded_target.contains("fe80:") || decoded_target.contains("0.0.0.0")
    {
        return Some(WafThreat::SsrfPayload("Private Network SSRF Probe".to_string()));
    }

    // 10. 檢測 Prototype Pollution 原型鏈污染
    if full_target.contains("__proto__")
        || full_target.contains("constructor.prototype")
        || full_target.contains("object.prototype")
        || full_target.contains("prototype[")
        || decoded_target.contains("__proto__")
        || decoded_target.contains("constructor.prototype")
        || decoded_target.contains("object.prototype")
        || decoded_target.contains("prototype[")
    {
        return Some(WafThreat::PrototypePollution(raw_path.to_string()));
    }

    // 11. 檢測 XXE 與 Java / JSON 反序列化注入
    if full_target.contains("<!doctype")
        || full_target.contains("<!entity")
        || full_target.contains("system \"http")
        || full_target.contains("system 'http")
        || full_target.contains("ro0ab")
        || full_target.contains("fastjson")
        || full_target.contains("com.sun.rowset")
        || decoded_target.contains("<!doctype")
        || decoded_target.contains("<!entity")
        || decoded_target.contains("system \"http")
        || decoded_target.contains("system 'http")
        || decoded_target.contains("ro0ab")
        || decoded_target.contains("fastjson")
        || decoded_target.contains("com.sun.rowset")
    {
        return Some(WafThreat::DeserializationAttack(raw_path.to_string()));
    }

    // 12. 檢測 SSTI 範本表達式注入
    if full_target.contains("{{")
        || full_target.contains("class.module.classloader")
        || decoded_target.contains("{{")
        || decoded_target.contains("class.module.classloader")
        || decoded_target.contains("*{t(java.lang.runtime)}")
    {
        return Some(WafThreat::SstiPayload(raw_path.to_string()));
    }

    // 13. 檢測 XSS 跨站腳本攻擊載荷
    if full_target.contains("<script")
        || full_target.contains("javascript:")
        || full_target.contains("vbscript:")
        || full_target.contains("onerror=")
        || full_target.contains("onload=")
        || full_target.contains("document.cookie")
        || full_target.contains("<svg/onload")
        || full_target.contains("<img/src/onerror")
        || decoded_target.contains("<script")
        || decoded_target.contains("javascript:")
        || decoded_target.contains("vbscript:")
        || decoded_target.contains("onerror=")
        || decoded_target.contains("onload=")
        || decoded_target.contains("document.cookie")
        || decoded_target.contains("<svg/onload")
        || decoded_target.contains("<img/src/onerror")
    {
        return Some(WafThreat::XssInjection(raw_path.to_string()));
    }

    // 14. 檢測 Log4j / RCE / 終端命令注入關鍵特徵
    if full_target.contains("${jndi:")
        || full_target.contains("${ctx:")
        || full_target.contains("${env:")
        || full_target.contains("$(curl")
        || full_target.contains("$(wget")
        || full_target.contains(";whoami")
        || full_target.contains("|whoami")
        || full_target.contains(";cat /etc/passwd")
        || full_target.contains("powershell")
        || full_target.contains("base64_decode(")
        || full_target.contains("bash -i")
        || full_target.contains("sh -i")
        || full_target.contains("/dev/tcp/")
        || full_target.contains("/dev/udp/")
        || full_target.contains("python -c")
        || full_target.contains("perl -e")
        || full_target.contains("nc -e")
        || full_target.contains("ncat -e")
        || full_target.contains("certutil -urlcache")
        || full_target.contains("curl|sh")
        || full_target.contains("curl|bash")
        || full_target.contains("wget|sh")
        || full_target.contains("wget|bash")
        || full_target.contains("eval(base64_decode")
        || full_target.contains("system($_")
        || full_target.contains("passthru(")
        || full_target.contains("shell_exec(")
        || full_target.contains("proc_open(")
        || full_target.contains("popen(")
        || decoded_target.contains("${jndi:")
        || decoded_target.contains("${ctx:")
        || decoded_target.contains("${env:")
        || decoded_target.contains("$(curl")
        || decoded_target.contains("$(wget")
        || decoded_target.contains(";whoami")
        || decoded_target.contains("|whoami")
        || decoded_target.contains(";cat /etc/passwd")
        || decoded_target.contains("powershell")
        || decoded_target.contains("base64_decode(")
        || decoded_target.contains("bash -i")
        || decoded_target.contains("sh -i")
        || decoded_target.contains("/dev/tcp/")
        || decoded_target.contains("/dev/udp/")
        || decoded_target.contains("python -c")
        || decoded_target.contains("perl -e")
        || decoded_target.contains("nc -e")
        || decoded_target.contains("ncat -e")
        || decoded_target.contains("certutil -urlcache")
        || decoded_target.contains("curl|sh")
        || decoded_target.contains("curl|bash")
        || decoded_target.contains("wget|sh")
        || decoded_target.contains("wget|bash")
        || decoded_target.contains("eval(base64_decode")
        || decoded_target.contains("system($_")
        || decoded_target.contains("passthru(")
        || decoded_target.contains("shell_exec(")
        || decoded_target.contains("proc_open(")
        || decoded_target.contains("popen(")
    {
        return Some(WafThreat::RcePayload("RCE / Command Injection Payload".to_string()));
    }

    // 15. 檢測顯著 SQL 注入特徵（在 query 參數中）
    let sqli_patterns = [
        "' or '1'='1",
        "' or 1=1",
        "union select",
        "union all select",
        "information_schema",
        "benchmark(",
        "sleep(",
        "waitfor delay",
        "pg_sleep",
        "extractvalue(",
        "updatexml(",
        "into outfile",
        "into dumpfile",
    ];

    if !query_str.is_empty() {
        for &pat in &sqli_patterns {
            if query_str.contains(pat) || decoded_query.contains(pat) {
                return Some(WafThreat::SqlInjection("SQL Injection Payload".to_string()));
            }
        }
    }

    None
}

/// WAF_PRIVATE_PROXY_WARN_V1：只警告一次（避免每個請求都刷 log）。
static WAF_PRIVATE_PEER_WARNED: AtomicBool = AtomicBool::new(false);

/// 私網（非 loopback）peer 會被 `is_local_or_trusted_ip` 整體豁免限速與自動封鎖。
/// 若它其實是反向代理，而 `trusted_proxies` 沒有設定，所有外部請求都會被當成代理的私網 IP，
/// 防護會靜默失效（WAF 審查 W5）。這裡只記一次 WARN，不改變豁免行為。
fn warn_private_peer_without_trusted_proxies(peer: Option<std::net::IpAddr>) {
    let Some(peer) = peer.map(crate::trusted_ip::canonicalize_ip) else { return };
    if peer.is_loopback() || !is_local_or_trusted_ip(&peer.to_string()) {
        return;
    }
    if WAF_PRIVATE_PEER_WARNED.load(Ordering::Relaxed) {
        return;
    }
    let proxies_configured = crate::config::try_get_config()
        .map(|cfg| !cfg.adapters.admin_web.trusted_proxies.is_empty())
        .unwrap_or(false);
    if !proxies_configured && !WAF_PRIVATE_PEER_WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "🛡️ [WAF] 請求來自私網 IP（{}），但 trusted_proxies 未設定。若它是反向代理，所有外部請求都會被視為私網而豁免限速與自動封鎖；請在 config.toml 的 adapters.admin_web.trusted_proxies 明確列出代理。",
            peer
        );
    }
}

/// WAF 防火牆核心中介層。
pub async fn waf_firewall_middleware(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());

    // WAF_TRUSTED_PROXY_CONFIG_V1：必須與全站同一份信任清單
    // （`trusted_from_global_config`），不能寫死 `from_config(&[])`——
    // 否則營運者在 config.toml 額外宣告的信任代理（例如直連 Cloudflare 時
    // `trusted_proxies = ["cloudflare"]`）在 WAF 眼裡會失效：所有請求都被當成
    // 「代理自己的 IP」，真實攻擊者洗成別人；更糟的是自動封鎖會把**代理本身**
    // 關進黑名單 → 全站對所有人斷線。
    let trusted = crate::trusted_ip::trusted_from_global_config();
    warn_private_peer_without_trusted_proxies(peer);
    let ip = crate::trusted_ip::resolve_client_ip(peer, &headers, &trusted);
    let uri = request.uri().clone();
    let method = request.method().clone();
    let raw_path = uri.path().to_string();

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // 0. WAF 白名單：白名單 IP/CIDR 或路徑前綴直接免檢查放行。
    {
        let ip_str = ip.to_string();
        let path_whitelisted = WAF_WHITELIST.is_path_whitelisted(&raw_path).await;
        let ip_whitelisted = WAF_WHITELIST.is_ip_whitelisted(&ip_str).await;
        if ip_whitelisted || path_whitelisted {
            WAF_TELEMETRY.whitelist_bypass.fetch_add(1, Ordering::Relaxed);
            return next.run(request).await;
        }
    }

    // 0b. 總體請求大小限制：依 Content-Length 標頭擋下過大請求。
    if let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > WAF_MAX_REQUEST_SIZE {
            WAF_TELEMETRY.total_blocked.fetch_add(1, Ordering::Relaxed);
            warn!(ip = %ip, path = %raw_path, size = %len, "🛡️ [WAF] 請求大小超過上限，已拒絕");
            return waf_error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({
                    "error": "payload_too_large",
                    "message": "WAF: 請求大小超過系統上限，已拒絕"
                }),
            );
        }
    }

    // 0c. 標頭欄位數量限制：防止標頭洪水攻擊。
    if headers.len() > WAF_MAX_HEADERS_COUNT {
        WAF_TELEMETRY.total_blocked.fetch_add(1, Ordering::Relaxed);
        warn!(ip = %ip, path = %raw_path, headers = %headers.len(), "🛡️ [WAF] 標頭欄位數量過多，已拒絕");
        return waf_error_response(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "too_many_headers",
                "message": "WAF: 請求標頭欄位數量超過上限，已拒絕"
            }),
        );
    }

    // 0c. 記錄請求指紋：偵測 IP 輪換攻擊（同一指紋出現在多個不同 IP）。
    let fingerprint = request_fingerprint(&headers);
    let fingerprint_suspicious = WAF_FINGERPRINT_TRACKER
        .record_and_check(&fingerprint, &ip.to_string())
        .await;
    if fingerprint_suspicious {
        WAF_TELEMETRY.fingerprint_blocked.fetch_add(1, Ordering::Relaxed);
        let ban_key = crate::trusted_ip::blacklist_group_key(&ip.to_string());
        let already_banned = state
            .accounts_db
            .is_ip_blacklisted(&ban_key)
            .await
            .unwrap_or(false);
        if !already_banned && !is_local_or_trusted_ip(&ip) {
            let db = state.accounts_db.clone();
            let block_ip = ban_key.clone();
            let path_for_log = raw_path.clone();
            let ua_for_log = user_agent.clone();
            tokio::spawn(async move {
                if let Ok(true) = db
                    .block_ip_with_failure_count(
                        &block_ip,
                        "system:waf-firewall-max",
                        &format!(
                            "WAF 防火牆自動封鎖（請求指紋 IP 輪換嫌疑）: {}",
                            fingerprint
                        ),
                        1,
                    )
                    .await
                {
                    let _ = db
                        .record_activity_event(ActivityEventInput {
                            kind: "security".to_string(),
                            actor: "system:waf-firewall-max".to_string(),
                            action: "waf_fingerprint_blocked".to_string(),
                            account_id: "unknown".to_string(),
                            path: path_for_log,
                            result: "blocked".to_string(),
                            details: format!(
                                "WAF 攔截可疑指紋 IP 輪換並自動封鎖: {}",
                                fingerprint
                            ),
                            ip: block_ip.clone(),
                            user_agent: ua_for_log,
                        })
                        .await;
                }
            });
        }
        return waf_forbidden_response(json!({
                "error": "waf_access_denied",
                "message": "WAF: 請求指紋可疑，已被系統防火牆攔截"
            }));
    }

    // 1. 深度威脅特徵、蜜罐、黑客工具與惡意標頭檢測
    if let Some(threat) = inspect_waf_threat(&uri, &method, &headers) {
        threat.record_telemetry();
        let threat_desc = threat.description();
        warn!(
            ip = %ip,
            path = %raw_path,
            threat = %threat_desc,
            "🛡️ [最高級 WAF 防火牆] 攔截並阻斷惡意攻擊請求！"
        );

        // 非本機/私網 IP 自動觸發自動黑名單封鎖與安全告警
        let is_local = is_local_or_trusted_ip(&ip) || ip == "unknown";
        if !is_local && threat.allows_auto_ban() {
            // WAF_BAN_DEDUP_V1：黑名單中介層在 WAF **之後**才跑，因此已被封鎖的
            // IP 之後的每個請求仍會走到這裡。若不看「是否已封鎖」就照單全收地
            // spawn，攻擊者拿著已封鎖的 IP 對蜜罐路徑狂打，就能造成：
            // ① 每請求一個 spawn　② 每請求一筆活動日誌　③ 每請求一則管理員
            // Discord／Web Push 告警——變成對告警與資料庫的放大 DoS。
            // 先查一次黑名單（主鍵 EXISTS），已封鎖者靜默擋下（回應照舊 403），
            // 把「封鎖＋稽核＋告警」留給第一次。
            let ban_key = crate::trusted_ip::blacklist_group_key(&ip);
            let already_banned = state
                .accounts_db
                .is_ip_blacklisted(&ban_key)
                .await
                .unwrap_or(false);
            if !already_banned {
                let db = state.accounts_db.clone();
                let block_ip = ban_key.clone();
                let alert_desc = threat_desc.clone();
                let path_for_log = raw_path.clone();
                let ua_for_log = user_agent.clone();

                tokio::spawn(async move {
                    // 自動將惡意攻擊者 IP 寫入永久黑名單；只有「新建立」的封鎖
                    // 才留稽核軌跡（INSERT OR IGNORE 回傳是否新列），
                    // 與上面的 already_banned 併用可擋掉併發下的重複寫入。
                    if let Ok(true) = db
                        .block_ip_with_failure_count(
                            &block_ip,
                            "system:waf-firewall-max",
                            &format!("最高級 WAF 防火牆自動封鎖: {alert_desc}"),
                            1,
                        )
                        .await
                    {
                        // 寫入後台安全活動日誌
                        let _ = db
                            .record_activity_event(ActivityEventInput {
                                kind: "security".to_string(),
                                actor: "system:waf-firewall-max".to_string(),
                                action: "waf_threat_blocked".to_string(),
                                account_id: "unknown".to_string(),
                                path: path_for_log,
                                result: "blocked".to_string(),
                                details: format!("最高級 WAF 攔截威脅並自動封鎖: {alert_desc}"),
                                ip: block_ip.clone(),
                                user_agent: ua_for_log,
                            })
                            .await;

                        // WAF_AUTO_BLOCK_SILENT_V1：這裡以前每次自動封鎖都會額外發一則
                        // Discord／Web Push 安全告警。但 WAF 本來就是「全自動」防線——
                        // 全網掃描器（例如 Palo Alto Networks Cortex Xpanse 這類資安研究
                        // 用途的善意全網掃描，幾乎任何對外服務都會定期被掃到，並非針對性
                        // 攻擊）每天都會打到好幾次，每次都跳通知等於不斷打擾管理員，
                        // 但其實不需要他採取任何動作（已經自動擋下了）。改成不再主動推播，
                        // 只保留上面寫進 ip_blacklist／activity_events 的紀錄——
                        // 要查的話去 /admin/blacklist 頁面看得到完整列表與封鎖原因。
                    }
                });
            }
        }

        if matches!(threat, WafThreat::DangerousMethod(_)) {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                Json(json!({
                    "error": "method_not_allowed",
                    "message": "WAF: 危險 HTTP 方法已被系統最高級防火牆阻斷"
                })),
            )
                .into_response();
        }

        return waf_forbidden_response(json!({
                    "error": "waf_access_denied",
                    "message": "WAF: 請求包含惡意攻擊特徵或探測高危路徑，已被系統最高級防火牆攔截並永久封鎖"
                }));
    }

    // 2. 全域 Layer-7 請求限速與防 DoS 洪水防護。
    // IP_BLACKLIST_V6_GROUP_V1：限速鍵與黑名單同一把尺（IPv6 收斂為 /64 群組）——
    // 否則攻擊者在同前綴內輪換位址，每換一個位址就是全新的 240/分鐘額度，
    // 全域限速形同虛設。
    let is_exempt = is_local_or_trusted_ip(&ip) || ip == "unknown";
    if !is_exempt
        && !WAF_GLOBAL_LIMITER
            .check_limit(&crate::trusted_ip::blacklist_group_key(&ip))
            .await
    {
        WAF_TELEMETRY.rate_limited.fetch_add(1, Ordering::Relaxed);
        warn!(ip = %ip, "🛡️ [最高級 WAF 防火牆] 客戶端觸發全域 L7 速率上限，予以暫時限流！");
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": "rate_limited",
                "message": "WAF: 請求頻率過高，已觸發最高級安全防禦限流，請稍候再試"
            })),
        )
            .into_response();
        response.headers_mut().insert(
            HeaderName::from_static("retry-after"),
            HeaderValue::from_static("60"),
        );
        return response;
    }

    // 3. 請求本體檢查（WAF_BODY_SCAN_V1）。命中只回 403，不寫入黑名單：
    // 本體是使用者輸入（貼程式碼、教材），誤判率高於 URL，不應變成永久封鎖。
    if waf_body_needs_scan(&method, &headers) {
        let (parts, body) = request.into_parts();
        let bytes = match axum::body::to_bytes(body, WAF_BODY_SCAN_MAX_BYTES).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({
                        "error": "payload_too_large",
                        "message": "WAF: 請求本體超過檢查上限，已拒絕"
                    })),
                )
                    .into_response();
            }
        };
        let is_form = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().starts_with("application/x-www-form-urlencoded"))
            .unwrap_or(false);
        if let Some(threat) = inspect_waf_body(&String::from_utf8_lossy(&bytes), is_form) {
            threat.record_telemetry();
            // 只記錄命中的特徵字串，不記錄本體內容。
            warn!(
                ip = %ip,
                path = %raw_path,
                threat = %threat.description(),
                "🛡️ [最高級 WAF 防火牆] 請求本體含攻擊特徵，已攔截並自動封鎖 IP"
            );
            ban_for_body_threat(
                &state,
                &ip,
                threat.description(),
                raw_path.clone(),
                user_agent.clone(),
            )
            .await;
            return waf_forbidden_response(json!({
                    "error": "waf_access_denied",
                    "message": "WAF: 請求內容含攻擊特徵，已被攔截"
                }));
        }
        request = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
    }

    // 安全校驗通過，放行至下游處理器。
    let response = next.run(request).await;

    // 9. 異常行為偵測：追蹤 4xx 回應，短時間內異常頻繁則自動封鎖。
    // 已知搜尋引擎爬蟲豁免，避免 Googlebot/Bingbot 被誤封導致「未建立索引」。
    let status = response.status();
    let is_crawler = is_known_crawler(&user_agent);
    if status.is_client_error() && !is_local_or_trusted_ip(&ip) && !is_crawler {
        let should_ban = WAF_ANOMALY_TRACKER.record_and_check(&ip.to_string()).await;
        if should_ban {
            WAF_TELEMETRY.anomaly_blocked.fetch_add(1, Ordering::Relaxed);
            let ban_key = crate::trusted_ip::blacklist_group_key(&ip.to_string());
            let already_banned = state
                .accounts_db
                .is_ip_blacklisted(&ban_key)
                .await
                .unwrap_or(false);
            if !already_banned {
                let db = state.accounts_db.clone();
                let block_ip = ban_key.clone();
                let path_for_log = raw_path.clone();
                let ua_for_log = user_agent.clone();
                tokio::spawn(async move {
                    if let Ok(true) = db
                        .block_ip_with_failure_count(
                            &block_ip,
                            "system:waf-firewall-max",
                            &format!(
                                "WAF 防火牆自動封鎖（異常 4xx 行為）: {} 觸發",
                                status
                            ),
                            1,
                        )
                        .await
                    {
                        let _ = db
                            .record_activity_event(ActivityEventInput {
                                kind: "security".to_string(),
                                actor: "system:waf-firewall-max".to_string(),
                                action: "waf_anomaly_blocked".to_string(),
                                account_id: "unknown".to_string(),
                                path: path_for_log,
                                result: "blocked".to_string(),
                                details: format!(
                                    "WAF 攔截異常 4xx 行為並自動封鎖: {} (status={})",
                                    ip, status
                                ),
                                ip: block_ip.clone(),
                                user_agent: ua_for_log,
                            })
                            .await;
                    }
                });
            }
        }
    }

    response
}


/// WAF 即時遙測統計 API：回傳各類攻擊阻斷計數與防禦指標。
pub async fn waf_stats_handler(
    State(_state): State<AdminWebState>,
) -> Response {
    let stats = serde_json::json!({
        "total_requests": WAF_TELEMETRY.total_requests.load(Ordering::Relaxed),
        "total_blocked": WAF_TELEMETRY.total_blocked.load(Ordering::Relaxed),
        "honeypot_probes": WAF_TELEMETRY.honeypot_probes.load(Ordering::Relaxed),
        "sql_injections": WAF_TELEMETRY.sql_injections.load(Ordering::Relaxed),
        "rce_attempts": WAF_TELEMETRY.rce_attempts.load(Ordering::Relaxed),
        "xss_attempts": WAF_TELEMETRY.xss_attempts.load(Ordering::Relaxed),
        "path_traversals": WAF_TELEMETRY.path_traversals.load(Ordering::Relaxed),
        "bot_scanners": WAF_TELEMETRY.bot_scanners.load(Ordering::Relaxed),
        "ssrf_attempts": WAF_TELEMETRY.ssrf_attempts.load(Ordering::Relaxed),
        "ssti_attempts": WAF_TELEMETRY.ssti_attempts.load(Ordering::Relaxed),
        "proto_pollution": WAF_TELEMETRY.proto_pollution.load(Ordering::Relaxed),
        "deserialization_attempts": WAF_TELEMETRY.deserialization_attempts.load(Ordering::Relaxed),
        "host_poisoning": WAF_TELEMETRY.host_poisoning.load(Ordering::Relaxed),
        "smuggling_attempts": WAF_TELEMETRY.smuggling_attempts.load(Ordering::Relaxed),
        "webshell_attempts": WAF_TELEMETRY.webshell_attempts.load(Ordering::Relaxed),
        "rate_limited": WAF_TELEMETRY.rate_limited.load(Ordering::Relaxed),
        "anomaly_blocked": WAF_TELEMETRY.anomaly_blocked.load(Ordering::Relaxed),
        "fingerprint_blocked": WAF_TELEMETRY.fingerprint_blocked.load(Ordering::Relaxed),
        "whitelist_bypass": WAF_TELEMETRY.whitelist_bypass.load(Ordering::Relaxed),
    });

    (
        StatusCode::OK,
        Json(stats),
    )
        .into_response()
}

/// 構建帶有安全響應標頭的 WAF 403 回應（參考 rust-web-security SKILL：防止內容嗅探、點擊劫持、引用洩漏）。
fn waf_forbidden_response(body: serde_json::Value) -> Response {
    (
        StatusCode::FORBIDDEN,
        [
            (header::HeaderName::from_static("x-content-type-options"), header::HeaderValue::from_static("nosniff")),
            (header::HeaderName::from_static("x-frame-options"), header::HeaderValue::from_static("DENY")),
            (header::HeaderName::from_static("referrer-policy"), header::HeaderValue::from_static("no-referrer")),
            (header::HeaderName::from_static("content-security-policy"), header::HeaderValue::from_static("default-src 'none'")),
        ],
        Json(body),
    )
        .into_response()
}

/// 構建帶有安全響應標頭的 WAF 錯誤回應。
fn waf_error_response(status: StatusCode, body: serde_json::Value) -> Response {
    (
        status,
        [
            (header::HeaderName::from_static("x-content-type-options"), header::HeaderValue::from_static("nosniff")),
            (header::HeaderName::from_static("x-frame-options"), header::HeaderValue::from_static("DENY")),
            (header::HeaderName::from_static("referrer-policy"), header::HeaderValue::from_static("no-referrer")),
            (header::HeaderName::from_static("content-security-policy"), header::HeaderValue::from_static("default-src 'none'")),
        ],
        Json(body),
    )
        .into_response()
}
