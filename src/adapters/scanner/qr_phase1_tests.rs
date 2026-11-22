//! In-process registry contracts; no school endpoint or real credentials.
use super::*;
use tokio::sync::mpsc;
use tokio::time::timeout;

const PROVIDER: &str = "mock.invalid";
const ROLLCALL: u64 = 42;
const LIMIT: Duration = Duration::from_secs(2);

fn submission(account: &str, token: &str, code: &str) -> ScannerSubmission {
    ScannerSubmission {
        provider: PROVIDER.into(), rollcall_id: ROLLCALL,
        account_id: account.into(), token: token.into(), qr_data: code.into(),
        client_observed_age_ms: None,
    }
}

fn query(account: &str, token: &str) -> ScannerStatusQuery {
    ScannerStatusQuery { provider: PROVIDER.into(), rollcall_id: ROLLCALL,
        account_id: account.into(), token: token.into() }
}

#[tokio::test]
async fn fanout_is_not_school_confirmation() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let mut receivers = Vec::new();
    let mut token = String::new();
    for i in 0..12 {
        let (tx, rx) = mpsc::channel(2);
        let link = registry.register_pending(PROVIDER, ROLLCALL, &format!("mock-{i}"),
            tx, Duration::from_secs(60), None).await.unwrap();
        if i == 0 { token = link.token.clone(); }
        assert_eq!(link.token, token);
        receivers.push(rx);
    }
    let result = timeout(LIMIT, registry.submit(submission("mock-0", &token, "synthetic-qr")))
        .await.unwrap().unwrap();
    assert_eq!(result.delivered_count, 12);
    assert!(result.failed_accounts.is_empty());
    for rx in &mut receivers {
        assert_eq!(timeout(LIMIT, rx.recv()).await.unwrap().as_deref(), Some("synthetic-qr"));
    }
    let before = registry.status(query("mock-0", &token)).await.unwrap();
    assert_eq!((before.total_count, before.pending_count, before.completed_count), (12, 12, 0));
    // This simulates a caller that has separately confirmed school status; the
    // registry itself does not perform that confirmation in this test.
    registry.mark_completed(PROVIDER, ROLLCALL, "mock-0").await;
    let after = registry.status(query("mock-0", &token)).await.unwrap();
    assert_eq!((after.total_count, after.pending_count, after.completed_count), (12, 11, 1));
}

#[tokio::test]
async fn invalid_token_and_unknown_account_do_not_dispatch() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let (tx, mut rx) = mpsc::channel(2);
    let link = registry.register_pending(PROVIDER, ROLLCALL, "mock-0", tx,
        Duration::from_secs(60), None).await.unwrap();
    let bad = registry.submit(submission("mock-0", "wrong-token", "synthetic-qr")).await;
    assert!(matches!(bad, Err(ScannerError::InvalidToken)));
    let unknown = registry.submit(submission("unknown", &link.token, "synthetic-qr")).await;
    assert!(matches!(unknown, Err(ScannerError::AccountNotPending)));
    assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
    let status = registry.status(query("mock-0", &link.token)).await.unwrap();
    assert_eq!((status.pending_count, status.completed_count), (1, 0));
}

#[tokio::test]
async fn recent_qr_is_replayed_to_late_registration() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let (first_tx, mut first_rx) = mpsc::channel(2);
    let link = registry.register_pending(PROVIDER, ROLLCALL, "first", first_tx,
        Duration::from_secs(60), None).await.unwrap();
    registry.submit(submission("first", &link.token, "synthetic-qr")).await.unwrap();
    assert_eq!(timeout(LIMIT, first_rx.recv()).await.unwrap().as_deref(), Some("synthetic-qr"));
    let (late_tx, mut late_rx) = mpsc::channel(2);
    let late = registry.register_pending(PROVIDER, ROLLCALL, "late", late_tx,
        Duration::from_secs(60), None).await.unwrap();
    assert!(late.already_submitted);
    assert_eq!(timeout(LIMIT, late_rx.recv()).await.unwrap().as_deref(), Some("synthetic-qr"));
}

