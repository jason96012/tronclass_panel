//! Trusted reverse-proxy aware client IP resolution.
//!
//! The Admin dashboard sits behind a reverse proxy (Cloudflare Tunnel / Caddy).
//! Such proxies append the real client IP in `CF-Connecting-IP`,
//! `True-Client-IP`, `X-Real-IP` or `X-Forwarded-For`. **If the origin were to
//! trust those headers unconditionally, anyone able to reach the origin
//! directly could forge a client IP and bypass the login rate limiter or get a
//! victim's IP added to the blacklist.**
//!
//! We therefore only honour the forwarded headers when the connection actually
//! arrived from a proxy that is on the operator-configured trust list. Requests
//! from any other source fall back to the real peer address, so a public origin
//! cannot be abused to spoof its client IP.
//!
//! ## Trusted proxy configuration
//!
//! [`AdminWebConfig::trusted_proxies`] accepts each of:
//!
//! - an exact IP, e.g. `127.0.0.1` or `::1`;
//! - a CIDR block, e.g. `104.16.0.0/13`;
//! - the keyword `cloudflare`, which expands to Cloudflare's published edge
//!   ranges (handy when the origin is proxied directly rather than tunnelled);
//! - the keyword `loopback`, the loopback range (`127.0.0.0/8` and `::1`).
//!
//! Loopback is always trusted (a local `cloudflared`/`caddy` is the documented
//! deployment), so the tunnel case works out of the box.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http;
use axum::http::request::Parts;

use crate::config;

// Cloudflare publishes these ranges; kept in sync so `trusted_proxies =
// ["cloudflare"]` works without external lookup at runtime.
const CLOUDFLARE_V4: &[&str] = &[
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
];
const CLOUDFLARE_V6: &[&str] = &[
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

/// A single matched network prefix.
#[derive(Debug, Clone, Copy)]
struct Net {
    bits: u32,
    mask_len: u32,
    net: u128,
}

impl Net {
    /// Whether `value` (a u128 encoding of an address of the same width) lies
    /// inside this network.
    fn contains(&self, value: u128) -> bool {
        if self.mask_len == 0 {
            return true;
        }
        let shift = self.bits - self.mask_len;
        (value >> shift) == (self.net >> shift)
    }
}

/// A list of trusted network prefixes that may set forwarded client-IP headers.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    v4: Vec<Net>,
    v6: Vec<Net>,
}

impl TrustedProxies {
    /// Build from raw config strings, always including loopback.
    pub fn from_config(entries: &[String]) -> Self {
        let mut this = Self::default();
        this.add_loopback();
        for entry in entries {
            let entry = entry.trim();
            match entry.to_ascii_lowercase().as_str() {
                "loopback" | "localhost" => this.add_loopback(),
                "cloudflare" => {
                    for cidr in CLOUDFLARE_V4 {
                        this.add_cidr(cidr);
                    }
                    for cidr in CLOUDFLARE_V6 {
                        this.add_cidr(cidr);
                    }
                }
                _ => this.add_cidr(entry),
            }
        }
        this
    }

    fn add_loopback(&mut self) {
        self.v4.push(Net {
            bits: 32,
            mask_len: 8,
            // u128 has no From<Ipv4Addr>; go through u32 first.
            net: u128::from(u32::from(Ipv4Addr::new(127, 0, 0, 0))),
        });
        self.v6.push(Net {
            bits: 128,
            mask_len: 128,
            net: u128::from(Ipv6Addr::LOCALHOST),
        });
    }

    fn add_cidr(&mut self, cidr: &str) {
        let (ip_str, len_str) = match cidr.split_once('/') {
            Some((ip, len)) => (ip, Some(len)),
            None => (cidr, None),
        };
        let Ok(ip) = IpAddr::from_str(ip_str.trim()) else {
            return;
        };
        let (bits, value) = match ip {
            IpAddr::V4(v4) => (32u32, u128::from(u32::from(v4))),
            IpAddr::V6(v6) => (128u32, u128::from(v6)),
        };
        let mask_len = match len_str {
            Some(len) => match len.trim().parse::<u32>() {
                Ok(0) if bits == 32 => 0,
                Ok(len) if len <= bits => len,
                _ => return,
            },
            None => bits,
        };
        let net = Net {
            bits,
            mask_len,
            net: value,
        };
        match ip {
            IpAddr::V4(_) => self.v4.push(net),
            IpAddr::V6(_) => self.v6.push(net),
        }
    }

