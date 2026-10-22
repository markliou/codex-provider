//! Codex's verdict on one upstream failure, read with Codex's own code.
//!
//! The pool owns this file; the reading itself is the vendored codex-api code
//! (`sse/`, `endpoint/responses_websocket.rs`, `api_bridge.rs`), called the way
//! Codex calls it, and the verdict is what a native Codex concludes: the error
//! it maps the failure to (`map_api_error`), the category it reports
//! (`CodexErr::to_codex_protocol_error`), and whether it would send the same
//! request again (`CodexErr::retry_delay`). Codex has one account, so the
//! verdict says nothing about another account; which account the pool tries
//! next, and how long it rests one, stay the pool's own decision.
//!
//! Each entry point mirrors one Codex read path; keep them in step with the
//! vendored files rather than with the pool's classification table, since
//! the point is to learn where the two disagree.

use crate::api_bridge::map_api_error;
use crate::error::ApiError;
use crate::sse::ResponsesStreamEvent;
use crate::sse::process_responses_event;
use codex_client::TransportError;
use codex_http_client::RetryAfter;
use codex_protocol::error::CodexErrKind;
use http::HeaderMap;
use http::StatusCode;
use serde::Serialize;

/// What a native Codex makes of one failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    /// Codex acts on this message itself. When false, Codex skips it and the
    /// stream's end decides; the rest of the verdict is then what Codex
    /// reports when the stream closes without a completed response.
    pub acted: bool,
    /// codex-api's reading of the failure (`ApiError` variant).
    pub api: String,
    /// Codex's error kind (`CodexErrKind`, snake_case).
    pub kind: &'static str,
    /// The category Codex reports to its client (`CodexErrorInfo`).
    pub info: String,
    /// A native Codex would send the same request again.
    pub retryable: bool,
    /// Upstream's own retry advice, when it gave one.
    pub retry_after_ms: Option<u64>,
}

fn variant_name(debug: String) -> String {
    debug
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_string()
}

fn verdict(error: ApiError, acted: bool) -> Verdict {
    let api = variant_name(format!("{error:?}"));
    let error = map_api_error(error);
    let kind: &'static str = CodexErrKind::from(error.details()).into();
    let info = variant_name(format!("{:?}", error.to_codex_protocol_error()));
    Verdict {
        acted,
        api,
        kind,
        info,
        // codex-protocol error.rs retry_delay: None means terminal. Attempt one
        // is the first retry.
        retryable: error.retry_delay(/*retry_count*/ 1).is_some(),
        retry_after_ms: error
            .server_retry_delay()
            .map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
    }
}

/// One SSE event's data, read as codex-api sse/responses.rs
/// process_sse_with_treatment reads it: an event that does not parse is
/// skipped; an error from process_responses_event is the stream's failure
/// (FlexUnavailable and ResponseIncomplete at once, any other when the stream
/// closes); anything else is no failure, and a stream that then closes without
/// response.completed fails with Codex's "stream closed" error.
pub fn sse_event(data: &str) -> Verdict {
    let closed = || ApiError::Stream("stream closed before response.completed".into());
    match serde_json::from_str::<ResponsesStreamEvent>(data) {
        Ok(event) => match process_responses_event(event) {
            Err(error) => verdict(error.into_api_error(), /*acted*/ true),
            Ok(_) => verdict(closed(), /*acted*/ false),
        },
        Err(_) => verdict(closed(), /*acted*/ false),
    }
}

/// One Responses WebSocket text message, read as codex-api
/// endpoint/responses_websocket.rs run_websocket_response_stream reads it: a
/// wrapped error first, then the event itself through process_responses_event;
/// a message that is neither leaves the socket to end the response, which
/// Codex reports as closed before response.completed.
pub fn ws_message(text: &str) -> Verdict {
    if let Some(error) = crate::endpoint::responses_websocket::pool_wrapped_error(text) {
        return verdict(error, /*acted*/ true);
    }
    let closed = || ApiError::Stream("websocket closed by server before response.completed".into());
    match serde_json::from_str::<ResponsesStreamEvent>(text) {
        Ok(event) => match process_responses_event(event) {
            Err(error) => verdict(error.into_api_error(), /*acted*/ true),
            Ok(_) => verdict(closed(), /*acted*/ false),
        },
        Err(_) => verdict(closed(), /*acted*/ false),
    }
}

