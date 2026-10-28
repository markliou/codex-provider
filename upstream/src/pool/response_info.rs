//! What the pool reads from an upstream answer: the response id, token usage,
//! and how a stream ended (its terminal event and failure class).
//!
//! A port of proxyResponseInfo, responseInfoFromSSEBlock and
//! terminalFailureClass in the Go pool's main.go. The failure classes decide
//! retry, failover and cooldown, so the code table keeps Go's comments on why
//! each code sits where it does.

use super::plan::claim_string;
use super::plan::clean_metadata_text;
use super::plan::clean_metadata_token;
use super::quota::sanitized_error_code;
use super::time::Timestamp;
use serde_json::Map;
use serde_json::Value;

/// The terminal failure handed to Codex's own classifier is bounded (Go
/// codexVerdictDataLimit).
pub const VERDICT_DATA_LIMIT: usize = 64 << 10;
/// OpenAI's minimum prompt size for caching to engage; requests below it can
/// never cache, so they are excluded from cold-start accounting.
pub const PROMPT_CACHE_MIN_TOKENS: u64 = 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptCacheUsage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: u64,
    pub output_present: bool,
    pub present: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ProxyResponseInfo {
    pub response_id: String,
    pub usage: PromptCacheUsage,
    pub completed_at: Timestamp,
    pub terminal_event: String,
    pub terminal_failure_class: String,
    pub terminal_error_code: String,
    /// Upstream's own explanation, bounded and stripped of control
    /// characters. It exists for the service log and nothing else: it must
    /// never enter runtime state, a routing event, or an admin response.
    pub terminal_error_message: String,
    /// The terminal failure event exactly as upstream sent it, and the stream
    /// it came on ("ws" for the Responses WebSocket, empty for SSE). They exist
    /// only to ask Codex's own code what it makes of the failure; like the
    /// message they never enter state, an event or a response.
    pub terminal_data: String,
    pub terminal_source: String,
    /// Why upstream ended a response.incomplete early
    /// (response.incomplete_details.reason, read where Codex reads it:
    /// codex-api sse/responses.rs at the pinned release). Without it the log
    /// said only that a turn stopped short, and a run of them could not be
    /// told apart as a content filter, an output cap or a context limit. Log
    /// only, like the message: it never decides a class, a retry or a route.
    pub terminal_incomplete_reason: String,
    pub request_id: String,
    pub failover_from_account_id: String,
    pub failover_outcome: String,
    /// Why a terminal failure could not be retried. A capacity failure is
    /// retryable only while the lifecycle-only preamble is still buffered, so
    /// these record whether that window had already closed and what closed
    /// it. Streaming responses only.
    pub precommit_committed: bool,
    pub precommit_close_reason: String,
    /// How much the window held when it closed. Without them a bounds closure
    /// cannot be told apart from a bound set too low, nor the block bound from
    /// the byte bound.
    pub precommit_blocks: i64,
    pub precommit_bytes: i64,
    /// How long the client waited for the first byte (headers included)
    /// because of the pre-commit window.
    pub precommit_hold_ms: i64,
}

impl ProxyResponseInfo {
    pub fn merge(&mut self, mut next: ProxyResponseInfo) {
        if !next.response_id.is_empty() {
            self.response_id = next.response_id;
        }
        if next.usage.present {
            // A streaming gateway may report cache-write usage on an
            // intermediate event and omit the field from the final summary.
            // Once observed, retain that explicit value; absence in a later
            // event must not turn it into a confirmed zero or erase it.
            if next.usage.cache_write_tokens.is_none() && self.usage.cache_write_tokens.is_some() {
                next.usage.cache_write_tokens = self.usage.cache_write_tokens;
            }
            if !next.usage.output_present && self.usage.output_present {
                next.usage.output_tokens = self.usage.output_tokens;
                next.usage.output_present = true;
            }
            self.usage = next.usage;
        }
        if !next.completed_at.is_zero() {
            self.completed_at = next.completed_at;
        }
        if !next.terminal_event.is_empty() {
            self.terminal_event = next.terminal_event;
            self.terminal_failure_class = next.terminal_failure_class;
            self.terminal_error_code = next.terminal_error_code;
            self.terminal_error_message = next.terminal_error_message;
            self.terminal_data = next.terminal_data;
            self.terminal_source = next.terminal_source;
            self.terminal_incomplete_reason = next.terminal_incomplete_reason;
        }
    }
}

