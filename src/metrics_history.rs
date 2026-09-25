//! METRICS_HISTORY_V1：Uptime 與延遲的時間序列（純記憶體 ring buffer）。
//!
//! - 每 `SAMPLE_INTERVAL`（60 秒）取一筆系統快照：RAM 使用率、本程序 RSS、
//!   load average、監控中帳號數、連續失敗加總、以及最近一分鐘的輪詢延遲
//!   （由 `monitor.rs` 在每次 `do_poll_and_attend` 結束時呼叫 `record_poll_latency`）。
//! - 保留 `MAX_SAMPLES`（1440 筆 = 24 小時）；重啟後歸零，不落地——
//!   這份資料是「看趨勢」用，不值得為它多一張表。
//! - `/admin/api/health/history` 回傳整段給維護頁畫 SVG 折線。
//!
//! 取樣 task 由 `main.rs` 啟動；沒有 tokio runtime 時（單元測試）record_* 仍可安全呼叫。

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);
pub const MAX_SAMPLES: usize = 1440;

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    /// Unix 秒。
    pub ts: i64,
    /// 系統 RAM 使用率 0–100（無法讀 /proc/meminfo 時為 null）。
    pub mem_pct: Option<f32>,
    /// 本程序 RSS（bytes）。
    pub rss_bytes: Option<u64>,
    /// 1 分鐘 load average。
    pub load1: Option<f32>,
    /// 目前 is_running 的帳號數／總帳號數。
    pub running: u32,
    pub total: u32,
    /// 全部帳號 consecutive_failures 加總。
    pub failures: u32,
    /// 這一分鐘內輪詢延遲（毫秒）：平均、最大、次數；沒有輪詢時皆為 null/0。
    pub poll_avg_ms: Option<u32>,
    pub poll_max_ms: Option<u32>,
    pub poll_count: u32,
    /// 這一分鐘內輪詢失敗次數（Err 回傳）。
    pub poll_errors: u32,
}

#[derive(Default)]
struct PendingLatency {
    sum_ms: u64,
    max_ms: u32,
    count: u32,
    errors: u32,
}

struct Store {
    samples: VecDeque<Sample>,
    pending: PendingLatency,
    started_at: i64,
}

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();

fn store() -> &'static Mutex<Store> {
    STORE.get_or_init(|| {
        Mutex::new(Store {
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            pending: PendingLatency::default(),
            started_at: chrono::Local::now().timestamp(),
        })
    })
}

/// monitor.rs 在每次輪詢結束時呼叫；`ok=false` 代表這次輪詢回 Err。
pub fn record_poll_latency(elapsed: Duration, ok: bool) {
    let ms = elapsed.as_millis().min(u128::from(u32::MAX)) as u32;
    let mut guard = store().lock().unwrap_or_else(|p| p.into_inner());
    let p = &mut guard.pending;
    p.sum_ms += u64::from(ms);
    p.max_ms = p.max_ms.max(ms);
    p.count += 1;
    if !ok {
        p.errors += 1;
    }
}

/// 由取樣 task 每分鐘呼叫一次；`running/total/failures` 由呼叫端從 AccountRegistry 算好傳入。
pub fn push_sample(running: u32, total: u32, failures: u32) {
    let (mem_pct, rss_bytes, load1) = (read_mem_pct(), read_self_rss(), read_load1());
    let mut guard = store().lock().unwrap_or_else(|p| p.into_inner());
    let pending = std::mem::take(&mut guard.pending);
    let sample = Sample {
        ts: chrono::Local::now().timestamp(),
        mem_pct,
        rss_bytes,
        load1,
        running,
        total,
        failures,
        poll_avg_ms: (pending.count > 0).then(|| (pending.sum_ms / u64::from(pending.count)) as u32),
        poll_max_ms: (pending.count > 0).then_some(pending.max_ms),
        poll_count: pending.count,
        poll_errors: pending.errors,
    };
    if guard.samples.len() >= MAX_SAMPLES {
        guard.samples.pop_front();
    }
    guard.samples.push_back(sample);
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub started_at: i64,
    pub interval_secs: u64,
    pub max_samples: usize,
    pub samples: Vec<Sample>,
}

pub fn snapshot() -> Snapshot {
    let guard = store().lock().unwrap_or_else(|p| p.into_inner());
    Snapshot {
        started_at: guard.started_at,
        interval_secs: SAMPLE_INTERVAL.as_secs(),
        max_samples: MAX_SAMPLES,
        samples: guard.samples.iter().cloned().collect(),
    }
}

// ── /proc 讀取（Linux；其他平台回 None） ────────────────────────────────

fn read_mem_pct() -> Option<f32> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    let get_kb = |name: &str| -> Option<u64> {
        raw.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    let total = get_kb("MemTotal:")?;
    let avail = get_kb("MemAvailable:")?;
    if total == 0 {
        return None;
    }
    Some(((total.saturating_sub(avail)) as f32 / total as f32 * 100.0).clamp(0.0, 100.0))
}

fn read_self_rss() -> Option<u64> {
    let raw = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = raw.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages.saturating_mul(4096))
}

fn read_load1() -> Option<f32> {
    let raw = std::fs::read_to_string("/proc/loadavg").ok()?;
    raw.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_is_aggregated_into_next_sample() {
        record_poll_latency(Duration::from_millis(100), true);
        record_poll_latency(Duration::from_millis(300), false);
        push_sample(3, 4, 1);
        let snap = snapshot();
        let last = snap.samples.last().expect("has sample");
        assert_eq!(last.poll_count, 2);
        assert_eq!(last.poll_errors, 1);
        assert_eq!(last.poll_avg_ms, Some(200));
        assert_eq!(last.poll_max_ms, Some(300));
        assert_eq!((last.running, last.total, last.failures), (3, 4, 1));
        // pending 已被清空：下一筆沒有輪詢資料
        push_sample(3, 4, 1);
        let last = snapshot().samples.last().cloned().expect("has sample");
        assert_eq!(last.poll_count, 0);
        assert_eq!(last.poll_avg_ms, None);
    }

    #[test]
    fn ring_buffer_is_bounded() {
        for _ in 0..(MAX_SAMPLES + 5) {
            push_sample(0, 0, 0);
        }
        assert!(snapshot().samples.len() <= MAX_SAMPLES);
    }
}
