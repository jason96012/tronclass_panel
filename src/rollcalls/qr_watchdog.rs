//! QR_STALL_WATCHDOG_V1: source-local health state, not a global monitor restart.
//! The supervisor owns cancellation; this module never sends attendance requests.
use std::time::{Duration, Instant};

pub(crate) const QR_REPLAY_MAX_AGE: Duration = Duration::from_secs(15);
pub(crate) const QR_MAX_RECOVERIES: u32 = 2;
const DELIVERY_STALL: Duration = Duration::from_secs(60);
const STEP_STALL: Duration = Duration::from_secs(90);
const RECOVERY_COOLDOWN: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QrPhase {
    Starting,
    WaitingInput,
    Checking,
    WaitingTurn,
    Submitting,
    Verifying,
}

#[derive(Debug)]
pub(crate) struct QrHealth {
    phase: QrPhase,
    phase_since: Instant,
    queued_since: Option<Instant>,
    seen_qr: bool,
    uncertain_since: Option<Instant>,
    uncertain_count: u32,
    pub(crate) recoveries: u32,
    last_recovery: Option<Instant>,
    pub(crate) notice: Option<String>,
}

impl QrHealth {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            phase: QrPhase::Starting, phase_since: now, queued_since: None,
            seen_qr: false, uncertain_since: None, uncertain_count: 0,
            recoveries: 0, last_recovery: None, notice: None,
        }
    }

    // A new receiver is a new attempt, but MUST NOT erase the recovery budget.
    pub(crate) fn registered(&mut self, now: Instant) {
        self.phase = QrPhase::Starting;
        self.phase_since = now;
        self.queued_since = None;
        self.seen_qr = false;
        self.clear_uncertain();
        if self.recoveries > 0 {
            self.notice = Some(format!(
                "已自動重建這個帳號的 QR 流程（{}/{}次）；等待新碼與學校確認，不需重啟全部監控",
                self.recoveries, QR_MAX_RECOVERIES
            ));
        }
    }

    pub(crate) fn enqueued(&mut self, now: Instant) {
        self.seen_qr = true;
        // Repeated scans must not hide a receiver that never consumes anything.
        self.queued_since.get_or_insert(now);
    }

    pub(crate) fn enter(&mut self, phase: QrPhase, now: Instant) {
        if self.phase != phase {
            self.phase = phase;
            self.phase_since = now;
        }
        if phase == QrPhase::WaitingTurn {
            // The handler acknowledges an actual recv(), including LINE input.
            self.seen_qr = true;
            self.queued_since = None;
            self.notice = None;
        }
    }

    pub(crate) fn uncertain(&mut self, now: Instant) {
        self.uncertain_since.get_or_insert(now);
        self.uncertain_count = self.uncertain_count.saturating_add(1);
    }

    pub(crate) fn clear_uncertain(&mut self) {
        self.uncertain_since = None;
        self.uncertain_count = 0;
    }

    pub(crate) fn stall_reason(&self, now: Instant) -> Option<&'static str> {
        if !self.seen_qr || self.phase == QrPhase::WaitingTurn {
            return None; // No scan yet, or legitimately waiting for the group turn.
        }
        if self.phase == QrPhase::WaitingInput {
            // 剛結束一次慢查詢時給 receiver 消化佇列的機會，避免忙完瞬間誤判。
            if now.saturating_duration_since(self.phase_since) >= Duration::from_secs(5)
                && self.queued_since.is_some_and(|at| now.saturating_duration_since(at) >= DELIVERY_STALL)
            {
                return Some("QR 已入列超過60秒，但處理流程未接收");
            }
        } else if now.saturating_duration_since(self.phase_since) >= STEP_STALL {
            return Some("QR 處理階段超過90秒沒有推進");
        }
        // A failed parse / explicit rejection never increments this counter.
        // Do not interrupt a normal PUT or confirmation just because it is slow.
        if matches!(self.phase, QrPhase::WaitingInput | QrPhase::Checking)
            && self.uncertain_count >= 3
            && self.uncertain_since.is_some_and(|at| now.saturating_duration_since(at) >= STEP_STALL)
        {
            return Some("QR 已多次送出但持續90秒未取得學校確認");
        }
        None
    }

    pub(crate) fn manual_recheck_safe(&self) -> bool {
        !matches!(self.phase, QrPhase::Submitting | QrPhase::Verifying)
    }

    pub(crate) fn can_recover(&self, now: Instant) -> bool {
        self.recoveries < QR_MAX_RECOVERIES
            && self.last_recovery.map_or(true, |at| now.saturating_duration_since(at) >= RECOVERY_COOLDOWN)
    }

    pub(crate) fn begin_recovery(&mut self, now: Instant) -> bool {
        if !self.can_recover(now) { return false; }
        self.recoveries += 1;
        self.last_recovery = Some(now);
        self.notice = Some(format!("偵測到 QR 流程停滯，正在自動恢復（{}/{}次）", self.recoveries, QR_MAX_RECOVERIES));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_and_group_queue_are_not_stalls() {
        let at = Instant::now();
        let mut h = QrHealth::new(at);
        h.enter(QrPhase::WaitingInput, at);
        assert!(h.stall_reason(at + Duration::from_secs(1000)).is_none());
        h.enqueued(at);
        h.enter(QrPhase::WaitingTurn, at);
        assert!(h.stall_reason(at + Duration::from_secs(1000)).is_none());
    }

    #[test]
    fn repeated_scans_do_not_extend_unconsumed_deadline() {
        let at = Instant::now();
        let mut h = QrHealth::new(at);
        h.enter(QrPhase::WaitingInput, at);
        h.enqueued(at);
        h.enqueued(at + Duration::from_secs(59));
        assert!(h.stall_reason(at + Duration::from_secs(59)).is_none());
        assert!(h.stall_reason(at + Duration::from_secs(60)).is_some());
        h.enter(QrPhase::WaitingTurn, at + Duration::from_secs(61));
        h.enter(QrPhase::WaitingInput, at + Duration::from_secs(62));
        assert!(h.stall_reason(at + Duration::from_secs(200)).is_none());
    }

    #[test]
    fn verification_has_grace_but_not_infinite_time() {
        let at = Instant::now();
        let mut h = QrHealth::new(at);
        h.enqueued(at);
        h.enter(QrPhase::Verifying, at);
        assert!(h.stall_reason(at + Duration::from_secs(89)).is_none());
        assert!(h.stall_reason(at + Duration::from_secs(90)).is_some());
    }

    #[test]
    fn repeated_unknown_results_are_separate_from_explicit_rejections() {
        let at = Instant::now();
        let mut h = QrHealth::new(at);
        h.enter(QrPhase::WaitingTurn, at);
        for secs in [0, 30, 60] { h.uncertain(at + Duration::from_secs(secs)); }
        h.enter(QrPhase::WaitingInput, at + Duration::from_secs(61));
        assert!(h.stall_reason(at + Duration::from_secs(90)).is_some());
        h.clear_uncertain();
        assert!(h.stall_reason(at + Duration::from_secs(200)).is_none());
    }

    #[test]
    fn registration_cannot_reset_cooldown_or_budget() {
        let at = Instant::now();
        let mut h = QrHealth::new(at);
        assert!(h.begin_recovery(at));
        h.registered(at + Duration::from_secs(5));
        assert!(!h.can_recover(at + Duration::from_secs(119)));
        assert!(h.begin_recovery(at + Duration::from_secs(120)));
        h.registered(at + Duration::from_secs(125));
        assert!(!h.can_recover(at + Duration::from_secs(1000)));
        assert_eq!(h.recoveries, 2);
    }
    #[test]
    fn manual_recheck_waits_for_put_and_verify_only() {
        let now = Instant::now();
        let mut health = QrHealth::new(now);
        for phase in [QrPhase::Submitting, QrPhase::Verifying] {
            health.enter(phase, now);
            assert!(!health.manual_recheck_safe());
        }
        for phase in [QrPhase::Starting, QrPhase::WaitingInput, QrPhase::Checking, QrPhase::WaitingTurn] {
            health.enter(phase, now);
            assert!(health.manual_recheck_safe());
        }
    }

}
