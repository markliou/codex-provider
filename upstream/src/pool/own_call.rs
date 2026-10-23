//! The pool's own upstream calls, composed the way a native Codex composes
//! them (RUST_PORT.md step 1, SPEC.md 2.3.2 own calls).
//!
//! The pool names a call and what it is sent with (the account's credential
//! and the agent of the Codex that last used it); this module writes the
//! request: method, headers in Codex's order, and body bytes. Each composition
//! follows the Codex source cited at it. Codex's own functions cannot be called
//! instead: backend-client builds its headers in a private `headers()` and
//! returns only its typed reading of the answer, which drops fields the pool
//! routes on (`limit_reached`, `code_review_rate_limit`, plan names it does not
//! know), and login's refresh lives in a private module. So the request is
//! written here from that source, the answer goes back to the pool untouched,
//! and the tests hold the composition to the vectors the Go pool produced
//! (upstream/testdata/own-call-vectors.json) and to the measured order.
//!
//! A request is returned in three parts, as codex-http-client assembles one:
//! the headers set before the body step, the JSON body (the body step adds its
//! `content-type`), and the headers a client adds after it (login's default
//! client adds `originator` and `user-agent` that way). The HTTP client adds
//! `accept: */*` last on its own.

use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

/// One of the pool's own calls, as the pool posts it to `/own`.
#[derive(Debug, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum OwnCall {
    /// backend-client rate_limit_resets.rs get_rate_limit_status (`/wham/usage`).
    Usage(BackendCall),
    /// backend-client client.rs get_accounts_check (`/wham/accounts/check`).
    AccountsCheck(BackendCall),
    /// backend-client rate_limit_resets.rs list_rate_limit_reset_credits.
    ResetCredits(BackendCall),
    /// backend-client rate_limit_resets.rs consume_rate_limit_reset_credit_request.
    ConsumeResetCredit {
        #[serde(flatten)]
        backend: BackendCall,
        redeem_request_id: String,
        #[serde(default)]
        credit_id: Option<String>,
    },
    /// login auth/manager.rs request_chatgpt_token_refresh.
    TokenRefresh(RefreshCall),
}

#[derive(Debug, Deserialize)]
pub struct BackendCall {
    pub url: String,
    pub user_agent: String,
    pub access_token: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub fedramp: bool,
}

#[derive(Debug, Deserialize)]
pub struct RefreshCall {
    pub url: String,
    pub user_agent: String,
    pub originator: String,
    pub client_id: String,
    pub refresh_token: String,
}

/// backend-client rate_limit_resets.rs ConsumeRateLimitResetCreditRequest:
/// serde writes the fields in declaration order, and leaves `credit_id` out
/// when none is named.
#[derive(Serialize)]
struct ConsumeRateLimitResetCreditRequest<'a> {
    redeem_request_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    credit_id: Option<&'a str>,
}

#[derive(Debug)]
pub struct Composed {
    pub method: Method,
    pub url: String,
    pub before_body: HeaderMap,
    /// JSON bytes; the body step adds `content-type: application/json`.
    pub json_body: Option<Vec<u8>>,
    pub after_body: HeaderMap,
}

pub fn compose(call: &OwnCall) -> Result<Composed, String> {
    match call {
        OwnCall::Usage(backend)
        | OwnCall::AccountsCheck(backend)
        | OwnCall::ResetCredits(backend) => Ok(Composed {
            method: Method::GET,
            url: backend.url.clone(),
            before_body: backend_headers(backend)?,
            json_body: None,
            after_body: HeaderMap::new(),
        }),
        // `.headers(self.headers())`, then `.header(CONTENT_TYPE, json)` and
        // `.json(&request)`: the body step's content-type follows the
        // backend-client headers.
        OwnCall::ConsumeResetCredit {
            backend,
            redeem_request_id,
            credit_id,
        } => {
            let body = serde_json::to_vec(&ConsumeRateLimitResetCreditRequest {
                redeem_request_id,
                credit_id: credit_id.as_deref().filter(|id| !id.is_empty()),
            })
            .map_err(|error| error.to_string())?;
            Ok(Composed {
                method: Method::POST,
                url: backend.url.clone(),
                before_body: backend_headers(backend)?,
                json_body: Some(body),
                after_body: HeaderMap::new(),
            })
        }
        // login oauth/client.rs refresh + exchange with TokenEncoding::Json:
        // the parameters collected into a BTreeMap (so sorted by key) and
        // `.header("Content-Type", "application/json").json(..)`, sent by
        // login's default client, whose default headers (originator, then
        // user-agent) come after the request's own.
        OwnCall::TokenRefresh(refresh) => {
            let parameters: BTreeMap<&str, &str> = [
                ("grant_type", "refresh_token"),
                ("client_id", refresh.client_id.as_str()),
                ("refresh_token", refresh.refresh_token.as_str()),
            ]
            .into_iter()
            .collect();
            let body = serde_json::to_vec(&parameters).map_err(|error| error.to_string())?;
            let mut after_body = HeaderMap::new();
            insert(&mut after_body, "originator", &refresh.originator)?;
            insert(&mut after_body, "user-agent", &refresh.user_agent)?;
            Ok(Composed {
                method: Method::POST,
                url: refresh.url.clone(),
                before_body: HeaderMap::new(),
                json_body: Some(body),
                after_body,
            })
        }
    }
}

