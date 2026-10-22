//! Request authentication for the pool's HTTP surface: the client API key and
//! the admin session.
//!
//! A port of requireAPIKey, requestAPIKey, signSession and validSession in
//! cmd/codex-pool/main.go. The route table and handlers come with the server
//! (step 4.5).

use super::app::App;
use super::crypto::constant_time_eq;
use super::crypto::hmac_sha256;
use super::time::Timestamp;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::HeaderMap;

/// A query parameter, decoded as Go's url.Values.Get does.
pub fn query_param(query: &str, name: &str) -> String {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

/// The client's API key: a Bearer Authorization, X-Goog-Api-Key, X-Api-Key,
/// or the key / auth_token query parameter, the first one present.
pub fn request_api_key(headers: &HeaderMap, query: &str) -> String {
    let header = |name: &str| super::http_io::header_value(headers, name).to_string();
    for value in [
        header("authorization"),
        header("x-goog-api-key"),
        header("x-api-key"),
        query_param(query, "key"),
        query_param(query, "auth_token"),
    ] {
        let value = match value.strip_prefix("Bearer ") {
            Some(rest) => rest.trim().to_string(),
            None => value,
        };
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

impl App {
    pub fn valid_api_key(&self, value: &str) -> bool {
        !value.is_empty()
            && self
                .settings
                .api_keys
                .iter()
                .any(|key| constant_time_eq(value.as_bytes(), key.as_bytes()))
    }

    pub fn sign_session(&self, user: &str, expires: Timestamp) -> String {
        let payload = format!("{user}|{}", expires.unix());
        let mac = hmac_sha256(&self.settings.session_key, payload.as_bytes());
        URL_SAFE_NO_PAD.encode(format!("{payload}|{}", URL_SAFE_NO_PAD.encode(mac)))
    }

    pub fn valid_session(&self, token: &str) -> bool {
        let Ok(decoded) = URL_SAFE_NO_PAD.decode(token) else {
            return false;
        };
        let decoded = String::from_utf8_lossy(&decoded).into_owned();
        let parts: Vec<&str> = decoded.split('|').collect();
        if parts.len() != 3 || parts[0] != self.settings.admin_user {
            return false;
        }
        let Ok(expiry) = parts[1].parse::<i64>() else {
            return false;
        };
        let expires = Timestamp::from_unix(expiry);
        if Timestamp::now().after(&expires) {
            return false;
        }
        constant_time_eq(
            token.as_bytes(),
            self.sign_session(parts[0], expires).as_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::testing::test_app;

    #[test]
    fn reads_the_api_key_as_go_does() {
        let mut headers = HeaderMap::new();
        assert_eq!(request_api_key(&headers, "key=from%20query"), "from query");
        headers.insert("x-api-key", http::HeaderValue::from_static("xkey"));
        assert_eq!(request_api_key(&headers, "key=q"), "xkey");
        headers.insert(
            "authorization",
            http::HeaderValue::from_static("Bearer  client-key "),
        );
        assert_eq!(request_api_key(&headers, ""), "client-key");
        let (app, _dir) = test_app(vec![]);
        assert!(app.valid_api_key("client-key"));
        assert!(!app.valid_api_key("other"));
        assert!(!app.valid_api_key(""));
        let token = app.sign_session("admin", Timestamp::now().add(super::super::time::hours(1)));
        assert!(app.valid_session(&token));
        assert!(!app.valid_session(
            &app.sign_session("admin", Timestamp::now().add(-super::super::time::hours(1)))
        ));
        assert!(!app.valid_session(&app.sign_session(
            "intruder",
            Timestamp::now().add(super::super::time::hours(1))
        )));
    }
}
