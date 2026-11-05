//! WAF 回歸測試：接線與本體上限（WAF_DETECT_V2、SSTI／原型鏈上限）。

#[cfg(test)]
mod waf_detect_wiring_tests {
    //! WAF_DETECT_V2 接線測試：確認 URL、Referer、本體三條路徑都會呼叫結構化偵測。
    use super::super::*;
    use axum::http::{header, HeaderMap, HeaderValue, Method, Uri};

    fn threat(path_and_query: &str, headers: &HeaderMap) -> Option<WafThreat> {
        let uri: Uri = path_and_query.parse().expect("test uri");
        inspect_waf_threat(&uri, &Method::GET, headers)
    }

    #[test]
    fn url_structured_sqli_and_xss_are_blocked() {
        let h = HeaderMap::new();
        assert!(matches!(
            threat("/courses?id=1%27%20or%20%271%27%3D%271", &h),
            Some(WafThreat::SqlInjection(_))
        ));
        assert!(matches!(
            threat("/search?q=%3Cdetails%20open%20ontoggle%3Dalert(1)%3E", &h),
            Some(WafThreat::XssInjection(_))
        ));
    }

    #[test]
    fn referer_and_cookie_are_scanned() {
        let mut h = HeaderMap::new();
        h.insert(header::REFERER, HeaderValue::from_static("https://example.com/?x=1' or 1=1"));
        let hit = threat("/courses", &h);
        assert!(matches!(hit, Some(WafThreat::HeaderPayload(_))));
        assert!(!hit.unwrap().allows_auto_ban(), "標頭命中只回 403，不封鎖 IP");

        let mut h2 = HeaderMap::new();
        h2.insert(header::COOKIE, HeaderValue::from_static("note=%3Csvg%20onload%3Dalert(1)%3E"));
        assert!(matches!(threat("/courses", &h2), Some(WafThreat::HeaderPayload(_))));

        // 搜尋引擎帶入的 Referer（使用者搜尋 union select）：會回 403，但不得封鎖。
        let mut h3 = HeaderMap::new();
        h3.insert(header::REFERER, HeaderValue::from_static("https://www.google.com/search?q=union+select"));
        assert!(matches!(threat("/courses", &h3), Some(WafThreat::HeaderPayload(_))));
    }

    #[test]
    fn ordinary_requests_are_not_blocked() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("session=abc123; theme=dark"));
        assert!(threat("/courses?page=2&sort=name", &h).is_none());
    }

    #[test]
    fn body_structured_detection_and_prose_allowance() {
        assert!(matches!(
            inspect_waf_body("name=x; drop table users", false),
            Some(WafThreat::BodyPayload(_))
        ));
        assert!(matches!(
            inspect_waf_body("{\"text\":\"<svg onload=alert(1)>\"}", false),
            Some(WafThreat::BodyPayload(_))
        ));
        assert!(inspect_waf_body("Fix the bugs; update the docs before Friday", false).is_none());
        assert!(inspect_waf_body("{\"text\":\"第三週作業已完成\"}", false).is_none());
    }
}

#[cfg(test)]
mod ssti_proto_limit_tests {
    //! WAF_SSTI_PROTO_SCOPED_V1 與 WAF_BODY_LIMIT_TIER_V1 的接線測試。
    use super::super::*;
    use axum::http::{HeaderMap, Method, Uri};

    const MB: usize = 1024 * 1024;

