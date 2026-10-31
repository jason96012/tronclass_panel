//! LOGIN_GEO_ALERT_V1：登入來源 IP 的國家判定。
//!
//! 只給「新位置登入」告警用（`check_unusual_login_location`），目的：
//! 台灣境內的新 IP（換 Wi-Fi、行動網路換基地台、學校／家裡）不再通知，
//! 只有境外、Tor、或其他真正異常才通知。
//!
//! 來源優先順序：
//! 1. Cloudflare `CF-IPCountry` 標頭（Tunnel 部署免費附帶；只在可信代理送來時採用，
//!    `sanitize_forwarded_headers_middleware` 已把不可信來源的這個標頭拿掉）。
//!    特殊值：`XX` = 未知、`T1` = Tor 出口節點。
//! 2. 沒有標頭時查 `https://ipwho.is/{ip}`（免金鑰、免費額度足夠登入量），
//!    結果以 IP 為鍵快取 24 小時、最多 512 筆；逾時 4 秒；失敗回「未知」。
//!
//! 私有／loopback IP 不查（呼叫端已先排除）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_CACHE_ENTRIES: usize = 512;
const LOOKUP_TIMEOUT_SECS: u64 = 4;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeoInfo {
    /// ISO 3166-1 alpha-2 大寫；空字串 = 未知。
    pub country_code: String,
    /// 英文國名（ipwho.is）；CF 標頭來源時為空。
    pub country: String,
    pub region: String,
    pub city: String,
    pub isp: String,
    /// Cloudflare 標記為 Tor 出口節點（`T1`）。
    pub is_tor: bool,
    /// "cloudflare" / "ipwho.is" / ""
    pub source: &'static str,
}

impl GeoInfo {
    /// 給通知用的一行位置描述，例如「日本 (JP) · Tokyo · NTT」；未知時「未知位置」。
    pub fn label(&self) -> String {
        if self.is_tor {
            return "Tor 出口節點".to_string();
        }
        if self.country_code.is_empty() {
            return "未知位置".to_string();
        }
        let mut parts: Vec<String> = Vec::new();
        parts.push(format!(
            "{} ({})",
            country_name_zh(&self.country_code).unwrap_or(if self.country.is_empty() {
                self.country_code.as_str()
            } else {
                self.country.as_str()
            }),
            self.country_code
        ));
        let place = if !self.city.is_empty() {
            self.city.clone()
        } else {
            self.region.clone()
        };
        if !place.is_empty() {
            parts.push(place);
        }
        if !self.isp.is_empty() {
            parts.push(self.isp.clone());
        }
        parts.join(" · ")
    }
}

/// 常見國家／地區的中文名（其餘顯示英文名或代碼）。
fn country_name_zh(code: &str) -> Option<&'static str> {
    Some(match code {
        "TW" => "台灣",
        "JP" => "日本",
        "KR" => "南韓",
        "CN" => "中國",
        "HK" => "香港",
        "MO" => "澳門",
        "SG" => "新加坡",
        "MY" => "馬來西亞",
        "TH" => "泰國",
        "VN" => "越南",
        "PH" => "菲律賓",
        "ID" => "印尼",
        "IN" => "印度",
        "AU" => "澳洲",
        "NZ" => "紐西蘭",
        "US" => "美國",
        "CA" => "加拿大",
        "GB" => "英國",
        "DE" => "德國",
        "FR" => "法國",
        "NL" => "荷蘭",
        "RU" => "俄羅斯",
        "BR" => "巴西",
        _ => return None,
    })
}

/// 解析 Cloudflare `CF-IPCountry` 標頭值。
pub fn from_cloudflare_header(value: &str) -> Option<GeoInfo> {
    let code = value.trim().to_ascii_uppercase();
    if code.is_empty() {
        return None;
    }
    if code == "T1" {
        return Some(GeoInfo {
            is_tor: true,
            source: "cloudflare",
            ..Default::default()
        });
    }
    if code == "XX" || code.len() != 2 || !code.bytes().all(|b| b.is_ascii_uppercase()) {
        // 未知：仍回 Some（來源可信、結論是未知），呼叫端不必再查外部服務。
        return Some(GeoInfo {
            source: "cloudflare",
            ..Default::default()
        });
    }
    Some(GeoInfo {
        country_code: code,
        source: "cloudflare",
        ..Default::default()
    })
}

