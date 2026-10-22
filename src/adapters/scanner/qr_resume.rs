//! QR_MANUAL_RESUME_V1: authenticated, same-rollcall rechecks owned by monitor batches.
use super::*;
use crate::api::rollcall::Rollcall;
use crate::adapters::requests::RequestAccountState;

#[derive(Default)]
pub(super) struct QrResumeState {
    pub templates: HashMap<String, Rollcall>,
    pub requested: HashSet<String>,
    pub errors: HashMap<String, String>,
    pub revision: u64,
    pub last_request: Option<Instant>,
}

#[derive(Clone)]
pub(super) struct QrWake {
    force_poll: Arc<tokio::sync::Notify>,
    running: Arc<Mutex<bool>>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct QrRecoveryStatus {
    pub recheck_revision: u64,
    pub rechecking_count: usize,
    pub recheck_error_count: usize,
}

impl QrResumeState {
    pub fn status(&self) -> QrRecoveryStatus {
        QrRecoveryStatus {
            recheck_revision: self.revision,
            rechecking_count: self.requested.len(),
            recheck_error_count: self.errors.len(),
        }
    }
}

impl QrScannerRegistry {
    pub(crate) async fn bind_qr_wake(&self, provider: &str, account: &RequestAccountState) {
        self.qr_wakes.lock().await.insert((provider.to_string(), account.account_id.clone()), QrWake {
            force_poll: Arc::clone(&account.force_poll_tx), running: Arc::clone(&account.is_running),
        });
    }

    pub(crate) fn qr_resume_subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.qr_resume_signal.subscribe()
    }

    pub(crate) async fn remember_qr(&self, provider: &str, account: &str, rollcall: &Rollcall) {
        if let Some(entry) = self.inner.lock().await.get_mut(&PendingQrKey::new(provider, rollcall.rollcall_id)) {
            entry.resume.templates.insert(account.to_string(), rollcall.clone());
        }
    }

    pub(crate) async fn qr_manual_rollcalls(&self, provider: &str, account: &str) -> Vec<Rollcall> {
        let inner = self.inner.lock().await;
        let now = Instant::now();
        let mut result: Vec<_> = inner.iter().filter(|(key, entry)|
            key.provider == provider && now < entry.expires_at && entry.resume.requested.contains(account)
        ).filter_map(|(_, entry)| entry.resume.templates.get(account).cloned()).collect();
        result.sort_by_key(|rc| rc.rollcall_id);
        result
    }

    pub(crate) async fn qr_manual_requested(&self, provider: &str, id: u64, account: &str) -> bool {
        self.inner.lock().await.get(&PendingQrKey::new(provider, id))
            .is_some_and(|entry| Instant::now() < entry.expires_at && entry.resume.requested.contains(account))
    }

    pub(crate) async fn qr_manual_safe(&self, provider: &str, id: u64, account: &str) -> bool {
        self.inner.lock().await.get(&PendingQrKey::new(provider, id))
            .and_then(|entry| entry.watchdogs.get(account))
            .map_or(true, |health| health.manual_recheck_safe())
    }

    pub(crate) async fn qr_remaining(&self, provider: &str, id: u64) -> Option<Duration> {
        self.inner.lock().await.get(&PendingQrKey::new(provider, id))
            .map(|entry| entry.expires_at.saturating_duration_since(Instant::now()))
    }

    // Only the worker that performed the school probe resolves this request.
    // Normal mark_completed deliberately does NOT clear requested: an older PUT must not
    // satisfy a newer camera-open request without its explicit recheck.
    pub(crate) async fn finish_qr_manual(&self, provider: &str, id: u64, account: &str, absent: bool, error: Option<&str>) {
        if let Some(entry) = self.inner.lock().await.get_mut(&PendingQrKey::new(provider, id)) {
            entry.resume.requested.remove(account);
            if absent { entry.completed_accounts.remove(account); }
            if let Some(error) = error {
                entry.resume.errors.insert(account.to_string(), truncate_chars(error, 240).to_string());
            } else {
                entry.resume.errors.remove(account);
            }
        }
    }

