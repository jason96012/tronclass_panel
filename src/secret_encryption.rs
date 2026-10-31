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
//!
//! STORE_ENVELOPE_AAD_V1（fix128，v3 信封，加密強度最大化）:
//! - 新寫入一律 `enc:v3:<key_id>:<nonce><ct>`，加解密使用 AES-256-GCM 的
//!   **AAD（附加認證資料）綁定「用途字串」**（`tronclass.store.v3|<purpose>`，
//!   例如 `accounts.password`）——攜帶 DB 寫入權的攻擊者無法把 A 欄位／A 帳號
//!   的密文搬到 B 欄位／B 帳號複用密文（密文搬家攻擊會在 GCM 驗證失敗）。
//! - 讀取雙讀：v1／v2 以空 AAD（沿用舊行為），v3 驗證用途對應；env v1/v2 列經由
//!   `needs_rewrap` 判定為「需重加密」，讀取路徑自動重寫成 v3。

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use tracing::warn;

/// Prefix distinguishing our AES-GCM envelope from legacy plaintext (v1 format).
const ENC_PREFIX: &str = "enc:v1:";
/// MASTER_KEY_ROTATION_V1：v2 信封前綴，後接 16 字元 key id 與 `:`。
const ENC_PREFIX_V2: &str = "enc:v2:";
/// STORE_ENVELOPE_AAD_V1（fix128）：v3 信封前綴——AES-GCM AAD 綁定用途。
const ENC_PREFIX_V3: &str = "enc:v3:";

/// 此儲存值是否為加密信封（v1／v2／v3）。
fn is_envelope(stored: &str) -> bool {
    stored.starts_with(ENC_PREFIX)
        || stored.starts_with(ENC_PREFIX_V2)
        || stored.starts_with(ENC_PREFIX_V3)
}