#[tokio::test]
async fn closed_receiver_is_not_counted_as_accepted() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let (first_tx, mut first_rx) = mpsc::channel(2);
    let link = registry.register_pending(PROVIDER, ROLLCALL, "first", first_tx,
        Duration::from_secs(60), None).await.unwrap();
    let (closed_tx, closed_rx) = mpsc::channel(2);
    registry.register_pending(PROVIDER, ROLLCALL, "closed", closed_tx,
        Duration::from_secs(60), None).await.unwrap();
    drop(closed_rx);
    let result = registry.submit(submission("first", &link.token, "synthetic-qr")).await.unwrap();
    assert_eq!(result.delivered_count, 1);
    assert_eq!(result.failed_accounts, vec!["closed".to_string()]);
    assert_eq!(timeout(LIMIT, first_rx.recv()).await.unwrap().as_deref(), Some("synthetic-qr"));
}

#[tokio::test]
async fn stale_cached_qr_is_not_replayed_to_new_registration() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let (tx, mut rx) = mpsc::channel(2);
    let link = registry.register_pending(PROVIDER, ROLLCALL, "first", tx,
        Duration::from_secs(60), None).await.unwrap();
    registry.submit(submission("first", &link.token, "synthetic-qr")).await.unwrap();
    assert_eq!(timeout(LIMIT, rx.recv()).await.unwrap().as_deref(), Some("synthetic-qr"));
    {
        let mut inner = registry.inner.lock().await;
        let entry = inner.get_mut(&PendingQrKey::new(PROVIDER, ROLLCALL)).unwrap();
        entry.submitted_qr_at = Some(Instant::now() - QR_REPLAY_MAX_AGE - Duration::from_secs(1));
    }
    let (late_tx, mut late_rx) = mpsc::channel(2);
    let late = registry.register_pending(PROVIDER, ROLLCALL, "late", late_tx,
        Duration::from_secs(60), None).await.unwrap();
    assert!(!late.already_submitted);
    assert!(matches!(late_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
}

#[tokio::test]
async fn reregistration_does_not_send_new_qr_to_old_receiver() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let (old_tx, mut old_rx) = mpsc::channel(2);
    let link = registry.register_pending(PROVIDER, ROLLCALL, "mock-0", old_tx,
        Duration::from_secs(60), None).await.unwrap();
    // Keep a second member so the shared registration and token remain valid.
    let (peer_tx, _peer_rx) = mpsc::channel(2);
    registry.register_pending(PROVIDER, ROLLCALL, "peer", peer_tx,
        Duration::from_secs(60), None).await.unwrap();
    registry.unregister_pending(PROVIDER, ROLLCALL, "mock-0").await;
    let (new_tx, mut new_rx) = mpsc::channel(2);
    registry.register_pending(PROVIDER, ROLLCALL, "mock-0", new_tx,
        Duration::from_secs(60), None).await.unwrap();
    registry.submit(submission("mock-0", &link.token, "synthetic-new")).await.unwrap();
    assert_eq!(timeout(LIMIT, new_rx.recv()).await.unwrap().as_deref(), Some("synthetic-new"));
    assert!(matches!(old_rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)));
}

#[tokio::test]
async fn idle_cleanup_preserves_registered_accounts_even_after_expiry() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let (tx, _rx) = mpsc::channel(2);
    registry.register_pending(PROVIDER, ROLLCALL, "mock-0", tx,
        Duration::from_secs(60), None).await.unwrap();
    let key = PendingQrKey::new(PROVIDER, ROLLCALL);
    let now = Instant::now();
    registry.inner.lock().await.get_mut(&key).unwrap().expires_at = now - Duration::from_secs(1);
    registry.cleanup_expired_idle_qr(now);
    assert!(registry.inner.lock().await.contains_key(&key));
    // Completed status is retained by unregister, making the inactive entry
    // a useful regression case for the periodic sweep.
    registry.mark_completed(PROVIDER, ROLLCALL, "mock-0").await;
    registry.cleanup_expired_idle_qr(now);
    assert!(!registry.inner.lock().await.contains_key(&key));
}

#[tokio::test]
async fn idle_cleanup_never_waits_for_a_busy_registry() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let guard = registry.inner.lock().await;
    registry.cleanup_expired_idle_qr(Instant::now());
    drop(guard);
}

