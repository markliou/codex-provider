//! Hashes used by routing and bounded operational identifiers.
//!
//! The formulas mirror `cmd/codex-pool/main.go`: SHA-256 is truncated only for
//! display/cache identifiers, while placement consumes the first 53 bits of
//! the full digest. Keep the NUL separator and base64 alphabet unchanged;
//! changing either silently moves existing sticky sessions.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

/// Return the raw SHA-256 digest used by `stickyBalanceScore`.
pub fn sticky_balance_digest(sticky_key: &str, upstream_identity: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(sticky_key.as_bytes());
    hasher.update([0]);
    hasher.update(upstream_identity.as_bytes());
    hasher.finalize().into()
}

/// Weighted rendezvous score used by `placementScoreLocked`.
pub fn placement_score(sticky_key: &str, upstream_identity: &str, weight: f64) -> f64 {
    let digest = sticky_balance_digest(sticky_key, upstream_identity);
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let sample = u64::from_be_bytes(bytes) >> 11;
    let u = (sample as f64 + 0.5) / (1_u64 << 53) as f64;
    weight / -go_natural_log(u)
}

// Go's math.Log on amd64 uses this FreeBSD/SunPro FDLIBM polynomial (the
// Go source is src/math/log_amd64.s). Rust's intrinsic ln can differ by one
// ulp, which changes rendezvous ordering, so keep the measured operation order
// here for bit-for-bit placement compatibility; the 1,024 placement vectors
// compare the score's bits. Only the placement draw calls it, with x in
// (0, 1), so zero, negatives, infinities and NaN are not handled. The vectors
// come from the amd64 build the pool runs on; Go elsewhere uses math/log.go,
// whose operation order is not checked here.
fn go_natural_log(x: f64) -> f64 {
    const SQRT2_OVER_2: f64 = f64::from_bits(0x3fe6_a09e_667f_3bcd);
    const LN2_HI: f64 = f64::from_bits(0x3fe6_2e42_fee0_0000);
    const LN2_LO: f64 = f64::from_bits(0x3dea_39ef_3579_3c76);
    const L1: f64 = f64::from_bits(0x3fe5_5555_5555_5593);
    const L2: f64 = f64::from_bits(0x3fd9_9999_9997_fa04);
    const L3: f64 = f64::from_bits(0x3fd2_4924_9422_9359);
    const L4: f64 = f64::from_bits(0x3fcc_71c5_1d8e_78af);
    const L5: f64 = f64::from_bits(0x3fc7_4664_96cb_03de);
    const L6: f64 = f64::from_bits(0x3fc3_9a09_d078_c69f);
    const L7: f64 = f64::from_bits(0x3fc2_f112_df3e_5244);

    let bits = x.to_bits();
    let mut f1 = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3fe0_0000_0000_0000);
    let mut ki = ((bits >> 52) & 0x7ff) as i32 - 0x3fe;
    if f1 < SQRT2_OVER_2 {
        f1 *= 2.0;
        ki -= 1;
    }
    let f = f1 - 1.0;
    let k = ki as f64;
    let s = f / (2.0 + f);
    let s2 = s * s;
    let s4 = s2 * s2;
    let t1 = s2 * (L1 + s4 * (L3 + s4 * (L5 + s4 * L7)));
    let t2 = s4 * (L2 + s4 * (L4 + s4 * L6));
    let r = t1 + t2;
    let hfsq = 0.5 * f * f;
    k * LN2_HI - ((hfsq - (s * (hfsq + r) + k * LN2_LO)) - f)
}

/// Go's `shortHash`: first 16 SHA-256 bytes, raw URL-safe base64 without `=`.
pub fn short_hash(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    URL_SAFE_NO_PAD.encode(&digest[..16])
}

/// Hash an operational identifier after applying Go's Unicode whitespace trim.
/// An empty value returns Go's empty string rather than a sentinel hash.
pub fn operational_identifier_hash(kind: &str, value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    let mut material = Vec::with_capacity(kind.len() + 1 + value.len());
    material.extend_from_slice(kind.as_bytes());
    material.push(0);
    material.extend_from_slice(value.as_bytes());
    short_hash(&material)
}

/// Hash the model/source/value material used for an upstream cache key.
pub fn prompt_cache_key_hash(model: &str, source: &str, value: &str) -> String {
    let mut material = String::with_capacity(model.len() + source.len() + value.len() + 2);
    material.push_str(model);
    material.push(':');
    material.push_str(source);
    material.push(':');
    material.push_str(value);
    format!("cp_{}", short_hash(material.as_bytes()))
}

/// API-key fingerprint, retaining the distinction between empty and non-empty
/// input exactly as Go does (only the all-whitespace case becomes anonymous).
pub fn api_key_fingerprint(value: &str) -> String {
    if value.trim().is_empty() {
        "anonymous".to_owned()
    } else {
        short_hash(value.as_bytes())
    }
}

/// Bucket index used by prompt-cache grouping.
pub fn prompt_cache_bucket_index(sticky_key: &str, buckets: i32) -> i32 {
    if buckets <= 1 {
        return 0;
    }
    let digest = Sha256::digest(sticky_key.as_bytes());
    i32::from(digest[0]) % buckets
}

