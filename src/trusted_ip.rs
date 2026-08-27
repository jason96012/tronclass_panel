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
            net: u128::from(Ipv4Addr::new(127, 0, 0, 0)),
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
            IpAddr::V4(v4) => (32u32, u128::from(v4)),
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

    fn is_trusted(&self, peer: Option<IpAddr>) -> bool {
        match peer {
            Some(IpAddr::V4(v4)) => {
                let value = u128::from(v4);
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

/// Sanitize an untrusted forwarded value into a canonical IP string, or `None`.
fn normalized_ip(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    value.parse::<IpAddr>().ok().map(|ip| ip.to_string())
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
}
