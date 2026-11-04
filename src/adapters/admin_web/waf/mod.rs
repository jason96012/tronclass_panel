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
//!
//! WAF_MODULE_SPLIT_V1（A1 模組化拆分，行為不變）。
//!
//! 結構：
//! - `threat`：威脅類型（WafThreat）。
//! - `telemetry`：遙測計數器與統計 API。
//! - `rate_limit`：L7 全域速率限制。
//! - `anomaly`：每 IP 4xx 異常計數與自動封鎖門檻。
//! - `fingerprint`：請求指紋（IPv6 以 /64 群組計數）。
//! - `memory`：記憶體上限與本機／私網判斷。
//! - `whitelist`：白名單（IP、CIDR、路徑前綴）。
//! - `body_scan`：請求本體掃描與本體上限分級。
//! - `signatures`：靜態特徵庫。
//! - `detect`：威脅檢測主邏輯。
//! - `middleware`：中介層組裝與錯誤回應。
//!
//! 對外僅重導出 `waf_firewall_middleware`、`waf_stats_handler`；其餘為模組內部可見（`pub(super)`）。

mod anomaly;
mod body_scan;
mod detect;
mod engine;
mod fingerprint;
mod memory;
mod middleware;
mod rules;
mod ssrf;
mod rate_limit;
mod signatures;
mod telemetry;
mod threat;
mod whitelist;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_corpus;
#[cfg(test)]
mod tests_detector;
#[cfg(test)]
mod tests_engine;
#[cfg(test)]
mod tests_rules;
#[cfg(test)]
mod tests_c5;
#[cfg(test)]
mod tests_bypass;
#[cfg(test)]
mod tests_parity;

#[allow(unused_imports)]
use anomaly::*;
#[allow(unused_imports)]
use body_scan::*;
#[allow(unused_imports)]
use detect::*;
#[allow(unused_imports)]
use fingerprint::*;
#[allow(unused_imports)]
use memory::*;
#[allow(unused_imports)]
use rate_limit::*;
#[allow(unused_imports)]
use signatures::*;
#[allow(unused_imports)]
use telemetry::*;
#[allow(unused_imports)]
use threat::*;
#[allow(unused_imports)]
use whitelist::*;

pub use middleware::waf_firewall_middleware;
pub use telemetry::waf_stats_handler;
pub use memory::is_local_or_trusted_ip;
pub use threat::WafThreat;
pub use engine::WafEngine;
pub use rules::load_rules_from_env;
