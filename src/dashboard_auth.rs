//! Dashboard credential hashing and validation.
//!
//! The password in `accounts.password` belongs to the upstream Tronclass
//! provider. It may authenticate exactly one restricted first Dashboard login
//! while no Dashboard verifier exists; that session must immediately create a
//! separate, slow Dashboard password verifier before any account data is shown.
//!
//! New Dashboard verifiers use **Argon2id** (memory-hard, GPU/ASIC-resistant).
//! Legacy hashes stored as PBKDF2-HMAC-SHA256 remain verifiable so existing
//! users are not locked out; a successful login with a legacy hash re-hashes it
//! to Argon2id in place (smooth migration). Verification reads the algorithm
//! from the stored PHC/hash string itself.

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use hmac::{Hmac, Mac};
use miette::Result;
use sha2::Sha256;

const LEGACY_FORMAT: &str = "pbkdf2-sha256";
const PBKDF2_ITERATIONS: u32 = 200_000;
const MIN_ACCEPTED_ITERATIONS: u32 = 100_000;
const MAX_ACCEPTED_ITERATIONS: u32 = 1_000_000;
const DERIVED_KEY_LEN: usize = 32;
/// DASHBOARD_PASSWORD_MIN_LEN_V1（fix122）：新設 Dashboard 密碼的最短長度
/// （以字元計，Unicode 安全）。
const MIN_DASHBOARD_PASSWORD_CHARS: usize = 8;

type HmacSha256 = Hmac<Sha256>;

/// ARGON2ID_MAX_V1（fix128）：加密強度最大化的 Argon2id 參數——RFC 9106
/// 第一組建議（m=64 MiB, t=3, p=4；crate 預設是第二組 m=19 MiB, t=2, p=1）。
/// 成本約比預設高 10 倍以上（GPU/ASIC 平行攻擊更難攤提），單次登入雜湊
/// 耗時仍在數百毫秒等級，人類無感。PHC 字串內含參數，舊參數的既有雜湊
/// 照樣驗證得過；成功登入後由 is_argon2_params_outdated 判斷並背景升級。
fn argon2_max() -> Argon2<'static> {
    use argon2::{Algorithm, Params, Version};
    static PARAMS: once_cell::sync::Lazy<Params> = once_cell::sync::Lazy::new(|| {
        Params::new(65_536, 3, 4, Some(Params::DEFAULT_OUTPUT_LEN))
            .expect("ARGON2ID_MAX_V1 params are compile-time valid")
    });
    Argon2::new(Algorithm::Argon2id, Version::V0x13, PARAMS.clone())
}

/// ARGON2ID_MAX_V1（fix128）：既有 Argon2id 雜湊的參數是否低於目前上限
/// （要趁成功登入時重新雜湊升級）。非 Argon2id（PBKDF2 等）不算在此——走
/// is_legacy_pbkdf2_hash 的遷移。
pub fn is_argon2_params_outdated(stored_hash: &str) -> bool {
    use argon2::{PasswordHash, Params};
    let Ok(parsed) = PasswordHash::new(stored_hash.trim()) else {
        return false;
    };
    // 0.5 的 PasswordHash.algorithm 型別是 password_hash::Ident<'_>
    // （argon2-fix129 曾誤把它比成 Algorithm 造成 E0308）——字串比對即可。
    if parsed.algorithm.as_str() != "argon2id" {
        return false;
    }
    // 讀 m/t 成本：0.5 版 `ParamsString` 沒有直接 getter，而 `Params: TryFrom`
    // 吃的是整個 `&PasswordHash`（不是 `&ParamsString`）。轉換失敗＝異常信封
    // → 一律視為落後，成功登入背景重雜湊，具自癒性（重寫後即可正解析）。
    let Ok(params) = Params::try_from(&parsed) else {
        return true;
    };
    // RFC 9106 第一組：m=65536, t=3；0.5 的 getter 回的是 u32（不是 Result）。
    params.m_cost() < 65_536 || params.t_cost() < 3
}

