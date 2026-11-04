//! C5 測試：Content-Length 預檢與本體掃描記憶體預算（未經 cargo 編譯或執行，見 CHANGELOG）。

#[cfg(test)]
mod c5_tests {
    use super::super::engine::WafConfig;
    use super::super::*;
    use axum::http::{header, HeaderMap, HeaderValue};
    use std::time::Duration;

    #[test]
    fn content_length_parses_and_ignores_garbage() {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static(" 123 "));
        assert_eq!(declared_content_length(&h), Some(123));

        let mut g = HeaderMap::new();
        g.insert(header::CONTENT_LENGTH, HeaderValue::from_static("abc"));
        assert_eq!(declared_content_length(&g), None);
        assert_eq!(declared_content_length(&HeaderMap::new()), None);
    }

    #[test]
    fn units_round_up_and_are_at_least_one() {
        assert_eq!(body_scan_units(1), 1);
        assert_eq!(body_scan_units(4 * 1024 * 1024), 4);
        assert_eq!(body_scan_units(4 * 1024 * 1024 + 1), 5);
        assert_eq!(body_scan_units(32 * 1024 * 1024), 32);
    }

    #[test]
    fn default_budget_fits_largest_body_limit() {
        assert!(WafConfig::default().body_scan_budget_mb >= 32);
    }

    #[tokio::test]
    async fn budget_blocks_when_exhausted_and_releases_on_drop() {
        let cfg = WafConfig {
            body_scan_budget_mb: 4,
            body_scan_wait: Duration::from_millis(50),
            ..Default::default()
        };
        let waf = WafEngine::new(cfg);
        let first = acquire_body_scan_slot(&waf, 4 * 1024 * 1024).await;
        assert!(first.is_some());
        // 預算已用完：等不到，呼叫端必須回 503（不得放行）。
        assert!(acquire_body_scan_slot(&waf, 1024).await.is_none());
        drop(first);
        assert!(acquire_body_scan_slot(&waf, 1024).await.is_some());
    }
}
