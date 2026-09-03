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
//!
//! MASTER_KEY_ROTATION_V1（master key 版號輪替）:
//! - 新寫入一律使用 v2 信封 `enc:v2:<key_id>:<nonce><ct>`，其中 key id 是
//!   SHA-256(key) 的前 8 bytes hex——只能識別、無法反推 key。
//! - `TRONCLASS_MASTER_KEY_OLD`（逗號或空白分隔的多把舊 key）僅供解密與
//!   背景重加密：v1 信封依序嘗試目前／舊 key；v2 信封依 key id 直接選 key。
//! - 讀取帳號時（`db::accounts`）自動把舊信封重加密為目前 key 的 v2，
//!   全部帳號完成後 operator 即可自環境移除舊 key。
//! - 輪替程序：產生新 key → 設 `TRONCLASS_MASTER_KEY=<新>`、
//!   `TRONCLASS_MASTER_KEY_OLD=<舊>` → 重啟 → 等待 log 出現各帳號
//!   「已重加密」→ 移除舊 key 環境變數再重啟。

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use tracing::warn;

/// Prefix distinguishing our AES-GCM envelope from legacy plaintext (v1 format).
const ENC_PREFIX: &str = "enc:v1:";
/// MASTER_KEY_ROTATION_V1：v2 信封前綴，後接 16 字元 key id 與 `:`。
const ENC_PREFIX_V2: &str = "enc:v2:";

/// 此儲存值是否為加密信封（v1 或 v2）。
fn is_envelope(stored: &str) -> bool {
    stored.starts_with(ENC_PREFIX) || stored.starts_with(ENC_PREFIX_V2)
}

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
                if is_envelope(stored) {
                    match decrypt_layered(stored, key) {
                        Some(plaintext) => StoreSecret(plaintext),
                        None => {
                            // AT_REST_DECRYPT_FAIL_V1
                            // enc: 開頭但解不開 → 設定的 key 與寫入時皆不一致
                            // （或雙重加密超過可解層數）。回退成原文僅供辨識，
                            // 以此值登入必然失敗。明確記錄警告以便察覺。
                            tracing::warn!(
                                "at-rest 解密失敗：enc 信封無法以目前／舊 master key 解密（key id 不符、key 遺失，或多層污染）；以此帳號登入將失敗，請核對 TRONCLASS_MASTER_KEY(_OLD)"
                            );
                            StoreSecret(stored.to_string())
                        }
                    }
                } else {
                    StoreSecret(stored.to_string())
                }
            }
            None => {
                // MASTER_KEY_ROTATION_V1：未設 key 但值是加密信封＝一定解不開，
                // 回退原文當密碼必然導致登入失敗。收斂為一次 warn 以便察覺。
                if is_envelope(stored) {
                    static WARNED_NO_KEY: once_cell::sync::OnceCell<()> =
                        once_cell::sync::OnceCell::new();
                    WARNED_NO_KEY.get_or_init(|| {
                        warn!(
                            "資料庫內有加密信封（enc:…）但未設定 TRONCLASS_MASTER_KEY；密碼／cookie 無法解密，該帳號登入將失敗"
                        );
                    });
                }
                StoreSecret(stored.to_string())
            }
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

/// MASTER_KEY_ROTATION_V1：舊 key 清單（僅供解密與背景重加密）。
///
/// `TRONCLASS_MASTER_KEY_OLD` 以逗號或空白分隔多把 32-byte hex key；
/// 無效項目略過並 warn。未設定＝無舊 key（v1 信封只能用目前 key 解）。
fn old_master_keys() -> Vec<[u8; 32]> {
    let raw = match std::env::var("TRONCLASS_MASTER_KEY_OLD") {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let mut keys = Vec::new();
    for part in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match hex::decode(part).ok() {
            Some(bytes) if bytes.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                keys.push(arr);
            }
            _ => warn!("TRONCLASS_MASTER_KEY_OLD 內含無效 key（需 64 個 hex 字元），已略過該項"),
        }
    }
    keys
}

/// MASTER_KEY_ROTATION_V1：由 key 推導 16 字元 hex 的 key id。
///
/// SHA-256(key) 前 8 bytes——僅供識別信封是用哪把 key 加密，無法反推 key。
fn key_id(key: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key);
    hex::encode(&digest[..8])
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
    // MASTER_KEY_ROTATION_V1：一律寫 v2 信封（帶 key id），輪替時可直接對應 key。
    Some(format!(
        "{ENC_PREFIX_V2}{}:{}{}",
        key_id(&key),
        hex::encode(nonce_bytes),
        STANDARD.encode(ciphertext)
    ))
}

