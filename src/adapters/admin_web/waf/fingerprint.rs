//! 請求指紋追蹤（IPv6 以 /64 群組計數）。

use super::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::{header, HeaderMap, StatusCode};
use tokio::sync::Mutex;

// WAF_AUTO_BLOCK_NOTIFY_V2：WAF 新建立自動封鎖時，透過 notify_waf_auto_block 只發 Discord 給
// admin_user_id（同一 IP 24 小時去重），附「解除封鎖」按鈕。不發 LINE、不發 Web Push。

/// WAF_MEMORY_BOUND_V1：請求指紋追蹤表的鍵數硬上限。原本沒有任何清理，可無限成長。
pub(super) const WAF_FINGERPRINT_MAX_KEYS: usize = 10_000;
/// 指紋鍵的長度上限（UA 可達數 KB，攻擊者可用來放大記憶體）。
pub(super) const WAF_FINGERPRINT_KEY_MAX_BYTES: usize = 512;
/// 每個指紋最多記住的 IP 數。判斷門檻是 10，記到 32 不影響判斷結果。
pub(super) const WAF_FINGERPRINT_MAX_IPS_PER_KEY: usize = 32;
/// 同一指紋出現在這麼多不同 IP 即視為可疑。
pub(super) const WAF_FINGERPRINT_IP_THRESHOLD: usize = 10;
/// WAF_FINGERPRINT_WINDOW_V1：只計算最近這段時間內出現過的 IP（原本是程序存活期間的累積計數，
/// 造成一般 iPhone 用戶被誤封，見 activity_events 2026-10-31 21:56:38）。
pub(super) const WAF_FINGERPRINT_WINDOW: Duration = Duration::from_secs(3600);
/// 單一請求指紋：基於 User-Agent + Accept-Language + Accept-Encoding 等標頭，
/// 用於偵測同一攻擊者輪換 IP 的情況。
pub(super) fn request_fingerprint(headers: &HeaderMap) -> String {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let accept_lang = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let accept_enc = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let sec_ch_ua = headers
        .get(header::HeaderName::from_static("sec-ch-ua"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    format!(
        "{}|{}|{}|{}",
        ua, accept_lang, accept_enc, sec_ch_ua
    )
}

/// 請求指紋追蹤：同一指紋在多個 IP 出現時，提示 IP 輪換攻擊。
#[derive(Clone)]
pub struct WafFingerprintTracker {
    /// 指紋 -> （最後活動時間、見過的 IP）
    pub(super) seen: Arc<Mutex<HashMap<String, FingerprintEntry>>>,
    pub(super) threshold: usize,
    pub(super) window: Duration,
    pub(super) max_ips_per_key: usize,
    pub(super) key_max_bytes: usize,
    pub(super) max_keys: usize,
    /// R-02：組合鍵（指紋 hash｜IP 群組）-> 最後一次攻擊信號的時間。
    /// 指紋可疑且組合在時間窗內有信號，才會進入指紋封鎖。
    pub(super) signals: Arc<Mutex<HashMap<String, Instant>>>,
}

#[derive(Clone)]
pub(super) struct FingerprintEntry {
    pub(super) last_seen: Instant,
    /// (IP, 最後出現時間)；超出時間窗的會在下次存取時移除。
    pub(super) ips: Vec<(String, Instant)>,
}

impl Default for WafFingerprintTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl WafFingerprintTracker {
    pub fn new() -> Self {
        Self::with_limits(
            WAF_FINGERPRINT_IP_THRESHOLD,
            WAF_FINGERPRINT_WINDOW,
            WAF_FINGERPRINT_MAX_IPS_PER_KEY,
            WAF_FINGERPRINT_KEY_MAX_BYTES,
            WAF_FINGERPRINT_MAX_KEYS,
        )
    }

    /// 可注入門檻與上限的建構子（A2）。
    pub fn with_limits(
        threshold: usize,
        window: Duration,
        max_ips_per_key: usize,
        key_max_bytes: usize,
        max_keys: usize,
    ) -> Self {
        Self {
            seen: Arc::new(Mutex::new(HashMap::new())),
            threshold,
            window,
            max_ips_per_key,
            key_max_bytes,
            max_keys,
            signals: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// R-02：記錄組合的攻擊信號（威脅命中，或非 401／403／429 的 4xx）。
    pub async fn mark_signal(&self, pair: &str) {
        self.mark_signal_at(pair, Instant::now()).await;
    }

    pub(super) async fn mark_signal_at(&self, pair: &str, now: Instant) {
        let window_start = now.checked_sub(self.window).unwrap_or(now);
        let mut map = self.signals.lock().await;
        map.insert(pair.to_string(), now);
        bound_tracker(&mut *map, self.max_keys, |t| *t <= window_start, |t| Some(*t));
    }

    /// R-02：組合在時間窗（與指紋追蹤相同，預設 1 小時）內是否有攻擊信號。
    pub async fn has_recent_signal(&self, pair: &str) -> bool {
        self.has_recent_signal_at(pair, Instant::now()).await
    }

    pub(super) async fn has_recent_signal_at(&self, pair: &str, now: Instant) -> bool {
        let window_start = now.checked_sub(self.window).unwrap_or(now);
        let map = self.signals.lock().await;
        map.get(pair).is_some_and(|t| *t > window_start)
    }

    /// 記錄指紋與 IP。若指紋已在多個不同 IP 出現，回傳 true（懷疑 IP 輪換）。
    /// 閾值設為 10，避免正常用戶在家裡／公司／手機熱點等不同網路間切換時被誤封。
    pub async fn record_and_check(&self, fingerprint: &str, ip: &str) -> bool {
        self.record_and_check_at(fingerprint, ip, Instant::now()).await
    }

    /// 同 `record_and_check`，但時間由呼叫端提供（測試用）。
    pub(super) async fn record_and_check_at(&self, fingerprint: &str, ip: &str, now: Instant) -> bool {
        let key = truncate_to(fingerprint, self.key_max_bytes);
        // FINGERPRINT_GROUP_V1：計數單位與封鎖單位一致（IPv6 收斂為 /64）。
        // 否則同一支手機的隱私位址輪換（同一 /64 內不斷換位址）會被算成多個 IP，
        // 一小時內就達到門檻，造成正常用戶誤封（2026-10-31 iPhone 誤判即此原因）。
        let group = crate::trusted_ip::blacklist_group_key(ip);
        let window_start = now.checked_sub(self.window).unwrap_or(now);
        let mut map = self.seen.lock().await;
        let entry = map.entry(key).or_insert_with(|| FingerprintEntry {
            last_seen: now,
            ips: Vec::new(),
        });
        entry.last_seen = now;
        entry.ips.retain(|(_, t)| *t > window_start);
        if let Some(slot) = entry.ips.iter_mut().find(|(a, _)| *a == group) {
            slot.1 = now;
        } else if entry.ips.len() < self.max_ips_per_key {
            entry.ips.push((group, now));
        }
        let suspicious = entry.ips.len() >= self.threshold;
        bound_tracker(
            &mut *map,
            self.max_keys,
            |e| e.last_seen <= window_start,
            |e| Some(e.last_seen),
        );
        suspicious
    }
}

/// 指紋鍵截斷到預設上限（測試用；執行期使用 `truncate_to` 與實例上限）。
#[cfg(test)]
pub(super) fn truncate_fingerprint_key(fingerprint: &str) -> String {
    truncate_to(fingerprint, WAF_FINGERPRINT_KEY_MAX_BYTES)
}

/// 指紋鍵截斷到指定上限（以字元邊界為準）。
pub(super) fn truncate_to(fingerprint: &str, max_bytes: usize) -> String {
    if fingerprint.len() <= max_bytes {
        return fingerprint.to_string();
    }
    let mut end = max_bytes;
    while !fingerprint.is_char_boundary(end) {
        end -= 1;
    }
    fingerprint[..end].to_string()
}

/// R-02：狀態碼是否算作攻擊信號。
/// 算：4xx（404、400 等探測與錯誤請求）。不算：401（session 過期）、403（存取控制）、429（限流）。
/// 5xx 不算（通常是伺服器錯誤，不是來源的行為）。
pub(super) fn is_fingerprint_signal_status(status: StatusCode) -> bool {
    status.is_client_error() && !matches!(status.as_u16(), 401 | 403 | 429)
}

/// 指紋封鎖第一次的時長（秒）：5 分鐘；之後每次加倍。
pub(super) const FP_BLOCK_BASE_SECS: i64 = 5 * 60;
/// 單次封鎖時長上限：24 小時。這是時間封鎖，不是永久封鎖。
pub(super) const FP_BLOCK_CAP_SECS: i64 = 24 * 60 * 60;
/// 上次封鎖結束後連續這段時間沒有再犯，次數歸零。
pub(super) const FP_STRIKE_RESET_SECS: i64 = 24 * 60 * 60;

/// 指紋封鎖的組合鍵：`sha256(指紋)` 前 8 bytes（16 hex）＋`|`＋IP 群組（IPv4 單一 IP，IPv6 為 /64）。
/// 放行必須綁定 IP 群組：只用指紋當鍵，同一款常見 UA 的所有人會因一次放行全部豁免。
pub(super) fn fingerprint_pair(fingerprint: &str, ip: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(fingerprint.as_bytes());
    let fp_hash = hex::encode(&digest[..8]);
    format!("{fp_hash}|{}", crate::trusted_ip::blacklist_group_key(ip))
}

/// 計算下一次指紋封鎖。回傳 `(新次數, 解除時間 unix 秒)`。
/// 時長：5 分鐘 × 2^(次數-1)，上限 24 小時。
pub(super) fn next_fingerprint_block(prev_strikes: i64, prev_blocked_until: i64, now: i64) -> (i64, i64) {
    let quiet_long_enough =
        prev_strikes > 0 && now.saturating_sub(prev_blocked_until) >= FP_STRIKE_RESET_SECS;
    let base = if quiet_long_enough { 0 } else { prev_strikes.max(0) };
    let strikes = base.saturating_add(1);
    let shift = (strikes - 1).min(30) as u32;
    let duration = FP_BLOCK_BASE_SECS
        .saturating_mul(1i64 << shift)
        .min(FP_BLOCK_CAP_SECS);
    (strikes, now.saturating_add(duration))
}

#[cfg(test)]
mod block_schedule_tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps_at_24h() {
        let now = 1_000_000;
        let mut strikes = 0;
        let mut until = 0;
        let mut durations = Vec::new();
        for _ in 0..12 {
            let (s, u) = next_fingerprint_block(strikes, until, now);
            durations.push(u - now);
            strikes = s;
            until = u;
        }
        assert_eq!(&durations[..4], &[300, 600, 1200, 2400]);
        assert_eq!(*durations.last().unwrap(), FP_BLOCK_CAP_SECS);
        assert!(durations.iter().all(|d| *d <= FP_BLOCK_CAP_SECS), "時長不得超過 24 小時");
        assert_eq!(strikes, 12);
    }

    #[test]
    fn strikes_reset_only_after_full_quiet_day() {
        let block_end = 2_000_000;
        // 還差 1 秒才滿 24 小時：次數延續。
        let (s, _) = next_fingerprint_block(5, block_end, block_end + FP_STRIKE_RESET_SECS - 1);
        assert_eq!(s, 6);
        // 滿 24 小時沒有再犯：歸零，從第 1 次重新計算。
        let (s, u) = next_fingerprint_block(5, block_end, block_end + FP_STRIKE_RESET_SECS);
        assert_eq!(s, 1);
        assert_eq!(u - (block_end + FP_STRIKE_RESET_SECS), FP_BLOCK_BASE_SECS);
    }

    #[test]
    fn pair_is_stable_and_scoped_to_ip_group() {
        let a = fingerprint_pair("UA-X|en|gzip", "203.0.113.7");
        assert_eq!(a, fingerprint_pair("UA-X|en|gzip", "203.0.113.7"), "同輸入必須得到同鍵");
        let (hash, group) = a.split_once('|').expect("格式為 hash|group");
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(group.contains("203.0.113.7"));
        assert_ne!(a, fingerprint_pair("UA-Y|en|gzip", "203.0.113.7"), "不同指紋必須分開");
        assert_ne!(a, fingerprint_pair("UA-X|en|gzip", "203.0.113.8"), "不同 IP 必須分開");
    }

    #[test]
    fn ipv6_privacy_addresses_share_one_pair() {
        let a = fingerprint_pair("UA-X", "2001:db8:1:2:aaaa:bbbb:cccc:0001");
        let b = fingerprint_pair("UA-X", "2001:db8:1:2:dead:beef:0000:0002");
        assert_eq!(a, b, "同一 /64 的隱私位址輪換應共用一組封鎖與放行");
    }
}

#[cfg(test)]
mod signal_gate_tests {
    use super::*;

    fn tracker() -> WafFingerprintTracker {
        WafFingerprintTracker::with_limits(10, Duration::from_secs(3600), 32, 512, 100)
    }

    /// R-02：信號只在時間窗內有效（與指紋追蹤相同：1 小時）。
    #[tokio::test]
    async fn signal_expires_after_the_window() {
        let t = tracker();
        let base = Instant::now();
        let pair = "0123456789abcdef|203.0.113.7";
        assert!(!t.has_recent_signal_at(pair, base).await, "沒有信號時不得封鎖");
        t.mark_signal_at(pair, base).await;
        assert!(t.has_recent_signal_at(pair, base + Duration::from_secs(3599)).await);
        assert!(!t.has_recent_signal_at(pair, base + Duration::from_secs(3601)).await);
    }

    /// R-02：信號以組合為單位，不影響同指紋的其他 IP 群組。
    #[tokio::test]
    async fn signal_is_scoped_to_one_pair() {
        let t = tracker();
        let now = Instant::now();
        t.mark_signal_at("0123456789abcdef|203.0.113.7", now).await;
        assert!(!t.has_recent_signal_at("0123456789abcdef|198.51.100.9", now).await);
        assert!(!t.has_recent_signal_at("fedcba9876543210|203.0.113.7", now).await);
    }

    /// R-02：只有探測類 4xx 算信號；401、403、429 與 5xx 不算。
    #[test]
    fn only_probing_4xx_counts_as_a_signal() {
        for code in [400u16, 404, 405, 410] {
            assert!(is_fingerprint_signal_status(StatusCode::from_u16(code).unwrap()), "{code} 應算信號");
        }
        for code in [200u16, 301, 401, 403, 429, 500, 503] {
            assert!(!is_fingerprint_signal_status(StatusCode::from_u16(code).unwrap()), "{code} 不應算信號");
        }
    }
}