    /// Public wrapper over [`Self::is_trusted`] so other modules can gate
    /// proxy-supplied metadata (e.g. `X-Forwarded-Proto`, `CF-Visitor`) on the
    /// same trust decision used for client-IP resolution.
    ///
    /// A `None` peer (no `ConnectInfo`, e.g. unit-test `oneshot` calls) is
    /// never trusted.
    pub fn is_trusted_peer(&self, peer: Option<IpAddr>) -> bool {
        self.is_trusted(peer)
    }

    fn is_trusted(&self, peer: Option<IpAddr>) -> bool {
        match peer {
            Some(IpAddr::V4(v4)) => {
                let value = u128::from(u32::from(v4));
                self.v4.iter().any(|n| n.contains(value))
            }
            Some(IpAddr::V6(v6)) => {
                let value = u128::from(v6);
                self.v6.iter().any(|n| n.contains(value))
            }
            None => false,
        }
    }
}

// IP_CANONICAL_V4_MAPPED_V1：黑名單與節流計數都以「字串」比對 IP 身分。
// 若伺服器以雙疊（`[::]`，Linux 預設 v6only=0）監聽，IPv4 客戶端的 peer 會被
// 作業系統包成 `::ffff:203.0.113.9`；這個字串與黑名單裡的 `203.0.113.9` 不相等
// → 封鎖被繞過、失敗計數也換了鑰匙（自動封鎖永遠湊不到門檻）。
// 統一在這裡把 v4-mapped 位址收斂回純 IPv4，讓同一台機器永遠只有一個字串身分。
pub fn canonicalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            // 刻意只收斂 ::ffff:0:0/96（v4-mapped）。不使用 `to_ipv4()`：
            // 它會把 `::1`、`::` 這些 compatible 位址誤轉成 0.0.0.x，
            // 反而把 IPv6 loopback 改寫成別的身分。
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// IP_BLACKLIST_V6_GROUP_V1：節流與黑名單共用的「群組鍵」。
///
/// IPv6 終端通常握有整個 /64（作業系統會自動輪換 privacy addresses，電信業者
/// 也多配發 /56～/64）：只封、只算單一 128-bit 位址等於沒封——換一個位址就
/// 繞過。因此 IPv6 一律收斂成「前 64 位元前綴」作為封鎖與計數單位
/// （fail2ban 與 Cloudflare 的標準做法）；IPv4 不變。
/// 無法解析的字串（例如 `"unknown"`）原樣回傳，交由上層自行處理。
pub fn blacklist_group_key(ip: &str) -> String {
    let trimmed = ip.trim();
    match trimmed.parse::<IpAddr>() {
        Ok(parsed) => match canonicalize_ip(parsed) {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => {
                let seg = v6.segments();
                format!(
                    "{}/64",
                    Ipv6Addr::new(seg[0], seg[1], seg[2], seg[3], 0, 0, 0, 0)
                )
            }
        },
        Err(_) => trimmed.to_string(),
    }
}

/// 把管理員輸入（或既有黑名單列字串）正規化成「儲存鍵」：
/// IPv4 原樣、`::ffff:a.b.c.d` 收斂成 IPv4、IPv6 收斂為 `/64` 群組鍵；
/// 也接受既有清單裡的 `"xxxx/64"` 形式（解除封鎖時前端原樣送回）並重新正規化。
/// 其他輸入一律拒絕（回 `None`）。
pub fn normalize_blacklist_key(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    if value.parse::<IpAddr>().is_ok() {
        return Some(blacklist_group_key(value));
    }
    if let Some((addr, len)) = value.split_once('/') {
        if len.trim() == "64" && matches!(addr.trim().parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
            return Some(blacklist_group_key(addr));
        }
    }
    None
}

/// Sanitize an untrusted forwarded value into a canonical IP string, or `None`.
fn normalized_ip(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    value
        .parse::<IpAddr>()
        .ok()
        .map(canonicalize_ip)
        .map(|ip| ip.to_string())
}