/// MASTER_KEY_ROTATION_V1：依信封版別解密。
///
/// - v2 `enc:v2:<key_id>:<payload>`：依 key id 比對目前＋舊 key 直接選 key。
/// - v1 `enc:v1:<payload>`：依序嘗試目前 key 與舊 key（GCM 驗證失敗＝用錯
///   key，天然安全，逐一嘗試不會誤解密）。
fn decrypt_any(stored: &str, current: [u8; 32]) -> Option<String> {
    let olds = old_master_keys();
    let candidates = || std::iter::once(current).chain(olds.into_iter());
    if let Some(rest) = stored.strip_prefix(ENC_PREFIX_V2) {
        let (id_hex, payload) = rest.split_once(':')?;
        if id_hex.len() != 16 {
            return None;
        }
        for key in candidates() {
            if key_id(&key) == id_hex {
                return decrypt_payload(payload, key);
            }
        }
        None
    } else {
        let payload = stored.strip_prefix(ENC_PREFIX)?;
        for key in candidates() {
            if let Some(plaintext) = decrypt_payload(payload, key) {
                return Some(plaintext);
            }
        }
        None
    }
}

/// 解 `<nonce_hex24><b64_ct>` payload（v1／v2 共用格式）。
fn decrypt_payload(payload: &str, key: [u8; 32]) -> Option<String> {
    if payload.len() < 24 {
        return None;
    }
    let (nonce_hex, b64) = payload.split_at(24);
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

/// MASTER_KEY_ROTATION_V1：逐層解開信封（防雙重加密污染）。
///
/// 歷史 bug 可能把「解密失敗 fallback 的密文字串」當成密碼回寫，形成
/// `enc:v2( enc:v1( 明文 ) )` 的多層結構——單層解密「成功」但得到的仍是
/// 信封字串，登入必然失敗且不出現解密警告。這裡連續解到不是信封為止
/// （上限 4 層），讓此類污染自動修復。
fn decrypt_layered(stored: &str, current: [u8; 32]) -> Option<String> {
    let mut value = stored.to_string();
    for _ in 0..4 {
        if !is_envelope(&value) {
            return Some(value);
        }
        value = decrypt_any(&value, current)?;
    }
    None
}

/// MASTER_KEY_ROTATION_V1：此儲存值是否需要重加密至目前 key。
///
/// v1 信封一律需要（輪替後補上 key id 標記）；v2 信封僅在 key id ≠
/// 目前 key 時需要；純文字或未啟用加密＝不需要。
pub fn needs_rewrap(stored: &str) -> bool {
    let Some(current) = master_key() else {
        return false;
    };
    if let Some(rest) = stored.strip_prefix(ENC_PREFIX_V2) {
        match rest.split_once(':') {
            Some((id_hex, _)) => id_hex != key_id(&current),
            None => true,
        }
    } else {
        stored.starts_with(ENC_PREFIX)
    }
}

/// MASTER_KEY_ROTATION_V1：把舊信封解密後以目前 key 重加密成 v2。
///
/// 失敗（未啟用加密、非信封、解不開）回 `None`，呼叫端應保留原值。
/// 使用多層解密——雙重加密的污染值會被重寫為乾淨的單層 v2。
pub fn rewrap_stored(stored: &str) -> Option<String> {
    let current = master_key()?;
    if !is_envelope(stored) {
        return None;
    }
    let plaintext = decrypt_layered(stored, current)?;
    encrypt_with_key(&plaintext, current)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 以固定 nonce 構造 v1 信封（僅測試用；量產路徑一律寫 v2）。
    fn build_v1(plaintext: &str, key: [u8; 32]) -> Option<String> {
        let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
        let nonce = [0u8; 12];
        let ciphertext = cipher
            .encrypt(&Nonce::from(nonce), plaintext.as_bytes())
            .ok()?;
        Some(format!(
            "{ENC_PREFIX}{}{}",
            hex::encode(nonce),
            STANDARD.encode(ciphertext)
        ))
    }

    #[test]
    fn store_secret_roundtrip() {
        // Test the envelope primitives directly with a known key.
        let key = [7u8; 32];
        let enc = encrypt_with_key("sup3rs3cret", key).unwrap();
        // MASTER_KEY_ROTATION_V1：新寫入為 v2 信封（帶 key id）
        assert!(enc.starts_with("enc:v2:"));
        assert!(enc.contains(&key_id(&key)));
        assert_eq!(decrypt_any(&enc, key).as_deref(), Some("sup3rs3cret"));
        // Wrong key must not decrypt.
        let bad = [8u8; 32];
        assert!(decrypt_any(&enc, bad).is_none());
    }

    #[test]
    fn legacy_plaintext_passes_through() {
        // With no key configured, StoreSecret stores plaintext.
        std::env::remove_var("TRONCLASS_MASTER_KEY");
        let s = StoreSecret::new("plain");
        assert_eq!(s.store_value(), "plain");
        assert_eq!(StoreSecret::from_stored("plain").as_str(), "plain");
    }

    // ── MASTER_KEY_ROTATION_V1 ─────────────────────────────────────────────────

    // 需要操作環境變數的測試集中於單一測試，避免並行測試互相污染。
    #[test]
    fn rotation_rewrap_matrix() {
        std::env::set_var("TRONCLASS_MASTER_KEY", hex::encode([10u8; 32]));
        std::env::set_var("TRONCLASS_MASTER_KEY_OLD", hex::encode([9u8; 32]));
        let old_key = [9u8; 32];
        let cur = [10u8; 32];

        let v1_old = build_v1("secret1", old_key).unwrap();
        let v2_old = encrypt_with_key("secret3", old_key).unwrap();
        let v2_cur = encrypt_with_key("secret2", cur).unwrap();

        // 讀取：v1 由舊 key 解出；v2 依 key id 找到（含舊 key）
        assert_eq!(StoreSecret::from_stored(&v1_old).as_str(), "secret1");
        assert_eq!(StoreSecret::from_stored(&v2_old).as_str(), "secret3");
        assert_eq!(StoreSecret::from_stored(&v2_cur).as_str(), "secret2");

        // needs_rewrap：v1 → true；v2(舊 key id) → true；v2(目前 key id) → false；純文字 → false
        assert!(needs_rewrap(&v1_old));
        assert!(needs_rewrap(&v2_old));
        assert!(!needs_rewrap(&v2_cur));
        assert!(!needs_rewrap("plain"));

        // rewrap：v1(舊 key) → v2(目前 key) 且明文不變
        let rewrapped = rewrap_stored(&v1_old).unwrap();
        assert!(rewrapped.starts_with("enc:v2:"));
        assert!(rewrapped.contains(&key_id(&cur)));
        assert_eq!(StoreSecret::from_stored(&rewrapped).as_str(), "secret1");
        assert!(!needs_rewrap(&rewrapped));

        // rewrap：v2(舊 key) → v2(目前 key)
        let rewrapped2 = rewrap_stored(&v2_old).unwrap();
        assert_eq!(StoreSecret::from_stored(&rewrapped2).as_str(), "secret3");
        assert!(!needs_rewrap(&rewrapped2));

        // 解不開的垃圾信封 → rewrap None（呼叫端保留原值）
        let garbage = format!(
            "{ENC_PREFIX}{}{}",
            hex::encode([1u8; 12]),
            STANDARD.encode([2u8; 40])
        );
        assert!(rewrap_stored(&garbage).is_none());

        std::env::remove_var("TRONCLASS_MASTER_KEY");
        std::env::remove_var("TRONCLASS_MASTER_KEY_OLD");
    }

    #[test]
    fn double_encryption_unwraps_and_cleans() {
        std::env::set_var("TRONCLASS_MASTER_KEY", hex::encode([10u8; 32]));
        std::env::set_var("TRONCLASS_MASTER_KEY_OLD", hex::encode([9u8; 32]));
        let cur = [10u8; 32];

        // 模擬污染：解密失敗 fallback 的 v1 密文被當成「密碼」再加密一層 v2
        let inner = build_v1("real-password", [9u8; 32]).unwrap();
        let polluted = encrypt_with_key(&inner, cur).unwrap();

        // 多層解開：讀取時直接得到真實明文
        assert_eq!(StoreSecret::from_stored(&polluted).as_str(), "real-password");

        // rewrap 後寫回乾淨的單層 v2，且仍解出真實明文
        let cleaned = rewrap_stored(&polluted).unwrap();
        assert!(cleaned.starts_with("enc:v2:"));
        assert_eq!(StoreSecret::from_stored(&cleaned).as_str(), "real-password");
        // 三層也應解開（上限 4 層）
        let triple = encrypt_with_key(&polluted, cur).unwrap();
        assert_eq!(StoreSecret::from_stored(&triple).as_str(), "real-password");

        std::env::remove_var("TRONCLASS_MASTER_KEY");
        std::env::remove_var("TRONCLASS_MASTER_KEY_OLD");
    }
}
