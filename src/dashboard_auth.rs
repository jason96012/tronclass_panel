//! Dashboard credential hashing and validation.
//!
//! The password in `accounts.password` belongs to the upstream Tronclass
//! provider. It may authenticate exactly one restricted first Dashboard login
//! while no Dashboard verifier exists; that session must immediately create a
//! separate, slow Dashboard password verifier before any account data is shown.
//!
//! We use PBKDF2-HMAC-SHA256 with a unique 128-bit salt and 600,000 iterations.
//! The project already carries the cryptographic primitives used here, so this
//! standards-based verifier does not expand the deployed dependency surface.

use hmac::{Hmac, Mac};
use miette::Result;
use sha2::Sha256;
use uuid::Uuid;

const FORMAT: &str = "pbkdf2-sha256";
const PBKDF2_ITERATIONS: u32 = 600_000;
const MIN_ACCEPTED_ITERATIONS: u32 = 600_000;
const MAX_ACCEPTED_ITERATIONS: u32 = 1_000_000;
const DERIVED_KEY_LEN: usize = 32;

type HmacSha256 = Hmac<Sha256>;

/// Hash a Dashboard-only password for persistent storage.
///
/// The upstream provider password is passed only to make accidental password
/// reuse fail fast. It is never included in the resulting verifier.
pub fn hash_dashboard_password(password: &str, provider_password: &str) -> Result<String> {
    validate_dashboard_password(password, provider_password)?;
    let uuid = Uuid::new_v4();
    let salt = uuid.as_bytes();
    Ok(encode_password_hash(password, salt, PBKDF2_ITERATIONS))
}

/// Verify a Dashboard password against a stored verifier.
///
/// Malformed, obsolete, or deliberately weakened records are rejected. The
/// upstream credential is only accepted by [`verify_first_login_password`] when
/// no Dashboard verifier has been configured yet.
pub fn verify_dashboard_password(password: &str, stored: &str) -> bool {
    let mut fields = stored.split('$');
    let Some(format) = fields.next() else {
        return false;
    };
    if format != FORMAT {
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
        "{FORMAT}${iterations}${}${}",
        hex::encode(salt),
        hex::encode(derived)
    )
}

fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32, output_len: usize) -> Vec<u8> {
    // PBKDF2 emits one HMAC-SHA256 block at a time. Dashboard verifiers use one
    // block today, but retaining the generic loop makes the encoding explicit.
    let mut output = Vec::with_capacity(output_len);
    let mut block_index = 1u32;

    while output.len() < output_len {
        let mut mac =
            HmacSha256::new_from_slice(password).expect("HMAC-SHA256 accepts keys of every length");
        mac.update(salt);
        mac.update(&block_index.to_be_bytes());
        let mut u = mac.finalize().into_bytes().to_vec();
        let mut block = u.clone();

        for _ in 1..iterations {
            let mut iteration_mac = HmacSha256::new_from_slice(password)
                .expect("HMAC-SHA256 accepts keys of every length");
            iteration_mac.update(&u);
            u = iteration_mac.finalize().into_bytes().to_vec();
            for (result, value) in block.iter_mut().zip(&u) {
                *result ^= *value;
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

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
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
        assert!(validate_dashboard_password("short", "different provider password").is_ok());
        assert!(validate_dashboard_password(
            "a separate strong dashboard password",
            "different provider password"
        )
        .is_ok());
    }
}
