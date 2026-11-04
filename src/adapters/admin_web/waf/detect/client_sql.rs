//! 用戶端與資料庫注入檢測：XSS、SQL 注入、Referer／Cookie 載荷。
//!
//! 由 detect.rs 依類別拆出（A3 結構化；每個檢測器仍獨立可測）。

use super::*;

/// XSS 載荷（子字串）。
pub(in crate::adapters::admin_web::waf) struct XssPlainDetector;

/// XSS（結構化，標籤屬性與協議）。
pub(in crate::adapters::admin_web::waf) struct XssStructuredDetector;

/// SQL 注入關鍵字（query）。
pub(in crate::adapters::admin_web::waf) struct SqlKeywordDetector;

/// SQL 注入（結構化，路徑與 query）。
pub(in crate::adapters::admin_web::waf) struct SqlStructuredDetector;

/// Referer 與 Cookie 中的 SQLi／XSS（只回 403）。
pub(in crate::adapters::admin_web::waf) struct HeaderPayloadDetector;

impl ThreatDetector for XssPlainDetector {
    fn name(&self) -> &'static str {
        "xss_plain"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let raw_path = ctx.raw_path;
        let full_target: &str = &ctx.full_target;
        let decoded_target: &str = &ctx.decoded_target;
        // 13. 檢測 XSS 跨站腳本攻擊載荷
        if full_target.contains("<script")
            || full_target.contains("javascript:")
            || full_target.contains("vbscript:")
            || full_target.contains("onerror=")
            || full_target.contains("onload=")
            || full_target.contains("document.cookie")
            || full_target.contains("<svg/onload")
            || full_target.contains("<img/src/onerror")
            || decoded_target.contains("<script")
            || decoded_target.contains("javascript:")
            || decoded_target.contains("vbscript:")
            || decoded_target.contains("onerror=")
            || decoded_target.contains("onload=")
            || decoded_target.contains("document.cookie")
            || decoded_target.contains("<svg/onload")
            || decoded_target.contains("<img/src/onerror")
        {
            return Some(WafThreat::XssInjection(raw_path.to_string()));
        }
        None
    }
}

impl ThreatDetector for XssStructuredDetector {
    fn name(&self) -> &'static str {
        "xss_structured"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let full_target: &str = &ctx.full_target;
        // 13b. WAF_DETECT_V2：結構化 XSS（標籤屬性內的事件處理器、危險協議、DOM 外洩）。
        if let Some(label) = crate::adapters::admin_web::waf_detect::detect_xss(&full_target, crate::adapters::admin_web::waf_detect::Scope::Url) {
            return Some(WafThreat::XssInjection(format!("結構化 XSS：{label}")));
        }
        None
    }
}

impl ThreatDetector for SqlKeywordDetector {
    fn name(&self) -> &'static str {
        "sql_keyword"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let query_str: &str = &ctx.query_str;
        let decoded_query: &str = &ctx.decoded_query;
        // 15. 檢測顯著 SQL 注入特徵（在 query 參數中）
        let sqli_patterns = [
            "' or '1'='1",
            "' or 1=1",
            "union select",
            "union all select",
            "information_schema",
            "benchmark(",
            "sleep(",
            "waitfor delay",
            "pg_sleep",
            "extractvalue(",
            "updatexml(",
            "into outfile",
            "into dumpfile",
        ];

        if !query_str.is_empty() {
            for &pat in &sqli_patterns {
                if query_str.contains(pat) || decoded_query.contains(pat) {
                    return Some(WafThreat::SqlInjection("SQL Injection Payload".to_string()));
                }
            }
        }
        None
    }
}

impl ThreatDetector for SqlStructuredDetector {
    fn name(&self) -> &'static str {
        "sql_structured"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let full_target: &str = &ctx.full_target;
        // 15b. WAF_DETECT_V2：結構化 SQLi（路徑與查詢字串）。
        if let Some(label) = crate::adapters::admin_web::waf_detect::detect_sqli(&full_target, crate::adapters::admin_web::waf_detect::Scope::Url) {
            return Some(WafThreat::SqlInjection(format!("結構化 SQLi：{label}")));
        }
        None
    }
}

impl ThreatDetector for HeaderPayloadDetector {
    fn name(&self) -> &'static str {
        "header_payload"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let headers = ctx.headers;
        // 15c. WAF_DETECT_V2：Referer 與 Cookie 也掃描（攻擊者可把載荷放在標頭）。
        for name in [header::REFERER, header::COOKIE] {
            if let Some(value) = headers.get(&name).and_then(|v| v.to_str().ok()) {
                if let Some(label) = crate::adapters::admin_web::waf_detect::detect_sqli(value, crate::adapters::admin_web::waf_detect::Scope::Url) {
                    return Some(WafThreat::HeaderPayload(format!("結構化 SQLi：{label}")));
                }
                if let Some(label) = crate::adapters::admin_web::waf_detect::detect_xss(value, crate::adapters::admin_web::waf_detect::Scope::Url) {
                    return Some(WafThreat::HeaderPayload(format!("結構化 XSS：{label}")));
                }
            }
        }
        None
    }
}
