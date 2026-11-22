//! L7 全域速率限制（滑動窗口）。

use super::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

// WAF_AUTO_BLOCK_NOTIFY_V2：WAF 新建立自動封鎖時，透過 notify_waf_auto_block 只發 Discord 給
// admin_user_id（同一 IP 24 小時去重），附「解除封鎖」按鈕。不發 LINE、不發 Web Push。

/// WAF 全域滑動窗口時間跨度（60 秒）。
pub(super) const WAF_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
/// 每外部 IP 每分鐘最大請求上限（240 次 / 分鐘，相當於平均 4 req/sec）。
pub(super) const WAF_RATE_LIMIT_MAX_REQUESTS: usize = 240;
/// 滑動窗口快取清理閾值（最多保留 5000 個獨立 IP 記錄）。
pub(super) const WAF_RATE_LIMIT_MAX_IPS: usize = 5000;
/// 全域 L7 速率限制器狀態。
#[derive(Clone)]
pub struct WafRateLimiter {
    pub(super) attempts: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    pub(super) max_requests: usize,
    pub(super) window: Duration,
    pub(super) max_ips: usize,
}

impl Default for WafRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl WafRateLimiter {
    pub fn new() -> Self {
        Self::with_limits(WAF_RATE_LIMIT_MAX_REQUESTS, WAF_RATE_LIMIT_WINDOW, WAF_RATE_LIMIT_MAX_IPS)
    }

    /// 可注入上限的建構子（A2：每個引擎實例各自的限速參數，測試與設定共用）。
    pub fn with_limits(max_requests: usize, window: Duration, max_ips: usize) -> Self {
        Self {
            attempts: Arc::new(Mutex::new(HashMap::new())),
            max_requests,
            window,
            max_ips,
        }
    }

    /// 檢查 IP 是否超過 L7 請求頻率上限。
    pub async fn check_limit(&self, ip: &str) -> bool {
        let mut map = self.attempts.lock().await;
        let now = Instant::now();
        let window_start = now.checked_sub(self.window).unwrap_or(now);

        let entry = map.entry(ip.to_string()).or_default();
        entry.retain(|t| *t > window_start);

        if entry.len() >= self.max_requests {
            return false;
        }

        entry.push(now);

        // 容量超過上限時清理（有硬上限，且有遲滯，不會每個請求都整張表掃描）。
        bound_tracker(
            &mut *map,
            self.max_ips,
            |v| v.last().map_or(true, |t| *t <= window_start),
            |v| v.last().copied(),
        );
        true
    }
}