pub fn bounded_verdict_data(data: &str) -> String {
    if data.len() <= VERDICT_DATA_LIMIT {
        return data.to_string();
    }
    let mut cut = VERDICT_DATA_LIMIT;
    while !data.is_char_boundary(cut) {
        cut -= 1;
    }
    data[..cut].to_string()
}

/// The SSE block's event name and its data lines joined.
pub fn sse_block_event_and_data(block: &str) -> (String, String) {
    let mut event_type = String::new();
    let mut data_lines: Vec<&str> = Vec::new();
    for line in block.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = clean_metadata_token(rest.trim());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim());
        }
    }
    (event_type, data_lines.join("\n"))
}

fn json_object(data: &str) -> Option<Map<String, Value>> {
    serde_json::from_str(data).ok()
}

pub fn response_event_type_from_sse_block(block: &str) -> String {
    let (event_type, data) = sse_block_event_and_data(block);
    if data.is_empty() || data == "[DONE]" {
        return event_type;
    }
    let Some(payload) = json_object(&data) else {
        return event_type;
    };
    let payload_type = clean_metadata_token(claim_string(&payload, "type"));
    if payload_type.is_empty() {
        event_type
    } else {
        payload_type
    }
}

pub fn response_info_from_sse_block(block: &str) -> ProxyResponseInfo {
    let (mut event_type, data) = sse_block_event_and_data(block);
    let bare_terminal = |event_type: &str| {
        if is_terminal_response_event(event_type) && event_type != "response.completed" {
            ProxyResponseInfo {
                terminal_event: event_type.to_string(),
                terminal_failure_class: terminal_failure_class(event_type, "").to_string(),
                ..ProxyResponseInfo::default()
            }
        } else {
            ProxyResponseInfo::default()
        }
    };
    if data.is_empty() || data == "[DONE]" {
        return bare_terminal(&event_type);
    }
    let Some(payload) = json_object(&data) else {
        return bare_terminal(&event_type);
    };
    let payload_type = clean_metadata_token(claim_string(&payload, "type"));
    if is_terminal_response_event(&payload_type) {
        event_type = payload_type;
    }
    let source = payload
        .get("response")
        .and_then(Value::as_object)
        .unwrap_or(&payload);
    let mut info = response_info_from_payload(Some(source));
    if let Some(error) = source.get("error").and_then(Value::as_object) {
        info.terminal_error_code = terminal_error_code(error);
        info.terminal_error_message = terminal_error_message(error);
    }
    if event_type == "error" {
        // The standalone Responses streaming error shape carries code/message
        // at the event root rather than inside response.error.
        let code = sanitized_error_code(claim_string(&payload, "code"));
        if !code.is_empty() {
            info.terminal_error_code = code;
        }
        let message = terminal_error_message(&payload);
        if !message.is_empty() {
            info.terminal_error_message = message;
        }
    }
    if event_type == "response.incomplete" {
        info.terminal_incomplete_reason = clean_metadata_token(
            source
                .get("incomplete_details")
                .and_then(Value::as_object)
                .map(|details| claim_string(details, "reason"))
                .unwrap_or_default(),
        );
        if info.terminal_incomplete_reason == "interrupted" {
            // Codex reads an incomplete response stopped as "interrupted" as
            // the turn's normal end, not a failure: codex-api
            // sse/responses.rs (rust-v0.164.0) errors only for any other
            // reason and parses this one as ResponseCompleted. Upstream sends
            // it when Codex steers or preempts a running turn (core
            // tests/suite/pending_input.rs steer_interrupts_and_drains_websocket).
            // Recorded as a failure, each steer raised the failure count
            // although nothing went wrong.
            event_type = "response.completed".to_string();
        }
    }
    if is_terminal_response_event(&event_type) {
        if event_type != "response.completed" {
            info.terminal_data = bounded_verdict_data(&data);
        }
        info.terminal_failure_class =
            terminal_failure_class(&event_type, &info.terminal_error_code).to_string();
        if info.terminal_failure_class == "unknown" && event_type == "error" {
            // A Responses WebSocket wraps an HTTP refusal as {"type":"error",
            // "status":429,...}; Codex classifies one with no code it knows by
            // that status, as it would the HTTP answer, so the pool does too.
            if let Some(status) =
                uint64_field(&payload, "status").or_else(|| uint64_field(&payload, "status_code"))
            {
                info.terminal_failure_class = status_failure_class(status).to_string();
            }
        }
        info.terminal_event = event_type;
    }
    info
}

