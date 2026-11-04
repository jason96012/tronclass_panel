//! C3：SSRF 私網目標判斷（純標準庫，不引入 ipnet）。
//!
//! 舊做法是子字串比對（`contains("10.")`），會誤判 `?score=10.5`、`/files/2010.jpg`，
//! 也漏掉 `localhost` 與 `http://0/`。這裡改為解析主機：
//! - URL 授權段（`://` 或前導 `//` 之後）：依 inet_aton 規則解析（`127.1`、`0177.0.0.1`、`2130706433`、`0`），
//!   並檢查 `localhost`、IPv6（含 `::ffff:` 映射）、以及 `nip.io` 類網域中嵌入的 IP。
//! - 參數值（`=` 之後）：只接受嚴格的四段點分十進位或 `[IPv6]`，因此 `10.5`、`1.10.2` 這類數字不會被當成 IP。
//!
//! 私網範圍：loopback、10/8、172.16/12、192.168/16、169.254/16、0/8、100.64/10（CGNAT）、
//! IPv6 的 `::1`、`::`、`fc00::/7`、`fe80::/10`。

use std::net::{Ipv4Addr, Ipv6Addr};

/// 授權段結束字元。
const AUTH_END: &[char] = &['/', '?', '#', '\\', '"', '\'', '<', '>', '&', ',', ';', '|', ' ', '\t', '\r', '\n'];
/// 參數值結束字元。
const VALUE_END: &[char] = &['&', ';', ',', '(', ')', '"', '\'', '<', '>', '|', ' ', '\t', '\r', '\n'];

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostMode {
    /// URL 授權段：完整 inet_aton 規則。
    Url,
    /// 參數值本身：只接受嚴格的四段點分十進位。
    Bare,
}

/// 回傳命中的私網主機字串；未命中回傳 None。輸入可為原始或已解碼文字。
pub(super) fn private_network_host(target: &str) -> Option<String> {
    let lower = target.to_ascii_lowercase();
    for auth in url_authorities(&lower) {
        if let Some(host) = authority_host(auth) {
            if host_is_private(&host, HostMode::Url) {
                return Some(host);
            }
        }
    }
    for token in bare_values(&lower) {
        if let Some(host) = bare_host(token) {
            if host_is_private(&host, HostMode::Bare) {
                return Some(host);
            }
        }
    }
    None
}