    pub async fn request_qr_resume(&self, query: ScannerStatusQuery) -> Result<QrRecoveryStatus, ScannerError> {
        let provider = query.provider.trim();
        let account = query.account_id.trim();
        let token = query.token.trim();
        if provider.is_empty() || account.is_empty() || token.is_empty() {
            return Err(ScannerError::InvalidSubmission("missing required field"));
        }
        // Authorize before reading any wake handles; then revalidate after releasing locks.
        let members: HashSet<String> = {
            let inner = self.inner.lock().await;
            let entry = inner.get(&PendingQrKey::new(provider, query.rollcall_id)).ok_or(ScannerError::PendingNotFound)?;
            if Instant::now() >= entry.expires_at { return Err(ScannerError::Expired); }
            if entry.token != token { return Err(ScannerError::InvalidToken); }
            if !entry.accounts.contains_key(account) && !entry.completed_accounts.contains(account) {
                return Err(ScannerError::AccountNotPending);
            }
            entry.accounts.keys().chain(entry.completed_accounts.iter()).cloned().collect()
        };
        let wakes: Vec<_> = {
            let wakes = self.qr_wakes.lock().await;
            members.iter().filter_map(|id| wakes.get(&(provider.to_string(), id.clone()))
                .cloned().map(|wake| (id.clone(), wake))).collect()
        };
        let mut active = HashMap::new();
        for (id, wake) in wakes {
            let running = *wake.running.lock().await;
            if running { active.insert(id, wake); }
        }
        let mut inner = self.inner.lock().await;
        let entry = inner.get_mut(&PendingQrKey::new(provider, query.rollcall_id)).ok_or(ScannerError::PendingNotFound)?;
        let now = Instant::now();
        if now >= entry.expires_at { return Err(ScannerError::Expired); }
        if entry.token != token { return Err(ScannerError::InvalidToken); }
        if !entry.accounts.contains_key(account) && !entry.completed_accounts.contains(account) {
            return Err(ScannerError::AccountNotPending);
        }
        // A monitor stopped after the first click must not pin the whole group in rechecking.
        let stopped: Vec<_> = entry.resume.requested.iter().filter(|id| !active.contains_key(*id)).cloned().collect();
        for id in stopped {
            entry.resume.requested.remove(&id);
            entry.resume.errors.insert(id, "監控已停止；未恢復此帳號，請啟用監控後再重查".to_string());
        }
        // Coalesce double-clicks, multiple tabs and auto-close checks. Never reset automatic budget.
        if !entry.resume.requested.is_empty()
            || entry.resume.last_request.is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(10)) {
            return Ok(entry.resume.status());
        }
        entry.resume.last_request = Some(now);
        entry.resume.revision = entry.resume.revision.saturating_add(1);
        entry.resume.errors.clear();
        // A manual/auto-close recheck must never replay a code cached before this request.
        entry.used = false;
        entry.submitted_qr_data = None;
        entry.submitted_qr_at = None;
        entry.submission_revision = entry.submission_revision.wrapping_add(1);
        let members: HashSet<_> = entry.accounts.keys().chain(entry.completed_accounts.iter()).cloned().collect();
        for id in members {
            if let Some(wake) = active.get(&id).filter(|_| entry.resume.templates.contains_key(&id)) {
                entry.resume.requested.insert(id);
                wake.force_poll.notify_one();
            } else {
                entry.resume.errors.insert(id, "監控未啟用或恢復資料不可用；請先啟用該帳號監控再重試".to_string());
            }
        }
        let result = entry.resume.status();
        info!(provider, rollcall_id = query.rollcall_id, rechecking = result.rechecking_count,
            unavailable = result.recheck_error_count, "QR同場重新核對已排入原監控流程（不是簽到成功）");
        drop(inner);
        self.qr_resume_signal.send_modify(|version| *version = version.wrapping_add(1));
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    async fn enrolled(registry: &QrScannerRegistry, id: u64, account: &str) -> ScannerLink {
        let (tx, _rx) = mpsc::channel(4);
        let link = registry.register_pending("fju", id, account, tx, Duration::from_secs(600)).await.unwrap();
        registry.remember_qr("fju", account, &Rollcall { rollcall_id: id, status: "absent".into(), ..Default::default() }).await;
        registry.qr_wakes.lock().await.insert(("fju".into(), account.into()), QrWake {
            force_poll: Arc::new(tokio::sync::Notify::new()), running: Arc::new(Mutex::new(true)),
        });
        link
    }

    fn query(link: &ScannerLink, account: &str) -> ScannerStatusQuery {
        ScannerStatusQuery { provider: "fju".into(), rollcall_id: 42, account_id: account.into(), token: link.token.clone() }
    }

    #[tokio::test]
    async fn manual_rechecks_same_group_completed_and_pending_only() {
        let registry = QrScannerRegistry::new("https://scanner.example.test");
        let link = enrolled(&registry, 42, "a").await;
        enrolled(&registry, 42, "b").await;
        enrolled(&registry, 99, "c").await;
        registry.mark_completed("fju", 42, "a").await;
        let response = registry.request_qr_resume(query(&link, "a")).await.unwrap();
        assert_eq!(response.rechecking_count, 2);
        assert!(registry.qr_manual_requested("fju", 42, "a").await);
        assert!(registry.qr_manual_requested("fju", 42, "b").await);
        assert!(!registry.qr_manual_requested("fju", 99, "c").await);
        assert_eq!(registry.qr_manual_rollcalls("fju", "a").await[0].rollcall_id, 42);
    }

