//! 多帳號同一點名的送出協調。
//!
//! 多個帳號同時處理同一個數字 / QR Code 點名時，透過這裡分配送出序號，
//! 讓第一個帳號立即送出，第二個延遲 0.5 秒，第三個延遲 1.0 秒，以此類推。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, info};

const GROUP_SUBMIT_STATE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupSubmitKind {
    Number,
    QrCode,
}

impl std::fmt::Display for GroupSubmitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupSubmitKind::Number => write!(f, "number"),
            GroupSubmitKind::QrCode => write!(f, "qrcode"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GroupSubmitKey {
    provider: String,
    rollcall_id: u64,
    kind: GroupSubmitKind,
}

#[derive(Debug)]
struct SubmitSequence {
    next_slot: usize,
    account_slots: HashMap<String, usize>,
    last_used: Instant,
}

static GROUP_SUBMIT_STATE: Lazy<Mutex<HashMap<GroupSubmitKey, SubmitSequence>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 等待同一 provider + rollcall + 類型下輪到此帳號送出。
///
/// 第一個帳號不延遲，第二個延遲 0.5 秒，第三個延遲 1.0 秒，以此類推。
pub async fn wait_group_submit_turn(
    provider: &str,
    rollcall_id: u64,
    kind: GroupSubmitKind,
    account_id: &str,
) {
    let slot = reserve_group_submit_slot(provider, rollcall_id, kind, account_id).await;
    let interval_ms = crate::runtime_settings::group_submit_interval_ms().await;
    let delay = Duration::from_millis(interval_ms * slot as u64);

    if delay > Duration::ZERO {
        info!(
            provider = %provider,
            rollcall_id = rollcall_id,
            kind = %kind,
            account = %account_id,
            slot = slot,
            delay_ms = delay.as_millis(),
            "多帳號點名送出等待輪次"
        );
        sleep(delay).await;
    } else {
        debug!(
            provider = %provider,
            rollcall_id = rollcall_id,
            kind = %kind,
            account = %account_id,
            slot = slot,
            "多帳號點名第一個送出，不延遲"
        );
    }
}

async fn reserve_group_submit_slot(
    provider: &str,
    rollcall_id: u64,
    kind: GroupSubmitKind,
    account_id: &str,
) -> usize {
    let key = GroupSubmitKey {
        provider: provider.to_string(),
        rollcall_id,
        kind,
    };

    let mut state = GROUP_SUBMIT_STATE.lock().await;
    cleanup_submit_state(&mut state);

    let sequence = state.entry(key).or_insert_with(|| SubmitSequence {
        next_slot: 0,
        account_slots: HashMap::new(),
        last_used: Instant::now(),
    });
    sequence.last_used = Instant::now();

    if let Some(slot) = sequence.account_slots.get(account_id) {
        return *slot;
    }

    let slot = sequence.next_slot;
    sequence.next_slot += 1;
    sequence.account_slots.insert(account_id.to_string(), slot);
    slot
}

fn cleanup_submit_state(state: &mut HashMap<GroupSubmitKey, SubmitSequence>) {
    state.retain(|_, sequence| sequence.last_used.elapsed() < GROUP_SUBMIT_STATE_TTL);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_account_reuses_same_submit_slot() {
        let first = reserve_group_submit_slot("p", 1, GroupSubmitKind::QrCode, "acc1").await;
        let same = reserve_group_submit_slot("p", 1, GroupSubmitKind::QrCode, "acc1").await;
        let second = reserve_group_submit_slot("p", 1, GroupSubmitKind::QrCode, "acc2").await;

        assert_eq!(first, 0);
        assert_eq!(same, 0);
        assert_eq!(second, 1);
    }
}
