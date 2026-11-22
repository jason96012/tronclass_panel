//! 威脅類型定義與分類（WafThreat）。

use super::*;
use std::sync::atomic::Ordering;


// WAF_AUTO_BLOCK_NOTIFY_V2：WAF 新建立自動封鎖時，透過 notify_waf_auto_block 只發 Discord 給
// admin_user_id（同一 IP 24 小時去重），附「解除封鎖」按鈕。不發 LINE、不發 Web Push。

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
    /// WAF_DETECT_V2：Referer 或 Cookie 含結構化攻擊特徵。只回 403、不封鎖 IP：
    /// Referer 會帶入使用者的搜尋字串（例如 Google 搜尋 `union select`），誤判率高。
    HeaderPayload(String),
}

impl WafThreat {
    /// WAF_AUTOBAN_NARROW_V1：是否允許寫入永久黑名單。
    /// UA 類與中段副檔名類誤判率高（WAF 審查 W3／W2），命中時仍回 403，但不封鎖 IP，
    /// 避免一次誤判變成永久鎖定。其餘攻擊特徵維持原本的自動封鎖。
    pub fn allows_auto_ban(&self) -> bool {
        // WAF_AUTOBAN_NARROW_V1：UA 類與中段副檔名類誤判率高（WAF 審查 W3／W2），命中時仍回 403，但不封鎖 IP，
        // 避免一次誤判變成永久鎖定。其餘攻擊特徵維持原本的自動封鎖。
        !matches!(
            self,
            Self::MaliciousUserAgent(_) | Self::WebshellMasquerade(_) | Self::HeaderPayload(_)
        )
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
            Self::HeaderPayload(p) => format!("標頭攻擊載荷（Referer／Cookie）: {p}"),
        }
    }

    pub fn record_telemetry(&self, t: &WafTelemetry) {
        t.total_blocked.fetch_add(1, Ordering::Relaxed);
        match self {
            Self::HoneypotProbe(_) => {
                t.honeypot_probes.fetch_add(1, Ordering::Relaxed);
            }
            Self::PathTraversal(_) => {
                t.path_traversals.fetch_add(1, Ordering::Relaxed);
            }
            Self::RcePayload(_) => {
                t.rce_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::SqlInjection(_) => {
                t.sql_injections.fetch_add(1, Ordering::Relaxed);
            }
            Self::XssInjection(_) => {
                t.xss_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::SsrfPayload(_) => {
                t.ssrf_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::SstiPayload(_) => {
                t.ssti_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::PrototypePollution(_) => {
                t.proto_pollution.fetch_add(1, Ordering::Relaxed);
            }
            Self::DeserializationAttack(_) => {
                t.deserialization_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::HostHeaderPoisoning(_) => {
                t.host_poisoning.fetch_add(1, Ordering::Relaxed);
            }
            Self::RequestSmuggling(_) => {
                t.smuggling_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::WebshellMasquerade(_) => {
                t.webshell_attempts.fetch_add(1, Ordering::Relaxed);
            }
            Self::MaliciousUserAgent(_) => {
                t.bot_scanners.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}
