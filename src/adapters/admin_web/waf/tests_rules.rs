//! A4 規則基線測試（WAF_RULES_V1）。注意：本檔未經 cargo 編譯或執行，見 CHANGELOG。

#[cfg(test)]
mod rules_tests {
    use super::super::engine::WafEngine;
    use super::super::rules::{RulesSource, WafRules};
    use super::super::*;
    use axum::http::{header, HeaderMap, HeaderValue, Uri};
    use std::str::FromStr;

    fn uri(s: &str) -> Uri {
        Uri::from_str(s).expect("test uri")
    }

    fn ua(value: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::USER_AGENT, HeaderValue::from_static(value));
        h
    }

    #[test]
    fn valid_file_is_normalized_and_deduplicated() {
        let r = WafRules::parse(
            "version = 2\nhoneypot_paths = [\"/Secret-Admin\", \"/secret-admin\"]\nuser_agents = [\"  EvilBot \"]\n",
        )
        .expect("valid");
        assert_eq!(r.version, 2);
        assert_eq!(r.source, RulesSource::File);
        assert_eq!(r.honeypot_paths, vec!["/secret-admin".to_string()]);
        assert_eq!(r.user_agents, vec!["evilbot".to_string()]);
    }

    #[test]
    fn unknown_field_rejects_whole_file() {
        assert!(WafRules::parse("version = 1\nbogus = 1\n").is_err());
    }

    #[test]
    fn version_zero_is_rejected() {
        assert!(WafRules::parse("version = 0\n").is_err());
    }

    #[test]
    fn short_or_relative_paths_are_rejected() {
        assert!(WafRules::parse("version = 1\nhoneypot_paths = [\"/a\"]\n").is_err());
        assert!(WafRules::parse("version = 1\nhoneypot_paths = [\"secret\"]\n").is_err());
    }

    #[test]
    fn too_many_entries_are_rejected() {
        let items: Vec<String> = (0..501).map(|i| format!("\"/p{i:04}\"")).collect();
        let text = format!("version = 1\nhoneypot_paths = [{}]\n", items.join(","));
        assert!(WafRules::parse(&text).is_err());
    }

    #[test]
    fn honeypot_rule_matches_path_and_children_case_insensitively() {
        let r = WafRules::parse("version = 1\nhoneypot_paths = [\"/secret-admin\"]\n").expect("valid");
        let none = HeaderMap::new();
        assert!(matches!(r.check(&uri("/secret-admin/login"), &none), Some(WafThreat::HoneypotProbe(_))));
        assert!(matches!(r.check(&uri("/Secret-Admin"), &none), Some(WafThreat::HoneypotProbe(_))));
        assert!(r.check(&uri("/secret-adminx"), &none).is_none());
    }

    #[test]
    fn user_agent_rule_matches_substring() {
        let r = WafRules::parse("version = 1\nuser_agents = [\"evilbot\"]\n").expect("valid");
        assert!(matches!(r.check(&uri("/"), &ua("EvilBot/1.0")), Some(WafThreat::MaliciousUserAgent(_))));
        assert!(r.check(&uri("/"), &ua("Mozilla/5.0")).is_none());
    }

    #[test]
    fn builtin_rules_add_nothing() {
        assert!(WafRules::builtin().check(&uri("/secret-admin"), &HeaderMap::new()).is_none());
    }

    #[test]
    fn missing_file_falls_back_to_builtin_silently() {
        let (r, warn) = WafRules::load_or_builtin(std::path::Path::new("/nonexistent/waf_rules_test.toml"));
        assert_eq!(r.source, RulesSource::Builtin);
        assert!(warn.is_none());
    }

    #[test]
    fn invalid_file_falls_back_to_builtin_with_warning() {
        let path = std::env::temp_dir().join(format!("waf_rules_test_{}.toml", std::process::id()));
        std::fs::write(&path, "version = 1\nbogus = true\n").expect("write");
        let (r, warn) = WafRules::load_or_builtin(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(r.source, RulesSource::Builtin);
        assert!(warn.is_some());
    }

    #[test]
    fn engine_with_rules_detects_extra_path_and_default_engine_does_not() {
        let rules = WafRules::parse("version = 1\nhoneypot_paths = [\"/secret-admin\"]\n").expect("valid");
        let with = WafEngine::with_rules(Default::default(), rules);
        let plain = WafEngine::default();
        let none = HeaderMap::new();
        assert!(with.inspect_threat(&uri("/secret-admin"), &axum::http::Method::GET, &none).is_some());
        assert!(plain.inspect_threat(&uri("/secret-admin"), &axum::http::Method::GET, &none).is_none());
    }

    #[test]
    fn rules_cannot_disable_builtin_detection() {
        // 內建蜜罐 /.env 必須仍然命中，即使附加規則檔存在且合法。
        let rules = WafRules::parse("version = 1\nhoneypot_paths = [\"/secret-admin\"]\n").expect("valid");
        let with = WafEngine::with_rules(Default::default(), rules);
        assert!(with
            .inspect_threat(&uri("/.env"), &axum::http::Method::GET, &HeaderMap::new())
            .is_some());
    }
}
