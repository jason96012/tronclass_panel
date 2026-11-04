//! 請求本體掃描與本體上限分級。

use super::*;

use axum::http::{header, HeaderMap, Method};

// WAF_AUTO_BLOCK_NOTIFY_V2：WAF 新建立自動封鎖時，透過 notify_waf_auto_block 只發 Discord 給
// admin_user_id（同一 IP 24 小時去重），附「解除封鎖」按鈕。不發 LINE、不發 Web Push。
use crate::adapters::admin_web::AdminWebState;
use crate::db::ActivityEventInput;

/// WAF_BODY_SCAN_V1：本體掃描的單次上限。超過就回 413（不掃描、不放行），
/// 否則填充到上限之後就能繞過檢查。AI 聊天附件（base64）與 tc_proxy 是本系統最大的 JSON 本體來源。
pub(super) const WAF_BODY_SCAN_MAX_BYTES: usize = 32 * 1024 * 1024;
/// WAF_BODY_LIMIT_TIER_V1（2026-11-03 決定）：一般路徑本體上限 4MB，超過回 413（失敗即關閉）。
/// 需要較大本體的路徑（TC 代理、AI 聊天、檔案上傳、作業繳交）維持 32MB 上限，**仍然完整掃描**。
/// 上限只決定緩衝大小，不會讓任何請求跳過掃描。
pub(super) const WAF_BODY_SCAN_DEFAULT_BYTES: usize = 4 * 1024 * 1024;
pub(super) const WAF_BODY_SCAN_LARGE_PREFIXES: &[&str] = &[
    "/admin/api/ai",             // AI 聊天附件（base64）
    "/admin/api/files/upload",   // multipart，本體掃描本來就不適用，維持原上限
    "/admin/api/homework/submit",
    "/tc",
    "/api",
    "/course",
    "/static",
    "/uploads",
    "/bulletin-list",
];

/// 本體掃描的高信心特徵。刻意只取少數：完整的 URL 規則（例如 `sleep(`、`powershell`）
/// 會把程式碼與教材內容誤判，因此不套用到本體。
pub(super) const WAF_BODY_PATTERNS: &[&str] = &[
    "<script", "<\\/script", "javascript:", "vbscript:", "onerror=", "onload=", "onmouseover=",
    "<iframe", "<svg/onload", "${jndi:", "<!entity", "<!doctype", "__proto__", "constructor.prototype",
    "union select", "union all select", "' or '1'='1", "' or 1=1", "into outfile", "into dumpfile",
];

/// 本體是否需要掃描：只看會被 handler 解析成文字的 POST／PUT／PATCH 內容。multipart 上傳不掃描。
/// 依路徑取得本體掃描上限（WAF_BODY_LIMIT_TIER_V1）。前綴採目錄邊界比對。
pub(super) fn waf_body_scan_limit(path: &str) -> usize {
    if WAF_BODY_SCAN_LARGE_PREFIXES
        .iter()
        .any(|p| path_under_prefix(path, p))
    {
        WAF_BODY_SCAN_MAX_BYTES
    } else {
        WAF_BODY_SCAN_DEFAULT_BYTES
    }
}

pub(super) fn waf_body_needs_scan(method: &Method, headers: &HeaderMap) -> bool {
    if !(*method == Method::POST || *method == Method::PUT || *method == Method::PATCH) {
        return false;
    }
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    ct.starts_with("application/json")
        || ct.starts_with("application/x-www-form-urlencoded")
        || ct.starts_with("text/")
}