static CACHE: Lazy<Mutex<HashMap<String, (Instant, GeoInfo)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(LOOKUP_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(3))
        .pool_max_idle_per_host(2)
        .user_agent("TronclassPanel/1.0 (login-geo-check)")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

fn cache_get(ip: &str) -> Option<GeoInfo> {
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    cache.retain(|_, (at, _)| now.saturating_duration_since(*at) < CACHE_TTL);
    cache.get(ip).map(|(_, info)| info.clone())
}

fn cache_put(ip: &str, info: &GeoInfo) {
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if cache.len() >= MAX_CACHE_ENTRIES && !cache.contains_key(ip) {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(ip.to_string(), (Instant::now(), info.clone()));
}

fn pick_str(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| crate::text_utils::truncate_chars(s, 60).to_string())
        .unwrap_or_default()
}

fn parse_ipwho(body: &serde_json::Value) -> Option<GeoInfo> {
    if body.get("success").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    let code = pick_str(body, "country_code").to_ascii_uppercase();
    if code.len() != 2 {
        return None;
    }
    let isp = body
        .get("connection")
        .map(|c| {
            let isp = pick_str(c, "isp");
            if isp.is_empty() {
                pick_str(c, "org")
            } else {
                isp
            }
        })
        .unwrap_or_default();
    Some(GeoInfo {
        country_code: code,
        country: pick_str(body, "country"),
        region: pick_str(body, "region"),
        city: pick_str(body, "city"),
        isp,
        is_tor: false,
        source: "ipwho.is",
    })
}

/// 查外部服務（含快取）。查不到／逾時回 `GeoInfo::default()`（country_code 為空）。
/// 失敗結果也快取（避免服務掛掉時每次登入都等 4 秒），但只快取 10 分鐘。
pub async fn lookup(ip: &str) -> GeoInfo {
    let ip = ip.trim();
    if ip.is_empty() || ip.parse::<std::net::IpAddr>().is_err() {
        return GeoInfo::default();
    }
    if let Some(hit) = cache_get(ip) {
        return hit;
    }
    let url = format!("https://ipwho.is/{ip}");
    let info = match CLIENT.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(body) => parse_ipwho(&body),
            Err(error) => {
                tracing::debug!(%ip, %error, "ipwho.is 回應解析失敗");
                None
            }
        },
        Ok(resp) => {
            tracing::debug!(%ip, status = %resp.status(), "ipwho.is 回應非 2xx");
            None
        }
        Err(error) => {
            tracing::debug!(%ip, %error, "ipwho.is 查詢失敗");
            None
        }
    };
    match info {
        Some(info) => {
            cache_put(ip, &info);
            info
        }
        None => {
            let unknown = GeoInfo::default();
            // 短快取：把 Instant 往前撥，讓 TTL 只剩 10 分鐘。
            let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
            let short_ttl_at = Instant::now()
                .checked_sub(CACHE_TTL - Duration::from_secs(10 * 60))
                .unwrap_or_else(Instant::now);
            cache.insert(ip.to_string(), (short_ttl_at, unknown.clone()));
            unknown
        }
    }
}

/// 綜合判定：優先用 Cloudflare 標頭，沒有才查外部。
pub async fn resolve(ip: &str, cf_ipcountry: Option<&str>) -> GeoInfo {
    if let Some(info) = cf_ipcountry.and_then(from_cloudflare_header) {
        // 標頭說「未知」（XX）時仍試一次外部查詢，能查到就補上。
        if !info.country_code.is_empty() || info.is_tor {
            return info;
        }
    }
    lookup(ip).await
}

