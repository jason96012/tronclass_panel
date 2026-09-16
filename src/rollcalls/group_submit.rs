//! 多帳號同一點名的送出協調。
//!
//! GROUP_SUBMIT_RELAY_V1（fix67）：同一場點名由多個帳號處理時，用一個「下一個可送出時間」
//! 的游標把送出時間錯開：
//!
//! - 第一位帳號立即送出（游標還是空的）；送出前先把游標推到「現在 + 抽到的間隔」。
//! - 之後每個帳號等到游標時間才送出；送出前再把游標往後推同樣的一個間隔。
//! - 間隔 = `base + U(0, jitter)`，`base`／`jitter` 由 runtime settings 決定
//!   （預設 base 600 ms、隨機上限 2000 ms；面板可調）。
//!
//! 兩個結構性保證（不靠參數大小巧合）：
//! 1. **順序不亂**：游標只會往前走，後位的排定時間一定不早於前一位。
//! 2. **最小間隔 = base**：游標每次至少前進 `base`，不會有兩個帳號幾乎同時送出。
//!
//! 同一帳號在同一場點名重試（例如 QR Code 重新取得 payload 後重送）會**沿用原本的排定時間**，
//! 不會再排一次隊——否則一直重試的帳號會把游標越推越遠，反而餓死其他帳號。
//!
//! 等的是「前一位**送出**的時間」，不是「前一位送完」：所以有人回應慢也不會卡住整串。

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
    /// 下一個可以送出的時間。None＝這場點名還沒有帳號排過。
    cursor: Option<Instant>,
    /// 這場點名每個帳號排定的送出時間（重試時沿用，不再排隊）。
    account_turns: HashMap<String, Instant>,
    last_used: Instant,
}

static GROUP_SUBMIT_STATE: Lazy<Mutex<HashMap<GroupSubmitKey, SubmitSequence>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 這一格要隔多久：`base + U(0, jitter)`。
///
/// `roll` 由呼叫端提供隨機值，換算成 0..=jitter 的毫秒數；`jitter = 0` 時完全等於
/// 固定間隔（舊行為）。`saturating` 系列運算確保不會溢位（KB §4：不 panic）。
fn submit_gap_ms(base_ms: u64, jitter_ms: u64, roll: u64) -> u64 {
    if jitter_ms == 0 {
        return base_ms;
    }
    let span = jitter_ms.saturating_add(1);
    base_ms.saturating_add(roll % span)
}

/// 排定這一格（只排隊、不等待）。
///
/// 回傳 `(排定時間, 是否沿用舊排定, 這一格抽到的間隔毫秒)`。
async fn reserve_turn(
    key: GroupSubmitKey,
    account_id: &str,
    now: Instant,
    base_ms: u64,
    jitter_ms: u64,
) -> (Instant, bool, u64) {
    let mut state = GROUP_SUBMIT_STATE.lock().await;
    cleanup_submit_state(&mut state);

    let sequence = state.entry(key).or_insert_with(|| SubmitSequence {
        cursor: None,
        account_turns: HashMap::new(),
        last_used: now,
    });
    sequence.last_used = now;

    if let Some(existing) = sequence.account_turns.get(account_id).copied() {
        // 同一帳號在同一場點名的重試：沿用原本的排定時間，不再排一次隊，
        // 否則一直重試的帳號會把游標越推越遠，其他帳號反而被餓死。
        return (existing, true, 0);
    }

    let gap_ms = submit_gap_ms(base_ms, jitter_ms, rand::random::<u64>());
    let planned = match sequence.cursor {
        Some(cursor) if cursor > now => cursor,
        _ => now,
    };
    sequence.cursor = planned.checked_add(Duration::from_millis(gap_ms));
    sequence.account_turns.insert(account_id.to_string(), planned);
    (planned, false, gap_ms)
}