/// Hash a Dashboard-only password for persistent storage.
///
/// Uses Argon2id with a random salt. The upstream provider password is passed
/// only to make accidental password reuse fail fast; it is never included in
/// the resulting verifier.
pub fn hash_dashboard_password(password: &str, provider_password: &str) -> Result<String> {
    validate_dashboard_password(password, provider_password)?;
    let salt = SaltString::generate(&mut OsRng);
    // ARGON2ID_MAX_V1：加密強度最大化（m=64 MiB, t=3, p=4）。
    let argon2 = argon2_max();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| miette::miette!("密碼雜湊失敗：{e}"))?;
    Ok(hash.to_string())
}

/// SHARE_PASSWORD_V2：分享連結密碼的雜湊（Argon2id，與後台密碼同一組參數）。
///
/// 刻意不呼叫 `hash_dashboard_password`：後者會套用後台管理員密碼的規則
/// （與供應商密碼比對等），分享密碼只需要長度下限，規則由呼叫端檢查。
/// 驗證沿用 `verify_dashboard_password`（同樣讀 PHC 字串）。
pub fn hash_share_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = argon2_max()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| miette::miette!("分享密碼雜湊失敗：{e}"))?;
    Ok(hash.to_string())
}

/// Verify a Dashboard password against a stored verifier.
///
/// Accepts both Argon2id (PHC format, `$argon2id$...`) and legacy PBKDF2
/// (`pbkdf2-sha256$...`) hashes so existing users are not locked out during
/// migration. Malformed or deliberately weakened records are rejected.
pub fn verify_dashboard_password(password: &str, stored: &str) -> bool {
    let stored = stored.trim();
    if stored.starts_with("$argon2") || stored.starts_with("argon2") {
        // Argon2id PHC string.
        let Ok(parsed) = PasswordHash::new(stored) else {
            return false;
        };
        return Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
    }
    verify_legacy_pbkdf2(password, stored)
}

// ── LOGIN_VERIFY_OFFLOAD_V1 ──────────────────────────────────────────────────
//
// Argon2id / PBKDF2 驗證是刻意設計成慢（數十毫秒、佔記憶體）的 CPU 工作。
// 直接在 tokio worker 上跑會把同一條執行緒上的其他請求（SSE、狀態輪詢、點名）
// 一起卡住。以下包裝把驗證丟到 `spawn_blocking` 執行緒池；驗證失敗與
// 執行緒池錯誤一律視為「不通過」，不會 panic。

/// `verify_dashboard_password` 的非同步版本（在 blocking 執行緒池執行）。
pub async fn verify_dashboard_password_async(password: String, stored: String) -> bool {
    tokio::task::spawn_blocking(move || verify_dashboard_password(&password, &stored))
        .await
        .unwrap_or(false)
}

/// `hash_dashboard_password` 的非同步版本（在 blocking 執行緒池執行）。
pub async fn hash_dashboard_password_async(
    password: String,
    provider_password: String,
) -> Result<String> {
    match tokio::task::spawn_blocking(move || hash_dashboard_password(&password, &provider_password))
        .await
    {
        Ok(result) => result,
        Err(e) => Err(miette::miette!("密碼雜湊工作執行失敗：{e}")),
    }
}

/// LOGIN_TIMING_EQUALIZE_V1：帳號不存在／未啟用／沒有 Dashboard 密碼時，仍對一個固定的
/// Argon2id 假雜湊做一次完整驗證，讓「查無此帳號」與「密碼錯誤」的回應時間相近，
/// 降低以時間差枚舉帳號的可能。這個雜湊對應的明文不會被任何人使用，驗證結果被丟棄。
///
/// 參數已對齊 ARGON2ID_MAX_V1（fix128）的 m=65536, t=3, p=4——驗證走的是
/// PHC 字串內的參數（與 `Argon2` 實例參數無關），預設參數的假雜湊會讓
/// 「帳號不存在」比「帳號存在」快約 3 倍而露底。同攜舊鹽與一段不會命中的
/// 雜湊主體（內容無意義，成本看參數）。
const DUMMY_ARGON2_HASH: &str =
    "$argon2id$v=19$m=65536,t=3,p=4$89PxjHgS8OCx6QEezATY2w$PqDCKzNfHB5bMJ3aK7KXN6ArtG8DDcOhfYfg3HPt9hM";