pub fn is_terminal_response_event(value: &str) -> bool {
    matches!(
        value,
        "error" | "response.completed" | "response.failed" | "response.incomplete"
    )
}

/// Names the cause of a pre-commit failover. Both classes take the same path,
/// but an operator scanning the outcome column must not be sent looking at
/// quota for a request that moved because of a server fault.
pub fn stream_retry_failover_outcome(class: &str) -> &'static str {
    if class == "transient" {
        "stream_transient_failover"
    } else {
        "stream_capacity_failover"
    }
}

/// Whether a terminal failure may be discarded and retried within one client
/// request. Both classes describe a server-side condition that another
/// attempt or another identity can serve; request, authentication, incomplete
/// and unknown failures may not be retried, because none of them is evidence
/// that a second attempt would go any better.
pub fn retryable_terminal_failure_class(class: &str) -> bool {
    matches!(class, "capacity" | "transient")
}

/// Names an upstream error by its code, or by its type when it has none. The
/// Codex backend reports a spent allowance by type alone, on a Responses
/// WebSocket as {"type":"error","status":429,"error":{"type":
/// "usage_limit_reached",...}}, the shape Codex itself parses. Read by code
/// only, that failure was "unknown": handed to Codex as final, with no
/// cooldown, while the lineage stayed bound to the spent account.
pub fn terminal_error_code(error: &Map<String, Value>) -> String {
    let code = sanitized_error_code(claim_string(error, "code"));
    if code.is_empty() {
        sanitized_error_code(claim_string(error, "type"))
    } else {
        code
    }
}

/// Upstream's explanation for the log. Upstream's message is where the
/// request id its support asks for lives, so it is kept bounded and stripped
/// of control characters rather than dropped, but it stays out of state and
/// out of every response.
pub fn terminal_error_message(error: &Map<String, Value>) -> String {
    clean_metadata_text(claim_string(error, "message"), 300)
}

pub fn terminal_failure_class(event_type: &str, code: &str) -> &'static str {
    if event_type == "response.incomplete" {
        return "incomplete";
    }
    if event_type != "error" && event_type != "response.failed" {
        return "";
    }
    // The codes below are the ones Codex itself recognizes (codex-api
    // sse/responses_error.rs, api_bridge.rs and endpoint/responses_websocket.rs
    // at the pinned release), each sorted by what another account can do about
    // it. testdata/codex-upstream-failures.json holds the shapes, copied from
    // Codex's own tests, that keep this table honest; a code Codex learns in a
    // later release belongs here and there together.
    match sanitized_error_code(code).as_str() {
        "rate_limit_exceeded"
        | "insufficient_quota"
        | "usage_not_included"
        | "usage_limit_reached"
        | "server_is_overloaded"
        | "slow_down"
        | "credit_balance_exhausted"
        | "organization_spend_limit_exceeded"
        | "project_spend_limit_exceeded"
        | "organization_usage_limit_exceeded" => "capacity",
        // A server-side fault upstream chose to deliver inside the stream.
        // These two codes are documented and unambiguous, so recognizing them
        // is not the guessing that "unknown" exists to prevent. Left
        // unclassified they were handed to the caller untouched while the same
        // fault reported as an HTTP 5xx got a cooldown and a fallback: one
        // condition, two opposite outcomes, decided only by where upstream put
        // it.
        "server_error" | "internal_server_error" => "transient",
        "account_auth_failed"
        | "authentication_error"
        | "invalid_api_key"
        | "invalid_token"
        | "token_invalidated"
        | "token_revoked"
        | "unauthorized"
        | "forbidden" => "authentication",
        "context_length_exceeded"
        | "invalid_prompt"
        | "cyber_policy"
        | "bio_policy"
        | "misalignment_policy_violation"
        | "content_policy_violation"
        | "policy_violation"
        | "invalid_request_error"
        // Codex 0.165 ends the turn on it, in a stream or an HTTP 400/403/404
        // answer, without retrying (codex-api error.rs
        // parse_terminal_provider_error, ApiError::ModelNotFound): the model
        // named in the request does not exist upstream.
        | "model_not_found" => "request",
        // Recognized, and Codex's to handle: it reconnects on the WebSocket
        // connection limit, resends in full when a previous response is gone,
        // and reports flex unavailability itself. Another account would not do
        // better, so they pass through untouched, like "unknown", but they are
        // not counted as shapes the pool failed to understand.
        "websocket_connection_limit_reached"
        | "previous_response_not_found"
        | "flex_unavailable" => "passthrough",
        _ => "unknown",
    }
}