/// 等待同一 provider + rollcall + 類型下輪到此帳號送出。
///
/// 第一位不等待；之後每位等到游標（＝前一位送出時間 + 抽到的間隔）才送出。
pub async fn wait_group_submit_turn(
    provider: &str,
    rollcall_id: u64,
    kind: GroupSubmitKind,
    account_id: &str,
) {
    let base_ms = crate::runtime_settings::group_submit_interval_ms().await;
    let jitter_ms = crate::runtime_settings::group_submit_jitter_ms().await;
    let now = Instant::now();
    let key = GroupSubmitKey {
        provider: provider.to_string(),
        rollcall_id,
        kind,
    };

    let (planned, reused, gap_ms) = reserve_turn(key, account_id, now, base_ms, jitter_ms).await;
    let wait = planned.saturating_duration_since(Instant::now());

    if wait > Duration::ZERO {
        info!(
            provider = %provider,
            rollcall_id = rollcall_id,
            kind = %kind,
            account = %account_id,
            base_ms = base_ms,
            jitter_ms = jitter_ms,
            gap_ms = gap_ms,
            wait_ms = wait.as_millis(),
            retry = reused,
            "多帳號點名送出等待輪次"
        );
        sleep(wait).await;
    } else {
        debug!(
            provider = %provider,
            rollcall_id = rollcall_id,
            kind = %kind,
            account = %account_id,
            base_ms = base_ms,
            jitter_ms = jitter_ms,
            gap_ms = gap_ms,
            retry = reused,
            "多帳號點名立即送出（游標已到或第一位）"
        );
    }
}

fn cleanup_submit_state(state: &mut HashMap<GroupSubmitKey, SubmitSequence>) {
    state.retain(|_, sequence| sequence.last_used.elapsed() < GROUP_SUBMIT_STATE_TTL);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(rollcall_id: u64) -> GroupSubmitKey {
        GroupSubmitKey {
            provider: "test".to_string(),
            rollcall_id,
            kind: GroupSubmitKind::Number,
        }
    }

    #[test]
    fn gap_is_base_plus_bounded_random() {
        for roll in [0u64, 1, 999, u64::MAX] {
            let gap = submit_gap_ms(600, 2000, roll);
            assert!((600..=2600).contains(&gap), "gap={gap} 超出 600..=2600");
        }
        // 隨機上限 0 ＝ 固定間隔（舊行為）。
        assert_eq!(submit_gap_ms(600, 0, u64::MAX), 600);
        assert_eq!(submit_gap_ms(0, 0, 123), 0);
    }

    #[tokio::test]
    async fn first_account_is_immediate_and_orders_the_rest() {
        let now = Instant::now();
        let (first, reused, _) = reserve_turn(key(101), "acc1", now, 600, 2000).await;
        assert_eq!(first, now);
        assert!(!reused);

        let (second, reused2, gap2) = reserve_turn(key(101), "acc2", now, 600, 2000).await;
        assert!(!reused2);
        assert!(second >= now + Duration::from_millis(600));
        assert!(second <= now + Duration::from_millis(2600));
        assert!((600..=2600).contains(&gap2));
    }

    #[tokio::test]
    async fn retry_reuses_turn_without_pushing_cursor() {
        let now = Instant::now();
        let (first, _, _) = reserve_turn(key(102), "acc1", now, 600, 2000).await;
        let (second, _, _) = reserve_turn(key(102), "acc2", now, 600, 2000).await;
        let (retry, reused, gap) = reserve_turn(key(102), "acc1", now, 600, 2000).await;
        assert!(reused);
        assert_eq!(retry, first);
        assert_eq!(gap, 0);
        // 游標沒有被重試推走：第三個帳號仍排在原本的位置之後。
        let (third, _, _) = reserve_turn(key(102), "acc3", now, 600, 2000).await;
        assert!(third >= second + Duration::from_millis(600));
    }

    #[tokio::test]
    async fn late_arrival_does_not_wait() {
        let now = Instant::now();
        let _ = reserve_turn(key(103), "acc1", now, 600, 2000).await;
        let late = now + Duration::from_secs(30);
        let (planned, reused, _) = reserve_turn(key(103), "acc2", late, 600, 2000).await;
        assert!(!reused);
        assert_eq!(planned, late);
    }

    #[tokio::test]
    async fn consecutive_gaps_never_below_base() {
        let now = Instant::now();
        let mut previous: Option<Instant> = None;
        for index in 0..12 {
            let account = format!("acc{index}");
            let (planned, _, _) = reserve_turn(key(104), &account, now, 600, 2000).await;
            if let Some(previous) = previous {
                assert!(planned.saturating_duration_since(previous) >= Duration::from_millis(600));
            }
            previous = Some(planned);
        }
    }
}
