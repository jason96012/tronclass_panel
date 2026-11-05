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

/// R-20：本體預算與逾時由設定檔 `[waf]` 區段提供；超出範圍會夾到允許範圍。
#[cfg(test)]
mod tests_r20_runtime_config {
    use super::super::engine::WafConfig;
    use crate::config::WafRuntimeConfig;
    use std::time::Duration;

    #[test]
    fn defaults_match_previous_hardcoded_values() {
        let c = WafConfig::from_runtime(&WafRuntimeConfig::default());
        assert_eq!(c.body_scan_budget_mb, 128);
        assert_eq!(c.body_scan_wait, Duration::from_secs(10));
        assert_eq!(c.body_read_timeout, Duration::from_secs(30));
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let c = WafConfig::from_runtime(&WafRuntimeConfig {
            body_scan_budget_mb: 0,
            body_scan_wait_secs: 999,
            body_read_timeout_secs: 1,
        });
        assert_eq!(c.body_scan_budget_mb, 1, "預算不得為 0（否則所有掃描都會 503）");
        assert_eq!(c.body_scan_wait, Duration::from_secs(120));
        assert_eq!(c.body_read_timeout, Duration::from_secs(5));
    }

    #[test]
    fn partial_toml_section_keeps_other_defaults() {
        let rt: WafRuntimeConfig = toml::from_str("body_scan_budget_mb = 256").expect("parse");
        assert_eq!(rt.body_scan_budget_mb, 256);
        assert_eq!(rt.body_scan_wait_secs, 10);
        assert_eq!(rt.body_read_timeout_secs, 30);
    }
}

