//! Per-account cached HTTP client.
//!
//! Building a `reqwest::Client` allocates a fresh connection pool, TLS state and
//! cookie jar every time. Before this module existed, the dashboard homework
//! handler built one `Client` on **every** API call
//! (`admin_web::build_client_from_saved_cookie`) and the background homework
//! prefetcher built one per account every 4 minutes
//! (`homework_reminders::fetch_todos_with_saved_session`). All of the pooled
//! connections, DNS and TLS session state were thrown away on every rebuild,
//! which is expensive against slow, rate-limited upstream school servers.
//!
//! This module caches a single `Arc<reqwest::Client>` per account and only
//! rebuilds it when the **session cookie** (or the base URL / request timeout)
//! actually changes — i.e. after a re-auth. Because `reqwest::Client` is `Clone`
//! and cheaply shares its internal connection pool, callers clone it freely and
//! still reuse the pooled connections.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use crate::account::AccountConfig;

/// How long an unused cached client is retained before it becomes eligible for
/// eviction. Clients for deleted accounts are dropped this way without any
/// explicit bookkeeping at every deletion site.
const IDLE_TTL: Duration = Duration::from_secs(15 * 60);

/// Upper bound on cached entries, so deleted accounts cannot grow the map
/// without bound.
const MAX_ENTRIES: usize = 128;

struct CachedClient {
    client: Arc<reqwest::Client>,
    /// Fingerprint of (session_cookie, base_url, request_timeout_secs). When it
    /// changes the client is rebuilt so pooled connections carry the fresh
    /// session. base_url / timeout are included defensively so a config edit
    /// also invalidates the entry.
    fingerprint: String,
    last_used: Instant,
}

static CACHE: Lazy<Mutex<HashMap<String, CachedClient>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn fingerprint(account: &AccountConfig) -> String {
    // \u{0} acts as an unambiguous field separator; a cookie can never contain
    // a NUL byte, so there is no collision between adjacent concatenated fields.
    format!(
        "{}\u{0}{}\u{0}{}",
        account.session_cookie.trim(),
        account.base_url(),
        account.request_timeout_secs,
    )
}

fn build(account: &AccountConfig) -> Option<Arc<reqwest::Client>> {
    let saved_cookie = account.session_cookie.trim();
    if saved_cookie.is_empty() {
        return None;
    }

    let url = reqwest::Url::parse(account.base_url()).ok()?;
    let jar = Arc::new(reqwest::cookie::Jar::default());
    for part in saved_cookie.split(';') {
        let part = part.trim();
        if !part.is_empty() {
            jar.add_cookie_str(part, &url);
        }
    }

    crate::api::build_http_client(jar, account.request_timeout_secs)
        .ok()
        .map(Arc::new)
}

/// Returns a cached HTTP client for `account`, rebuilding it **only** when the
/// session cookie (or base URL / timeout) has changed since the previous call.
///
/// Returns `None` when the account has no stored session cookie or the client
/// could not be built; callers are expected to fall back to a full login flow.
pub fn session_http_client(account: &AccountConfig) -> Option<Arc<reqwest::Client>> {
    let fingerprint = fingerprint(account);
    let now = Instant::now();

    // Fast path: a cached entry matches the current fingerprint.
    {
        let mut cache = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len() >= MAX_ENTRIES {
            // Opportunistic eviction of idle entries before growth gets worse.
            cache.retain(|_, entry| now.duration_since(entry.last_used) < IDLE_TTL);
        }
        if let Some(entry) = cache.get_mut(&account.id) {
            if entry.fingerprint == fingerprint {
                entry.last_used = now;
                return Some(Arc::clone(&entry.client));
            }
        }
    }

    // Build outside the lock: `build_http_client` may do a little allocation and
    // we never want to hold the global lock while creating a client.
    let client = build(account)?;

    {
        let mut cache = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.insert(
            account.id.clone(),
            CachedClient {
                client: Arc::clone(&client),
                fingerprint,
                last_used: now,
            },
        );
    }

    Some(client)
}

/// Drops the cached client for an account (used when the account is removed).
pub fn invalidate(account_id: &str) {
    if let Ok(mut cache) = CACHE.lock() {
        cache.remove(account_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApiConfig, ProviderConfig, ProviderKind};

    fn test_account(id: &str, cookie: &str) -> AccountConfig {
        AccountConfig {
            id: id.to_string(),
            provider: "tku".to_string(),
            username: "u".to_string(),
            password: "p".to_string(),
            enabled: true,
            line_user_id: String::new(),
            discord_user_id: String::new(),
            session_cookie: cookie.to_string(),
            is_admin: false,
            provider_config: ProviderConfig {
                kind: ProviderKind::TKU,
                base_url: "https://iclass.tku.edu.tw".to_string(),
                api: ApiConfig {
                    base_url: "https://iclass.tku.edu.tw".to_string(),
                    poll_interval_secs: 10,
                    request_timeout_secs: 30,
                },
                ..Default::default()
            },
            request_timeout_secs: 30,
        }
    }

    #[test]
    fn returns_none_when_no_cookie() {
        let account = test_account("a1", "");
        assert!(session_http_client(&account).is_none());
    }

    #[test]
    fn same_fingerprint_reuses_same_client() {
        invalidate("a1");
        let account = test_account("a1", "session=abc");
        let first = session_http_client(&account).expect("client should build");
        let second = session_http_client(&account).expect("client should build");
        // Reuse: both clones share the same inner Arc.
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn changed_cookie_rebuilds_client() {
        invalidate("a2");
        let first = session_http_client(&test_account("a2", "session=abc")).expect("build");
        let second =
            session_http_client(&test_account("a2", "session=xyz")).expect("rebuild");
        // Cookie changed → different inner client.
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn invalidate_forces_rebuild() {
        invalidate("a3");
        let first = session_http_client(&test_account("a3", "session=abc")).expect("build");
        invalidate("a3");
        let second = session_http_client(&test_account("a3", "session=abc")).expect("rebuild");
        assert!(!Arc::ptr_eq(&first, &second));
    }
}
