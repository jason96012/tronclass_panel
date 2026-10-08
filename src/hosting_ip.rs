//! HOSTING_IP_BLOCK_V1：偵測來源 IP 是否屬於雲端／VPS／機房代管商
//! （AWS、Google Cloud、Microsoft Azure，以及其他各家 VPS／hosting 業者），
//! 不分國家、全站套用。
//!
//! 背景（使用者需求原文）：「像 AWS 或 Google Cloud 這種能不能自動阻擋不要讓
//! 他進入我的伺服器」——起因是 WAF 攔到的惡意掃描器（Palo Alto Networks
//! Cortex Xpanse 之類的全網掃描）幾乎都是從雲端機房 IP 發出的，而這個
//! Dashboard 的真實使用者（學生）幾乎不可能用雲端主機連線（他們用的是家用
//! 寬頻、校園網路、行動網路），所以把「這個 IP 是不是機房／VPS」本身當成一個
//! 獨立的全站封鎖條件是合理的。
//!
//! 涵蓋範圍刻意選「所有常見 VPS／機房代管商」而不是只列 AWS/GCP/Azure 三家：
//! 多數中小型 VPS 業者（DigitalOcean、Vultr、Hetzner、OVH、阿里雲、騰訊雲…）
//! 並不像 AWS/GCP/Azure 一樣公開「完整官方 IP 區段清單」，若自己維護一份
//! 「各家雲端商官方範圍」合併表，涵蓋率低、更新成本高，而且每家格式都不同。
//! 改用 ip-api.com 免費 JSON API 的 `hosting` 欄位（免金鑰、非商業用途免費，
//! 不需要付費方案）——這個欄位本來就是靠對方持續維護的「機房／代管／colocation」
//! ASN／IP 資料庫判斷，一次查詢就橫跨幾乎所有已知業者，不必我們自己維護清單。
//!
//! 查詢失敗／逾時：fail-open（視為「不是機房 IP」，不擋），與專案既有
//! `geoip` 模組查詢失敗時的處理原則一致——安全性加強功能本身掛掉，
//! 不該變成「把所有人都擋在外面」。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 查詢失敗（逾時／對方服務掛掉）的短快取——避免對方掛掉時，同一個 IP
/// 短時間內反覆重打，但也不會因為一次性的失敗就快取一整天都不擋。
const FAILURE_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_CACHE_ENTRIES: usize = 512;
const LOOKUP_TIMEOUT_SECS: u64 = 4;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostingInfo {
    /// 是否判定為雲端／VPS／機房代管商出口 IP。
    pub is_hosting: bool,
    pub isp: String,
    pub org: String,
    /// ISO 3166-1 alpha-2 大寫；空字串＝未知。
    pub country_code: String,
    pub country: String,
    pub city: String,
}

impl HostingInfo {
    /// 給封鎖頁與稽核紀錄用的一行描述，例如「Google LLC · 美國 (US) · Brussels」；
    /// 完全查不到資訊時退回「未知來源」。
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let org = if !self.isp.is_empty() {
            self.isp.clone()
        } else {
            self.org.clone()
        };
        if !org.is_empty() {
            parts.push(org);
        }
        if !self.country_code.is_empty() {
            parts.push(if !self.country.is_empty() {
                format!("{} ({})", self.country, self.country_code)
            } else {
                self.country_code.clone()
            });
        }
        if !self.city.is_empty() {
            parts.push(self.city.clone());
        }
        if parts.is_empty() {
            "未知來源".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

static CACHE: Lazy<Mutex<HashMap<String, (Instant, Option<HostingInfo>)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(LOOKUP_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(3))
        .pool_max_idle_per_host(2)
        .user_agent("TronclassPanel/1.0 (hosting-ip-check)")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

fn cache_get(ip: &str) -> Option<Option<HostingInfo>> {
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    cache.retain(|_, (at, info)| {
        let ttl = if info.is_some() { CACHE_TTL } else { FAILURE_CACHE_TTL };
        now.saturating_duration_since(*at) < ttl
    });
    cache.get(ip).map(|(_, info)| info.clone())
}

fn cache_put(ip: &str, info: &Option<HostingInfo>) {
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

fn parse_ip_api(body: &serde_json::Value) -> Option<HostingInfo> {
    if body.get("status").and_then(|v| v.as_str()) != Some("success") {
        return None;
    }
    let is_hosting = body.get("hosting").and_then(|v| v.as_bool()).unwrap_or(false);
    Some(HostingInfo {
        is_hosting,
        isp: pick_str(body, "isp"),
        org: pick_str(body, "org"),
        country_code: pick_str(body, "countryCode").to_ascii_uppercase(),
        country: pick_str(body, "country"),
        city: pick_str(body, "city"),
    })
}

/// 查詢（含快取）。查不到／逾時回傳 `None`——呼叫端據此 fail-open（不擋）。
pub async fn lookup(ip: &str) -> Option<HostingInfo> {
    let ip = ip.trim();
    if ip.is_empty() || ip.parse::<std::net::IpAddr>().is_err() {
        return None;
    }
    if let Some(hit) = cache_get(ip) {
        return hit;
    }
    // 免金鑰方案是 HTTP（非 HTTPS）端點；僅查詢公開可見的 IP 歸屬資訊
    // （不含任何使用者個資），風險與既有 geoip 模組查 ipwho.is 相當。
    let url = format!("http://ip-api.com/json/{ip}?fields=status,message,hosting,isp,org,countryCode,country,city");
    let info = match CLIENT.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(body) => parse_ip_api(&body),
            Err(error) => {
                tracing::debug!(%ip, %error, "ip-api.com 回應解析失敗");
                None
            }
        },
        Ok(resp) => {
            tracing::debug!(%ip, status = %resp.status(), "ip-api.com 回應非 2xx");
            None
        }
        Err(error) => {
            tracing::debug!(%ip, %error, "ip-api.com 查詢失敗");
            None
        }
    };
    cache_put(ip, &info);
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosting_true() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"status":"success","hosting":true,"isp":"Google LLC","org":"Google LLC","countryCode":"US","country":"United States","city":"Brussels"}"#,
        )
        .unwrap();
        let info = parse_ip_api(&body).unwrap();
        assert!(info.is_hosting);
        assert_eq!(info.label(), "Google LLC · United States (US) · Brussels");
    }

    #[test]
    fn parses_hosting_false() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"status":"success","hosting":false,"isp":"Chunghwa Telecom","countryCode":"TW","country":"Taiwan","city":"Taipei"}"#,
        )
        .unwrap();
        let info = parse_ip_api(&body).unwrap();
        assert!(!info.is_hosting);
    }

    #[test]
    fn fail_status_is_none() {
        let body: serde_json::Value =
            serde_json::from_str(r#"{"status":"fail","message":"invalid query"}"#).unwrap();
        assert!(parse_ip_api(&body).is_none());
        assert_eq!(HostingInfo::default().label(), "未知來源");
    }
}
