//! 伺服器端注入檢測：SSRF、原型鏈污染、反序列化、SSTI、RCE。
//!
//! 由 detect.rs 依類別拆出（A3 結構化；每個檢測器仍獨立可測）。

use super::*;

/// 雲端中繼資料與 SSRF 探測位址。
pub(in crate::adapters::admin_web::waf) struct SsrfMetadataDetector;

/// 私有網路段 SSRF 探測。
pub(in crate::adapters::admin_web::waf) struct SsrfPrivateNetworkDetector;

/// 原型鏈污染（子字串）。
pub(in crate::adapters::admin_web::waf) struct PrototypePollutionDetector;

/// 原型鏈污染（結構化，URL 範圍）。
pub(in crate::adapters::admin_web::waf) struct PrototypeStructuredDetector;

/// XXE 與反序列化注入。
pub(in crate::adapters::admin_web::waf) struct DeserializationDetector;

/// SSTI 範本表達式（子字串）。
pub(in crate::adapters::admin_web::waf) struct SstiPlainDetector;

/// SSTI（結構化，範本引擎指紋）。
pub(in crate::adapters::admin_web::waf) struct SstiStructuredDetector;

/// RCE 與終端命令注入。
pub(in crate::adapters::admin_web::waf) struct RceDetector;

impl ThreatDetector for SsrfMetadataDetector {
    fn name(&self) -> &'static str {
        "ssrf_metadata"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let full_target: &str = &ctx.full_target;
        let decoded_target: &str = &ctx.decoded_target;
        // 9. 檢測 SSRF 與雲端中繼資料位址探測 (Cloud Metadata Guard & Extended SSRF)
        if full_target.contains("169.254.169.254")
            || full_target.contains("metadata.google.internal")
            || full_target.contains("100.100.100.200")
            || full_target.contains("fd00:ec2::254")
            || full_target.contains("0x7f000001")
            || full_target.contains("0177.0.0.1")
            || full_target.contains("2130706433")
            || full_target.contains("0x7f.1")
            || full_target.contains("::ffff:127.0.0.1")
            || full_target.contains("127.0.0.1.nip.io")
            || full_target.contains("localtest.me")
            || full_target.contains("vcap.me")
            || full_target.contains("burpcollaborator.net")
            || full_target.contains("interactsh.com")
            || full_target.contains("canarytokens.com")
            || full_target.contains("oast.fun")
            || full_target.contains("oast.live")
            || full_target.contains("oast.site")
            || full_target.contains("oast.pro")
            || decoded_target.contains("169.254.169.254")
            || decoded_target.contains("metadata.google.internal")
            || decoded_target.contains("100.100.100.200")
            || decoded_target.contains("fd00:ec2::254")
            || decoded_target.contains("0x7f000001")
            || decoded_target.contains("0177.0.0.1")
            || decoded_target.contains("2130706433")
            || decoded_target.contains("0x7f.1")
            || decoded_target.contains("::ffff:127.0.0.1")
            || decoded_target.contains("127.0.0.1.nip.io")
            || decoded_target.contains("localtest.me")
            || decoded_target.contains("vcap.me")
            || decoded_target.contains("burpcollaborator.net")
            || decoded_target.contains("interactsh.com")
            || decoded_target.contains("canarytokens.com")
            || decoded_target.contains("oast.fun")
            || decoded_target.contains("oast.live")
            || decoded_target.contains("oast.site")
            || decoded_target.contains("oast.pro")
        {
            return Some(WafThreat::SsrfPayload("Cloud Metadata / SSRF Probe".to_string()));
        }
        None
    }
}

impl ThreatDetector for SsrfPrivateNetworkDetector {
    fn name(&self) -> &'static str {
        "ssrf_private_network"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let full_target: &str = &ctx.full_target;
        let decoded_target: &str = &ctx.decoded_target;
        // 9b. 私網 SSRF：解析主機（見 waf/ssrf.rs），取代舊的子字串比對（C3）。
        if super::super::ssrf::private_network_host(full_target).is_some()
            || super::super::ssrf::private_network_host(decoded_target).is_some()
        {
            return Some(WafThreat::SsrfPayload("Private Network SSRF Probe".to_string()));
        }
        None
    }
}

impl ThreatDetector for PrototypePollutionDetector {
    fn name(&self) -> &'static str {
        "prototype_pollution"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let raw_path = ctx.raw_path;
        let full_target: &str = &ctx.full_target;
        let decoded_target: &str = &ctx.decoded_target;
        // 10. 檢測 Prototype Pollution 原型鏈污染
        if full_target.contains("__proto__")
            || full_target.contains("constructor.prototype")
            || full_target.contains("object.prototype")
            || full_target.contains("prototype[")
            || decoded_target.contains("__proto__")
            || decoded_target.contains("constructor.prototype")
            || decoded_target.contains("object.prototype")
            || decoded_target.contains("prototype[")
        {
            return Some(WafThreat::PrototypePollution(raw_path.to_string()));
        }
        None
    }
}

impl ThreatDetector for PrototypeStructuredDetector {
    fn name(&self) -> &'static str {
        "prototype_structured"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let full_target: &str = &ctx.full_target;
        // 10b. WAF_SSTI_PROTO_SCOPED_V1：結構化原型鏈污染（URL 範圍，含 JS API 與括號路徑）。
        if let Some(label) = crate::adapters::admin_web::waf_detect::detect_proto_pollution(&full_target, crate::adapters::admin_web::waf_detect::Scope::Url) {
            return Some(WafThreat::PrototypePollution(format!("結構化原型鏈污染：{label}")));
        }
        None
    }
}