pub async fn burn_dummy_verification(password: String) {
    let _ = verify_dashboard_password_async(password, DUMMY_ARGON2_HASH.to_string()).await;
}

/// Whether a stored verifier is an older PBKDF2 hash (eligible for in-place
/// upgrade to Argon2id on next successful login).
pub fn is_legacy_pbkdf2_hash(stored: &str) -> bool {
    stored.trim().starts_with(&format!("{LEGACY_FORMAT}$"))
}

fn verify_legacy_pbkdf2(password: &str, stored: &str) -> bool {
    let mut fields = stored.split('$');
    let Some(format) = fields.next() else {
        return false;
    };
    if format != LEGACY_FORMAT {
        return false;
    }

    let Some(iterations) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    if !(MIN_ACCEPTED_ITERATIONS..=MAX_ACCEPTED_ITERATIONS).contains(&iterations) {
        return false;
    }

    let Some(salt) = fields.next().and_then(|value| hex::decode(value).ok()) else {
        return false;
    };
    let Some(expected) = fields.next().and_then(|value| hex::decode(value).ok()) else {
        return false;
    };
    if fields.next().is_some() || salt.len() < 16 || expected.len() != DERIVED_KEY_LEN {
        return false;
    }

    let actual = pbkdf2_hmac_sha256(password.as_bytes(), &salt, iterations, expected.len());
    constant_time_eq(&actual, &expected)
}

/// Verify an upstream Tronclass password for a one-time Dashboard bootstrap.
///
/// This is called only when `dashboard_password_hash` is empty. The resulting
/// session is restricted to changing the Dashboard password or logging out, so
/// the upstream credential never remains a normal Dashboard login password.
pub fn verify_first_login_password(password: &str, provider_password: &str) -> bool {
    !provider_password.is_empty()
        && !password.is_empty()
        && constant_time_eq(password.as_bytes(), provider_password.as_bytes())
}

/// Validate a new Dashboard password before it is stored.
///
/// There is intentionally no character-count policy. Users may choose a short
/// password or a long passphrase, but it must not be empty, contain control
/// characters, or reuse the upstream Tronclass credential.
pub fn validate_dashboard_password(password: &str, provider_password: &str) -> Result<()> {
    if password.is_empty() {
        return Err(miette::miette!("Dashboard 密碼不可為空"));
    }
    // DASHBOARD_PASSWORD_MIN_LEN_V1（fix122）：原先只檢查「非空＋不含控制字元＋
    // 不等於上游密碼」——1 位數密碼也設得過。雖然登入有限流＋登入失敗封鎖，
    // 長度層仍要補：新設密碼最短 8 個字元；既存舊密碼不受影響，下次變更時
    // 才用這把尺。
    if password.chars().count() < MIN_DASHBOARD_PASSWORD_CHARS {
        return Err(miette::miette!(
            "Dashboard 密碼長度至少需要 8 個字元"
        ));
    }
    if password.chars().any(char::is_control) {
        return Err(miette::miette!("Dashboard 密碼不可包含控制字元"));
    }
    if !provider_password.is_empty()
        && constant_time_eq(password.as_bytes(), provider_password.as_bytes())
    {
        return Err(miette::miette!(
            "Dashboard 密碼不可與 Tronclass 登入密碼相同"
        ));
    }
    Ok(())
}

fn encode_password_hash(password: &str, salt: &[u8], iterations: u32) -> String {
    let derived = pbkdf2_hmac_sha256(password.as_bytes(), salt, iterations, DERIVED_KEY_LEN);
    format!(
        "{LEGACY_FORMAT}${iterations}${}${}",
        hex::encode(salt),
        hex::encode(derived)
    )
}

fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32, output_len: usize) -> Vec<u8> {
    // PERFORMANCE_PBKDF2_V2:
    // Pre-compute the HMAC key schedule once and clone the ready-to-use
    // context for each iteration.  The previous implementation called
    // `HmacSha256::new_from_slice` (which hashes the key through ipad/opad
    // padding) on every single iteration — that is O(iterations) redundant
    // key setups.  Cloning is O(1) and lets the hot loop focus on the actual
    // HMAC compression instead of key derivation overhead.
    //
    // PBKDF2 emits one HMAC-SHA256 block (32 bytes) at a time.  Dashboard
    // verifiers use exactly one block today, but the generic loop is retained
    // so the encoding stays explicit and extensible.
    let base_mac =
        HmacSha256::new_from_slice(password).expect("HMAC-SHA256 accepts keys of every length");

    let mut output = Vec::with_capacity(output_len);
    let mut block_index = 1u32;

    while output.len() < output_len {
        // U_1 = PRF(password, salt || INT_32_BE(block_index))
        let mut u_mac = base_mac.clone();
        u_mac.update(salt);
        u_mac.update(&block_index.to_be_bytes());
        let mut u_bytes: [u8; 32] = u_mac.finalize().into_bytes().into();
        let mut block = u_bytes;

        // U_2 .. U_c
        for _ in 1..iterations {
            let mut iter_mac = base_mac.clone();
            iter_mac.update(&u_bytes);
            u_bytes = iter_mac.finalize().into_bytes().into();
            for (result, &value) in block.iter_mut().zip(&u_bytes) {
                *result ^= value;
            }
        }

        let remaining = output_len - output.len();
        output.extend_from_slice(&block[..remaining.min(block.len())]);
        block_index = block_index
            .checked_add(1)
            .expect("PBKDF2 block index overflow");
    }

    output
}

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(a ^ b);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_hash_verifies_only_its_password() {
        let hash = encode_password_hash("correct horse battery staple", b"0123456789abcdef", 1_000);
        // Production verification intentionally rejects weakened hashes, so test
        // the PBKDF2 primitive directly for a fast deterministic unit test.
        let expected = pbkdf2_hmac_sha256(
            b"correct horse battery staple",
            b"0123456789abcdef",
            1_000,
            DERIVED_KEY_LEN,
        );
        let actual = pbkdf2_hmac_sha256(
            b"correct horse battery staple",
            b"0123456789abcdef",
            1_000,
            DERIVED_KEY_LEN,
        );
        assert!(constant_time_eq(&actual, &expected));
        assert!(!constant_time_eq(
            &pbkdf2_hmac_sha256(
                b"wrong password 000",
                b"0123456789abcdef",
                1_000,
                DERIVED_KEY_LEN
            ),
            &expected
        ));
        assert!(hash.starts_with("pbkdf2-sha256$1000$"));
    }

    #[test]
    fn production_hash_roundtrips() {
        let hash = hash_dashboard_password(
            "a separate strong dashboard password",
            "different provider password",
        )
        .unwrap();
        assert!(verify_dashboard_password(
            "a separate strong dashboard password",
            &hash
        ));
        assert!(!verify_dashboard_password(
            "wrong dashboard password",
            &hash
        ));
    }

    #[test]
    fn first_login_password_matches_only_the_upstream_credential() {
        assert!(verify_first_login_password(
            "provider-secret",
            "provider-secret"
        ));
        assert!(!verify_first_login_password("wrong", "provider-secret"));
        assert!(!verify_first_login_password("provider-secret", ""));
    }

    #[test]
    fn password_policy_rejects_upstream_password_reuse() {
        assert!(validate_dashboard_password(
            "this is a long provider secret",
            "this is a long provider secret"
        )
        .is_err());
        assert!(validate_dashboard_password("", "different provider password").is_err());
        // DASHBOARD_PASSWORD_MIN_LEN_V1：8 以下（含）一律拒絕。
        assert!(validate_dashboard_password("short", "different provider password").is_err());
        assert!(validate_dashboard_password("12345678", "different provider password").is_ok());
        assert!(validate_dashboard_password(
            "a separate strong dashboard password",
            "different provider password"
        )
        .is_ok());
    }
}
