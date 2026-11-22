//! A2 引擎測試：實例間狀態隔離；門檻來自 WafConfig；預設值與舊常數一致。

#[cfg(test)]
mod engine_tests {
    //! A2：每個 WafEngine 實例擁有獨立狀態；門檻來自 WafConfig；預設值與舊常數一致。
    use super::super::*;
    use super::super::engine::WafConfig;
    use axum::http::{HeaderMap, Method, Uri};
    use std::sync::atomic::Ordering;
    use std::str::FromStr;
    use std::time::Duration;

    #[test]
    fn default_config_matches_legacy_constants() {
        let c = WafConfig::default();
        assert_eq!(c.rate_limit_max_requests, 240);
        assert_eq!(c.rate_limit_window, Duration::from_secs(60));
        assert_eq!(c.rate_limit_max_ips, 5000);
        assert_eq!(c.anomaly_max_4xx, 30);
        assert_eq!(c.anomaly_window, Duration::from_secs(300));
        assert_eq!(c.anomaly_max_keys, 10_000);
        assert_eq!(c.fingerprint_ip_threshold, 10);
        assert_eq!(c.fingerprint_window, Duration::from_secs(3600));
        assert_eq!(c.fingerprint_max_ips_per_key, 32);
        assert_eq!(c.fingerprint_key_max_bytes, 512);
        assert_eq!(c.fingerprint_max_keys, 10_000);
    }

    #[test]
    fn two_engines_do_not_share_telemetry() {
        let a = WafEngine::default();
        let b = WafEngine::default();
        let uri = Uri::from_str("/.env").expect("uri");
        assert!(a.inspect_threat(&uri, &Method::GET, &HeaderMap::new()).is_some());
        assert_eq!(a.telemetry.total_requests.load(Ordering::Relaxed), 1);
        assert_eq!(b.telemetry.total_requests.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn rate_limit_threshold_comes_from_config_and_is_per_instance() {
        let cfg = WafConfig { rate_limit_max_requests: 3, ..WafConfig::default() };
        let limited = WafEngine::new(cfg);
        for _ in 0..3 {
            assert!(limited.rate_limiter.check_limit("203.0.113.9").await);
        }
        assert!(!limited.rate_limiter.check_limit("203.0.113.9").await);
        // 另一個引擎的計數獨立，不受影響。
        assert!(WafEngine::default().rate_limiter.check_limit("203.0.113.9").await);
    }

    #[tokio::test]
    async fn anomaly_threshold_comes_from_config() {
        let e = WafEngine::new(WafConfig { anomaly_max_4xx: 2, ..WafConfig::default() });
        assert!(!e.anomaly.record_and_check("198.51.100.7").await);
        assert!(!e.anomaly.record_and_check("198.51.100.7").await);
        assert!(e.anomaly.record_and_check("198.51.100.7").await);
    }

    #[tokio::test]
    async fn fingerprint_threshold_comes_from_config() {
        let e = WafEngine::new(WafConfig { fingerprint_ip_threshold: 3, ..WafConfig::default() });
        let fp = "ua|en|gzip|sec";
        assert!(!e.fingerprint.record_and_check(fp, "198.51.100.1").await);
        assert!(!e.fingerprint.record_and_check(fp, "198.51.100.2").await);
        assert!(e.fingerprint.record_and_check(fp, "198.51.100.3").await);
        // 預設引擎門檻 10，同樣三個 IP 不會觸發。
        let d = WafEngine::default();
        assert!(!d.fingerprint.record_and_check(fp, "198.51.100.1").await);
        assert!(!d.fingerprint.record_and_check(fp, "198.51.100.2").await);
        assert!(!d.fingerprint.record_and_check(fp, "198.51.100.3").await);
    }
}