/// Resolve the effective client IP for a request.
///
/// `peer` is the real TCP peer address (from `ConnectInfo`). When it is a
/// trusted proxy, the forwarded headers are honoured (preferring
/// `CF-Connecting-IP`, then `True-Client-IP`, then `X-Real-IP`, then the first
/// entry of `X-Forwarded-For`). Otherwise the peer address itself is used so a
/// public origin cannot spoof the value.
pub fn resolve_client_ip(
    peer: Option<IpAddr>,
    headers: &http::HeaderMap,
    trusted: &TrustedProxies,
) -> String {
    // IP_CANONICAL_V4_MAPPED_V1：先做位址收斂——雙疊監聽下 `::ffff:127.0.0.1`
    // 也應被判為 loopback（可信任的本機代理），且最後輸出的字串才會與
    // 黑名單／節流鍵一致，不會出現「同一台機器兩個身分」。
    let peer = peer.map(canonicalize_ip);
    let from_trusted_proxy = trusted.is_trusted(peer);

    if from_trusted_proxy {
        for name in ["cf-connecting-ip", "true-client-ip", "x-real-ip"] {
            if let Some(value) = headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(normalized_ip)
            {
                return value;
            }
        }
        if let Some(value) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|value| value.split(',').next())
            .and_then(normalized_ip)
        {
            return value;
        }
    }

    match peer {
        Some(ip) => ip.to_string(),
        None => "unknown".to_string(),
    }
}

/// Build the trusted-proxy set from the current global config (loopback
/// always included). Falls back to loopback-only when config is unavailable.
pub fn trusted_from_global_config() -> TrustedProxies {
    let entries = config::try_get_config()
        .map(|cfg| cfg.adapters.admin_web.trusted_proxies.clone())
        .unwrap_or_default();
    TrustedProxies::from_config(&entries)
}

/// axum extractor: yields the effective client IP string for the request.
///
/// The peer address must have been attached by serving with
/// `into_make_service_with_connect_info::<SocketAddr>()`; when it is absent
/// (e.g. unit-test `oneshot` calls) the peer is `None` and forwarded headers
/// are not trusted.
pub struct ClientIp(pub String);