/// A non-success HTTP answer, as codex-http-client transport.rs hands it on:
/// TransportError::Http with the response headers, its text body and the
/// retry advice those headers carry.
pub fn http_response(status: StatusCode, headers: HeaderMap, body: Option<String>) -> Verdict {
    let retry_after = RetryAfter::from_headers(&headers);
    verdict(
        ApiError::Transport(TransportError::Http {
            status,
            url: None,
            headers: Some(headers),
            body,
            retry_after,
        }),
        /*acted*/ true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn camel(snake: &str) -> String {
        snake
            .split('_')
            .map(|word| {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                    .unwrap_or_default()
            })
            .collect()
    }

    // Every failure shape the pool's Go classification is tested with
    // (cmd/codex-pool/testdata/codex-upstream-failures.json), read by Codex's
    // code along the path its source file names. The fixture's `codex` column
    // was written by hand from Codex's source; this holds it to what that
    // source concludes, by ApiError variant or by the CodexErr kind it maps to.
    // It caught "stream overload": Codex's own malformed-failure test shape,
    // which Codex reads as a stream error, not as an overload.
    #[test]
    fn fixture_shapes_read_as_their_codex_column_says() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../cmd/codex-pool/testdata/codex-upstream-failures.json"
        ))
        .expect("fixture");
        let mut mismatches = Vec::new();
        for failure in fixture["failures"].as_array().expect("failures") {
            let source = failure["source"].as_str().unwrap_or_default();
            let event = &failure["event"];
            let verdict = if source.ends_with("responses_websocket.rs") {
                ws_message(&event.to_string())
            } else if source.ends_with("api_bridge.rs") {
                // Codex reads this one from an HTTP 429 body; the fixture holds
                // the body inside an in-stream shape for the Go side.
                http_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    HeaderMap::new(),
                    Some(event["response"].to_string()),
                )
            } else {
                sse_event(&event.to_string())
            };
            let expected = failure["codex"].as_str().unwrap_or_default();
            let expected = expected
                .split(|c: char| !c.is_ascii_alphanumeric())
                .next()
                .unwrap_or_default();
            if expected != verdict.api && expected != camel(verdict.kind) {
                mismatches.push(format!(
                    "{}: fixture says {expected}, Codex reads api={} kind={}",
                    failure["name"], verdict.api, verdict.kind
                ));
            }
        }
        assert_eq!(mismatches, Vec::<String>::new());
    }

    #[test]
    fn spent_allowance_on_http_is_terminal() {
        let body = r#"{"error":{"type":"usage_limit_reached","message":"limit","plan_type":"plus","resets_at":1893456000}}"#;
        let verdict = http_response(
            StatusCode::TOO_MANY_REQUESTS,
            HeaderMap::new(),
            Some(body.to_string()),
        );
        assert_eq!(verdict.kind, "usage_limit_reached");
        assert!(verdict.acted);
        assert!(!verdict.retryable);
    }

    #[test]
    fn server_error_on_http_is_retried() {
        let verdict = http_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            HeaderMap::new(),
            Some(r#"{"error":{"type":"server_error"}}"#.to_string()),
        );
        assert!(verdict.retryable);
    }

    #[test]
    fn failed_response_reports_its_code() {
        let data = r#"{"type":"response.failed","response":{"id":"resp_1","error":{"code":"context_length_exceeded","message":"too long"}}}"#;
        let verdict = sse_event(data);
        assert!(verdict.acted);
        assert_eq!(verdict.kind, "context_window_exceeded");
        assert!(!verdict.retryable);
    }

    #[test]
    fn sse_error_event_is_left_to_the_stream_end() {
        let data = r#"{"type":"error","code":"server_error","message":"boom"}"#;
        let verdict = sse_event(data);
        assert!(!verdict.acted);
        assert_eq!(verdict.kind, "stream");
        assert!(verdict.retryable);
    }

    #[test]
    fn wrapped_websocket_refusal_is_read_by_status() {
        let text = r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"limit"}}"#;
        let verdict = ws_message(text);
        assert!(verdict.acted);
        assert_eq!(verdict.kind, "usage_limit_reached");
        assert!(!verdict.retryable);
    }

    #[test]
    fn websocket_connection_limit_is_retried() {
        let text = r#"{"type":"error","error":{"code":"websocket_connection_limit_reached"}}"#;
        let verdict = ws_message(text);
        assert!(verdict.acted);
        assert!(verdict.retryable);
    }
}
