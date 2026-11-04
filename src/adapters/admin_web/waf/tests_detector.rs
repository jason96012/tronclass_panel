//! A3 檢測器單元測試：每個檢測器可獨立測試；鏈順序鎖定；組合器第一命中即回傳。

#[cfg(test)]
mod detector_tests {
    //! A3：每個檢測器可獨立單元測試；鏈的順序鎖定；組合器第一個命中即回傳。
    use super::super::detect::*;
    use super::super::*;
    use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Uri};
    use std::str::FromStr;

    fn one_header(name: HeaderName, value: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_static(value));
        h
    }

    /// 與語料測試相同：非安全字元先百分比編碼（既有 %XX 保留），再解析為 Uri。
    fn enc(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b"/?=&._-%:;,*()+@~".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    fn run(det: &dyn ThreatDetector, path: &str, method: &Method, headers: &HeaderMap) -> Option<WafThreat> {
        let uri = Uri::from_str(&enc(path)).expect("test uri");
        let ctx = DetectionContext::new(&uri, method, headers);
        det.detect(&ctx)
    }

    #[test]
    fn chain_order_is_fixed() {
        assert_eq!(
            CompositeDetector::standard().names(),
            vec![
                "dangerous_method",
                "oversized_uri",
                "bypass_header",
                "host_header",
                "request_smuggling",
                "malicious_user_agent",
                "honeypot",
                "dangerous_extension",
                "webshell_masquerade",
                "path_traversal",
                "null_byte",
                "crlf_injection",
                "ssrf_metadata",
                "ssrf_private_network",
                "prototype_pollution",
                "prototype_structured",
                "deserialization",
                "ssti_plain",
                "ssti_structured",
                "xss_plain",
                "xss_structured",
                "rce",
                "sql_keyword",
                "sql_structured",
                "header_payload",
            ]
        );
    }

    #[test]
    fn detector_names_are_unique_and_at_least_ten() {
        let names = CompositeDetector::standard().names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
        assert!(names.len() >= 10);
    }

    #[test]
    fn every_detector_fires_on_its_own_sample() {
        let get = Method::GET;
        let none = HeaderMap::new();
        let long_uri = format!("/x?q={}", "a".repeat(WAF_MAX_URI_LENGTH + 4));
        let cases: Vec<(Box<dyn ThreatDetector>, String, Method, HeaderMap)> = vec![
            (Box::new(DangerousMethodDetector), "/courses".into(), Method::TRACE, none.clone()),
            (Box::new(OversizedUriDetector), long_uri, get.clone(), none.clone()),
            (Box::new(BypassHeaderDetector), "/admin".into(), get.clone(), one_header(HeaderName::from_static("x-original-url"), "/admin")),
            (Box::new(HostHeaderDetector), "/".into(), get.clone(), one_header(header::HOST, "evil.com@good.com")),
            (Box::new(RequestSmugglingDetector), "/".into(), Method::POST, {
                let mut h = one_header(header::CONTENT_LENGTH, "5");
                h.insert(header::TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
                h
            }),
            (Box::new(MaliciousUserAgentDetector), "/".into(), get.clone(), one_header(header::USER_AGENT, "sqlmap/1.7")),
            (Box::new(HoneypotDetector), "/.env".into(), get.clone(), none.clone()),
            (Box::new(DangerousExtensionDetector), "/f.sql".into(), get.clone(), none.clone()),
            (Box::new(WebshellMasqueradeDetector), "/hw.php.zip".into(), get.clone(), none.clone()),
            (Box::new(PathTraversalDetector), "/x?a=../../etc/passwd".into(), get.clone(), none.clone()),
            (Box::new(NullByteDetector), "/x?a=%00".into(), get.clone(), none.clone()),
            (Box::new(CrlfInjectionDetector), "/x?a=%0d%0aSet-Cookie".into(), get.clone(), none.clone()),
            (Box::new(SsrfMetadataDetector), "/x?a=169.254.169.254".into(), get.clone(), none.clone()),
            (Box::new(SsrfPrivateNetworkDetector), "/x?u=http://10.0.0.1/".into(), get.clone(), none.clone()),
            (Box::new(PrototypePollutionDetector), "/x?q=__proto__".into(), get.clone(), none.clone()),
            (Box::new(PrototypeStructuredDetector), "/x?q=constructor[prototype]".into(), get.clone(), none.clone()),
            (Box::new(DeserializationDetector), "/x?q=fastjson".into(), get.clone(), none.clone()),
            (Box::new(SstiPlainDetector), "/x?q={{7*7}}".into(), get.clone(), none.clone()),
            (Box::new(SstiStructuredDetector), "/x?q=${7*7}".into(), get.clone(), none.clone()),
            (Box::new(XssPlainDetector), "/x?a=<script>alert(1)</script>".into(), get.clone(), none.clone()),
            (Box::new(XssStructuredDetector), "/x?q=<details open ontoggle=alert(1)>".into(), get.clone(), none.clone()),
            (Box::new(RceDetector), "/x?a=;whoami".into(), get.clone(), none.clone()),
            (Box::new(SqlKeywordDetector), "/x?q=sleep(5)".into(), get.clone(), none.clone()),
            (Box::new(SqlStructuredDetector), "/x?q=1;drop table users".into(), get.clone(), none.clone()),
            (Box::new(HeaderPayloadDetector), "/".into(), get.clone(), one_header(header::REFERER, "https://example.com/?q=1 union select 1,2")),
        ];
        assert_eq!(cases.len(), CompositeDetector::standard().names().len());
        for (det, path, method, headers) in &cases {
            assert!(
                run(det.as_ref(), path, method, headers).is_some(),
                "detector {} did not fire on its sample {path}",
                det.name()
            );
        }
    }

    #[test]
    fn benign_request_is_clean_for_every_detector() {
        let uri = Uri::from_str("/api/courses?page=2").expect("uri");
        let headers = one_header(header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0) Chrome/120");
        for det in standard_detectors() {
            assert!(run(det.as_ref(), &uri.to_string(), &Method::GET, &headers).is_none(), "{}", det.name());
        }
    }

    #[test]
    fn composite_returns_first_hit_in_chain_order() {
        // 同時命中 3（X-Original-URL）與 7（目錄穿越）：必須回傳較早的 3。
        let uri = Uri::from_str("/x?a=../../etc/passwd").expect("uri");
        let headers = one_header(HeaderName::from_static("x-original-url"), "/admin");
        let composite = CompositeDetector::standard().inspect(&uri, &Method::GET, &headers);
        let expected = BypassHeaderDetector.detect(&DetectionContext::new(&uri, &Method::GET, &headers));
        assert!(expected.is_some());
        assert_eq!(composite, expected);
    }

    #[test]
    fn composite_clean_request_returns_none() {
        let uri = Uri::from_str("/courses").expect("uri");
        assert!(CompositeDetector::standard().inspect(&uri, &Method::GET, &HeaderMap::new()).is_none());
    }
}