    #[test]
    fn body_limit_tiers_by_path() {
        assert_eq!(waf_body_scan_limit("/admin/api/action"), 4 * MB);
        assert_eq!(waf_body_scan_limit("/admin/api/login"), 4 * MB);
        // R-22（2026-11-05）：AI 聊天 20MB；其餘 AI 端點、TC 代理的 JSON／表單／文字 4MB。
        assert_eq!(waf_body_scan_limit("/admin/api/ai/opencode/chat"), 20 * MB);
        assert_eq!(waf_body_scan_limit("/admin/api/ai/opencode/config"), 4 * MB);
        assert_eq!(waf_body_scan_limit("/admin/api/ai/summarize"), 4 * MB);
        assert_eq!(waf_body_scan_limit("/admin/api/files/upload"), 32 * MB);
        assert_eq!(waf_body_scan_limit("/admin/api/homework/submit"), 32 * MB);
        assert_eq!(waf_body_scan_limit("/tc/x"), 4 * MB);
        assert_eq!(waf_body_scan_limit("/api/v1/items"), 4 * MB);
        assert_eq!(waf_body_scan_limit("/course/1/x"), 4 * MB);
        assert_eq!(waf_body_scan_limit("/static/app.js"), 4 * MB);
    }

    #[test]
    fn iclass_body_scan_exemption_is_prefix_scoped() {
        // 2026-11-05（A）：只有 iClass 代理前綴略過本體特徵比對；其餘路徑照常掃描。
        assert!(waf_body_scan_exempt("/tc/course/1/x"));
        assert!(waf_body_scan_exempt("/api/courses/1"));
        assert!(waf_body_scan_exempt("/course/1/learning-activity"));
        assert!(waf_body_scan_exempt("/bulletin-list/"));
        assert!(!waf_body_scan_exempt("/admin/api/ai/opencode/chat"));
        assert!(!waf_body_scan_exempt("/admin/api/login"));
        assert!(!waf_body_scan_exempt("/tcx"));
        assert!(!waf_body_scan_exempt("/apix/data"));
    }

    #[test]
    fn body_limit_prefix_uses_directory_boundary() {
        // 前綴不得借用：`/admin/api/aifoo` 不是 AI 路徑，`/tcx` 不是 TC 代理路徑。
        assert_eq!(waf_body_scan_limit("/admin/api/aifoo"), 4 * MB);
        assert_eq!(waf_body_scan_limit("/tcx"), 4 * MB);
        assert_eq!(waf_body_scan_limit("/apix/data"), 4 * MB);
    }

    #[test]
    fn url_structured_ssti_and_proto_are_wired() {
        let h = HeaderMap::new();
        let ssti: Uri = "/search?q=%24%7BT(java.lang.Runtime)%7D".parse().unwrap();
        match inspect_waf_threat(&ssti, &Method::GET, &h) {
            Some(WafThreat::SstiPayload(d)) => assert!(d.contains("結構化"), "{d}"),
            other => panic!("expected structured SSTI, got {other:?}"),
        }
        let proto: Uri = "/courses?constructor%5Bprototype%5D%5Brole%5D=admin".parse().unwrap();
        match inspect_waf_threat(&proto, &Method::GET, &h) {
            Some(WafThreat::PrototypePollution(d)) => assert!(d.contains("結構化"), "{d}"),
            other => panic!("expected structured prototype pollution, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_url_is_not_flagged_by_new_rules() {
        let h = HeaderMap::new();
        let u: Uri = "/courses?page=2&q=%E4%BD%9C%E6%A5%AD".parse().unwrap();
        assert!(inspect_waf_threat(&u, &Method::GET, &h).is_none());
    }

    #[test]
    fn body_structured_ssti_and_proto_are_wired() {
        match inspect_waf_body("{\"constructor\":{\"prototype\":{\"role\":\"admin\"}}}", false) {
            Some(WafThreat::BodyPayload(d)) => assert!(d.contains("結構化"), "{d}"),
            other => panic!("expected structured body pollution, got {other:?}"),
        }
        match inspect_waf_body("{\"t\":\"${T(java.lang.Runtime).getRuntime()}\"}", false) {
            Some(WafThreat::BodyPayload(d)) => assert!(d.contains("結構化"), "{d}"),
            other => panic!("expected structured body SSTI, got {other:?}"),
        }
        assert!(inspect_waf_body("{\"t\":\"請把 {{name}} 換成學生姓名\"}", false).is_none());
    }
}