/// Classifies a wrapped refusal by its HTTP status, the way the pool
/// classifies the same refusal arriving as an HTTP answer.
pub fn status_failure_class(status: u64) -> &'static str {
    match status {
        429 => "capacity",
        401 => "authentication",
        500.. => "transient",
        400.. => "request",
        _ => "unknown",
    }
}

pub fn response_info_from_payload(payload: Option<&Map<String, Value>>) -> ProxyResponseInfo {
    let Some(payload) = payload else {
        return ProxyResponseInfo::default();
    };
    ProxyResponseInfo {
        response_id: payload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        usage: prompt_cache_usage_from_payload(payload),
        ..ProxyResponseInfo::default()
    }
}

pub fn prompt_cache_usage_from_payload(payload: &Map<String, Value>) -> PromptCacheUsage {
    let Some(usage) = payload.get("usage").and_then(Value::as_object) else {
        return PromptCacheUsage::default();
    };
    let input =
        uint64_field(usage, "input_tokens").or_else(|| uint64_field(usage, "prompt_tokens"));
    let output = first_uint64_field(usage, &["output_tokens", "completion_tokens"]);
    const CACHED: &[&str] = &[
        "cached_tokens",
        "cache_read_tokens",
        "cache_read_input_tokens",
    ];
    const WRITTEN: &[&str] = &[
        "cache_write_tokens",
        "cache_creation_tokens",
        "cache_creation_input_tokens",
        "cache_write_input_tokens",
    ];
    let mut cached = None;
    let mut written = None;
    for name in ["input_tokens_details", "prompt_tokens_details"] {
        let Some(details) = usage.get(name).and_then(Value::as_object) else {
            continue;
        };
        if cached.is_none() {
            cached = first_uint64_field(details, CACHED);
        }
        if written.is_none() {
            written = first_uint64_field(details, WRITTEN);
        }
    }
    if cached.is_none() {
        cached = first_uint64_field(usage, CACHED);
    }
    if written.is_none() {
        written = first_uint64_field(usage, WRITTEN);
    }
    PromptCacheUsage {
        input_tokens: input.unwrap_or(0),
        cached_tokens: cached.unwrap_or(0),
        cache_write_tokens: written,
        output_tokens: output.unwrap_or(0),
        output_present: output.is_some(),
        present: input.is_some() || cached.is_some() || written.is_some() || output.is_some(),
    }
}

pub fn first_uint64_field(values: &Map<String, Value>, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| uint64_field(values, name))
}

