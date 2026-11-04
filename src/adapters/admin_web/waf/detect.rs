//! 威脅檢測（WAF_DETECTOR_CHAIN_V1，A3）。
//!
//! 每個檢測器只負責一類威脅，實作 `ThreatDetector`。`CompositeDetector` 依固定順序
//! 執行，第一個命中者即回傳。順序與重構前的 `inspect_waf_threat` 完全相同
//! （由 `waf::tests::detector_tests::chain_order_is_fixed` 與行為基準線鎖定）。

mod client_sql;
mod paths;
mod request;
mod server_side;

pub(super) use client_sql::*;
pub(super) use paths::*;
pub(super) use request::*;
pub(super) use server_side::*;

use super::*;

use axum::http::{header, HeaderMap, Method, Uri};

/// 快速多輪 URL-decode，遞迴還原多重跳脫編碼（最高 3 層），消除混淆繞過。
pub(super) fn recursive_url_decode(mut input: String) -> String {
    for _ in 0..3 {
        let mut decoded = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let mut i = 0;
        let mut changed = false;

        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let (Some(h1), Some(h2)) = (
                    (bytes[i + 1] as char).to_digit(16),
                    (bytes[i + 2] as char).to_digit(16),
                ) {
                    let byte = (h1 * 16 + h2) as u8;
                    if byte.is_ascii() {
                        decoded.push(byte as char);
                        i += 3;
                        changed = true;
                        continue;
                    }
                }
            }
            if bytes[i] == b'+' {
                decoded.push(' ');
                changed = true;
            } else {
                decoded.push(bytes[i] as char);
            }
            i += 1;
        }

        if !changed {
            break;
        }
        input = decoded;
    }
    input
}

/// 單次檢測的上下文。字串欄位皆為小寫，與重構前的計算方式一致。
pub(super) struct DetectionContext<'a> {
    pub(super) method: &'a Method,
    pub(super) uri: &'a Uri,
    pub(super) headers: &'a HeaderMap,
    pub(super) raw_path: &'a str,
    pub(super) lower_path: String,
    pub(super) query_str: String,
    pub(super) full_target: String,
    pub(super) decoded_target: String,
    pub(super) decoded_query: String,
}

impl<'a> DetectionContext<'a> {
    pub(super) fn new(uri: &'a Uri, method: &'a Method, headers: &'a HeaderMap) -> Self {
        let raw_path = uri.path();
        let lower_path = raw_path.to_ascii_lowercase();
        let query_str = uri.query().unwrap_or("").to_ascii_lowercase();
        let full_target = format!("{lower_path}?{query_str}");
        let decoded_target = recursive_url_decode(full_target.clone()).to_ascii_lowercase();
        let decoded_query = recursive_url_decode(query_str.clone()).to_ascii_lowercase();
        Self {
            method,
            uri,
            headers,
            raw_path,
            lower_path,
            query_str,
            full_target,
            decoded_target,
            decoded_query,
        }
    }
}

/// 單一威脅檢測器。`detect` 命中回傳 `Some`，否則 `None`。
pub(super) trait ThreatDetector: Send + Sync {
    fn name(&self) -> &'static str;
    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat>;
}

/// 標準檢測順序（與重構前的 inspect_waf_threat 相同）。
pub(super) fn standard_detectors() -> Vec<Box<dyn ThreatDetector>> {
    vec![
        Box::new(DangerousMethodDetector),
        Box::new(OversizedUriDetector),
        Box::new(BypassHeaderDetector),
        Box::new(HostHeaderDetector),
        Box::new(RequestSmugglingDetector),
        Box::new(MaliciousUserAgentDetector),
        Box::new(HoneypotDetector),
        Box::new(DangerousExtensionDetector),
        Box::new(WebshellMasqueradeDetector),
        Box::new(PathTraversalDetector),
        Box::new(NullByteDetector),
        Box::new(CrlfInjectionDetector),
        Box::new(SsrfMetadataDetector),
        Box::new(SsrfPrivateNetworkDetector),
        Box::new(PrototypePollutionDetector),
        Box::new(PrototypeStructuredDetector),
        Box::new(DeserializationDetector),
        Box::new(SstiPlainDetector),
        Box::new(SstiStructuredDetector),
        Box::new(XssPlainDetector),
        Box::new(XssStructuredDetector),
        Box::new(RceDetector),
        Box::new(SqlKeywordDetector),
        Box::new(SqlStructuredDetector),
        Box::new(HeaderPayloadDetector),
    ]
}

/// 依序執行所有檢測器的組合器。
pub(super) struct CompositeDetector {
    detectors: Vec<Box<dyn ThreatDetector>>,
}

impl CompositeDetector {
    pub(super) fn standard() -> Self {
        Self {
            detectors: standard_detectors(),
        }
    }

    /// 依順序執行，第一個命中者即回傳。
    pub(super) fn inspect(&self, uri: &Uri, method: &Method, headers: &HeaderMap) -> Option<WafThreat> {
        let ctx = DetectionContext::new(uri, method, headers);
        self.detectors.iter().find_map(|d| d.detect(&ctx))
    }

    #[cfg(test)]
    pub(super) fn names(&self) -> Vec<&'static str> {
        self.detectors.iter().map(|d| d.name()).collect()
    }
}

/// 測試用：以標準順序檢測（行為與重構前的 inspect_waf_threat 相同，不含遙測）。
#[cfg(test)]
pub(super) fn inspect_waf_threat(uri: &Uri, method: &Method, headers: &HeaderMap) -> Option<WafThreat> {
    CompositeDetector::standard().inspect(uri, method, headers)
}

