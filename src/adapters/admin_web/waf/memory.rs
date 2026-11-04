//! 記憶體上限工具、大小限制常數與本機／私網判斷。

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;


// WAF_AUTO_BLOCK_NOTIFY_V2：WAF 新建立自動封鎖時，透過 notify_waf_auto_block 只發 Discord 給
// admin_user_id（同一 IP 24 小時去重），附「解除封鎖」按鈕。不發 LINE、不發 Web Push。

/// URI / Query 最大長度限制（防止緩衝區溢位攻擊）。
pub(super) const WAF_MAX_URI_LENGTH: usize = 4096;

/// 總體請求大小上限（防止超大請求耗盡記憶體與頻寬）。
pub(super) const WAF_MAX_REQUEST_SIZE: usize = 64 * 1024 * 1024;
/// 標頭欄位數量上限（防止標頭洪水攻擊）。
pub(super) const WAF_MAX_HEADERS_COUNT: usize = 50;
/// JSON 最大嵌套深度（防止 JSON 炸彈攻擊）。
pub(super) const WAF_MAX_JSON_DEPTH: usize = 10;
/// 表單欄位數量上限（防止表單欄位洪水攻擊）。
pub(super) const WAF_MAX_FORM_FIELDS: usize = 100;

/// WAF_MEMORY_BOUND_V1：把追蹤表壓回上限內。
/// 1) 先丟掉 `is_stale` 為真的項目；2) 若仍超過 `max_keys`，丟掉最久沒活動的項目，
/// 直到剩下 `max_keys` 的 3/4（遲滯：避免接近上限時每個請求都做整張表掃描）。
/// 被淘汰的 IP 只會重新計數，不影響已寫入資料庫的封鎖紀錄。
pub(super) fn bound_tracker<V>(
    map: &mut HashMap<String, V>,
    max_keys: usize,
    is_stale: impl Fn(&V) -> bool,
    last_active: impl Fn(&V) -> Option<Instant>,
) {
    if map.len() <= max_keys {
        return;
    }
    map.retain(|_, v| !is_stale(v));
    let target = max_keys / 4 * 3;
    if map.len() <= target {
        return;
    }
    let excess = map.len() - target;
    let mut by_age: Vec<(Option<Instant>, String)> = map
        .iter()
        .map(|(k, v)| (last_active(v), k.clone()))
        .collect();
    by_age.sort_by_key(|(t, _)| *t);
    for (_, key) in by_age.into_iter().take(excess) {
        map.remove(&key);
    }
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

#[cfg(test)]
mod memory_bound_tests {
    use super::*;
    use super::super::*;
    use std::time::Duration;

    #[test]
    fn bound_tracker_is_noop_under_cap() {
        let mut map: HashMap<String, Instant> = HashMap::new();
        let now = Instant::now();
        for i in 0..50 {
            map.insert(format!("k{i}"), now);
        }
        bound_tracker(&mut map, 100, |_| false, |t| Some(*t));
        assert_eq!(map.len(), 50);
    }

    #[test]
    fn bound_tracker_evicts_oldest_down_to_three_quarters() {
        let base = Instant::now();
        let mut map: HashMap<String, Instant> = HashMap::new();
        for i in 0..101u64 {
            map.insert(format!("k{i}"), base + Duration::from_millis(i));
        }
        bound_tracker(&mut map, 100, |_| false, |t| Some(*t));
        assert_eq!(map.len(), 75);
        // 最新的項目必須保留，最舊的必須被淘汰。
        assert!(map.contains_key("k100"));
        assert!(!map.contains_key("k0"));
    }

    #[test]
    fn bound_tracker_drops_stale_before_evicting_live() {
        let base = Instant::now();
        let mut map: HashMap<String, Instant> = HashMap::new();
        for i in 0..120u64 {
            // 前 60 筆已過期（stale），後 60 筆仍有效。
            map.insert(format!("k{i}"), base + Duration::from_millis(i));
        }
        bound_tracker(&mut map, 100, |t| *t < base + Duration::from_millis(60), |t| Some(*t));
        assert_eq!(map.len(), 60);
        assert!(map.contains_key("k119"));
    }

    #[tokio::test]
    async fn rate_limiter_rule_unchanged_240_allowed_then_denied() {
        let limiter = WafRateLimiter::new();
        for _ in 0..WAF_RATE_LIMIT_MAX_REQUESTS {
            assert!(limiter.check_limit("198.51.100.10").await);
        }
        assert!(!limiter.check_limit("198.51.100.10").await);
    }

    #[tokio::test]
    async fn rate_limiter_memory_is_capped() {
        let limiter = WafRateLimiter::new();
        for i in 0..(WAF_RATE_LIMIT_MAX_IPS + 2000) {
            limiter.check_limit(&format!("10.{}.{}.{}", (i >> 16) & 255, (i >> 8) & 255, i & 255)).await;
        }
        let len = limiter.attempts.lock().await.len();
        assert!(len <= WAF_RATE_LIMIT_MAX_IPS, "len={len}");
    }

    #[tokio::test]
    async fn anomaly_threshold_31st_hit_triggers_ban() {
        // WAF_ANOMALY_THRESHOLD_30_V1：門檻 30，第 31 次才封鎖。
        assert_eq!(WAF_ANOMALY_MAX_4XX, 30);
        let tracker = WafAnomalyTracker::new();
        for _ in 0..WAF_ANOMALY_MAX_4XX {
            assert!(!tracker.record_and_check("198.51.100.20").await);
        }
        assert!(tracker.record_and_check("198.51.100.20").await);
    }

    #[tokio::test]
    async fn anomaly_memory_is_capped() {
        let tracker = WafAnomalyTracker::new();
        for i in 0..(WAF_ANOMALY_MAX_KEYS + 3000) {
            tracker.record_and_check(&format!("172.16.{}.{}", (i >> 8) & 255, i & 255)).await;
        }
        let len = tracker.hits.lock().await.len();
        assert!(len <= WAF_ANOMALY_MAX_KEYS, "len={len}");
    }

    #[tokio::test]
    async fn fingerprint_threshold_unchanged_tenth_ip_is_suspicious() {
        let tracker = WafFingerprintTracker::new();
        let fp = "Mozilla/5.0 test|zh-TW|gzip|";
        for i in 0..(WAF_FINGERPRINT_IP_THRESHOLD - 1) {
            assert!(!tracker.record_and_check(fp, &format!("203.0.113.{i}")).await);
        }
        assert!(tracker.record_and_check(fp, "203.0.113.200").await);
        // 同一個 IP 重複出現不增加計數。
        assert!(tracker.record_and_check(fp, "203.0.113.200").await);
    }

    /// FINGERPRINT_GROUP_V1：同一 IPv6 /64 內的隱私位址輪換只算一個來源。
    #[tokio::test]
    async fn fingerprint_privacy_address_rotation_in_one_64_counts_once() {
        let tracker = WafFingerprintTracker::new();
        let fp = "iphone-privacy-rotation";
        let mut flagged = false;
        for i in 0..(WAF_FINGERPRINT_IP_THRESHOLD * 3) {
            let addr = format!("2401:e180:8dd0:68df:{:x}:{:x}:{:x}:{:x}", i, i * 7 + 1, i * 13 + 2, i * 31 + 3);
            flagged |= tracker.record_and_check(fp, &addr).await;
        }
        assert!(!flagged, "同一 /64 的位址輪換不應觸發指紋封鎖");
    }

    /// FINGERPRINT_GROUP_V1：十個不同 /64 仍然會被判定為輪換嫌疑（攻擊偵測保留）。
    #[tokio::test]
    async fn fingerprint_ten_distinct_64_groups_still_flagged() {
        let tracker = WafFingerprintTracker::new();
        let fp = "rotating-attacker";
        let mut last = false;
        for i in 0..WAF_FINGERPRINT_IP_THRESHOLD {
            let addr = format!("2001:db8:{:x}::1", i + 1);
            last = tracker.record_and_check(fp, &addr).await;
        }
        assert!(last);
    }

    #[tokio::test]
    async fn fingerprint_ten_ips_within_window_still_flagged() {
        let tracker = WafFingerprintTracker::new();
        let fp = "iphone-window-flag";
        let t0 = Instant::now();
        let mut last = false;
        for i in 0..WAF_FINGERPRINT_IP_THRESHOLD {
            last = tracker
                .record_and_check_at(fp, &format!("203.0.113.{i}"), t0 + Duration::from_secs(60 * i as u64))
                .await;
        }
        assert!(last);
    }

    #[tokio::test]
    async fn fingerprint_ips_outside_window_do_not_count() {
        // 10 個 IP 每 400 秒出現一次，跨越 1 小時以上；任一時刻窗內最多 9 個，不應封鎖。
        let tracker = WafFingerprintTracker::new();
        let fp = "iphone-window-spread";
        let t0 = Instant::now();
        let mut last = false;
        for i in 0..10u64 {
            last = tracker
                .record_and_check_at(fp, &format!("198.18.0.{i}"), t0 + Duration::from_secs(400 * i))
                .await;
        }
        assert!(!last);
    }

    #[tokio::test]
    async fn fingerprint_old_history_expires_after_window() {
        // 舊的 9 個 IP 過期後，同一指紋的新 IP 不會因為歷史累積而被封鎖（修正前的誤判情境）。
        let tracker = WafFingerprintTracker::new();
        let fp = "iphone-old-history";
        let t0 = Instant::now();
        for i in 0..9 {
            tracker.record_and_check_at(fp, &format!("203.0.113.{i}"), t0).await;
        }
        let later = t0 + Duration::from_secs(2 * 3600);
        assert!(!tracker.record_and_check_at(fp, "203.0.113.200", later).await);
    }

    #[tokio::test]
    async fn fingerprint_per_key_ip_list_is_capped_but_decision_holds() {
        let tracker = WafFingerprintTracker::new();
        let fp = "shared-browser-ua";
        let mut last = false;
        for i in 0..200 {
            last = tracker.record_and_check(fp, &format!("192.0.2.{}", i % 250)).await;
        }
        assert!(last);
        let map = tracker.seen.lock().await;
        assert_eq!(map.get(fp).map(|e| e.ips.len()), Some(WAF_FINGERPRINT_MAX_IPS_PER_KEY));
    }

    #[tokio::test]
    async fn fingerprint_memory_is_capped_and_keys_truncated() {
        let tracker = WafFingerprintTracker::new();
        for i in 0..(WAF_FINGERPRINT_MAX_KEYS + 3000) {
            let fp = format!("ua-{i}-{}", "x".repeat(2000));
            tracker.record_and_check(&fp, "198.51.100.30").await;
        }
        let map = tracker.seen.lock().await;
        assert!(map.len() <= WAF_FINGERPRINT_MAX_KEYS, "len={}", map.len());
        assert!(map.keys().all(|k| k.len() <= WAF_FINGERPRINT_KEY_MAX_BYTES));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "中".repeat(300); // 900 bytes
        let t = truncate_fingerprint_key(&s);
        assert!(t.len() <= WAF_FINGERPRINT_KEY_MAX_BYTES);
        assert!(s.starts_with(&t));
    }
}
