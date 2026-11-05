//! 異常行為偵測：每 IP 4xx 計數與自動封鎖門檻。

use super::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

// WAF_AUTO_BLOCK_NOTIFY_V2：WAF 新建立自動封鎖時，透過 notify_waf_auto_block 只發 Discord 給
// admin_user_id（同一 IP 24 小時去重），附「解除封鎖」按鈕。不發 LINE、不發 Web Push。

/// WAF_MEMORY_BOUND_V1：異常行為追蹤表的鍵數硬上限（數值沿用原本的清理閾值）。
pub(super) const WAF_ANOMALY_MAX_KEYS: usize = 10_000;
/// WAF 異常行為偵測：單一 IP 在視窗內觸發 404/403 的最大次數，超過即自動封鎖。
/// WAF_ANOMALY_THRESHOLD_30_V1：門檻由 15 調整為 30（使用者於 2026-11-03 決定）。
/// 第 31 次 4xx 才自動封鎖；判斷規則本身（滑動窗口、排除 403／白名單）未變。
pub(super) const WAF_ANOMALY_MAX_4XX: usize = 30;
/// WAF_ANOMALY_401_SEPARATE_V1：401（session 過期）另計、獨立門檻（使用者 2026-11-04 決定）。
/// 401 不計入 4xx 門檻，避免 session 過期的正常用戶（例如教室 NAT 出口）被連帶封鎖；
/// 大量 401 的掃描仍會在此門檻被封鎖。
/// ⚠ 120 是暫定值（4 倍於 4xx 門檻），尚未以正式站 web_logs 量測，部署前需確認。
pub(super) const WAF_ANOMALY_MAX_401: usize = 120;
const _: () = assert!(WAF_ANOMALY_MAX_401 > WAF_ANOMALY_MAX_4XX);
/// WAF 異常行為偵測：視窗長度（秒）。
pub(super) const WAF_ANOMALY_WINDOW_SECS: u64 = 300;
// 異常 IP 黑名單保留時間（30 分鐘）定義在 `db/blacklist.rs`，
// 由 `AccountsDb::is_ip_blacklisted` 強制到期；這裡不再重複定義。
/// 已知搜尋引擎爬蟲 User-Agent 關鍵字，豁免異常行為偵測與自動封鎖。
pub(super) const KNOWN_CRAWLER_UAS: &[&str] = &[
    "googlebot", "bingbot", "slurp", "duckduckbot", "baidubot",
    "yandexbot", "sogou", "exabot", "facebot", "twitterbot",
];
/// WAF_ANOMALY_WHITELIST_V1：以下路徑預期會回傳 4xx（如地理封鎖頁、黑名單頁的聯絡管理員端點），
/// 不應計入異常行為偵測。與 `reject_blacklisted_ip_middleware`、`reject_foreign_admin_ip_middleware`、
/// `reject_hosting_ip_middleware` 的豁免清單保持一致。
pub(super) const WAF_ANOMALY_WHITELIST_PATHS: &[&str] = &[
    "/admin/api/geo-restrict/request-access",
    "/admin/api/geo-restrict/access-status",
    "/admin/api/blacklist/status",
];

/// 判斷路徑是否在異常行為偵測白名單中。
pub(super) fn waf_anomaly_whitelisted(path: &str) -> bool {
    WAF_ANOMALY_WHITELIST_PATHS.iter().any(|&p| path == p || path.starts_with(&format!("{}/", p)))
}

/// R-05：CSP 違規回報端點（瀏覽器只會 POST 到這裡）。這個端點的 4xx（路由缺失時的 404、GET 的 405）
/// 不是攻擊信號，不計入異常次數，也不算指紋信號。端點本身已有限流（每群組每分鐘 20 筆、
/// 全站主控台 300 筆、寫入 DB 10 筆，見 `health.rs` `csp_report_handler`）。
pub(super) const WAF_ANOMALY_REPORT_PATHS: &[&str] = &["/csp-report"];

/// 路徑是否為 CSP 回報端點（完整比對，子路徑不豁免）。
pub(super) fn waf_anomaly_report_path(path: &str) -> bool {
    WAF_ANOMALY_REPORT_PATHS.contains(&path)
}

/// 異常計數與指紋信號共用的排除判斷：既有白名單路徑，或 CSP 回報端點（R-05）。
pub(super) fn waf_anomaly_excluded_path(path: &str) -> bool {
    waf_anomaly_whitelisted(path) || waf_anomaly_report_path(path)
}

/// 異常行為偵測：追蹤每個 IP 的 4xx 觸發時間點（滑動窗口）。
#[derive(Clone)]
pub struct WafAnomalyTracker {
    pub(super) hits: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    pub(super) max_4xx: usize,
    pub(super) window: Duration,
    pub(super) max_keys: usize,
}

impl Default for WafAnomalyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl WafAnomalyTracker {
    pub fn new() -> Self {
        Self::with_limits(WAF_ANOMALY_MAX_4XX, Duration::from_secs(WAF_ANOMALY_WINDOW_SECS), WAF_ANOMALY_MAX_KEYS)
    }