impl ClientIp {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip());
        let trusted = trusted_from_global_config();
        Ok(ClientIp(resolve_client_ip(peer, &parts.headers, &trusted)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hmap() -> http::HeaderMap {
        http::HeaderMap::new()
    }

    fn trusted(list: &[&str]) -> TrustedProxies {
        let owned: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        TrustedProxies::from_config(&owned)
    }

    #[test]
    fn loopback_always_trusted() {
        let t = trusted(&[]);
        assert!(t.is_trusted(Some(IpAddr::from_str("127.0.0.1").unwrap())));
        assert!(t.is_trusted(Some(IpAddr::from_str("::1").unwrap())));
        assert!(!t.is_trusted(Some(IpAddr::from_str("8.8.8.8").unwrap())));
    }

    #[test]
    fn exact_ip_trusted() {
        let t = trusted(&["10.0.0.5"]);
        assert!(t.is_trusted(Some(IpAddr::from_str("10.0.0.5").unwrap())));
        assert!(!t.is_trusted(Some(IpAddr::from_str("10.0.0.6").unwrap())));
    }

    #[test]
    fn cidr_trusted() {
        let t = trusted(&["104.16.0.0/13"]);
        assert!(t.is_trusted(Some(IpAddr::from_str("104.20.200.1").unwrap())));
        assert!(!t.is_trusted(Some(IpAddr::from_str("104.31.0.1").unwrap())));
    }

    #[test]
    fn cloudflare_keyword_expands() {
        let t = trusted(&["cloudflare"]);
        assert!(t.is_trusted(Some(IpAddr::from_str("104.18.0.1").unwrap())));
        assert!(t.is_trusted(Some(IpAddr::from_str("2606:4700::1234").unwrap())));
    }

    #[test]
    fn untrusted_peer_ignores_forwarded_headers() {
        let mut headers = hmap();
        headers.insert(
            "x-forwarded-for",
            http::HeaderValue::from_static("6.6.6.6"),
        );
        let t = trusted(&[]);
        // Peer is public & not trusted → must NOT trust the forged header.
        let ip = resolve_client_ip(
            Some(IpAddr::from_str("203.0.113.9").unwrap()),
            &headers,
            &t,
        );
        assert_eq!(ip, "203.0.113.9");
    }

    #[test]
    fn trusted_proxy_prefers_cf_connecting_ip() {
        let mut headers = hmap();
        headers.insert(
            "cf-connecting-ip",
            http::HeaderValue::from_static("9.9.9.9"),
        );
        headers.insert(
            "x-forwarded-for",
            http::HeaderValue::from_static("8.8.8.8, 7.7.7.7"),
        );
        let t = trusted(&["203.0.113.9"]);
        let ip = resolve_client_ip(
            Some(IpAddr::from_str("203.0.113.9").unwrap()),
            &headers,
            &t,
        );
        assert_eq!(ip, "9.9.9.9");
    }

    #[test]
    fn trusted_proxy_falls_back_to_xff_first_hop() {
        let mut headers = hmap();
        headers.insert(
            "x-forwarded-for",
            http::HeaderValue::from_static("8.8.8.8, 7.7.7.7"),
        );
        let t = trusted(&["203.0.113.9"]);
        let ip = resolve_client_ip(
            Some(IpAddr::from_str("203.0.113.9").unwrap()),
            &headers,
            &t,
        );
        assert_eq!(ip, "8.8.8.8");
    }

    #[test]
    fn no_peer_returns_unknown() {
        let t = trusted(&[]);
        let ip = resolve_client_ip(None, &hmap(), &t);
        assert_eq!(ip, "unknown");
    }

    #[test]
    fn malformed_cidr_is_ignored() {
        let t = trusted(&["not-an-ip", "999.0.0.0/8", "127.0.0.1//8"]);
        // Only loopback remains.
        assert!(t.is_trusted(Some(IpAddr::from_str("127.0.0.1").unwrap())));
        assert!(!t.is_trusted(Some(IpAddr::from_str("10.0.0.1").unwrap())));
    }

    // ─── IP_CANONICAL_V4_MAPPED_V1 ───────────────────────────────────────────

    #[test]
    fn v4_mapped_peer_is_canonicalized_and_trusted_as_loopback() {
        // 雙疊（[::]）監聽時，本機代理的 IPv4 連線會以 ::ffff:127.0.0.1 抵達；
        // 收斂後必須回到 loopback 信任清單，且輸出字串是純 IPv4。
        let mut headers = hmap();
        headers.insert("cf-connecting-ip", http::HeaderValue::from_static("9.9.9.9"));
        let t = trusted(&[]);
        let ip = resolve_client_ip(
            Some(IpAddr::from_str("::ffff:127.0.0.1").unwrap()),
            &headers,
            &t,
        );
        assert_eq!(ip, "9.9.9.9");
    }

    #[test]
    fn v4_mapped_public_peer_keeps_single_blacklist_identity() {
        // 非信任來源：忽略偽造標頭，輸出收斂成純 IPv4——與黑名單儲存字串一致。
        let mut headers = hmap();
        headers.insert("x-forwarded-for", http::HeaderValue::from_static("6.6.6.6"));
        let t = trusted(&[]);
        let ip = resolve_client_ip(
            Some(IpAddr::from_str("::ffff:203.0.113.9").unwrap()),
            &headers,
            &t,
        );
        assert_eq!(ip, "203.0.113.9");
    }

    #[test]
    fn loopback_v6_is_not_remapped_to_ipv4() {
        // ::1 不是 v4-mapped，絕不能被改寫成 0.0.0.1。
        let ip = canonicalize_ip(IpAddr::from_str("::1").unwrap());
        assert_eq!(ip, IpAddr::from_str("::1").unwrap());
    }

    // ─── IP_BLACKLIST_V6_GROUP_V1 ─────────────────────────────────────────────

    #[test]
    fn v6_group_key_collapses_to_slash64() {
        assert_eq!(
            blacklist_group_key("2001:db8:abcd:1234:aaaa:bbbb:cccc:dddd"),
            "2001:db8:abcd:1234::/64"
        );
        assert_eq!(
            blacklist_group_key("2001:db8:abcd:1234::1"),
            "2001:db8:abcd:1234::/64"
        );
        assert_eq!(blacklist_group_key("203.0.113.9"), "203.0.113.9");
        assert_eq!(blacklist_group_key("::ffff:203.0.113.9"), "203.0.113.9");
        assert_eq!(blacklist_group_key("unknown"), "unknown");
    }

    #[test]
    fn normalize_blacklist_key_accepts_ip_and_stored_prefix() {
        assert_eq!(
            normalize_blacklist_key("203.0.113.9").as_deref(),
            Some("203.0.113.9")
        );
        assert_eq!(
            normalize_blacklist_key("2001:db8:abcd:1234:ffff::1").as_deref(),
            Some("2001:db8:abcd:1234::/64")
        );
        assert_eq!(
            normalize_blacklist_key("2001:DB8:abcd:1234::/64").as_deref(),
            Some("2001:db8:abcd:1234::/64")
        );
        assert_eq!(normalize_blacklist_key("1.2.3.0/24"), None);
        assert_eq!(normalize_blacklist_key("not-an-ip"), None);
        assert_eq!(normalize_blacklist_key(""), None);
    }
}