impl ThreatDetector for DeserializationDetector {
    fn name(&self) -> &'static str {
        "deserialization"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let raw_path = ctx.raw_path;
        let full_target: &str = &ctx.full_target;
        let decoded_target: &str = &ctx.decoded_target;
        // 11. 檢測 XXE 與 Java / JSON 反序列化注入
        if full_target.contains("<!doctype")
            || full_target.contains("<!entity")
            || full_target.contains("system \"http")
            || full_target.contains("system 'http")
            || full_target.contains("ro0ab")
            || full_target.contains("fastjson")
            || full_target.contains("com.sun.rowset")
            || decoded_target.contains("<!doctype")
            || decoded_target.contains("<!entity")
            || decoded_target.contains("system \"http")
            || decoded_target.contains("system 'http")
            || decoded_target.contains("ro0ab")
            || decoded_target.contains("fastjson")
            || decoded_target.contains("com.sun.rowset")
        {
            return Some(WafThreat::DeserializationAttack(raw_path.to_string()));
        }
        None
    }
}

impl ThreatDetector for SstiPlainDetector {
    fn name(&self) -> &'static str {
        "ssti_plain"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let raw_path = ctx.raw_path;
        let full_target: &str = &ctx.full_target;
        let decoded_target: &str = &ctx.decoded_target;
        // 12. 檢測 SSTI 範本表達式注入
        if full_target.contains("{{")
            || full_target.contains("class.module.classloader")
            || decoded_target.contains("{{")
            || decoded_target.contains("class.module.classloader")
            || decoded_target.contains("*{t(java.lang.runtime)}")
        {
            return Some(WafThreat::SstiPayload(raw_path.to_string()));
        }
        None
    }
}

impl ThreatDetector for SstiStructuredDetector {
    fn name(&self) -> &'static str {
        "ssti_structured"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let full_target: &str = &ctx.full_target;
        // 12b. WAF_SSTI_PROTO_SCOPED_V1：結構化 SSTI（URL 範圍，範本引擎指紋）。
        if let Some(label) = crate::adapters::admin_web::waf_detect::detect_ssti(&full_target, crate::adapters::admin_web::waf_detect::Scope::Url) {
            return Some(WafThreat::SstiPayload(format!("結構化 SSTI：{label}")));
        }
        None
    }
}

impl ThreatDetector for RceDetector {
    fn name(&self) -> &'static str {
        "rce"
    }

    fn detect(&self, ctx: &DetectionContext<'_>) -> Option<WafThreat> {
        let full_target: &str = &ctx.full_target;
        let decoded_target: &str = &ctx.decoded_target;
        // 14. 檢測 Log4j / RCE / 終端命令注入關鍵特徵
        if full_target.contains("${jndi:")
            || full_target.contains("${ctx:")
            || full_target.contains("${env:")
            || full_target.contains("$(curl")
            || full_target.contains("$(wget")
            || full_target.contains(";whoami")
            || full_target.contains("|whoami")
            || full_target.contains(";cat /etc/passwd")
            || full_target.contains("powershell")
            || full_target.contains("base64_decode(")
            || full_target.contains("bash -i")
            || full_target.contains("sh -i")
            || full_target.contains("/dev/tcp/")
            || full_target.contains("/dev/udp/")
            || full_target.contains("python -c")
            || full_target.contains("perl -e")
            || full_target.contains("nc -e")
            || full_target.contains("ncat -e")
            || full_target.contains("certutil -urlcache")
            || full_target.contains("curl|sh")
            || full_target.contains("curl|bash")
            || full_target.contains("wget|sh")
            || full_target.contains("wget|bash")
            || full_target.contains("eval(base64_decode")
            || full_target.contains("system($_")
            || full_target.contains("passthru(")
            || full_target.contains("shell_exec(")
            || full_target.contains("proc_open(")
            || full_target.contains("popen(")
            || decoded_target.contains("${jndi:")
            || decoded_target.contains("${ctx:")
            || decoded_target.contains("${env:")
            || decoded_target.contains("$(curl")
            || decoded_target.contains("$(wget")
            || decoded_target.contains(";whoami")
            || decoded_target.contains("|whoami")
            || decoded_target.contains(";cat /etc/passwd")
            || decoded_target.contains("powershell")
            || decoded_target.contains("base64_decode(")
            || decoded_target.contains("bash -i")
            || decoded_target.contains("sh -i")
            || decoded_target.contains("/dev/tcp/")
            || decoded_target.contains("/dev/udp/")
            || decoded_target.contains("python -c")
            || decoded_target.contains("perl -e")
            || decoded_target.contains("nc -e")
            || decoded_target.contains("ncat -e")
            || decoded_target.contains("certutil -urlcache")
            || decoded_target.contains("curl|sh")
            || decoded_target.contains("curl|bash")
            || decoded_target.contains("wget|sh")
            || decoded_target.contains("wget|bash")
            || decoded_target.contains("eval(base64_decode")
            || decoded_target.contains("system($_")
            || decoded_target.contains("passthru(")
            || decoded_target.contains("shell_exec(")
            || decoded_target.contains("proc_open(")
            || decoded_target.contains("popen(")
        {
            return Some(WafThreat::RcePayload("RCE / Command Injection Payload".to_string()));
        }
        None
    }
}