/// Go uint64Field over a decoded JSON number: a non-negative number, a float
/// truncated, anything else absent.
pub fn uint64_field(values: &Map<String, Value>, name: &str) -> Option<u64> {
    let number = values.get(name)?.as_number()?;
    if let Some(value) = number.as_u64() {
        return Some(value);
    }
    if number.as_i64().is_some() {
        return None;
    }
    let float = number.as_f64()?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (float >= 0.0).then_some(float as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn usage(json: serde_json::Value) -> PromptCacheUsage {
        prompt_cache_usage_from_payload(json.as_object().unwrap_or(&Map::new()))
    }

    // Go TestPromptCacheUsageParsesReadWriteVariantsAndAbsence.
    #[test]
    fn prompt_cache_usage_parses_read_write_variants_and_absence() {
        let nested = usage(serde_json::json!({"usage": {
            "input_tokens": 4096, "output_tokens": 321,
            "input_tokens_details": {"cached_tokens": 3072, "cache_write_tokens": 512}}}));
        assert!(nested.present && nested.output_present);
        assert_eq!(
            (
                nested.input_tokens,
                nested.cached_tokens,
                nested.cache_write_tokens,
                nested.output_tokens
            ),
            (4096, 3072, Some(512), 321)
        );
        let top_level = usage(serde_json::json!({"usage": {
            "prompt_tokens": 2048, "cache_read_input_tokens": 1024, "cache_creation_input_tokens": 0}}));
        assert!(top_level.present);
        assert_eq!(
            (top_level.cached_tokens, top_level.cache_write_tokens),
            (1024, Some(0))
        );
        let absent_write = usage(serde_json::json!({"usage": {
            "input_tokens": 1024, "input_tokens_details": {"cached_tokens": 768}}}));
        assert!(absent_write.present);
        assert_eq!(absent_write.cache_write_tokens, None);
        let chat =
            usage(serde_json::json!({"usage": {"prompt_tokens": 512, "completion_tokens": 77}}));
        assert!(chat.output_present);
        assert_eq!(chat.output_tokens, 77);
    }

    // Go TestStreamingUsageMergeRetainsObservedCacheWrite.
    #[test]
    fn streaming_usage_merge_retains_observed_cache_write() {
        let mut info = ProxyResponseInfo::default();
        info.merge(ProxyResponseInfo {
            usage: PromptCacheUsage {
                input_tokens: 4096,
                cached_tokens: 0,
                cache_write_tokens: Some(512),
                output_tokens: 25,
                output_present: true,
                present: true,
            },
            ..ProxyResponseInfo::default()
        });
        info.merge(ProxyResponseInfo {
            usage: PromptCacheUsage {
                input_tokens: 4096,
                cached_tokens: 3072,
                present: true,
                ..PromptCacheUsage::default()
            },
            ..ProxyResponseInfo::default()
        });
        assert_eq!(info.usage.cache_write_tokens, Some(512));
        assert_eq!(info.usage.cached_tokens, 3072);
        assert_eq!(
            (info.usage.output_present, info.usage.output_tokens),
            (true, 25)
        );
    }

    // Go TestResponseInfoFromSSEBlockClassifiesTerminalEvents.
    #[test]
    fn response_info_from_sse_block_classifies_terminal_events() {
        let cases = [
            ("completed", "response.completed", "", ""),
            (
                "rate limit",
                "response.failed",
                "rate_limit_exceeded",
                "capacity",
            ),
            ("quota", "response.failed", "insufficient_quota", "capacity"),
            (
                "overload",
                "response.failed",
                "server_is_overloaded",
                "capacity",
            ),
            ("auth", "response.failed", "invalid_token", "authentication"),
            (
                "context",
                "response.failed",
                "context_length_exceeded",
                "request",
            ),
            (
                "invalid prompt",
                "response.failed",
                "invalid_prompt",
                "request",
            ),
            ("policy", "response.failed", "cyber_policy", "request"),
            ("unknown", "response.failed", "future_error", "unknown"),
            ("incomplete", "response.incomplete", "", "incomplete"),
        ];
        for (name, event, code, class) in cases {
            let error_json = if code.is_empty() {
                String::new()
            } else {
                format!(r#","error":{{"code":"{code}","message":"not persisted"}}"#)
            };
            let block = format!(
                "event: {event}\ndata: {{\"type\":\"{event}\",\"response\":{{\"id\":\"resp_terminal\"{error_json}}}}}\n\n"
            );
            let info = response_info_from_sse_block(&block);
            assert_eq!(
                (
                    info.terminal_event.as_str(),
                    info.terminal_error_code.as_str(),
                    info.terminal_failure_class.as_str(),
                    info.response_id.as_str()
                ),
                (event, code, class, "resp_terminal"),
                "{name}"
            );
        }
        let malformed =
            response_info_from_sse_block("event: response.failed\ndata: {not-json}\n\n");
        assert_eq!(
            (
                malformed.terminal_event.as_str(),
                malformed.terminal_failure_class.as_str()
            ),
            ("response.failed", "unknown")
        );
        // The Codex backend names a spent allowance by error.type, with no code.
        let usage_limit = response_info_from_sse_block(
            "data: {\"type\":\"error\",\"status\":429,\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"The usage limit has been reached\"}}\n\n",
        );
        assert_eq!(
            (
                usage_limit.terminal_event.as_str(),
                usage_limit.terminal_error_code.as_str(),
                usage_limit.terminal_failure_class.as_str()
            ),
            ("error", "usage_limit_reached", "capacity")
        );
        let standalone = response_info_from_sse_block(
            "event: error\ndata: {\"type\":\"error\",\"code\":\"server_is_overloaded\",\"message\":\"not persisted\"}\n\n",
        );
        assert_eq!(
            (
                standalone.terminal_event.as_str(),
                standalone.terminal_error_code.as_str(),
                standalone.terminal_failure_class.as_str()
            ),
            ("error", "server_is_overloaded", "capacity")
        );
    }

    #[test]
    fn retry_classes() {
        assert!(
            retryable_terminal_failure_class("capacity")
                && retryable_terminal_failure_class("transient")
        );
        assert!(!retryable_terminal_failure_class("authentication"));
        assert_eq!(
            stream_retry_failover_outcome("transient"),
            "stream_transient_failover"
        );
        assert_eq!(
            stream_retry_failover_outcome("capacity"),
            "stream_capacity_failover"
        );
        assert_eq!(status_failure_class(503), "transient");
        assert_eq!(status_failure_class(404), "request");
        assert_eq!(status_failure_class(200), "unknown");
    }

    #[test]
    fn response_incomplete_keeps_upstream_reason_for_the_log() {
        // Shapes from Codex core/tests/suite/client.rs ("incomplete_details":
        // {"reason": "max_output_tokens"}) and scenarios_content_filter.rs
        // ("response": { "incomplete_details": { "reason": "content_filter" } }).
        let capped = response_info_from_sse_block(
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp-incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        );
        assert_eq!(
            (
                capped.terminal_event.as_str(),
                capped.terminal_failure_class.as_str(),
                capped.terminal_error_code.as_str(),
                capped.terminal_incomplete_reason.as_str()
            ),
            ("response.incomplete", "incomplete", "", "max_output_tokens")
        );
        let filtered = response_info_from_sse_block(
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"content_filter\"}}}\n\n",
        );
        assert_eq!(filtered.terminal_incomplete_reason, "content_filter");
        let mut merged = ProxyResponseInfo::default();
        merged.merge(filtered);
        assert_eq!(merged.terminal_incomplete_reason, "content_filter");
        // A steered turn ends as Codex reads it: completed, no failure.
        // Shape from Codex core/tests/suite/pending_input.rs
        // (interrupted["response"]["incomplete_details"] =
        // json!({"reason": "interrupted"})).
        let steered = response_info_from_sse_block(
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp-interrupted\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"interrupted\"}}}\n\n",
        );
        assert_eq!(
            (
                steered.terminal_event.as_str(),
                steered.terminal_failure_class.as_str(),
                steered.terminal_data.as_str(),
                steered.response_id.as_str()
            ),
            ("response.completed", "", "", "resp-interrupted")
        );
        // A failure that is not an incomplete answer carries no reason.
        let failed = response_info_from_sse_block(
            "data: {\"type\":\"response.failed\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        );
        assert_eq!(failed.terminal_incomplete_reason, "");
    }
}
