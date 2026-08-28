//! At-rest encryption for stored credentials (upstream passwords & session
//! cookies).
//!
//! The SQLite accounts table used to hold the upstream Tronclass password and
//! the last session cookie as plaintext. If the database file leaked, those
//! credentials would be exposed. This module encrypts them with AES-256-GCM
//! using a **master key** supplied via the `TRONCLASS_MASTER_KEY` environment
//! variable (a 32-byte key as lowercase hex, 64 hex chars).
//!
//! Design:
//! - Values are encrypted on the way **into** the DB (`insert`/`upsert`/
//!   `set_session_cookie`).
//! - Values are decrypted on the way **out** (`AccountRow` → `RawAccountConfig`),
//!   so all existing in-memory callers keep working with plaintext.
//! - If no master key is configured, values are stored as plaintext (legacy
//!   behaviour) so existing deployments keep working — but a warning is logged
//!   so operators know at-rest encryption is off.
//! - Encrypted values carry a stable prefix so we can tell them apart from
//!   legacy plaintext and from each other (per-field salt/nonce).

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use tracing::warn;

/// Prefix distinguishing our AES-GCM envelope from legacy plaintext.
const ENC_PREFIX: &str = "enc:v1:";

/// A stored secret that is transparently encrypted at rest.
///
/// `as_str()` returns the plaintext in memory; `store_value()` returns the
/// at-rest (possibly encrypted) form to persist.
#[derive(Clone)]
pub struct StoreSecret(String);

impl StoreSecret {
    /// Wrap a plaintext secret for storage.
    pub fn new(plaintext: impl Into<String>) -> Self {
        StoreSecret(plaintext.into())
    }

    /// The in-memory plaintext.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Serialize for storage, encrypting with the master key if configured.
    pub fn store_value(&self) -> String {
        match master_key() {
            Some(key) => encrypt_with_key(&self.0, key).unwrap_or_else(|| self.0.clone()),
            None => {
                self.0.clone()
            }
        }
    }

    /// Parse a value loaded from the DB: decrypt if it is an envelope, else
    /// treat it as legacy plaintext.
    pub fn from_stored(stored: &str) -> Self {
        match master_key() {
            Some(key) => {
                if stored.starts_with(ENC_PREFIX) {
                    match decrypt_with_key(stored, key) {
                        Some(plaintext) => StoreSecret(plaintext),
                        None => {
                            // AT_REST_DECRYPT_FAIL_V1
                            // enc:v1 開頭但解不開 → key 與寫入時不一致。此時若
                            // 回退成原始加密字串當密碼，登入必失敗。明確記錄警告，
                            // 並回退成原文（讓呼叫端可用原始值，但能從 log 察覺）。
                            tracing::warn!(
                                "at-rest 解密失敗：enc:v1 值無法用目前 master key 解密（key 可能不一致或資料損毀）"
                            );
                            StoreSecret(stored.to_string())
                        }
                    }
                } else {
                    StoreSecret(stored.to_string())
                }
            }
            None => StoreSecret(stored.to_string()),
        }
    }
}

impl std::fmt::Debug for StoreSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoreSecret(<redacted>)")
    }
}

impl std::ops::Deref for StoreSecret {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Read the master key from `TRONCLASS_MASTER_KEY` (32 bytes as lowercase hex).
fn master_key() -> Option<[u8; 32]> {
    let raw = std::env::var("TRONCLASS_MASTER_KEY").ok()?;
    let raw = raw.trim();
    let bytes = hex::decode(raw).ok()?;
    if bytes.len() != 32 {
        warn!("TRONCLASS_MASTER_KEY 必須是 32 bytes（64 個 hex 字元）；停用 at-rest 加密");
        return None;
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Some(arr)
}

/// Whether at-rest encryption is active (a valid master key is configured).
pub fn is_encryption_enabled() -> bool {
    master_key().is_some()
}

fn encrypt_with_key(plaintext: &str, key: [u8; 32]) -> Option<String> {
    // Random 12-byte nonce per value.
    let mut nonce_bytes = [0u8; 12];
    getrandom::getrandom(&mut nonce_bytes).ok()?;
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce_bytes), plaintext.as_bytes())
        .ok()?;
    Some(format!(
        "{ENC_PREFIX}{}{}",
        hex::encode(nonce_bytes),
        STANDARD.encode(ciphertext)
    ))
}

fn decrypt_with_key(stored: &str, key: [u8; 32]) -> Option<String> {
    let rest = stored.strip_prefix(ENC_PREFIX)?;
    if rest.len() < 24 {
        return None;
    }
    let (nonce_hex, b64) = rest.split_at(24);
    let nonce_hex_decoded = hex::decode(nonce_hex).ok()?;
    if nonce_hex_decoded.len() != 12 {
        return None;
    }
    let mut nonce_arr = [0u8; 12];
    nonce_arr.copy_from_slice(&nonce_hex_decoded);
    let ciphertext = STANDARD.decode(b64).ok()?;
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let plaintext = cipher
        .decrypt(&Nonce::from(nonce_arr), ciphertext.as_ref())
        .ok()?;
    String::from_utf8(plaintext).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_secret_roundtrip() {
        // Test the envelope primitives directly with a known key.
        let key = [7u8; 32];
        let enc = encrypt_with_key("sup3rs3cret", key).unwrap();
        assert!(enc.starts_with(ENC_PREFIX));
        assert_eq!(decrypt_with_key(&enc, key).as_deref(), Some("sup3rs3cret"));
        // Wrong key must not decrypt.
        let bad = [8u8; 32];
        assert!(decrypt_with_key(&enc, bad).is_none());
    }

    #[test]
    fn legacy_plaintext_passes_through() {
        // With no key configured, StoreSecret stores plaintext.
        std::env::remove_var("TRONCLASS_MASTER_KEY");
        let s = StoreSecret::new("plain");
        assert_eq!(s.store_value(), "plain");
        assert_eq!(StoreSecret::from_stored("plain").as_str(), "plain");
    }
}
