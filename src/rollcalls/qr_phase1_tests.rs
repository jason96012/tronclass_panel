//! Exact rollcall-state contracts; no mock or school HTTP requests.
use super::*;

fn mock_rollcall(id: u64, status: &str, expired: bool) -> Rollcall {
    Rollcall { rollcall_id: id, status: status.into(), is_expired: expired, ..Default::default() }
}

#[test]
fn confirmation_requires_matching_id_and_school_attendance_status() {
    let list = vec![mock_rollcall(1, "on_call_fine", false), mock_rollcall(2, "absent", false)];
    assert!(qr_attendance_confirmed(&list, 1));
    assert!(!qr_attendance_confirmed(&list, 2));
    assert!(!qr_attendance_confirmed(&list, 3));
    assert!(qr_attendance_confirmed(&[mock_rollcall(2, "late", false)], 2));
}

#[test]
fn watchdog_unknown_is_not_school_confirmation() {
    assert!(matches!(qr_watchdog_server_status(&[], 42), ServerRollcallStatus::UnknownError));
    assert!(matches!(qr_watchdog_server_status(&[mock_rollcall(42, "absent", false)], 42), ServerRollcallStatus::ActivePending));
    assert!(matches!(qr_watchdog_server_status(&[mock_rollcall(42, "on_call_fine", false)], 42), ServerRollcallStatus::AlreadyAttended));
    assert!(matches!(qr_watchdog_server_status(&[mock_rollcall(42, "absent", true)], 42), ServerRollcallStatus::ClosedOrExpired));
    assert!(matches!(qr_watchdog_server_status(&[mock_rollcall(42, "unrecognised", false)], 42), ServerRollcallStatus::UnknownError));
}