/// backend-client client.rs `headers()`: the user agent (`with_user_agent`),
/// then the auth provider's headers (model-provider bearer_auth_provider.rs:
/// authorization, ChatGPT-Account-ID, X-OpenAI-Fedramp), which `headers()`
/// repeats for an account id and FedRAMP without moving them.
fn backend_headers(call: &BackendCall) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    insert(&mut headers, "user-agent", &call.user_agent)?;
    insert(
        &mut headers,
        "authorization",
        &format!("Bearer {}", call.access_token),
    )?;
    if let Some(account_id) = call.account_id.as_deref().filter(|id| !id.is_empty()) {
        insert(&mut headers, "chatgpt-account-id", account_id)?;
    }
    if call.fedramp {
        insert(&mut headers, "x-openai-fedramp", "true")?;
    }
    Ok(headers)
}

fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<(), String> {
    let value = HeaderValue::from_str(value).map_err(|_| format!("invalid {name} value"))?;
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[derive(Deserialize)]
    struct Vector {
        input: serde_json::Value,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<String>,
    }

    fn wire_headers(composed: &Composed) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut push = |map: &HeaderMap| {
            for (name, value) in map {
                headers.push((
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                ));
            }
        };
        push(&composed.before_body);
        let content_type = HeaderMap::from_iter([(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )]);
        if composed.json_body.is_some() {
            push(&content_type);
        }
        push(&composed.after_body);
        headers
    }

    // The Go side's composition (codexOwnCall.request, used without
    // codex-upstream) and this one write every call the same way.
    #[test]
    fn composes_every_call_as_the_go_side_does() {
        let vectors: Vec<Vector> =
            serde_json::from_str(include_str!("../../testdata/own-call-vectors.json"))
                .unwrap_or_else(|error| panic!("own-call-vectors.json: {error}"));
        assert!(vectors.len() >= 7);
        for vector in vectors {
            let call: OwnCall = serde_json::from_value(vector.input.clone())
                .unwrap_or_else(|error| panic!("{}: {error}", vector.input));
            let composed = compose(&call).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(composed.method.as_str(), vector.method, "{}", vector.input);
            assert_eq!(composed.url, vector.url);
            assert_eq!(wire_headers(&composed), vector.headers, "{}", vector.input);
            let body = composed
                .json_body
                .as_ref()
                .map(|body| String::from_utf8_lossy(body).into_owned());
            assert_eq!(body, vector.body, "{}", vector.input);
        }
    }

    // The orders measured from a native Codex 0.163.0 (recorded in the Go
    // pool's codexBackendHeaderOrder and codexTokenRefreshHeaderOrder).
    #[test]
    fn keeps_the_measured_orders() {
        let call: OwnCall = serde_json::from_value(serde_json::json!({
            "call": "consume_reset_credit",
            "url": "https://chatgpt.example.test/backend-api/wham/rate-limit-reset-credits/consume",
            "user_agent": "codex_exec/0.163.0",
            "access_token": "replace-access",
            "account_id": "fake-account",
            "fedramp": true,
            "redeem_request_id": "fake-redeem",
        }))
        .unwrap_or_else(|error| panic!("{error}"));
        let composed = compose(&call).unwrap_or_else(|error| panic!("{error}"));
        let names: Vec<String> = wire_headers(&composed)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "user-agent",
                "authorization",
                "chatgpt-account-id",
                "x-openai-fedramp",
                "content-type"
            ]
        );
        assert_eq!(
            composed.json_body.as_deref(),
            Some(br#"{"redeem_request_id":"fake-redeem"}"#.as_slice())
        );
        // Go TestConsumeResetCreditBodyIsCodexs: Codex's
        // ConsumeRateLimitResetCreditRequest field order, with credit_id
        // after the request id when a credit is named.
        let named: OwnCall = serde_json::from_value(serde_json::json!({
            "call": "consume_reset_credit",
            "url": "https://chatgpt.example.test/backend-api/wham/rate-limit-reset-credits/consume",
            "user_agent": "codex_exec/0.163.0",
            "access_token": "replace-access",
            "redeem_request_id": "key",
            "credit_id": "credit",
        }))
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            compose(&named)
                .unwrap_or_else(|error| panic!("{error}"))
                .json_body
                .as_deref(),
            Some(br#"{"redeem_request_id":"key","credit_id":"credit"}"#.as_slice())
        );

        let call: OwnCall = serde_json::from_value(serde_json::json!({
            "call": "token_refresh",
            "url": "https://auth.example.test/oauth/token",
            "user_agent": "codex_exec/0.163.0",
            "originator": "codex_exec",
            "client_id": "fake-client",
            "refresh_token": "replace-refresh",
        }))
        .unwrap_or_else(|error| panic!("{error}"));
        let composed = compose(&call).unwrap_or_else(|error| panic!("{error}"));
        let names: Vec<String> = wire_headers(&composed)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["content-type", "originator", "user-agent"]);
    }

    #[test]
    fn refuses_an_unknown_call() {
        let unknown = serde_json::from_value::<OwnCall>(serde_json::json!({
            "call": "subscriptions",
            "url": "https://chatgpt.example.test/backend-api/subscriptions",
        }));
        assert!(unknown.is_err());
    }
}