/// Resolve the same upstream member identity as Go's
/// `upstreamIdentityKeyLocked`.  Authentication files are intentionally not
/// consulted here; step 4 passes only the sanitized account metadata across
/// the Rust boundary, and the Go vectors cover the no-auth-file fallback.
pub fn upstream_identity_key(
    auth_type: &str,
    account_id: &str,
    email: &str,
    organization_name: &str,
) -> String {
    if auth_type != "codex_device_auth" {
        return String::new();
    }
    let account_id = account_id.trim();
    if !account_id.is_empty() {
        let mut result = format!("codex-account:{account_id}");
        let email = normalize_email(email);
        if !email.is_empty() {
            result.push_str("|member:");
            result.push_str(&email);
        }
        return result;
    }
    let email = normalize_email(email);
    let organization_name = clean_organization_name(organization_name);
    if !email.is_empty() || !organization_name.is_empty() {
        return format!("codex-profile:{email}|{organization_name}");
    }
    String::new()
}

// Go's normalizeEmail: strings.ToLower maps rune by rune with Unicode's
// simple case mapping. str::to_lowercase applies the full mapping instead,
// which turns a dotted capital I into two characters and a word-final sigma
// into ς; either would give the same member a second identity, so lower-case
// per character and take the simple mapping's single result.
fn normalize_email(value: &str) -> String {
    value.trim().chars().map(simple_lowercase).collect()
}

fn simple_lowercase(character: char) -> char {
    // U+0130 is the one character whose full lower-case mapping is longer than
    // one character; its simple mapping is a plain i.
    if character == '\u{130}' {
        return 'i';
    }
    let mut lower = character.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => character,
    }
}

// Go's cleanOrganizationName, step for step: drop control characters, split
// on Unicode whitespace, join with one space, then cut at 120 characters.
// The cut comes after the join, so a name can end in the joining space; a
// version that stops while building the name gets that edge wrong.
fn clean_organization_name(value: &str) -> String {
    let without_controls: String = value.trim().chars().filter(|c| !c.is_control()).collect();
    let joined = without_controls
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    joined.chars().take(120).collect()
}

/// Apply the slot fallback used by `stickyBalanceScore` when no upstream
/// identity is known.
pub fn sticky_identity_or_slot(identity: &str, slot_id: &str) -> String {
    if identity.is_empty() {
        format!("slot:{slot_id}")
    } else {
        identity.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn hex_encode(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut result = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            result.push(HEX[(byte >> 4) as usize] as char);
            result.push(HEX[(byte & 0x0f) as usize] as char);
        }
        result
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct HashVector {
        kind: String,
        #[serde(default)]
        input: String,
        #[serde(default)]
        kind_input: String,
        #[serde(default)]
        value: String,
        #[serde(default)]
        model: String,
        #[serde(default)]
        source: String,
        #[serde(default)]
        sticky_key: String,
        #[serde(default)]
        identity: String,
        #[serde(default)]
        weight_bits: u64,
        #[serde(default)]
        buckets: i32,
        #[serde(default)]
        digest_hex: String,
        #[serde(default)]
        output: String,
        #[serde(default)]
        bucket: i32,
        #[serde(default)]
        score_bits: u64,
        #[serde(default)]
        slot_id: String,
        #[serde(default)]
        account_id: String,
        #[serde(default)]
        email: String,
        #[serde(default)]
        organization_name: String,
        #[serde(default)]
        auth_type: String,
    }

    fn vectors() -> Vec<HashVector> {
        serde_json::from_str(include_str!(
            "../../../cmd/codex-pool/testdata/hash-vectors.json"
        ))
        .expect("Go hash vectors must be valid JSON")
    }

    #[test]
    fn matches_go_hash_vectors() {
        for vector in vectors() {
            match vector.kind.as_str() {
                "short_hash" => assert_eq!(short_hash(vector.input.as_bytes()), vector.output),
                "operational_identifier_hash" => assert_eq!(
                    operational_identifier_hash(&vector.kind_input, &vector.value),
                    vector.output,
                ),
                "prompt_cache_key_hash" => assert_eq!(
                    prompt_cache_key_hash(&vector.model, &vector.source, &vector.value),
                    vector.output,
                ),
                "api_key_fingerprint" => {
                    assert_eq!(api_key_fingerprint(&vector.input), vector.output)
                }
                "prompt_cache_bucket" => assert_eq!(
                    prompt_cache_bucket_index(&vector.input, vector.buckets),
                    vector.bucket,
                ),
                "sticky_balance_score" => assert_eq!(
                    hex_encode(&sticky_balance_digest(
                        &vector.sticky_key,
                        &sticky_identity_or_slot(&vector.identity, &vector.slot_id),
                    )),
                    vector.digest_hex,
                ),
                "placement_score" => {
                    // The identity is derived here from the account fields,
                    // not taken from the vector, so the derivation is checked
                    // on every placement input too.
                    let identity = upstream_identity_key(
                        &vector.auth_type,
                        &vector.account_id,
                        &vector.email,
                        &vector.organization_name,
                    );
                    assert_eq!(identity, vector.identity, "identity of {}", vector.slot_id);
                    assert_eq!(
                        placement_score(
                            &vector.sticky_key,
                            &sticky_identity_or_slot(&identity, &vector.slot_id),
                            f64::from_bits(vector.weight_bits),
                        )
                        .to_bits(),
                        vector.score_bits,
                        "placement vector {} identity={} weight_bits={:016x}",
                        vector.slot_id,
                        identity,
                        vector.weight_bits,
                    );
                }
                "upstream_identity" => assert_eq!(
                    upstream_identity_key(
                        &vector.auth_type,
                        &vector.account_id,
                        &vector.email,
                        &vector.organization_name,
                    ),
                    vector.output,
                ),
                other => panic!("unknown hash vector kind {other}"),
            }
        }
    }
}
