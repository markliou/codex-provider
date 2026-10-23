//! Password hashes, random identifiers and the HMAC behind admin sessions.
//!
//! A port of newPasswordHash, verifyPasswordHash, randomUUID and randomID in
//! the Go pool's main.go. PBKDF2-HMAC-SHA256 is written here, as Go writes it
//! locally, so the image build depends on no further crate; the hash format
//! (`pbkdf2-sha256:<iterations>:<salt>:<hash>`, unpadded standard base64) is
//! the one `CODEX_POOL_ADMIN_PASSWORD_HASH` already holds, so the existing
//! admin password keeps working.

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::Digest;
use sha2::Sha256;
use std::io::Read;

const BLOCK: usize = 64;

pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(key_block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(key_block.map(|byte| byte ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32, key_length: usize) -> Vec<u8> {
    // HMAC with the key absorbed once: each round clones the keyed inner and
    // outer states instead of re-padding the key and hashing two more blocks,
    // as Go's crypto/pbkdf2 does through its hmac.Reset.
    let mut key_block = [0u8; BLOCK];
    if password.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(password));
    } else {
        key_block[..password.len()].copy_from_slice(password);
    }
    let mut keyed_inner = Sha256::new();
    keyed_inner.update(key_block.map(|byte| byte ^ 0x36));
    let mut keyed_outer = Sha256::new();
    keyed_outer.update(key_block.map(|byte| byte ^ 0x5c));
    let mac = |message: &[u8]| -> [u8; 32] {
        let mut inner = keyed_inner.clone();
        inner.update(message);
        let mut outer = keyed_outer.clone();
        outer.update(inner.finalize());
        outer.finalize().into()
    };
    let blocks = key_length.div_ceil(32);
    let mut derived = Vec::with_capacity(blocks * 32);
    for block in 1..=u32::try_from(blocks).unwrap_or(u32::MAX) {
        let mut message = salt.to_vec();
        message.extend_from_slice(&block.to_be_bytes());
        let mut u = mac(&message);
        let mut result = u;
        for _ in 1..iterations {
            u = mac(&u);
            for (out, byte) in result.iter_mut().zip(u) {
                *out ^= byte;
            }
        }
        derived.extend_from_slice(&result);
    }
    derived.truncate(key_length);
    derived
}

/// Bytes from the kernel's CSPRNG. The pool cannot run without one: a session
/// key or a redeem id from a weak source is worse than not starting.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut value = [0u8; N];
    let filled =
        std::fs::File::open("/dev/urandom").and_then(|mut source| source.read_exact(&mut value));
    if let Err(error) = filled {
        panic!("no random source: {error}");
    }
    value
}

/// A version 4 UUID. Upstream documents a UUID for the reset-credit redeem
/// request id, and an id it rejects would be indistinguishable from one it
/// never saw, so a retry could spend a second credit.
pub fn random_uuid() -> String {
    let mut value = random_bytes::<16>();
    value[6] = (value[6] & 0x0f) | 0x40;
    value[8] = (value[8] & 0x3f) | 0x80;
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        hex(&value[0..4]),
        hex(&value[4..6]),
        hex(&value[6..8]),
        hex(&value[8..10]),
        hex(&value[10..16])
    )
}

pub fn random_id() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<16>())
}

pub fn new_password_hash(password: &str) -> String {
    let salt = random_bytes::<16>();
    const ITERATIONS: u32 = 600_000;
    let derived = pbkdf2_sha256(password.as_bytes(), &salt, ITERATIONS, 32);
    format!(
        "pbkdf2-sha256:{ITERATIONS}:{}:{}",
        STANDARD_NO_PAD.encode(salt),
        STANDARD_NO_PAD.encode(derived)
    )
}

pub fn valid_password_hash(value: &str) -> bool {
    parse_password_hash(value).is_some()
}

pub fn verify_password_hash(encoded: &str, password: &str) -> bool {
    let Some((iterations, salt, expected)) = parse_password_hash(encoded) else {
        return false;
    };
    let actual = pbkdf2_sha256(password.as_bytes(), &salt, iterations, expected.len());
    constant_time_eq(&actual, &expected)
}

fn parse_password_hash(value: &str) -> Option<(u32, Vec<u8>, Vec<u8>)> {
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2-sha256" {
        return None;
    }
    let iterations: u32 = parts[1].parse().ok()?;
    if !(100_000..=5_000_000).contains(&iterations) {
        return None;
    }
    let salt = STANDARD_NO_PAD.decode(parts[2]).ok()?;
    let hash = STANDARD_NO_PAD.decode(parts[3]).ok()?;
    (salt.len() >= 16 && hash.len() == 32).then_some((iterations, salt, hash))
}

/// Go's subtle.ConstantTimeCompare: equal length and contents, in time that
/// does not depend on where they differ.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // RFC 4231 test case 2 and the PBKDF2-HMAC-SHA256 vector for
    // ("password", "salt", 1 and 4096 iterations).
    #[test]
    fn matches_the_published_vectors() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&pbkdf2_sha256(b"password", b"salt", 1, 32)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            hex(&pbkdf2_sha256(b"password", b"salt", 4096, 32)),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    // A hash Go's newPasswordHash wrote for "admin-password" still verifies.
    #[test]
    fn verifies_a_go_written_hash() {
        let salt = [7u8; 16];
        let derived = pbkdf2_sha256(b"admin-password", &salt, 100_000, 32);
        let encoded = format!(
            "pbkdf2-sha256:100000:{}:{}",
            STANDARD_NO_PAD.encode(salt),
            STANDARD_NO_PAD.encode(&derived)
        );
        assert!(valid_password_hash(&encoded));
        assert!(verify_password_hash(&encoded, "admin-password"));
        assert!(!verify_password_hash(&encoded, "wrong"));
        assert!(!valid_password_hash("pbkdf2-sha256:99:AAAA:AAAA"));
        let uuid = random_uuid();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert_eq!(random_id().len(), 22);
    }
}