/// STORE_ENVELOPE_AAD_V1（fix128）：v3 信封的 GCM AAD——把「這段密文是哪個
/// 欄位／用途」寫進 GCM 驗證，密文被搬家（跨欄位／跨帳號複用）時驗證必敗。
fn v3_aad(purpose: &str) -> Vec<u8> {
    format!("tronclass.store.v3|{purpose}").into_bytes()
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
    ///
    /// 無特定用途欄位時用這支（AAD 以空用途編碼）。敏感欄位請改用
    /// [`StoreSecret::store_value_with_purpose`] 做密文用途綁定。
    pub fn store_value(&self) -> String {
        self.store_value_with_purpose("")
    }

    /// STORE_ENVELOPE_AAD_V1（fix128）：以「用途」做 GCM AAD 綁定的加密寫法。
    /// `purpose` 是穩定的分類字串（例如 `"accounts.password"`），解密端需
    /// 以同一用途呼叫 [`StoreSecret::from_stored_with_purpose`]；跨用途／跨欄位
    /// 複用密文會讓 GCM 驗證失敗，被當成解密失敗處理。
    pub fn store_value_with_purpose(&self, purpose: &str) -> String {
        match master_key() {
            Some(key) => {
                encrypt_with_key(&self.0, key, purpose).unwrap_or_else(|| self.0.clone())
            }
            None => self.0.clone(),
        }
    }

    /// Parse a value loaded from the DB: decrypt if it is an envelope, else
    /// treat it as legacy plaintext.
    pub fn from_stored(stored: &str) -> Self {
        Self::from_stored_with_purpose(stored, "")
    }

    /// STORE_ENVELOPE_AAD_V1（fix128）：搭配用途的解密讀取。對 v3 信封要求
    /// 用途一致；v1／v2 舊信封仍以空 AAD 雙讀（`needs_rewrap` 會排定升級重寫）。
    pub fn from_stored_with_purpose(stored: &str, purpose: &str) -> Self {
        match master_key() {
            Some(key) => {
                if is_envelope(stored) {
                    match decrypt_layered(stored, key, purpose) {
                        Some(plaintext) => StoreSecret(plaintext),
                        None => {
                            // AT_REST_DECRYPT_FAIL_V1 + DECRYPT_FAIL_NO_SUBMIT_V1
                            // enc: 開頭但解不開 → 設定的 key 與寫入時皆不一致
                            // （或雙重加密超過可解層數）。絕不回傳原始密文——
                            // 舊行為把密文當密碼送出，會在學校端累計失敗、
                            // 觸發防護鎖定（真實事故）。改回空字串，登入入口
                            // 會直接阻止送出任何請求。
                            tracing::error!(
                                "at-rest 解密失敗：enc 信封無法以目前／舊 master key 解密；已將密碼視為空白並阻止登入送出（請檢查 TRONCLASS_MASTER_KEY(_OLD) 或重新儲存該帳號密碼）"
                            );
                            StoreSecret(String::new())
                        }
                    }
                } else {
                    StoreSecret(stored.to_string())
                }
            }
            None => {
                // MASTER_KEY_ROTATION_V1：未設 key 但值是加密信封＝一定解不開。
                // DECRYPT_FAIL_NO_SUBMIT_V1：回空字串（非密文），登入入口阻止送出。
                // PLAINTEXT_PASSTHROUGH_V2：未啟用 at-rest 加密的部署（DB 內是明文）必須原樣回傳。
                // 舊版這裡把「所有」值都清成空字串，導致未設 key 時每個帳號都讀不到密碼，
                // 登入失敗，進而讓啟動流程退出（見 main.rs 的 LOGIN_FAILURE_NONFATAL_V1）。
                if !is_envelope(stored) {
                    return StoreSecret(stored.to_string());
                }
                if is_envelope(stored) {
                    static WARNED_NO_KEY: once_cell::sync::OnceCell<()> =
                        once_cell::sync::OnceCell::new();
                    WARNED_NO_KEY.get_or_init(|| {
                        warn!(
                            "資料庫內有加密信封（enc:…）但未設定 TRONCLASS_MASTER_KEY；密碼以空白處理，登入將被阻止"
                        );
                    });
                }
                StoreSecret(String::new())
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

fn encrypt_with_key(plaintext: &str, key: [u8; 32], purpose: &str) -> Option<String> {
    use aes_gcm::aead::Payload;
    // Random 12-byte nonce per value.
    let mut nonce_bytes = [0u8; 12];
    getrandom::getrandom(&mut nonce_bytes).ok()?;
    let cipher = Aes256Gcm::new_from_slice(&key).ok()?;
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce_bytes),
            Payload {
                msg: plaintext.as_bytes(),
                aad: &v3_aad(purpose),
            },
        )
        .ok()?;
    // STORE_ENVELOPE_AAD_V1（fix128）：一律寫 v3 信封（帶 key id＋AAD 綁定）。
    Some(format!(
        "{ENC_PREFIX_V3}{}:{}{}",
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
fn decrypt_any(stored: &str, current: [u8; 32], purpose: &str) -> Option<String> {
    let olds = old_master_keys();
    let candidates = || std::iter::once(current).chain(olds.into_iter());
    if let Some(rest) = stored.strip_prefix(ENC_PREFIX_V3) {
        let (id_hex, payload) = rest.split_once(':')?;
        if id_hex.len() != 16 {
            return None;
        }
        // v3：GCM AAD 以呼叫端宣告的用途編碼——密文遭搬家時驗證必敗。
        let aad = v3_aad(purpose);
        for key in candidates() {
            if key_id(&key) == id_hex {
                return decrypt_payload(payload, key, &aad);
            }
        }
        None
    } else if let Some(rest) = stored.strip_prefix(ENC_PREFIX_V2) {
        let (id_hex, payload) = rest.split_once(':')?;
        if id_hex.len() != 16 {
            return None;
        }
        for key in candidates() {
            if key_id(&key) == id_hex {
                return decrypt_payload(payload, key, b"");
            }
        }
        None
    } else {
        let payload = stored.strip_prefix(ENC_PREFIX)?;
        for key in candidates() {
            if let Some(plaintext) = decrypt_payload(payload, key, b"") {
                return Some(plaintext);
            }
        }
        None
    }
}

/// 解 `<nonce_hex24><b64_ct>` payload（v1／v2 共用格式）。
fn decrypt_payload(payload: &str, key: [u8; 32], aad: &[u8]) -> Option<String> {
    use aes_gcm::aead::Payload;
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
        .decrypt(
            &Nonce::from(nonce_arr),
            Payload {
                msg: ciphertext.as_ref(),
                aad,
            },
        )
        .ok()?;
    String::from_utf8(plaintext).ok()
}

/// MASTER_KEY_ROTATION_V1：逐層解開信封（防雙重加密污染）。
///
/// 歷史 bug 可能把「解密失敗 fallback 的密文字串」當成密碼回寫，形成
/// `enc:v2( enc:v1( 明文 ) )` 的多層結構——單層解密「成功」但得到的仍是
/// 信封字串，登入必然失敗且不出現解密警告。這裡連續解到不是信封為止
/// （上限 4 層），讓此類污染自動修復。
fn decrypt_layered(stored: &str, current: [u8; 32], purpose: &str) -> Option<String> {
    let mut value = stored.to_string();
    for _ in 0..4 {
        if !is_envelope(&value) {
            return Some(value);
        }
        value = decrypt_any(&value, current, purpose)?;
    }
    None
}

/// LOGIN_FAIL_DIAG_V1：此字串是否為加密信封樣式（供登入失敗診斷，
/// 判別送出的密碼是否未被解密）。不記錄、不外洩內容。
pub fn looks_like_envelope(value: &str) -> bool {
    is_envelope(value)
}

/// MASTER_KEY_ROTATION_V1＋STORE_ENVELOPE_AAD_V1：此儲存值是否需要重加密。
///
/// v1 一律需要；v2 一律需要（fix128 起升級為用途綁定 v3）；v3 僅在 key id ≠
/// 目前 key 時需要；純文字或未啟用加密＝不需要。
pub fn needs_rewrap(stored: &str) -> bool {
    let Some(current) = master_key() else {
        return false;
    };
    if let Some(rest) = stored.strip_prefix(ENC_PREFIX_V3) {
        match rest.split_once(':') {
            Some((id_hex, _)) => id_hex != key_id(&current),
            None => true,
        }
    } else {
        stored.starts_with(ENC_PREFIX) || stored.starts_with(ENC_PREFIX_V2)
    }
}

/// SECRET_ROTATE_CLI_V1：目前 master key 的 key id（無 key 時 None）。
pub fn current_key_id() -> Option<String> {
    master_key().map(|key| key_id(&key))
}

/// SECRET_ROTATE_CLI_V1：`TRONCLASS_MASTER_KEY_OLD` 內可用的舊 key 數量。
pub fn old_key_count() -> usize {
    old_master_keys().len()
}

/// SECRET_ROTATE_CLI_V1：單一儲存值的輪替結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotateOutcome {
    /// 空字串或已是目前 key 的 v2 信封，不需動作。
    Unchanged,
    /// 原為明文，已用目前 key 加密。
    Encrypted(String),
    /// 原為舊 key／v1 信封，已重加密為目前 key 的 v2。
    Rewrapped(String),
    /// 是信封但目前與舊 key 都解不開——需要 operator 補 `TRONCLASS_MASTER_KEY_OLD`
    /// 或重新儲存密碼。
    Undecryptable,
}

/// SECRET_ROTATE_CLI_V1：把任意儲存值（明文／舊信封／目前信封）收斂成
/// 目前 key 的 v2 信封。呼叫前請確認 [`is_encryption_enabled`]。
pub fn rotate_stored(stored: &str) -> RotateOutcome {
    rotate_stored_with_purpose(stored, "")
}

/// STORE_ENVELOPE_AAD_V1（fix128）：同上，但以指定用途寫 v3。
pub fn rotate_stored_with_purpose(stored: &str, purpose: &str) -> RotateOutcome {
    let Some(current) = master_key() else {
        return RotateOutcome::Unchanged;
    };
    if stored.is_empty() {
        return RotateOutcome::Unchanged;
    }
    if !is_envelope(stored) {
        return match encrypt_with_key(stored, current, purpose) {
            Some(value) => RotateOutcome::Encrypted(value),
            None => RotateOutcome::Undecryptable,
        };
    }
    if !needs_rewrap(stored) {
        return RotateOutcome::Unchanged;
    }
    match decrypt_layered(stored, current, purpose)
        .and_then(|plain| encrypt_with_key(&plain, current, purpose))
    {
        Some(value) => RotateOutcome::Rewrapped(value),
        None => RotateOutcome::Undecryptable,
    }
}

/// MASTER_KEY_ROTATION_V1：把舊信封解密後以目前 key 重加密成 v2。
///
/// 失敗（未啟用加密、非信封、解不開）回 `None`，呼叫端應保留原值。
/// 使用多層解密——雙重加密的污染值會被重寫為乾淨的單層 v2。
pub fn rewrap_stored(stored: &str) -> Option<String> {
    rewrap_stored_with_purpose(stored, "")
}

/// STORE_ENVELOPE_AAD_V1（fix128）：同上，但以指定用途寫 v3。
///
/// PERF_SKIP_NOOP_REWRAP_V1：補上 `needs_rewrap` 短路檢查，與
/// `rotate_stored_with_purpose`（SECRET_ROTATE_CLI_V1 手動輪替指令）行為對齊。
/// 原本這裡沒有這層檢查：由於 AES-GCM 每次加密都會換一個新的隨機 nonce，就算
/// key 完全沒換過，「解密→用目前 key 重新加密」產生的密文字串也一定跟原本不同，
/// 導致呼叫端（`accounts.rs::rewrap_row_secrets`，`list()` 每次列出帳號都會跑
/// 一輪）誤判成「有變化」而寫回資料庫——等於每次 `list()`（後台多處頁面、健康
/// 檢查等共用的高頻函式）都要對全部帳號的 password／session_cookie 各做一次
/// AES-GCM 解密＋加密＋一次 UPDATE，即使從未真的換過 master key。這與本檔上方
/// `rewrap_row_secrets` 的文件註解「每個帳號只需重寫一次，完成後不再產生寫入」
/// 矛盾，屬於遺漏檢查造成的無謂浪費，故補齊。
///
/// 取捨說明（已與使用者確認、知情選擇本做法）：`needs_rewrap` 只檢查「最外層
/// 信封的 key id 是否等於目前 key」，無法分辨「外層 key id 已正確、但內部其實
/// 疊了舊版 bug 留下的雙重加密污染」這種少見情況（對應
/// `double_encryption_unwraps_but_rewrap_skips_when_outer_key_current` 測試模擬
/// 的歷史情境）。加上此檢查後，若還有這類歷史污染
/// 資料殘留，會不再被這個背景讀取路徑自動清乾淨。經確認目前資料庫應已不存在
/// 這類污染（清理機制已跑過夠多輪），且手動輪替指令 `rotate_stored_with_purpose`
/// 本來就已經是相同的短路邏輯，故此處對齊是合理取捨，而非單純疏漏。
pub fn rewrap_stored_with_purpose(stored: &str, purpose: &str) -> Option<String> {
    let current = master_key()?;
    if !is_envelope(stored) {
        return None;
    }
    if !needs_rewrap(stored) {
        return None;
    }
    let plaintext = decrypt_layered(stored, current, purpose)?;
    encrypt_with_key(&plaintext, current, purpose)
}

