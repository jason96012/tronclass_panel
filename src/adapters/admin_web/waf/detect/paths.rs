//! 路徑與檔案層檢測：危險副檔名、WebShell 偽裝、目錄穿越、空字節、CRLF。
//!
//! 由 detect.rs 依類別拆出（A3 結構化；每個檢測器仍獨立可測）。

use super::*;

/// 敏感備份與危險副檔名。
pub(in crate::adapters::admin_web::waf) struct DangerousExtensionDetector;

/// 雙重副檔名與 WebShell 偽裝。
pub(in crate::adapters::admin_web::waf) struct WebshellMasqueradeDetector;

/// 目錄穿越（含編碼變形）。
pub(in crate::adapters::admin_web::waf) struct PathTraversalDetector;

/// 空字節注入。
pub(in crate::adapters::admin_web::waf) struct NullByteDetector;

/// CRLF 注入。
pub(in crate::adapters::admin_web::waf) struct CrlfInjectionDetector;

impl ThreatDetector for DangerousExtensionDetector {
    fn name(&self) -> &'static str {
        "dangerous_extension"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let lower_path: &str = &ctx.lower_path;
        let decoded_target: &str = &ctx.decoded_target;
        // 6. 檢測敏感備份與副檔名探測
        if !lower_path.starts_with("/admin/static/") {
            for &ext in KNOWN_DANGEROUS_EXTENSIONS {
                if lower_path.ends_with(ext)
                    || decoded_target
                        .split('?')
                        .next()
                        .unwrap_or("")
                        .ends_with(ext)
                {
                    return Some(WafThreat::DangerousExtension(ext.to_string()));
                }
            }
        }
        None
    }
}

impl ThreatDetector for WebshellMasqueradeDetector {
    fn name(&self) -> &'static str {
        "webshell_masquerade"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let raw_path = ctx.raw_path;
        let lower_path: &str = &ctx.lower_path;
        let decoded_target: &str = &ctx.decoded_target;
        // 6b. 檢測雙重副檔名與 WebShell 偽裝探測（軍工級防護）
        let dangerous_middle_exts = [
            ".php.", ".phtml.", ".phar.", ".jsp.", ".asp.", ".aspx.", ".cgi.", ".pl.", ".py.", ".sh.", ".bat."
        ];
        // WAF_MIDEXT_PATH_ONLY_V1：只比對 path 部分。分享／預覽連結把檔名放在 query
        // （/pub/share?path=…），若比對整個 target，像 hw3.py.zip 這類正常檔名會被誤判。
        let decoded_path = decoded_target.split('?').next().unwrap_or("");
        for &d_ext in &dangerous_middle_exts {
            if lower_path.contains(d_ext) || decoded_path.contains(d_ext) {
                return Some(WafThreat::WebshellMasquerade(raw_path.to_string()));
            }
        }
        None
    }
}

impl ThreatDetector for PathTraversalDetector {
    fn name(&self) -> &'static str {
        "path_traversal"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let raw_path = ctx.raw_path;
        let full_target: &str = &ctx.full_target;
        let decoded_target: &str = &ctx.decoded_target;
        // 7. 檢測目錄穿越（Path Traversal / LFI / Unicode / Overlong UTF-8）
        if full_target.contains("../")
            || full_target.contains("..\\")
            || full_target.contains("..%2f")
            || full_target.contains("%2e%2e/")
            || full_target.contains("%2e%2e%2f")
            || full_target.contains("%252e%252e")
            || full_target.contains("..%5c")
            || full_target.contains("%2e%2e%5c")
            || full_target.contains("%c0%af")
            || full_target.contains("%e0%80%af")
            || full_target.contains("%c1%9c")
            || full_target.contains("%u002e")
            || full_target.contains("..//")
            || full_target.contains("..\\/")
            || decoded_target.contains("../")
            || decoded_target.contains("..\\")
            || decoded_target.contains("/etc/passwd")
            || decoded_target.contains("/etc/shadow")
            || decoded_target.contains("c:\\windows\\system32")
            || decoded_target.contains("%c0%af")
            || decoded_target.contains("%u002e")
        {
            return Some(WafThreat::PathTraversal(raw_path.to_string()));
        }
        None
    }
}

impl ThreatDetector for NullByteDetector {
    fn name(&self) -> &'static str {
        "null_byte"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let raw_path = ctx.raw_path;
        let full_target: &str = &ctx.full_target;
        let decoded_target: &str = &ctx.decoded_target;
        // 8. 檢測空字節注入（Null Byte Injection）與 CRLF 注入
        if full_target.contains("%00") || raw_path.contains('\0') || decoded_target.contains('\0') {
            return Some(WafThreat::RcePayload("Null Byte Injection".to_string()));
        }
        None
    }
}

impl ThreatDetector for CrlfInjectionDetector {
    fn name(&self) -> &'static str {
        "crlf_injection"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let full_target: &str = &ctx.full_target;
        let decoded_target: &str = &ctx.decoded_target;
        // 8b. CRLF 注入（與 8a 同屬步驟 8，拆開以便獨立測試；順序不變）。
        if full_target.contains("%0d%0a") || full_target.contains("\r\n") || decoded_target.contains("\r\n") {
            return Some(WafThreat::RcePayload("CRLF Injection".to_string()));
        }
        None
    }
}
