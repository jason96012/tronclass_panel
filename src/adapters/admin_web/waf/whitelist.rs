//! WAF 白名單（IP、CIDR、路徑前綴）與路徑規範化。

use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;

use tokio::sync::Mutex;

// WAF_AUTO_BLOCK_NOTIFY_V2：WAF 新建立自動封鎖時，透過 notify_waf_auto_block 只發 Discord 給
// admin_user_id（同一 IP 24 小時去重），附「解除封鎖」按鈕。不發 LINE、不發 Web Push。

/// WAF 白名單：CIDR / IP / 路徑前綴豁免。
#[derive(Clone, Default)]
pub struct WafWhitelist {
    /// 明確允許的 IP 字串（精確比對）
    pub(super) ips: Arc<Mutex<Vec<String>>>,
    /// CIDR 區塊（字串形式，如 "10.0.0.0/8"）
    pub(super) cidrs: Arc<Mutex<Vec<String>>>,
    /// 路徑前綴豁免（如 "/api/health", "/metrics"）
    pub(super) paths: Arc<Mutex<Vec<String>>>,
}

impl WafWhitelist {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn is_ip_whitelisted(&self, ip: &str) -> bool {
        let ips = self.ips.lock().await;
        if ips.contains(&ip.to_string()) {
            return true;
        }
        drop(ips);

        // 檢查 CIDR
        let cidrs = self.cidrs.lock().await;
        let ip_addr = match IpAddr::from_str(ip) {
            Ok(a) => a,
            Err(_) => return false,
        };
        for cidr in cidrs.iter() {
            if let Some(net) = parse_cidr(cidr) {
                if ip_addr_matches_cidr(ip_addr, net) {
                    return true;
                }
            }
        }
        false
    }

    /// WAF_WHITELIST_PATH_CANON_V1：路徑豁免必須同時滿足
    /// ① 原始路徑（未解碼）以前綴開頭，且位於目錄邊界上；
    /// ② 規範化後的路徑（解碼、全形斜線、反斜線）也位於前綴之下，且不含 `.`／`..`／空段。
    /// 任何一條不成立就不豁免，請求回到完整 WAF 檢查（失敗即完整檢查）。
    /// 注意：目前沒有任何程式碼寫入 `paths`，此函式在現行設定下不會放行任何請求。
    pub async fn is_path_whitelisted(&self, path: &str) -> bool {
        let paths = self.paths.lock().await;
        let Some(canon) = canonical_path_for_whitelist(path) else {
            return false;
        };
        paths.iter().any(|prefix| {
            let p = prefix.trim_end_matches('/');
            // 空前綴或「/」會豁免所有路徑，拒絕這種設定。
            if p.is_empty() || !p.starts_with('/') {
                return false;
            }
            path_under_prefix(path, p) && path_under_prefix(&canon, p)
        })
    }
}

/// 目錄邊界比對：`s == prefix`，或 `s` 以 `prefix/` 開頭（不會讓 `/healthcheck` 命中 `/health`）。
pub(super) fn path_under_prefix(s: &str, prefix: &str) -> bool {
    s == prefix
        || (s.starts_with(prefix) && s.as_bytes().get(prefix.len()) == Some(&b'/'))
}

/// 白名單用的路徑規範化。回傳 None 代表路徑形式可疑，不可豁免。
pub(super) fn canonical_path_for_whitelist(raw: &str) -> Option<String> {
    if raw.chars().any(|c| c.is_control()) {
        return None;
    }
    // 反覆解碼最多 3 輪（防雙重／三重編碼）。
    let mut current = raw.to_string();
    for _ in 0..3 {
        let next = percent_decode_path(&current);
        if next == current {
            break;
        }
        current = next;
    }
    if current.chars().any(|c| c.is_control()) {
        return None;
    }
    // 全形斜線與反斜線一律視為路徑分隔符（Windows／代理的正規化差異）。
    let folded: String = current
        .chars()
        .map(|c| match c {
            '\u{FF0F}' => '/',
            '\u{FF3C}' | '\\' => '/',
            other => other,
        })
        .collect();
    if !folded.starts_with('/') {
        return None;
    }
    if folded == "/" {
        return Some(folded);
    }
    let body = folded[1..].strip_suffix('/').unwrap_or(&folded[1..]);
    for seg in body.split('/') {
        // 空段、`.`、`..` 一律拒絕：攻擊者靠它們把前綴後面接到其他路由。
        if seg.is_empty() || seg == "." || seg == ".." {
            return None;
        }
    }
    Some(format!("/{body}"))
}