/// 收集所有 URL 授權段：`://` 之後，或前面是分隔字元的 `//`。
fn url_authorities(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            let ok = i == 0
                || matches!(
                    bytes[i - 1],
                    b':' | b'=' | b'?' | b'&' | b'"' | b'\'' | b'(' | b' ' | b',' | b';' | b'|' | b'\t'
                );
            if ok {
                let start = i + 2;
                let end = s[start..].find(AUTH_END).map_or(s.len(), |e| start + e);
                out.push(&s[start..end]);
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// 從授權段取出主機（去除 userinfo 與連接埠；支援 `[IPv6]`）。
fn authority_host(auth: &str) -> Option<String> {
    let hostport = auth.rsplit('@').next()?;
    if let Some(rest) = hostport.strip_prefix('[') {
        let end = rest.find(']')?;
        return Some(rest[..end].to_string());
    }
    let host = hostport.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// 取出每個 `=` 之後的第一個值片段。
fn bare_values(s: &str) -> Vec<&str> {
    s.split('=')
        .skip(1)
        .filter_map(|seg| seg.split(VALUE_END).next())
        .filter(|v| !v.is_empty())
        .collect()
}

/// 參數值片段取主機：`[IPv6]`、`裸 IPv6`、`host[:port][/path]`。
fn bare_host(token: &str) -> Option<String> {
    if let Some(rest) = token.strip_prefix('[') {
        let end = rest.find(']')?;
        return Some(rest[..end].to_string());
    }
    let head = token.split('/').next()?;
    if head.matches(':').count() > 1 {
        return Some(head.to_string());
    }
    let host = head.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

fn host_is_private(host: &str, mode: HostMode) -> bool {
    if host.contains(':') {
        return host.parse::<Ipv6Addr>().is_ok_and(ipv6_is_private);
    }
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    let v4 = match mode {
        HostMode::Url => parse_inet_aton(host),
        HostMode::Bare => parse_strict_dotted(host),
    };
    if let Some(v4) = v4 {
        return ipv4_is_private(v4);
    }
    mode == HostMode::Url && embedded_ip_domains(host).into_iter().any(ipv4_is_private)
}

/// `nip.io` 類網域：任一連續四段標籤（`10.1.2.3.nip.io`）或首段以連字號分隔（`10-0-0-1.sslip.io`）嵌入 IP。
fn embedded_ip_domains(host: &str) -> Vec<Ipv4Addr> {
    let labels: Vec<&str> = host.split('.').collect();
    let mut out: Vec<Ipv4Addr> = (0..labels.len().saturating_sub(3))
        .filter_map(|i| parse_strict_dotted(&labels[i..i + 4].join(".")))
        .collect();
    if labels.len() >= 3 {
        out.extend(parse_strict_dotted(&labels[0].replace('-', ".")));
    }
    out
}

/// 嚴格四段點分十進位（每段 1–3 位十進位數字，且不超過 255）。
fn parse_strict_dotted(s: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut oct = [0u8; 4];
    for (slot, part) in oct.iter_mut().zip(parts) {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse::<u8>().ok()?;
    }
    Some(Ipv4Addr::from(oct))
}

/// WHATWG／inet_aton 規則：1–4 段，每段可為十進位、八進位（前導 0）或十六進位（0x）。
fn parse_inet_aton(s: &str) -> Option<Ipv4Addr> {
    let mut parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 1 && parts.last() == Some(&"") {
        parts.pop();
    }
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let nums: Vec<u64> = parts.iter().map(|p| parse_ipv4_part(p)).collect::<Option<_>>()?;
    let n = nums.len();
    if nums[..n - 1].iter().any(|&v| v > 255) {
        return None;
    }
    let last_limit = 256u64.pow((5 - n) as u32) - 1;
    let last = nums[n - 1];
    if last > last_limit {
        return None;
    }
    let mut addr = last;
    for (i, &v) in nums[..n - 1].iter().enumerate() {
        addr += v << (8 * (3 - i));
    }
    Some(Ipv4Addr::from(addr as u32))
}

fn parse_ipv4_part(p: &str) -> Option<u64> {
    if p.is_empty() {
        return None;
    }
    let (digits, radix) = if let Some(h) = p.strip_prefix("0x") {
        (h, 16)
    } else if p.len() > 1 && p.starts_with('0') {
        (&p[1..], 8)
    } else {
        (p, 10)
    };
    if digits.is_empty() {
        return Some(0);
    }
    if !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

fn ipv4_is_private(a: Ipv4Addr) -> bool {
    let o = a.octets();
    a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_unspecified()
        || o[0] == 0
        || (o[0] == 100 && (64..=127).contains(&o[1]))
}

fn ipv6_is_private(a: Ipv6Addr) -> bool {
    let s = a.segments();
    if s[..5] == [0, 0, 0, 0, 0] && s[5] == 0xffff {
        // IPv4 映射位址（::ffff:a.b.c.d）：依內嵌的 IPv4 判斷。
        let [a0, a1] = s[6].to_be_bytes();
        let [b0, b1] = s[7].to_be_bytes();
        return ipv4_is_private(Ipv4Addr::new(a0, a1, b0, b1));
    }
    a.is_loopback() || a.is_unspecified() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::private_network_host as p;

    #[test]
    fn ssrf_private_targets_are_detected() {
        for t in [
            "/x?url=http://127.1/",
            "/x?url=http://0177.0.0.1/",
            "/x?url=http://2130706433/",
            "/x?url=http://0/",
            "/x?url=http://localhost/admin",
            "/x?url=http://foo.localhost/",
            "/x?url=http://[::1]/",
            "/x?url=http://[::ffff:127.0.0.1]/",
            "/x?url=http://[fd00::1]/",
            "/x?url=http://[fe80::1]/",
            "/x?url=http://10.0.0.1/",
            "/x?url=http://192.168.1.1:8080/",
            "/x?url=http://172.16.5.5/",
            "/x?url=http://169.254.169.254/latest/meta-data/",
            "/x?url=http://100.100.100.200/",
            "/x?url=http://user@10.0.0.1/",
            "/x?url=http://10.1.2.3.nip.io/",
            "/x?url=http://10-0-0-1.sslip.io/",
            "/x?url=gopher://127.0.0.1:6379/_",
            "/x?host=10.0.0.1",
            "/x?host=10.0.0.1:8080/admin",
            "/x?host=[::1]",
            "/x?host=localhost",
        ] {
            assert!(p(t).is_some(), "應命中：{t}");
        }
    }

    #[test]
    fn numeric_and_path_text_is_not_flagged() {
        for t in [
            "/x?score=10.5",
            "/x?version=1.10.2",
            "/files/2010.jpg",
            "/courses?price=10.50&lang=zh-TW",
            "/courses/1127.html?x=1",
            "/x?next=https://lms.example.edu/courses",
            "/x?url=https://example.com/10.0.0.1-report",
            "/search?q=what is 192.168.1.1",
            "/x?v=100.64",
            "/x?host=example.com",
        ] {
            assert!(p(t).is_none(), "不應命中：{t}");
        }
    }
}
