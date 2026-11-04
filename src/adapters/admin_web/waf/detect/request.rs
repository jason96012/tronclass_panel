//! HTTP 請求層檢測：方法、URI 長度、繞過標頭、Host、請求走私、惡意 UA、蜜罐路徑。
//!
//! 由 detect.rs 依類別拆出（A3 結構化；每個檢測器仍獨立可測）。

use super::*;

/// 危險 HTTP 方法（TRACE／TRACK／CONNECT）。
pub(in crate::adapters::admin_web::waf) struct DangerousMethodDetector;

/// URI 超過長度上限（防緩衝區溢位）。
pub(in crate::adapters::admin_web::waf) struct OversizedUriDetector;

/// WAF 繞過標頭（X-Original-URL 等）。
pub(in crate::adapters::admin_web::waf) struct BypassHeaderDetector;

/// Host 標頭投毒與畸形字元。
pub(in crate::adapters::admin_web::waf) struct HostHeaderDetector;

/// Content-Length 與 Transfer-Encoding 並存（請求走私）。
pub(in crate::adapters::admin_web::waf) struct RequestSmugglingDetector;

/// 惡意掃描工具 User-Agent。
pub(in crate::adapters::admin_web::waf) struct MaliciousUserAgentDetector;

/// 蜜罐弱點探針路徑。
pub(in crate::adapters::admin_web::waf) struct HoneypotDetector;

impl ThreatDetector for DangerousMethodDetector {
    fn name(&self) -> &'static str {
        "dangerous_method"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let method = ctx.method;
        // 1. 檢測危險 HTTP 方法
        let m = method.as_str().to_uppercase();
        if m == "TRACE" || m == "TRACK" || m == "CONNECT" {
            return Some(WafThreat::DangerousMethod(m));
        }
        None
    }
}

impl ThreatDetector for OversizedUriDetector {
    fn name(&self) -> &'static str {
        "oversized_uri"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let uri = ctx.uri;
        // 2. 檢測 URI 長度異常（防緩衝區溢位）
        let raw_uri = uri.to_string();
        if raw_uri.len() > WAF_MAX_URI_LENGTH {
            return Some(WafThreat::OversizedRequest(format!("長度 {}", raw_uri.len())));
        }
        None
    }
}

impl ThreatDetector for BypassHeaderDetector {
    fn name(&self) -> &'static str {
        "bypass_header"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let headers = ctx.headers;
        // 3. 檢測 WAF 偽造/繞過標頭
        if headers.contains_key("x-original-url") {
            return Some(WafThreat::BypassHeader("X-Original-URL".to_string()));
        }
        if headers.contains_key("x-rewrite-url") {
            return Some(WafThreat::BypassHeader("X-Rewrite-URL".to_string()));
        }
        if headers.contains_key("x-custom-ip-authorization") {
            return Some(WafThreat::BypassHeader("X-Custom-IP-Authorization".to_string()));
        }
        None
    }
}

impl ThreatDetector for HostHeaderDetector {
    fn name(&self) -> &'static str {
        "host_header"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let headers = ctx.headers;
        // 3b. 檢測 Host 標頭投毒與畸形字元（軍工級防護）
        if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
            if host.contains('@')
                || host.contains('\r')
                || host.contains('\n')
                || host.contains('\0')
                || host.chars().any(|c| c.is_ascii_control() || c.is_whitespace())
            {
                return Some(WafThreat::HostHeaderPoisoning(host.to_string()));
            }
        }
        None
    }
}

impl ThreatDetector for RequestSmugglingDetector {
    fn name(&self) -> &'static str {
        "request_smuggling"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let headers = ctx.headers;
        // 3c. 檢測 HTTP 請求走私與雙重長度標頭 (Request Smuggling Guard)
        if headers.contains_key(header::CONTENT_LENGTH) && headers.contains_key(header::TRANSFER_ENCODING) {
            return Some(WafThreat::RequestSmuggling("Conflicting Content-Length & Transfer-Encoding".to_string()));
        }
        None
    }
}

impl ThreatDetector for MaliciousUserAgentDetector {
    fn name(&self) -> &'static str {
        "malicious_user_agent"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let headers = ctx.headers;
        // 4. 檢測惡意 User-Agent 特徵
        if let Some(ua) = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()) {
            let ua_lower = ua.to_ascii_lowercase();
            for &bot in KNOWN_MALICIOUS_USER_AGENTS {
                if ua_lower.contains(bot) {
                    return Some(WafThreat::MaliciousUserAgent(ua.to_string()));
                }
            }
        }

        None
    }
}

impl ThreatDetector for HoneypotDetector {
    fn name(&self) -> &'static str {
        "honeypot"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let raw_path = ctx.raw_path;
        let lower_path: &str = &ctx.lower_path;
        let decoded_target: &str = &ctx.decoded_target;
        // 5. 檢測蜜罐弱點探針路徑
        for &probe in KNOWN_HONEYPOT_PATHS {
            if lower_path == probe
                || lower_path.starts_with(&format!("{probe}/"))
                || decoded_target.starts_with(probe)
            {
                return Some(WafThreat::HoneypotProbe(raw_path.to_string()));
            }
        }
        None
    }
}
