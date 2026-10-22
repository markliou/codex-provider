//! Each upstream account presents one fixed Codex installation id.
//!
//! A port of cmd/codex-pool/installation_id.go; keep the two in step until the
//! Go forwarder is retired. A native Codex keeps a random UUID in
//! CODEX_HOME/installation_id and sends it with every Responses request, as
//! `client_metadata["x-codex-installation-id"]` and as `installation_id` in
//! the `x-codex-turn-metadata` JSON, in the body's client_metadata and in the
//! header of that name (core responses_metadata.rs `client_metadata`,
//! `compatibility_headers`). Relayed as written, one client's id reached every
//! account it was placed on, so each account presents its own instead, kept in
//! the account's CODEX_HOME and made the way Codex makes it (core
//! installation_id.rs `resolve_installation_id`). Only the id's occurrences
//! inside client_metadata and that header change; every other byte, the
//! conversation included, is forwarded as written (SPEC.md 2.3.2).
//!
//! The UUID generator is the caller's, so this needs no dependency.

use serde::Deserialize;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::io;
use std::path::Path;

pub const FILE_NAME: &str = "installation_id";

/// The account's installation id from `codex_home`, made with `new_v4` (a
/// lowercase hyphenated v4 UUID) and written with mode 0644 when there is no
/// valid one yet.
pub fn resolve(codex_home: &Path, new_v4: impl FnOnce() -> String) -> io::Result<String> {
    let path = codex_home.join(FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            if let Some(id) = canonical_uuid(contents.trim()) {
                return Ok(id);
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let id = new_v4();
    std::fs::create_dir_all(codex_home)?;
    let temp = codex_home.join(format!("{FILE_NAME}.tmp"));
    std::fs::write(&temp, &id)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::rename(&temp, &path)?;
    Ok(id)
}

pub fn canonical_uuid(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    for (i, b) in bytes.iter().enumerate() {
        let ok = match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        };
        if !ok {
            return None;
        }
    }
    Some(value.to_ascii_lowercase())
}

#[derive(Deserialize)]
struct Top<'a> {
    #[serde(borrow, default)]
    client_metadata: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "x-codex-installation-id", default)]
    installation_id: Option<String>,
    #[serde(rename = "x-codex-turn-metadata", default)]
    turn_metadata: Option<String>,
}

#[derive(Deserialize)]
struct TurnMetadata {
    #[serde(default)]
    installation_id: Option<String>,
}

fn turn_metadata_installation_id(value: &str) -> Option<String> {
    serde_json::from_str::<TurnMetadata>(value)
        .ok()?
        .installation_id
        .filter(|id| !id.is_empty())
}

/// `body` with the installation id in its top-level client_metadata replaced
/// by `id`; bytes outside client_metadata are kept as they are.
pub fn with_installation_id<'a>(body: &'a str, id: &str) -> Cow<'a, str> {
    let Ok(top) = serde_json::from_str::<Top<'_>>(body) else {
        return Cow::Borrowed(body);
    };
    let Some(raw) = top.client_metadata else {
        return Cow::Borrowed(body);
    };
    let span = raw.get();
    if !span.starts_with('{') {
        return Cow::Borrowed(body);
    }
    let Ok(metadata) = serde_json::from_str::<Metadata>(span) else {
        return Cow::Borrowed(body);
    };
    let client = metadata
        .installation_id
        .filter(|id| !id.is_empty())
        .or_else(|| {
            metadata
                .turn_metadata
                .as_deref()
                .and_then(turn_metadata_installation_id)
        });
    let Some(client) = client.filter(|client| client != id) else {
        return Cow::Borrowed(body);
    };
    // The raw value borrows from `body`, so its offset is where it starts.
    let start = span.as_ptr() as usize - body.as_ptr() as usize;
    let end = start + span.len();
    Cow::Owned(format!(
        "{}{}{}",
        &body[..start],
        span.replace(&client, id),
        &body[end..]
    ))
}

/// An `x-codex-turn-metadata` value with its installation_id replaced by `id`.
pub fn turn_metadata_with_installation_id<'a>(value: &'a str, id: &str) -> Cow<'a, str> {
    match turn_metadata_installation_id(value) {
        Some(client) if client != id => Cow::Owned(value.replace(&client, id)),
        _ => Cow::Borrowed(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go installation_id_test.go clientInstallationID, clientTurnMetadata and
    // clientMetadataBody.
    const CLIENT: &str = "11111111-1111-4111-8111-111111111111";
    const ACCOUNT: &str = "33333333-3333-4333-8333-333333333333";

    fn turn_metadata() -> String {
        format!(
            r#"{{"installation_id":"{CLIENT}","session_id":"01a15f63-427f-7fa2-884a-01ebe2c60a47","thread_id":"01a15f63-427f-7fa2-884a-01ebe2c60a47","request_kind":"turn"}}"#
        )
    }

    fn body() -> String {
        let escaped = turn_metadata().replace('"', r#"\""#);
        format!(
            r#"{{"model":"gpt-test","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"my id is {CLIENT}"}}]}}],"stream":true,"store":false,"client_metadata":{{"x-codex-window-id":"01a15f63-427f-7fa2-884a-01ebe2c60a47:0","x-codex-installation-id":"{CLIENT}","x-codex-turn-metadata":"{escaped}"}},"prompt_cache_key":"thread-native"}}"#
        )
    }

    // Go TestWithInstallationIDChangesOnlyClientMetadata.
    #[test]
    fn changes_only_client_metadata() {
        let body = body();
        let got = with_installation_id(&body, ACCOUNT);
        let start = body.find(r#""client_metadata":"#).unwrap();
        let end = body.find(r#","prompt_cache_key""#).unwrap();
        let want = format!(
            "{}{}{}",
            &body[..start],
            body[start..end].replace(CLIENT, ACCOUNT),
            &body[end..]
        );
        assert_eq!(got, want);
        assert!(got.contains(&format!("my id is {CLIENT}")));
        assert_eq!(got.matches(ACCOUNT).count(), 2);
        for unchanged in [
            r#"{"model":"gpt-test","input":[]}"#,
            r#"{"model":"gpt-test","client_metadata":{"session_id":"s"}}"#,
            "not json",
        ] {
            assert_eq!(with_installation_id(unchanged, ACCOUNT), unchanged);
        }
        assert_eq!(
            turn_metadata_with_installation_id(&turn_metadata(), ACCOUNT),
            turn_metadata().replace(CLIENT, ACCOUNT)
        );
        let plain = r#"{"thread_id":"t"}"#;
        assert_eq!(turn_metadata_with_installation_id(plain, ACCOUNT), plain);
    }

    // Go TestAccountInstallationIDIsMadeOnceAsCodexMakesIt.
    #[test]
    fn is_made_once_as_codex_makes_it() {
        let home =
            std::env::temp_dir().join(format!("installation-id-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let first = resolve(&home, || ACCOUNT.to_string()).unwrap();
        assert_eq!(first, ACCOUNT);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o644);
        }
        let again = resolve(&home, || unreachable!("an existing id is kept")).unwrap();
        assert_eq!(again, first);
        std::fs::write(
            home.join(FILE_NAME),
            " 22222222-2222-4222-8222-22222222222A\n",
        )
        .unwrap();
        assert_eq!(
            resolve(&home, || unreachable!()).unwrap(),
            "22222222-2222-4222-8222-22222222222a"
        );
        std::fs::write(home.join(FILE_NAME), "not-an-id").unwrap();
        assert_eq!(resolve(&home, || CLIENT.to_string()).unwrap(), CLIENT);
        let _ = std::fs::remove_dir_all(&home);
    }
}
