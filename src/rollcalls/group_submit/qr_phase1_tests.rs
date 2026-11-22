//! Deterministic reservation contracts; not a real HTTP timing test.
use super::*;

#[tokio::test]
async fn accounts_have_ordered_reservations_and_retry_reuses_slot() {
    let key = GroupSubmitKey { provider: "mock.invalid".into(),
        rollcall_id: rand::random(), kind: GroupSubmitKind::QrCode };
    let now = Instant::now();
    let mut slots = Vec::new();
    for i in 0..12 {
        let (planned, reused, gap) = reserve_turn(key.clone(), &format!("mock-{i}"),
            now, 25, 0).await;
        assert!(!reused);
        assert_eq!(gap, 25);
        slots.push(planned);
    }
    for pair in slots.windows(2) {
        assert_eq!(pair[1].duration_since(pair[0]), Duration::from_millis(25));
    }
    let (retry, reused, _) = reserve_turn(key, "mock-3", now, 25, 0).await;
    assert!(reused);
    assert_eq!(retry, slots[3]);
}

#[test]
fn fixed_and_randomised_gap_are_within_configured_bounds() {
    assert_eq!(submit_gap_ms(500, 0, 12345), 500);
    for roll in 0..1000 {
        let gap = submit_gap_ms(500, 500, roll);
        assert!((500..=1000).contains(&gap));
    }
}