    #[tokio::test]
    async fn manual_rejects_foreign_token_account_and_cancelled_owner() {
        let registry = QrScannerRegistry::new("https://scanner.example.test");
        let link = enrolled(&registry, 42, "a").await;
        enrolled(&registry, 42, "b").await;
        let mut wrong = query(&link, "a"); wrong.token = "wrong".into();
        assert_eq!(registry.request_qr_resume(wrong).await.unwrap_err(), ScannerError::InvalidToken);
        assert_eq!(registry.request_qr_resume(query(&link, "outsider")).await.unwrap_err(), ScannerError::AccountNotPending);
        registry.unregister_pending("fju", 42, "a").await;
        assert_eq!(registry.request_qr_resume(query(&link, "a")).await.unwrap_err(), ScannerError::AccountNotPending);
        assert!(!registry.qr_manual_requested("fju", 42, "b").await);
    }

    #[tokio::test]
    async fn manual_coalesces_and_old_completion_cannot_resolve_new_check() {
        let registry = QrScannerRegistry::new("https://scanner.example.test");
        let link = enrolled(&registry, 42, "a").await;
        let first = registry.request_qr_resume(query(&link, "a")).await.unwrap();
        let second = registry.request_qr_resume(query(&link, "a")).await.unwrap();
        assert_eq!(first.recheck_revision, second.recheck_revision);
        registry.mark_completed("fju", 42, "a").await;
        assert!(registry.qr_manual_requested("fju", 42, "a").await);
        registry.finish_qr_manual("fju", 42, "a", true, None).await;
        assert!(!registry.is_qr_account_completed("fju", 42, "a").await);
        assert!(!registry.qr_manual_requested("fju", 42, "a").await);
    }

    #[tokio::test]
    async fn manual_unknown_blocks_cached_all_done_without_faking_absence() {
        let registry = QrScannerRegistry::new("https://scanner.example.test");
        let link = enrolled(&registry, 42, "a").await;
        registry.mark_completed("fju", 42, "a").await;
        registry.request_qr_resume(query(&link, "a")).await.unwrap();
        registry.finish_qr_manual("fju", 42, "a", false, Some("unknown")).await;
        let state = registry.status(query(&link, "a")).await.unwrap();
        assert_eq!(state.qr_recovery.recheck_error_count, 1);
        assert_eq!(state.completed_count, 1); // cached history is not silently rewritten as absent
        assert_eq!(state.qr_recovery.rechecking_count, 0);
        registry.mark_completed("fju", 42, "a").await;
        assert_eq!(registry.status(query(&link, "a")).await.unwrap().qr_recovery.recheck_error_count, 0);
    }

    #[tokio::test]
    async fn manual_preserves_token_deadline_and_automatic_budget() {
        let registry = QrScannerRegistry::new("https://scanner.example.test");
        let link = enrolled(&registry, 42, "a").await;
        let expiry = {
            let mut inner = registry.inner.lock().await;
            let entry = inner.get_mut(&PendingQrKey::new("fju", 42)).unwrap();
            entry.watchdogs.get_mut("a").unwrap().recoveries = 2;
            entry.used = true; entry.submitted_qr_data = Some("old".into()); entry.submitted_qr_at = Some(Instant::now());
            entry.expires_at
        };
        registry.request_qr_resume(query(&link, "a")).await.unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        let fresh = registry.register_pending("fju", 42, "a", tx, Duration::from_secs(10800)).await.unwrap();
        assert_eq!(fresh.token, link.token);
        assert!(rx.try_recv().is_err());
        let inner = registry.inner.lock().await;
        let entry = &inner[&PendingQrKey::new("fju", 42)];
        assert_eq!(entry.expires_at, expiry);
        assert_eq!(entry.watchdogs["a"].recoveries, 2);
    }

    #[tokio::test]
    async fn manual_stopped_account_is_reported_not_started() {
        let registry = QrScannerRegistry::new("https://scanner.example.test");
        let link = enrolled(&registry, 42, "a").await;
        let wake = registry.qr_wakes.lock().await[&("fju".into(), "a".into())].clone();
        *wake.running.lock().await = false;
        let result = registry.request_qr_resume(query(&link, "a")).await.unwrap();
        assert_eq!(result.rechecking_count, 0);
        assert_eq!(result.recheck_error_count, 1);
    }
}