/// 掃描本體。表單先做 URL 解碼；JSON 把 \uXXXX 的尖括號與引號跳脫還原（JSON 解析後才會變成 HTML）。
pub(super) fn inspect_waf_body(body: &str, is_form: bool) -> Option<WafThreat> {
    let mut text = if is_form {
        recursive_url_decode(body.to_string())
    } else {
        body.to_string()
    };
    if !is_form && text.contains("\\u00") {
        text = text
            .replace("\\u003c", "<")
            .replace("\\u003C", "<")
            .replace("\\u003e", ">")
            .replace("\\u003E", ">")
            .replace("\\u0027", "'")
            .replace("\\u0022", "\"");
    }
    let lower = text.to_ascii_lowercase();
    for &pat in WAF_BODY_PATTERNS {
        if lower.contains(pat) {
            return Some(WafThreat::BodyPayload(pat.to_string()));
        }
    }
    // WAF_DETECT_V2：結構化偵測，前段與後段各 2 MiB（完整本體仍由上面的子字串規則掃描）。
    let (head, tail) = crate::adapters::admin_web::waf_detect::body_windows(&text);
    for part in std::iter::once(head).chain(tail) {
        if let Some(label) = crate::adapters::admin_web::waf_detect::detect_sqli(part, crate::adapters::admin_web::waf_detect::Scope::Body) {
            return Some(WafThreat::BodyPayload(format!("結構化 SQLi：{label}")));
        }
        if let Some(label) = crate::adapters::admin_web::waf_detect::detect_xss(part, crate::adapters::admin_web::waf_detect::Scope::Body) {
            return Some(WafThreat::BodyPayload(format!("結構化 XSS：{label}")));
        }
        if let Some(label) = crate::adapters::admin_web::waf_detect::detect_ssti(part, crate::adapters::admin_web::waf_detect::Scope::Body) {
            return Some(WafThreat::BodyPayload(format!("結構化 SSTI：{label}")));
        }
        if let Some(label) = crate::adapters::admin_web::waf_detect::detect_proto_pollution(part, crate::adapters::admin_web::waf_detect::Scope::Body) {
            return Some(WafThreat::BodyPayload(format!("結構化原型鏈污染：{label}")));
        }
    }
    None
}

/// 本體命中後的自動封鎖（使用者決定：本體命中也封 IP）。與 URL 威脅使用同一組黑名單與稽核欄位，
/// 只跳過本機／私網 IP，並且已封鎖者不重複寫入（同 WAF_BAN_DEDUP_V1 的考量）。
pub(super) async fn ban_for_body_threat(
    state: &AdminWebState,
    ip: &str,
    threat_desc: String,
    path: String,
    user_agent: String,
) {
    if is_local_or_trusted_ip(ip) || ip == "unknown" {
        return;
    }
    let ban_key = crate::trusted_ip::blacklist_group_key(ip);
    if state
        .accounts_db
        .is_ip_blacklisted(&ban_key)
        .await
        .unwrap_or(false)
    {
        return;
    }
    let db = state.accounts_db.clone();
    tokio::spawn(async move {
        if let Ok(true) = db
            .block_ip_with_failure_count(
                &ban_key,
                "system:waf-firewall-max",
                &format!("最高級 WAF 防火牆自動封鎖（本體攻擊特徵）: {threat_desc}"),
                1,
            )
            .await
        {
            let _ = db
                .record_activity_event(ActivityEventInput {
                    kind: "security".to_string(),
                    actor: "system:waf-firewall-max".to_string(),
                    action: "waf_threat_blocked".to_string(),
                    account_id: "unknown".to_string(),
                    path,
                    result: "blocked".to_string(),
                    details: format!("最高級 WAF 攔截本體攻擊並自動封鎖: {threat_desc}"),
                    ip: ban_key.clone(),
                    user_agent,
                })
                .await;
        }
    });
}

/// C5：預算的計量單位（1 單位 = 1 MB）。
const WAF_BODY_SCAN_UNIT_BYTES: usize = 1024 * 1024;
/// C5：讀取本體的逾時。慢速送資料的客戶端不得無限期佔住掃描預算（逾時回 408）。
pub(super) const WAF_BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// C5：宣告的 Content-Length。無法解析時回 None，由緩衝上限把關（不視為攻擊，也不略過掃描）。
pub(super) fn declared_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// C5：一次掃描需要的預算單位數（向上取整，至少 1）。
pub(super) fn body_scan_units(limit: usize) -> u32 {
    let units = (limit + WAF_BODY_SCAN_UNIT_BYTES - 1) / WAF_BODY_SCAN_UNIT_BYTES;
    units.max(1) as u32
}

/// C5：取得本體掃描預算。等待超過 `body_scan_wait` 回 None；呼叫端必須回 503。
/// 這是失敗即關閉：沒有預算就不放行，也不跳過掃描。
pub(super) async fn acquire_body_scan_slot(
    waf: &WafEngine,
    limit: usize,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    // 單一請求的需求不得超過整體預算（否則永遠等不到）。
    let units = body_scan_units(limit).min(waf.body_scan_budget_mb as u32);
    let sem = waf.body_scan_budget.clone();
    match tokio::time::timeout(waf.body_scan_wait, sem.acquire_many_owned(units)).await {
        Ok(Ok(permit)) => Some(permit),
        _ => None,
    }
}
