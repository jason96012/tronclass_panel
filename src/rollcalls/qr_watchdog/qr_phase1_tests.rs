//! Pure state-machine checks using synthetic instants, no sleeping or I/O.
use super::*;

#[test]
fn no_scan_and_waiting_turn_do_not_look_stalled() {
    let now = Instant::now();
    let mut health = QrHealth::new(now);
    health.enter(QrPhase::WaitingInput, now);
    assert!(health.stall_reason(now + Duration::from_secs(600)).is_none());
    health.enqueued(now);
    health.enter(QrPhase::WaitingTurn, now);
    assert!(health.stall_reason(now + Duration::from_secs(600)).is_none());
}

#[test]
fn manual_recheck_does_not_interrupt_submit_or_verification() {
    let now = Instant::now();
    let mut health = QrHealth::new(now);
    health.enter(QrPhase::Submitting, now);
    assert!(!health.manual_recheck_safe());
    health.enter(QrPhase::Verifying, now);
    assert!(!health.manual_recheck_safe());
    health.enter(QrPhase::WaitingInput, now);
    assert!(health.manual_recheck_safe());
}

#[test]
fn repeated_scans_do_not_reset_delivery_stall_and_recovery_budget_is_bounded() {
    let now = Instant::now();
    let mut health = QrHealth::new(now);
    health.enter(QrPhase::WaitingInput, now);
    health.enqueued(now);
    health.enqueued(now + Duration::from_secs(59));
    assert!(health.stall_reason(now + Duration::from_secs(60)).is_some());
    assert!(health.begin_recovery(now + Duration::from_secs(60)));
    assert!(!health.begin_recovery(now + Duration::from_secs(61)));
    health.registered(now + Duration::from_secs(180));
    assert!(health.begin_recovery(now + Duration::from_secs(180)));
    assert!(!health.begin_recovery(now + Duration::from_secs(360)));
}

#[test]
fn repeated_uncertain_outcomes_remain_unknown_until_cleared() {
    let now = Instant::now();
    let mut health = QrHealth::new(now);
    health.enter(QrPhase::WaitingTurn, now); // records a received QR
    health.uncertain(now);
    health.uncertain(now + Duration::from_secs(1));
    health.uncertain(now + Duration::from_secs(2));
    health.enter(QrPhase::WaitingInput, now + Duration::from_secs(90));
    assert!(health.stall_reason(now + Duration::from_secs(91)).is_some());
    health.clear_uncertain();
    assert!(health.stall_reason(now + Duration::from_secs(91)).is_none());
}