    /// 可注入上限的建構子（A2）。
    pub fn with_limits(max_4xx: usize, window: Duration, max_keys: usize) -> Self {
        Self {
            hits: Arc::new(Mutex::new(HashMap::new())),
            max_4xx,
            window,
            max_keys,
        }
    }

    /// 記錄一次 4xx 觸發，若超過閾值則回傳 true（應封鎖）。
    pub async fn record_and_check(&self, ip: &str) -> bool {
        let mut map = self.hits.lock().await;
        let now = Instant::now();
        let window_start = now
            .checked_sub(self.window)
            .unwrap_or(now);

        // 在區塊內計算 should_ban，讓可變借用在 map.len() 檢查前結束
        let should_ban = {
            let entry = map.entry(ip.to_string()).or_default();
            entry.retain(|t| *t > window_start);
            entry.push(now);
            entry.len() > self.max_4xx
        };

        // 清理過舊 IP，控制記憶體（WAF_MEMORY_BOUND_V1：硬上限）
        bound_tracker(
            &mut *map,
            self.max_keys,
            |v| v.last().map_or(true, |t| *t <= window_start),
            |v| v.last().copied(),
        );

        should_ban
    }

    pub async fn clear(&self, ip: &str) {
        let mut map = self.hits.lock().await;
        map.remove(ip);
    }
}

/// 判斷是否為已知搜尋引擎爬蟲，豁免異常行為偵測與自動封鎖。
pub(super) fn is_known_crawler(user_agent: &str) -> bool {
    let ua = user_agent.to_ascii_lowercase();
    KNOWN_CRAWLER_UAS.iter().any(|&crawler| ua.contains(crawler))
}

#[cfg(test)]
mod tests_401_separate {
    use super::*;

    /// 401 與其他 4xx 各自累計：每條都在自己的門檻才會封鎖。
    #[tokio::test]
    async fn limit_is_checked_per_tracker_and_strictly_above_threshold() {
        let tracker = WafAnomalyTracker::with_limits(2, Duration::from_secs(300), 10);
        assert!(!tracker.record_and_check("203.0.113.7").await, "第 1 次不封鎖");
        assert!(!tracker.record_and_check("203.0.113.7").await, "第 2 次不封鎖（門檻為超過即封）");
        assert!(tracker.record_and_check("203.0.113.7").await, "第 3 次超過門檻才封鎖");
    }

    #[test]
    fn default_401_limit_is_looser_than_4xx_limit() {
        assert!(WAF_ANOMALY_MAX_401 > WAF_ANOMALY_MAX_4XX);
        assert_eq!(super::super::engine::WafConfig::default().anomaly_max_401, WAF_ANOMALY_MAX_401);
    }
}

#[cfg(test)]
mod tests_anomaly_group_key {
    use super::*;

    /// R-01：同一 IPv6 /64 的隱私位址輪換共用一個異常預算（與封鎖單位一致）。
    #[tokio::test]
    async fn ipv6_addresses_in_one_slash64_share_the_budget() {
        let tracker = WafAnomalyTracker::with_limits(2, Duration::from_secs(300), 10);
        let key = |addr: &str| crate::trusted_ip::blacklist_group_key(addr);
        assert!(!tracker.record_and_check(&key("2001:db8:1:2:aaaa:bbbb:cccc:0001")).await);
        assert!(!tracker.record_and_check(&key("2001:db8:1:2:dead:beef:0000:0002")).await);
        assert!(
            tracker.record_and_check(&key("2001:db8:1:2:1234:5678:9abc:0003")).await,
            "同一 /64 的第 3 次 4xx 應觸發封鎖"
        );
    }

    /// R-01：不同 /64 仍各自計數，IPv4 不受影響。
    #[tokio::test]
    async fn different_groups_and_ipv4_are_counted_separately() {
        let tracker = WafAnomalyTracker::with_limits(2, Duration::from_secs(300), 10);
        let key = |addr: &str| crate::trusted_ip::blacklist_group_key(addr);
        assert!(!tracker.record_and_check(&key("2001:db8:1:2::1")).await);
        assert!(!tracker.record_and_check(&key("2001:db8:9:9::1")).await);
        assert!(!tracker.record_and_check(&key("203.0.113.7")).await);
        assert!(!tracker.record_and_check(&key("203.0.113.8")).await);
    }
}

#[cfg(test)]
mod tests_report_path {
    //! R-05：CSP 回報端點的 4xx 不計入異常，也不算指紋信號。尚未編譯與執行。
    use super::*;

    #[test]
    fn csp_report_endpoint_is_excluded_by_exact_path_only() {
        assert!(waf_anomaly_report_path("/csp-report"));
        assert!(waf_anomaly_excluded_path("/csp-report"));
        assert!(!waf_anomaly_excluded_path("/csp-report/extra"), "子路徑不豁免");
        assert!(!waf_anomaly_excluded_path("/csp-reports"), "相近名稱不豁免");
        assert!(!waf_anomaly_excluded_path("/courses"), "一般路徑不豁免");
    }

    #[test]
    fn existing_whitelist_still_excluded() {
        assert!(waf_anomaly_excluded_path("/admin/api/blacklist/status"));
        assert!(waf_anomaly_excluded_path("/admin/api/blacklist/status/x"));
    }
}