/// 只解碼 `%XX`；`+` 在路徑中保持原字元。無效的 `%` 序列原樣保留。
pub(super) fn percent_decode_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod whitelist_path_tests {
    use super::*;

    async fn wl(prefixes: &[&str]) -> WafWhitelist {
        let w = WafWhitelist::new();
        *w.paths.lock().await = prefixes.iter().map(|s| s.to_string()).collect();
        w
    }

    #[tokio::test]
    async fn legitimate_paths_under_prefix_are_allowed() {
        let w = wl(&["/admin/api/health"]).await;
        assert!(w.is_path_whitelisted("/admin/api/health").await);
        assert!(w.is_path_whitelisted("/admin/api/health/").await);
        assert!(w.is_path_whitelisted("/admin/api/health/status").await);
    }

    #[tokio::test]
    async fn prefix_matching_respects_directory_boundary() {
        let w = wl(&["/admin/api/health"]).await;
        assert!(!w.is_path_whitelisted("/admin/api/healthcheck").await);
        assert!(!w.is_path_whitelisted("/admin/api/healthy/../x").await);
    }

    #[tokio::test]
    async fn scenario1_dot_dot_traversal_is_not_whitelisted() {
        let w = wl(&["/admin/api/health"]).await;
        assert!(!w.is_path_whitelisted("/admin/api/health/../admin/users").await);
        assert!(!w.is_path_whitelisted("/admin/api/health/./x").await);
    }

    #[tokio::test]
    async fn scenario2_encoded_slash_traversal_is_not_whitelisted() {
        let w = wl(&["/admin/api/health"]).await;
        assert!(!w.is_path_whitelisted("/admin/api/health%2F..%2Fadmin%2Fusers").await);
    }

    #[tokio::test]
    async fn scenario3_double_encoded_traversal_is_not_whitelisted() {
        let w = wl(&["/admin/api/health"]).await;
        assert!(!w.is_path_whitelisted("/admin/api/health%252F..%252Fadmin%252Fusers").await);
    }

    #[tokio::test]
    async fn scenario4_fullwidth_slash_traversal_is_not_whitelisted() {
        let w = wl(&["/admin/api/health"]).await;
        assert!(
            !w.is_path_whitelisted("/admin/api/health%EF%BC%8F..%EF%BC%8Fadmin%EF%BC%8Fusers")
                .await
        );
    }

    #[tokio::test]
    async fn scenario5_case_variant_traversal_is_not_whitelisted() {
        let w = wl(&["/admin/api/health"]).await;
        assert!(!w.is_path_whitelisted("/ADMIN/API/HEALTH/../ADMIN/USERS").await);
        // 大小寫不同的前綴不豁免（路由區分大小寫）。
        assert!(!w.is_path_whitelisted("/ADMIN/API/HEALTH").await);
    }

    #[tokio::test]
    async fn backslash_and_encoded_backslash_are_not_whitelisted() {
        let w = wl(&["/admin/api/health"]).await;
        assert!(!w.is_path_whitelisted("/admin/api/health\\..\\admin").await);
        assert!(!w.is_path_whitelisted("/admin/api/health%5c..%5cadmin").await);
    }

    #[tokio::test]
    async fn encoded_prefix_does_not_borrow_whitelist() {
        // 原始路徑必須以前綴字面開頭，編碼後才等於前綴者不豁免。
        let w = wl(&["/admin/api/health"]).await;
        assert!(!w.is_path_whitelisted("/admin/api/%68ealth").await);
    }

    #[tokio::test]
    async fn empty_or_root_prefix_is_refused() {
        let w = wl(&["/", ""]).await;
        assert!(!w.is_path_whitelisted("/admin/users").await);
        assert!(!w.is_path_whitelisted("/").await);
    }

    #[tokio::test]
    async fn empty_inner_segments_are_not_whitelisted() {
        let w = wl(&["/admin/api/health"]).await;
        assert!(!w.is_path_whitelisted("/admin/api/health//x").await);
    }

    #[tokio::test]
    async fn no_prefixes_configured_whitelists_nothing() {
        let w = wl(&[]).await;
        assert!(!w.is_path_whitelisted("/admin/api/health").await);
    }
}

/// 解析 CIDR 字串回傳 (network_addr, prefix_len)。
pub(super) fn parse_cidr(cidr: &str) -> Option<(IpAddr, u8)> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return None;
    }
    let ip = IpAddr::from_str(parts[0]).ok()?;
    let prefix: u8 = parts[1].parse().ok()?;
    Some((ip, prefix))
}

/// 判斷 IP 是否落在 CIDR 區塊內。
pub(super) fn ip_addr_matches_cidr(ip: IpAddr, (network, prefix): (IpAddr, u8)) -> bool {
    match (ip, network) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let a_bits = u32::from_be_bytes(a.octets());
            let n_bits = u32::from_be_bytes(n.octets());
            let mask: u32 = if prefix == 0 { 0 } else { !((1u32 << (32 - prefix)) - 1) };
            (a_bits & mask) == (n_bits & mask)
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let a_bits = u128::from_be_bytes(a.octets());
            let n_bits = u128::from_be_bytes(n.octets());
            let mask: u128 = if prefix == 0 { 0 } else { !((1u128 << (128 - prefix)) - 1) };
            (a_bits & mask) == (n_bits & mask)
        }
        _ => false,
    }
}