// These exercise in-memory registry behavior only; no school API is called.
#[tokio::test]
async fn number_closed_receiver_does_not_publish_failed_value() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let (tx, rx) = mpsc::channel(2);
    let link = registry.register_number_pending(PROVIDER, ROLLCALL, "mock-0", tx, Duration::from_secs(60)).await.unwrap();
    drop(rx);
    let submitted = registry.submit_number(NumberCodeSubmission { provider: PROVIDER.into(), rollcall_id: ROLLCALL,
        account_id: "mock-0".into(), token: link.token, code: Some("1234".into()), auto_lookup: Some(false) }).await;
    assert!(matches!(submitted, Err(ScannerError::NoDeliveredAccounts)));
    let inner = registry.number_inner.lock().await;
    assert!(inner.get(&PendingNumberKey::new(PROVIDER, ROLLCALL)).unwrap().submitted_input.is_none());
}

#[tokio::test]
async fn radar_closed_receiver_does_not_publish_failed_value() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let (tx, rx) = mpsc::channel(2);
    let link = registry.register_radar_pending(PROVIDER, ROLLCALL, "mock-0", tx, Duration::from_secs(60), &[], "").await.unwrap();
    drop(rx);
    let submitted = registry.submit_radar(RadarPointSubmission { provider: PROVIDER.into(), rollcall_id: ROLLCALL,
        account_id: "mock-0".into(), token: link.token, latitude: 25.0, longitude: 121.0, accuracy: None, from_device: None }).await;
    assert!(matches!(submitted, Err(ScannerError::NoDeliveredAccounts)));
    let inner = registry.radar_inner.lock().await;
    assert!(inner.get(&PendingNumberKey::new(PROVIDER, ROLLCALL)).unwrap().submitted_point.is_none());
}

#[tokio::test]
async fn expired_link_requests_do_not_remove_active_qr_entry() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let (tx, _rx) = mpsc::channel(2);
    let link = registry.register_pending(PROVIDER, ROLLCALL, "mock-0", tx,
        Duration::from_secs(60), None).await.unwrap();
    let key = PendingQrKey::new(PROVIDER, ROLLCALL);
    registry.inner.lock().await.get_mut(&key).unwrap().expires_at = Instant::now() - Duration::from_secs(1);
    assert!(matches!(registry.status(query("mock-0", &link.token)).await, Err(ScannerError::Expired)));
    assert!(matches!(registry.submit(submission("mock-0", &link.token, "synthetic")).await, Err(ScannerError::Expired)));
    assert!(registry.inner.lock().await.contains_key(&key));
    registry.unregister_pending(PROVIDER, ROLLCALL, "mock-0").await;
    registry.cleanup_expired_idle_qr(Instant::now());
    assert!(!registry.inner.lock().await.contains_key(&key));
}

#[tokio::test]
async fn radar_broadcast_publishes_only_when_a_receiver_accepts() {
    let registry = QrScannerRegistry::new("http://localhost:12345");
    let (tx, mut rx) = mpsc::channel(2);
    registry.register_radar_pending(PROVIDER, ROLLCALL, "mock-0", tx,
        Duration::from_secs(60), &[], "").await.unwrap();
    let point = RadarPointInput { latitude: 25.0, longitude: 121.0, accuracy: None, from_device: false };
    let result = registry.broadcast_radar_point(PROVIDER, Some(ROLLCALL), point).await;
    assert_eq!(result.delivered_count, 1);
    assert_eq!(timeout(LIMIT, rx.recv()).await.unwrap(), Some(point));
    let key = PendingNumberKey::new(PROVIDER, ROLLCALL);
    assert_eq!(registry.radar_inner.lock().await.get(&key).unwrap().submitted_point.as_ref().unwrap().latitude, 25.0);
    drop(rx);
    let other = RadarPointInput { latitude: 26.0, ..point };
    let failed = registry.broadcast_radar_point(PROVIDER, Some(ROLLCALL), other).await;
    assert_eq!(failed.delivered_count, 0);
    assert_eq!(registry.radar_inner.lock().await.get(&key).unwrap().submitted_point.as_ref().unwrap().latitude, 25.0);
}
