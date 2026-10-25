//! Requests from clients other than Codex (SPEC.md 2.3.4).
//!
//! In-house apps share the pool with Codex, and every request the pool sends
//! upstream must be one a native Codex would send. Nothing a client says about
//! itself is trusted: an app may call itself Codex, so every request is checked
//! field by field against what Codex's source sends (codex-api/src/common.rs
//! `ResponsesApiRequest`, core/src/client.rs `build_responses_request`, and the
//! fields earlier releases sent), and:
//!
//! - what is known not to be Codex and changes the answer is refused before it
//!   reaches upstream, with an error naming the field, so the app stops and its
//!   owner can fix it;
//! - what the pool has never seen is passed and logged, so a newer Codex (the
//!   Desktop after an auto-update, mid-task) is never stopped; a name an app
//!   invents is one upstream does not know either, and refuses itself;
//! - on /v1 only, what is missing or written differently is completed into the
//!   request a Codex custom-provider client sends (the shape the pool already
//!   forwards for Codex on /v1, measured by wire-capture's client scenario),
//!   and every trace of the app (its user agent, its own headers, identity-only
//!   fields, its cache key) is replaced;
//! - the native endpoint completes nothing: it forwards Codex's request exactly
//!   (installation id aside), so there the rules only check.
//!
//! Do not loosen a refusal into a silent removal: an app whose temperature or
//! output cap disappeared gets a different answer without knowing why, which
//! is what the refusal exists to prevent. Do not turn the unknown-name rule
//! into a refusal either: that stops a newer Codex in the middle of a task.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use http::HeaderMap;
use http::HeaderValue;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use uuid::Uuid;

use super::credentials::PINNED_CODEX_VERSION;
use super::http_io::header_value;

/// Where a request arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// /v1: Codex as a custom provider, or another app. Completed when it is
    /// not already a Codex request.
    V1,
    /// /k/<key>/backend-api/codex/: forwarded exactly, so only checked.
    Native,
}

impl Entry {
    pub fn label(self, websocket: bool) -> &'static str {
        match (self, websocket) {
            (Entry::V1, false) => "v1",
            (Entry::V1, true) => "v1/ws",
            (Entry::Native, false) => "native",
            (Entry::Native, true) => "native/ws",
        }
    }
}

/// Which request body is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Body {
    /// POST .../responses.
    Responses,
    /// POST .../responses/compact: a different body, so only the refusals
    /// apply.
    Compact,
    /// A response.create message on a Responses WebSocket.
    WebSocket,
}

/// Why a request is not sent. The message names the field, says why, and how
/// to fix it, so the app's owner can act on it without reading the pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub param: String,
    pub reason: String,
    pub fix: String,
}

impl Refusal {
    pub fn new(
        param: impl Into<String>,
        reason: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            param: param.into(),
            reason: reason.into(),
            fix: fix.into(),
        }
    }

    pub const CODE: &'static str = "non_codex_request";

    pub fn message(&self) -> String {
        format!(
            "codex-pool did not send this request: `{}` {}. The pool only sends requests that a native Codex (up to {PINNED_CODEX_VERSION}) sends. {}",
            self.param, self.reason, self.fix
        )
    }

    /// The HTTP error body, in the OpenAI error shape.
    pub fn error_body(&self) -> Value {
        serde_json::json!({"error": {
            "message": self.message(),
            "type": "invalid_request_error",
            "param": self.param,
            "code": Self::CODE,
        }})
    }

    /// The error event a Responses WebSocket carries: the wrapped error a
    /// Codex reads as an HTTP status (codex-api/src/endpoint/responses_websocket.rs
    /// parse_wrapped_websocket_error_event; recorded in
    /// testdata/codex-upstream-failures.json, "websocket usage limit"). A 400 is
    /// not retried, so the turn stops.
    pub fn websocket_event(&self) -> Value {
        serde_json::json!({
            "type": "error",
            "status": 400,
            "error": {
                "type": "invalid_request_error",
                "code": Self::CODE,
                "message": self.message(),
                "param": self.param,
            },
        })
    }
}

const REMOVE_IT: &str = "Remove it and send the request again.";

/// Top-level fields a Codex release sends: 0.164.0's ResponsesApiRequest plus
/// `instructions`, which releases up to 0.162 sent before moving the base
/// instructions into the input.
const CODEX_FIELDS: &[&str] = &[
    "model",
    "instructions",
    "stream",
    "service_tier",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "store",
    "stream_options",
    "include",
    "prompt_cache_key",
    "text",
    "client_metadata",
    "access_programs",
];

/// What a response.create message adds (ResponseCreateWsRequest and the
/// message's tag).
const WEBSOCKET_FIELDS: &[&str] = &["type", "previous_response_id", "generate"];

/// Fields every Responses request of a Codex release carries (the
/// non-optional ones of ResponsesApiRequest; `reasoning` is an Option written
/// even when empty).
const REQUIRED_FIELDS: &[&str] = &[
    "model",
    "stream",
    "input",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "store",
    "include",
];

/// Public Responses API parameters no Codex release sends, each of which
/// changes the answer.
const REFUSED_FIELDS: &[(&str, &str, &str)] = &[
    (
        "temperature",
        "sets sampling, which Codex leaves to the model",
        REMOVE_IT,
    ),
    (
        "top_p",
        "sets sampling, which Codex leaves to the model",
        REMOVE_IT,
    ),
    (
        "top_logprobs",
        "asks for token probabilities, which Codex never asks for",
        REMOVE_IT,
    ),
    (
        "max_output_tokens",
        "caps the answer's length, which Codex never does",
        REMOVE_IT,
    ),
    (
        "max_tool_calls",
        "caps the number of tool calls, which Codex never does",
        REMOVE_IT,
    ),
    (
        "previous_response_id",
        "continues a stored response, and Codex stores nothing upstream",
        "Send the whole conversation in `input` instead.",
    ),
    (
        "conversation",
        "continues a stored conversation, and Codex stores nothing upstream",
        "Send the whole conversation in `input` instead.",
    ),
    (
        "truncation",
        "asks upstream to cut the conversation, which Codex never does",
        REMOVE_IT,
    ),
    (
        "background",
        "runs the response in the background, which Codex never does",
        REMOVE_IT,
    ),
    (
        "prompt",
        "uses a stored prompt template, which Codex never does",
        "Send the instructions as a developer message in `input` instead.",
    ),
    (
        "context_management",
        "asks upstream to manage the context, which Codex never does",
        REMOVE_IT,
    ),
];

/// Public parameters no Codex sends that only label the caller or tune
/// caching; the answer is the same without them. /v1 removes them, so they
/// never carry an app's name upstream; the native endpoint, which rewrites
/// nothing, refuses them.
const IDENTITY_FIELDS: &[&str] = &[
    "user",
    "metadata",
    "safety_identifier",
    "prompt_cache_retention",
];

/// Item types a Codex release sends (codex-protocol models.rs ResponseItem;
/// `compaction_summary` is its alias of `compaction`).
const CODEX_ITEM_TYPES: &[&str] = &[
    "additional_tools",
    "message",
    "agent_message",
    "reasoning",
    "local_shell_call",
    "function_call",
    "tool_search_call",
    "function_call_output",
    "custom_tool_call",
    "custom_tool_call_output",
    "tool_search_output",
    "web_search_call",
    "image_generation_call",
    "compaction",
    "compaction_summary",
    "configuration_update",
    "compaction_trigger",
    "context_compaction",
];

/// Public Responses item types of hosted tools and stored items no Codex
/// sends.
const REFUSED_ITEM_TYPES: &[&str] = &[
    "item_reference",
    "file_search_call",
    "computer_call",
    "computer_call_output",
    "code_interpreter_call",
    "mcp_call",
    "mcp_list_tools",
    "mcp_approval_request",
    "mcp_approval_response",
    "local_shell_call_output",
    "shell_call",
    "shell_call_output",
    "apply_patch_call",
    "apply_patch_call_output",
];

/// Message content Codex sends (models.rs ContentItem).
const CODEX_CONTENT_TYPES: &[&str] = &["input_text", "input_image", "input_audio", "output_text"];

/// Tool types a Codex release declares (tools/src/tool_spec.rs ToolSpec, plus
/// the local shell and hosted image tools of earlier releases).
const CODEX_TOOL_TYPES: &[&str] = &[
    "function",
    "namespace",
    "tool_search",
    "web_search",
    "custom",
    "local_shell",
    "image_generation",
];

/// Hosted tools no Codex declares.
const REFUSED_TOOL_TYPES: &[&str] = &[
    "file_search",
    "code_interpreter",
    "computer_use_preview",
    "computer",
    "computer_use",
    "mcp",
    "shell",
    "apply_patch",
];

/// The older public names of the web search tool Codex declares as
/// `web_search`; /v1 renames them.
const WEB_SEARCH_PREVIEW_TYPES: &[&str] = &["web_search_preview", "web_search_preview_2025_03_11"];

/// `include` values Codex asks for (core/src/client.rs).
const CODEX_INCLUDES: &[&str] = &["reasoning.encrypted_content", "output.encrypted_content"];

/// Chat Completions parameters the pool cannot express in a Codex request.
const CHAT_REFUSED_FIELDS: &[(&str, &str)] = &[
    (
        "temperature",
        "sets sampling, which Codex leaves to the model",
    ),
    ("top_p", "sets sampling, which Codex leaves to the model"),
    ("n", "asks for several answers, which Codex never does"),
    ("stop", "sets stop sequences, which Codex never does"),
    (
        "max_tokens",
        "caps the answer's length, which Codex never does",
    ),
    (
        "max_completion_tokens",
        "caps the answer's length, which Codex never does",
    ),
    ("presence_penalty", "tunes sampling, which Codex never does"),
    (
        "frequency_penalty",
        "tunes sampling, which Codex never does",
    ),
    ("logit_bias", "tunes sampling, which Codex never does"),
    ("seed", "fixes sampling, which Codex never does"),
    (
        "logprobs",
        "asks for token probabilities, which Codex never asks for",
    ),
    (
        "top_logprobs",
        "asks for token probabilities, which Codex never asks for",
    ),
    (
        "response_format",
        "is not converted by the pool's Chat Completions bridge",
    ),
    (
        "tools",
        "is not converted by the pool's Chat Completions bridge",
    ),
    (
        "tool_choice",
        "is not converted by the pool's Chat Completions bridge",
    ),
    (
        "functions",
        "is not converted by the pool's Chat Completions bridge",
    ),
    (
        "function_call",
        "is not converted by the pool's Chat Completions bridge",
    ),
    (
        "parallel_tool_calls",
        "is not converted by the pool's Chat Completions bridge",
    ),
    ("audio", "asks for audio, which Codex never does"),
    (
        "modalities",
        "asks for other output types, which Codex never does",
    ),
    (
        "prediction",
        "sends a predicted output, which Codex never does",
    ),
    (
        "web_search_options",
        "is not converted by the pool's Chat Completions bridge",
    ),
];

/// Chat Completions parameters the bridge converts.
const CHAT_FIELDS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "reasoning_effort",
    "verbosity",
    "prompt_cache_key",
];

/// Chat Completions parameters that only label the caller or shape the
/// transport; the bridge drops them.
const CHAT_IDENTITY_FIELDS: &[&str] = &[
    "user",
    "metadata",
    "safety_identifier",
    "store",
    "stream_options",
    "service_tier",
    "prompt_cache_retention",
];

const USE_RESPONSES: &str =
    "Send it to /v1/responses in a shape a Codex request has, or remove it.";

/// Checks a Responses request (or a response.create message). Returns the
/// names the pool has never seen, which pass and are logged, or the refusal.
pub fn check_responses(
    payload: &Map<String, Value>,
    entry: Entry,
    body: Body,
    newer_client: bool,
) -> Result<Vec<String>, Refusal> {
    let native = entry == Entry::Native;
    let mut unknown = Vec::new();
    for (key, value) in payload {
        let key = key.as_str();
        if body == Body::WebSocket && WEBSOCKET_FIELDS.contains(&key) {
            continue;
        }
        if let Some((_, reason, fix)) = REFUSED_FIELDS.iter().find(|(name, _, _)| *name == key) {
            return Err(Refusal::new(key, *reason, *fix));
        }
        if IDENTITY_FIELDS.contains(&key) {
            if native {
                return Err(Refusal::new(
                    key,
                    "is not part of a Codex request, and the native endpoint forwards requests unchanged",
                    REMOVE_IT,
                ));
            }
            continue;
        }
        if !CODEX_FIELDS.contains(&key) {
            unknown.push(key.to_string());
            continue;
        }
        check_field(key, value, native, &mut unknown)?;
    }
    if native && body != Body::Compact {
        for field in REQUIRED_FIELDS {
            if payload.contains_key(*field) {
                continue;
            }
            // A newer Codex may have dropped a field the pinned release still
            // sends (0.163 dropped `instructions`); it passes and is logged.
            if newer_client {
                unknown.push(format!("missing {field}"));
                continue;
            }
            return Err(Refusal::new(
                *field,
                "is missing, and every Codex request carries it",
                "Send the request a Codex sends, or send it to /v1, which completes it.",
            ));
        }
    }
    Ok(unknown)
}

fn check_field(
    key: &str,
    value: &Value,
    native: bool,
    unknown: &mut Vec<String>,
) -> Result<(), Refusal> {
    match key {
        "stream" if native && value != &Value::Bool(true) => Err(Refusal::new(
            "stream",
            "must be true: Codex always streams",
            "Set `stream` to true, or send the request to /v1, which completes it.",
        )),
        "store" if native && value != &Value::Bool(false) => Err(Refusal::new(
            "store",
            "must be false: Codex stores nothing upstream",
            "Set `store` to false.",
        )),
        "tool_choice" if value.as_str() != Some("auto") => Err(Refusal::new(
            "tool_choice",
            "forces a tool choice, and Codex always lets the model choose (\"auto\")",
            "Set `tool_choice` to \"auto\" or remove it.",
        )),
        "include" => {
            let Some(values) = value.as_array() else {
                return Err(Refusal::new("include", "must be a list", REMOVE_IT));
            };
            for item in values {
                let name = item.as_str().unwrap_or_default();
                if !CODEX_INCLUDES.contains(&name) {
                    return Err(Refusal::new(
                        format!("include: {name}"),
                        "asks for output Codex never asks for",
                        "Remove that entry.",
                    ));
                }
            }
            Ok(())
        }
        "input" => match value {
            Value::String(_) if native => Err(Refusal::new(
                "input",
                "is a string, and Codex always sends a list of items",
                "Send a list of items, or send the request to /v1, which completes it.",
            )),
            Value::String(_) => Ok(()),
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    check_item(index, item, native, unknown)?;
                }
                Ok(())
            }
            _ => Err(Refusal::new("input", "must be a list of items", REMOVE_IT)),
        },
        "tools" => {
            let Some(tools) = value.as_array() else {
                return Err(Refusal::new("tools", "must be a list", REMOVE_IT));
            };
            for tool in tools {
                let kind = tool.get("type").and_then(Value::as_str).unwrap_or_default();
                if CODEX_TOOL_TYPES.contains(&kind) {
                    continue;
                }
                if WEB_SEARCH_PREVIEW_TYPES.contains(&kind) && !native {
                    continue;
                }
                if REFUSED_TOOL_TYPES.contains(&kind) || WEB_SEARCH_PREVIEW_TYPES.contains(&kind) {
                    return Err(Refusal::new(
                        format!("tools: {kind}"),
                        "is a tool Codex never declares",
                        "Remove that tool, or declare it as a function tool your app runs itself.",
                    ));
                }
                unknown.push(format!("tools: {kind}"));
            }
            Ok(())
        }
        "text" => {
            let format = value
                .get("format")
                .and_then(|format| format.get("type"))
                .and_then(Value::as_str);
            match format {
                None | Some("json_schema") => Ok(()),
                Some("text") if !native => Ok(()),
                Some("json_object" | "text") => Err(Refusal::new(
                    "text.format",
                    "asks for an output format Codex never asks for (Codex sends only json_schema)",
                    "Use a json_schema format or remove `text.format`.",
                )),
                Some(other) => {
                    unknown.push(format!("text.format: {other}"));
                    Ok(())
                }
            }
        }
        "stream_options" => {
            if let Some(options) = value.as_object() {
                for name in options.keys() {
                    match name.as_str() {
                        "reasoning_summary_delivery" => {}
                        "include_usage" | "include_obfuscation" if !native => {}
                        "include_usage" | "include_obfuscation" => {
                            return Err(Refusal::new(
                                format!("stream_options.{name}"),
                                "is not part of a Codex request, and the native endpoint forwards requests unchanged",
                                REMOVE_IT,
                            ));
                        }
                        other => unknown.push(format!("stream_options.{other}")),
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn check_item(
    index: usize,
    item: &Value,
    native: bool,
    unknown: &mut Vec<String>,
) -> Result<(), Refusal> {
    let Some(object) = item.as_object() else {
        return Err(Refusal::new(
            format!("input[{index}]"),
            "is not an item",
            REMOVE_IT,
        ));
    };
    let kind = match object.get("type").and_then(Value::as_str) {
        Some(kind) => kind,
        None if object.contains_key("role") && !native => "message",
        None => {
            return Err(Refusal::new(
                format!("input[{index}].type"),
                "is missing, and Codex names every item's type",
                "Add `\"type\": \"message\"` (or send the request to /v1, which completes it).",
            ));
        }
    };
    if REFUSED_ITEM_TYPES.contains(&kind) {
        return Err(Refusal::new(
            format!("input[{index}]: {kind}"),
            "is an item Codex never sends",
            "Remove it; Codex sends tool results as function_call_output items.",
        ));
    }
    if !CODEX_ITEM_TYPES.contains(&kind) {
        unknown.push(format!("input: {kind}"));
        return Ok(());
    }
    if kind != "message" {
        return Ok(());
    }
    if native && object.get("role").and_then(Value::as_str) == Some("system") {
        return Err(Refusal::new(
            format!("input[{index}].role"),
            "is system, and Codex sends instructions as developer messages",
            "Use the developer role.",
        ));
    }
    match object.get("content") {
        Some(Value::String(_)) if native => Err(Refusal::new(
            format!("input[{index}].content"),
            "is a string, and Codex always sends a list of content parts",
            "Send a list of input_text parts.",
        )),
        Some(Value::Array(parts)) => {
            for part in parts {
                let part_kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
                if CODEX_CONTENT_TYPES.contains(&part_kind) {
                    continue;
                }
                // Chat-style parts a /v1 bridge request carries; completed.
                if !native && matches!(part_kind, "text" | "image_url") {
                    continue;
                }
                if part_kind == "input_file" {
                    return Err(Refusal::new(
                        format!("input[{index}].content: input_file"),
                        "attaches a file, which Codex never does",
                        "Put the file's text in an input_text part, or send images as input_image.",
                    ));
                }
                unknown.push(format!("content: {part_kind}"));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Checks a Chat Completions request before the bridge converts it.
pub fn check_chat(payload: &Map<String, Value>) -> Result<Vec<String>, Refusal> {
    let mut unknown = Vec::new();
    for key in payload.keys() {
        let key = key.as_str();
        if let Some((_, reason)) = CHAT_REFUSED_FIELDS.iter().find(|(name, _)| *name == key) {
            return Err(Refusal::new(key, *reason, USE_RESPONSES));
        }
        if !CHAT_FIELDS.contains(&key) && !CHAT_IDENTITY_FIELDS.contains(&key) {
            unknown.push(key.to_string());
        }
    }
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for (index, message) in messages.iter().enumerate() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(role, "system" | "developer" | "user" | "assistant") {
            return Err(Refusal::new(
                format!("messages[{index}].role"),
                "is a role the pool's Chat Completions bridge does not convert",
                USE_RESPONSES,
            ));
        }
        for field in ["tool_calls", "function_call", "audio"] {
            if message.get(field).is_some_and(|value| !value.is_null()) {
                return Err(Refusal::new(
                    format!("messages[{index}].{field}"),
                    "is not converted by the pool's Chat Completions bridge",
                    USE_RESPONSES,
                ));
            }
        }
        if let Some(parts) = message.get("content").and_then(Value::as_array) {
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
                if !matches!(kind, "text" | "image_url") {
                    return Err(Refusal::new(
                        format!("messages[{index}].content: {kind}"),
                        "is not converted by the pool's Chat Completions bridge",
                        USE_RESPONSES,
                    ));
                }
            }
        }
    }
    Ok(unknown)
}

// ---- identity ----

/// A Codex user agent: "<originator>/<version> (<os> <os version>; <arch>)
/// <terminal>", optionally followed by " (<client>; <version>)"
/// (login/src/auth/default_client.rs get_codex_user_agent).
pub fn codex_user_agent(user_agent: &str) -> bool {
    regex_lite::Regex::new(
        r"^[A-Za-z0-9_][A-Za-z0-9_ .-]*/[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)? \([^()]*; [^()]*\)( .*)?$",
    )
    .is_ok_and(|pattern| pattern.is_match(user_agent))
}

/// Whether the client presents itself in Codex's format: a Codex user agent
/// and an originator naming the same product. Only the format is checked, so
/// a pass says nothing about who the client is; what it guarantees is that
/// nothing in these headers names an app.
pub fn codex_identity(headers: &HeaderMap) -> bool {
    let user_agent = header_value(headers, "user-agent").trim();
    let originator = header_value(headers, "originator").trim();
    codex_user_agent(user_agent)
        && !originator.is_empty()
        && super::credentials::codex_originator_from_user_agent(user_agent) == originator
}

pub fn identity_refusal() -> Refusal {
    Refusal::new(
        "user-agent",
        "is not in Codex's format (with an originator header naming the same product), and the native endpoint forwards headers unchanged",
        "Send the request to /v1, which presents it as Codex.",
    )
}

/// Whether the client's Codex release is newer than the pinned one, from its
/// user agent.
pub fn newer_than_pinned(headers: &HeaderMap) -> bool {
    let version = super::credentials::codex_client_version(headers);
    let parse = |text: &str| -> Vec<u64> {
        text.split(['-', '+'])
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    !version.is_empty() && parse(&version) > parse(PINNED_CODEX_VERSION)
}

/// Whether a /v1 request already is a Codex request: Codex's identity, its
/// session headers and its client metadata, as a Codex custom-provider client
/// sends them (wire-capture's client scenario). Such a request is forwarded as
/// it arrived; anything else is completed.
pub fn codex_shaped(headers: &HeaderMap, payload: &Map<String, Value>) -> bool {
    codex_identity(headers)
        && ["session-id", "session_id"]
            .iter()
            .any(|name| !header_value(headers, name).trim().is_empty())
        && !header_value(headers, "thread-id").trim().is_empty()
        && payload.get("client_metadata").is_some_and(Value::is_object)
}

// ---- completion ----

/// Model defaults for fields an app leaves out, from the pool's catalog.
#[derive(Clone, Debug)]
pub struct ModelDefaults {
    pub effort: String,
    pub verbosity: Option<String>,
    pub parallel_tool_calls: bool,
}

/// The Codex a completed request presents: the one that last used the
/// account (as for the pool's own calls), with the static part of its turn
/// metadata.
#[derive(Clone, Debug)]
pub struct Presented {
    pub user_agent: String,
    pub originator: String,
    pub beta_features: String,
    pub profile: Map<String, Value>,
    pub installation_id: String,
}

/// The static turn-metadata keys a Codex sends with every turn
/// (core/src/responses_metadata.rs CodexTurnMetadataPayload): recorded from the
/// account's last native Codex and replayed on completed requests. Workspaces
/// are never recorded; they name a person's checkout.
pub const PROFILE_KEYS: &[&str] = &[
    "agent_name",
    "thread_source",
    "turn_trigger",
    "sandbox",
    "sandbox_mode",
    "auto_review_enabled",
    "node_repl_auto_review_required",
    "node_repl_disabled",
    "analytics_enabled",
];

/// The profile of the bundled Codex CLI's `codex exec` (measured by
/// wire-capture), for the fallback agent of credentials.rs.
pub fn fallback_profile() -> Map<String, Value> {
    let mut profile = Map::new();
    for (key, value) in [
        ("agent_name", Value::from("/root")),
        ("thread_source", Value::from("user")),
        ("turn_trigger", Value::from("exec")),
        ("sandbox", Value::from("none")),
        ("sandbox_mode", Value::from("danger-full-access")),
        ("auto_review_enabled", Value::Bool(false)),
        ("node_repl_auto_review_required", Value::Bool(false)),
        ("node_repl_disabled", Value::Bool(false)),
        ("analytics_enabled", Value::Bool(true)),
    ] {
        profile.insert(key.to_string(), value);
    }
    profile
}

/// The profile part of a Codex turn-metadata header, empty when it carries
/// none.
pub fn profile_from_turn_metadata(value: &str) -> Map<String, Value> {
    let Ok(Value::Object(metadata)) = serde_json::from_str::<Value>(value) else {
        return Map::new();
    };
    metadata
        .into_iter()
        .filter(|(key, _)| PROFILE_KEYS.contains(&key.as_str()))
        .collect()
}

/// The identifiers of one turn of a completed conversation.
#[derive(Clone, Debug)]
pub struct TurnIds {
    pub thread_id: String,
    pub context_window_id: String,
    pub turn_id: String,
    pub turn_started_ms: i64,
    /// One per normalized input item, in order: the id Codex gives it, if
    /// any.
    pub item_ids: Vec<Option<String>>,
}

/// An app's request normalized to Codex's vocabulary, before identifiers.
#[derive(Clone, Debug, Default)]
pub struct Normalized {
    pub payload: Map<String, Value>,
    pub instructions: Option<String>,
    pub items: Vec<Value>,
    /// Fields removed because only Codex's own values may go upstream.
    pub removed: Vec<String>,
}

/// Rewrites what is written differently into Codex's vocabulary and removes
/// what only names the app. Values are kept; nothing is invented here.
pub fn normalize(payload: &Map<String, Value>) -> Normalized {
    let mut payload = payload.clone();
    let mut removed = Vec::new();
    for field in IDENTITY_FIELDS {
        if payload.remove(*field).is_some() {
            removed.push((*field).to_string());
        }
    }
    // Codex's own values replace these (completed below).
    for field in ["client_metadata", "prompt_cache_key"] {
        payload.remove(field);
    }
    let instructions = match payload.remove("instructions") {
        Some(Value::String(text)) if !text.is_empty() => Some(text),
        _ => None,
    };
    let items = match payload.remove("input") {
        Some(Value::String(text)) => vec![message("user", vec![text_part("user", &text)])],
        Some(Value::Array(items)) => items.into_iter().map(normalize_item).collect(),
        _ => Vec::new(),
    };
    if let Some(Value::Array(tools)) = payload.get_mut("tools") {
        for tool in tools.iter_mut() {
            let Some(object) = tool.as_object_mut() else {
                continue;
            };
            match object.get("type").and_then(Value::as_str) {
                Some(kind) if WEB_SEARCH_PREVIEW_TYPES.contains(&kind) => {
                    object.insert("type".into(), Value::from("web_search"));
                }
                // ResponsesApiTool always writes its description and strict
                // flag (tools/src/responses_api.rs); an app often leaves them
                // out.
                Some("function") => {
                    object
                        .entry("description")
                        .or_insert_with(|| Value::from(""));
                    object.entry("strict").or_insert(Value::Bool(false));
                }
                _ => {}
            }
        }
    }
    if let Some(Value::Object(text)) = payload.get_mut("text") {
        if text
            .get("format")
            .and_then(|format| format.get("type"))
            .and_then(Value::as_str)
            == Some("text")
        {
            text.remove("format");
        }
        if text.is_empty() {
            payload.remove("text");
        }
    }
    if let Some(Value::Object(options)) = payload.get_mut("stream_options") {
        options.retain(|name, _| name == "reasoning_summary_delivery");
        if options.is_empty() {
            payload.remove("stream_options");
        }
    }
    if matches!(
        payload.get("service_tier").and_then(Value::as_str),
        Some("auto" | "default")
    ) {
        payload.remove("service_tier");
    }
    Normalized {
        payload,
        instructions,
        items,
        removed,
    }
}

fn text_part(role: &str, text: &str) -> Value {
    let kind = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    serde_json::json!({"type": kind, "text": text})
}

fn message(role: &str, content: Vec<Value>) -> Value {
    serde_json::json!({"type": "message", "role": role, "content": content})
}

fn normalize_item(item: Value) -> Value {
    let Value::Object(mut object) = item else {
        return item;
    };
    if !object.contains_key("type") && object.contains_key("role") {
        object.insert("type".into(), Value::from("message"));
    }
    if object.get("type").and_then(Value::as_str) != Some("message") {
        return Value::Object(object);
    }
    let role = match object.get("role").and_then(Value::as_str) {
        Some("system") | None => "developer".to_string(),
        Some(role) => role.to_string(),
    };
    object.insert("role".into(), Value::from(role.as_str()));
    let content = match object.remove("content") {
        Some(Value::String(text)) => vec![text_part(&role, &text)],
        Some(Value::Array(parts)) => parts
            .into_iter()
            .map(|part| normalize_part(&role, part))
            .collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    };
    object.insert("content".into(), Value::Array(content));
    Value::Object(object)
}

/// Chat-style parts a bridged request carries, in Codex's vocabulary.
fn normalize_part(role: &str, part: Value) -> Value {
    match part.get("type").and_then(Value::as_str) {
        Some("text") => text_part(
            role,
            part.get("text").and_then(Value::as_str).unwrap_or_default(),
        ),
        Some("image_url") => {
            let image = part.get("image_url");
            let url = image
                .and_then(|image| image.get("url").or(Some(image)))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut converted = serde_json::json!({"type": "input_image", "image_url": url});
            if let Some(detail) = image
                .and_then(|image| image.get("detail"))
                .and_then(Value::as_str)
                && let Some(object) = converted.as_object_mut()
            {
                object.insert("detail".into(), Value::from(detail));
            }
            converted
        }
        _ => part,
    }
}

/// The id prefix Codex gives an item of this kind when it creates it (a
/// message "msg_", a tool output "fco_"; core history), or none.
pub fn id_prefix(item: &Value) -> Option<&'static str> {
    if item
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
    {
        return None;
    }
    match item.get("type").and_then(Value::as_str) {
        Some("message") => Some("msg_"),
        Some("function_call_output" | "custom_tool_call_output") => Some("fco_"),
        _ => None,
    }
}

/// The request a Codex custom-provider client sends, in its field order
/// (codex-api/src/common.rs ResponsesApiRequest): Codex's body and the
/// headers Codex adds to it (session, window, turn metadata, its agent).
pub fn complete(
    normalized: &Normalized,
    model: &str,
    defaults: &ModelDefaults,
    presented: &Presented,
    turn: &TurnIds,
) -> (Vec<u8>, HeaderMap) {
    let payload = &normalized.payload;
    let effort = payload
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .unwrap_or(&defaults.effort)
        .to_string();
    let window_id = format!("{}:0", turn.thread_id);
    let metadata = turn_metadata(presented, turn, &window_id, model, &effort);

    let mut out = Vec::new();
    out.push(b'{');
    let mut first = true;
    let mut field = |out: &mut Vec<u8>, name: &str| {
        if !first {
            out.push(b',');
        }
        first = false;
        write_string(out, name);
        out.push(b':');
    };
    field(&mut out, "model");
    write_string(&mut out, model);
    field(&mut out, "stream");
    out.extend_from_slice(b"true");
    if let Some(tier) = payload.get("service_tier") {
        field(&mut out, "service_tier");
        write_json(&mut out, tier);
    }
    field(&mut out, "input");
    out.push(b'[');
    let mut wrote_item = false;
    if let Some(text) = &normalized.instructions {
        // core/src/client.rs: the base instructions lead the input as a
        // developer message whose id is a v5 UUID of the text within the
        // thread's namespace.
        let namespace = Uuid::new_v5(&Uuid::NAMESPACE_OID, turn.thread_id.as_bytes());
        let id = format!("msg_{}", Uuid::new_v5(&namespace, text.as_bytes()));
        let mut item = message("developer", vec![text_part("developer", text)]);
        if let Some(object) = item.as_object_mut() {
            object.insert("id".into(), Value::from(id));
        }
        write_item(&mut out, &item);
        wrote_item = true;
    }
    for (index, item) in normalized.items.iter().enumerate() {
        if wrote_item {
            out.push(b',');
        }
        wrote_item = true;
        match turn.item_ids.get(index).cloned().flatten() {
            Some(id) => {
                let mut item = item.clone();
                if let Some(object) = item.as_object_mut() {
                    object.insert("id".into(), Value::from(id));
                }
                write_item(&mut out, &item);
            }
            None => write_item(&mut out, item),
        }
    }
    out.push(b']');
    if let Some(Value::Array(tools)) = payload.get("tools") {
        field(&mut out, "tools");
        out.push(b'[');
        for (index, tool) in tools.iter().enumerate() {
            if index > 0 {
                out.push(b',');
            }
            write_tool(&mut out, tool);
        }
        out.push(b']');
    }
    field(&mut out, "tool_choice");
    write_string(&mut out, "auto");
    field(&mut out, "parallel_tool_calls");
    let parallel = payload
        .get("parallel_tool_calls")
        .and_then(Value::as_bool)
        .unwrap_or(defaults.parallel_tool_calls);
    out.extend_from_slice(if parallel { b"true" } else { b"false" });
    field(&mut out, "reasoning");
    match payload.get("reasoning") {
        Some(Value::Object(reasoning)) => {
            let mut reasoning = reasoning.clone();
            reasoning.insert("effort".into(), Value::from(effort.as_str()));
            write_ordered(&mut out, &reasoning, &["effort", "summary"]);
        }
        _ => {
            let mut reasoning = Map::new();
            reasoning.insert("effort".into(), Value::from(effort.as_str()));
            write_ordered(&mut out, &reasoning, &["effort"]);
        }
    }
    field(&mut out, "store");
    out.extend_from_slice(b"false");
    if let Some(options) = payload.get("stream_options") {
        field(&mut out, "stream_options");
        write_json(&mut out, options);
    }
    field(&mut out, "include");
    match payload.get("include") {
        Some(include @ Value::Array(_)) => write_json(&mut out, include),
        _ => write_json(
            &mut out,
            &serde_json::json!(["reasoning.encrypted_content"]),
        ),
    }
    field(&mut out, "prompt_cache_key");
    write_string(&mut out, &turn.thread_id);
    match payload.get("text") {
        Some(Value::Object(text)) => {
            field(&mut out, "text");
            write_text_controls(&mut out, text);
        }
        _ => {
            if let Some(verbosity) = &defaults.verbosity {
                field(&mut out, "text");
                let mut text = Map::new();
                text.insert("verbosity".into(), Value::from(verbosity.as_str()));
                write_text_controls(&mut out, &text);
            }
        }
    }
    field(&mut out, "client_metadata");
    // Codex keeps client metadata in a HashMap, whose order differs between
    // requests; a fixed order would be the pool's own signature.
    let mut entries = vec![
        ("x-codex-installation-id", presented.installation_id.clone()),
        ("session_id", turn.thread_id.clone()),
        ("thread_id", turn.thread_id.clone()),
        ("x-codex-window-id", window_id.clone()),
        ("turn_id", turn.turn_id.clone()),
        ("root_turn_id", turn.turn_id.clone()),
        ("x-codex-turn-metadata", metadata.clone()),
    ];
    shuffle(&mut entries);
    out.push(b'{');
    for (index, (name, value)) in entries.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        write_string(&mut out, name);
        out.push(b':');
        write_string(&mut out, value);
    }
    out.push(b'}');
    out.push(b'}');

    let mut headers = HeaderMap::new();
    let mut set = |name: &'static str, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(name, value);
        }
    };
    if !presented.beta_features.is_empty() {
        set("x-codex-beta-features", &presented.beta_features);
    }
    set("x-codex-window-id", &window_id);
    set("x-codex-turn-metadata", &metadata);
    set("x-client-request-id", &turn.thread_id);
    set("session-id", &turn.thread_id);
    set("thread-id", &turn.thread_id);
    set("accept", "text/event-stream");
    set("originator", &presented.originator);
    set("user-agent", &presented.user_agent);
    (out, headers)
}

/// Codex's turn metadata for a completed turn, in CodexTurnMetadataPayload's
/// field order, ASCII-escaped as Codex writes it (to_ascii_json_string).
fn turn_metadata(
    presented: &Presented,
    turn: &TurnIds,
    window_id: &str,
    model: &str,
    effort: &str,
) -> String {
    let profile = |key: &str| presented.profile.get(key).cloned();
    let mut fields: Vec<(&str, Value)> = vec![
        (
            "installation_id",
            Value::from(presented.installation_id.as_str()),
        ),
        ("session_id", Value::from(turn.thread_id.as_str())),
        ("thread_id", Value::from(turn.thread_id.as_str())),
    ];
    if let Some(value) = profile("agent_name") {
        fields.push(("agent_name", value));
    }
    fields.extend([
        ("turn_id", Value::from(turn.turn_id.as_str())),
        ("window_id", Value::from(window_id)),
        ("window_number", Value::from(0)),
        (
            "context_window_id",
            Value::from(turn.context_window_id.as_str()),
        ),
        ("request_kind", Value::from("turn")),
        ("history_initialization", Value::from("new")),
        ("root_turn_id", Value::from(turn.turn_id.as_str())),
    ]);
    for key in [
        "thread_source",
        "turn_trigger",
        "sandbox",
        "sandbox_mode",
        "auto_review_enabled",
        "node_repl_auto_review_required",
        "node_repl_disabled",
    ] {
        if let Some(value) = profile(key) {
            fields.push((key, value));
        }
    }
    fields.push(("turn_started_at_unix_ms", Value::from(turn.turn_started_ms)));
    if let Some(value) = profile("analytics_enabled") {
        fields.push(("analytics_enabled", value));
    }
    fields.push(("model", Value::from(model)));
    fields.push(("reasoning_effort", Value::from(effort)));
    let mut out = Vec::new();
    out.push(b'{');
    for (index, (name, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        write_string(&mut out, name);
        out.push(b':');
        write_json(&mut out, value);
    }
    out.push(b'}');
    ascii_escaped(&String::from_utf8_lossy(&out))
}

fn ascii_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii() {
            out.push(character);
        } else {
            let mut units = [0u16; 2];
            for unit in character.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

fn shuffle<T>(items: &mut [T]) {
    for index in (1..items.len()).rev() {
        let draw = u32::from_le_bytes(super::crypto::random_bytes::<4>());
        let other = usize::try_from(draw).unwrap_or(0) % (index + 1);
        items.swap(index, other);
    }
}

// ---- writing in Codex's field order ----

fn write_string(out: &mut Vec<u8>, text: &str) {
    if let Ok(encoded) = serde_json::to_vec(text) {
        out.extend_from_slice(&encoded);
    }
}

fn write_json(out: &mut Vec<u8>, value: &Value) {
    if let Ok(encoded) = serde_json::to_vec(value) {
        out.extend_from_slice(&encoded);
    }
}

/// An object with the named keys first, in that order, then the rest in
/// serde's order.
fn write_ordered(out: &mut Vec<u8>, object: &Map<String, Value>, order: &[&str]) {
    write_ordered_with(out, object, order, &mut |out, _, value| {
        write_json(out, value)
    });
}

fn write_ordered_with(
    out: &mut Vec<u8>,
    object: &Map<String, Value>,
    order: &[&str],
    value_writer: &mut dyn FnMut(&mut Vec<u8>, &str, &Value),
) {
    out.push(b'{');
    let mut first = true;
    let keys = order
        .iter()
        .copied()
        .filter(|key| object.contains_key(*key))
        .chain(
            object
                .keys()
                .map(String::as_str)
                .filter(|key| !order.contains(key)),
        );
    for key in keys {
        let Some(value) = object.get(key) else {
            continue;
        };
        if !first {
            out.push(b',');
        }
        first = false;
        write_string(out, key);
        out.push(b':');
        value_writer(out, key, value);
    }
    out.push(b'}');
}

/// Field orders of the ResponseItem variants Codex serializes (models.rs).
fn item_order(kind: &str) -> &'static [&'static str] {
    match kind {
        "message" => &[
            "type",
            "encrypted_content",
            "status",
            "id",
            "role",
            "content",
            "phase",
        ],
        "function_call" => &[
            "type",
            "encrypted_content",
            "status",
            "id",
            "name",
            "namespace",
            "arguments",
            "encrypted_function_args",
            "call_id",
        ],
        "function_call_output" => &["type", "id", "call_id", "name", "namespace", "output"],
        "custom_tool_call" => &[
            "type",
            "encrypted_content",
            "id",
            "status",
            "call_id",
            "name",
            "namespace",
            "input",
        ],
        "custom_tool_call_output" => &["type", "id", "call_id", "name", "output"],
        "reasoning" => &["type", "id", "summary", "content", "encrypted_content"],
        _ => &["type", "id"],
    }
}

fn write_item(out: &mut Vec<u8>, item: &Value) {
    let Some(object) = item.as_object() else {
        write_json(out, item);
        return;
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    write_ordered_with(
        out,
        object,
        item_order(kind),
        &mut |out, key, value| match (key, value) {
            ("content", Value::Array(parts)) if kind == "message" => {
                out.push(b'[');
                for (index, part) in parts.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    match part.as_object() {
                        Some(part) => write_ordered(
                            out,
                            part,
                            &[
                                "type",
                                "text",
                                "image_url",
                                "file_id",
                                "detail",
                                "audio_url",
                                "annotations",
                                "logprobs",
                            ],
                        ),
                        None => write_json(out, part),
                    }
                }
                out.push(b']');
            }
            _ => write_json(out, value),
        },
    );
}

/// A tool in ToolSpec's field order (tools/src/responses_api.rs
/// ResponsesApiTool), its parameters in JsonSchema's.
fn write_tool(out: &mut Vec<u8>, tool: &Value) {
    let Some(object) = tool.as_object() else {
        write_json(out, tool);
        return;
    };
    write_ordered_with(
        out,
        object,
        &[
            "type",
            "name",
            "description",
            "strict",
            "defer_loading",
            "parameters",
        ],
        &mut |out, key, value| match (key, value) {
            ("parameters", Value::Object(schema)) => write_schema(out, schema),
            _ => write_json(out, value),
        },
    );
}

/// tools/src/json_schema/types.rs JsonSchema's field order, at every level.
const SCHEMA_ORDER: &[&str] = &[
    "$ref",
    "type",
    "description",
    "encrypted",
    "enum",
    "items",
    "minItems",
    "properties",
    "required",
    "additionalProperties",
    "anyOf",
    "oneOf",
    "allOf",
    "$defs",
    "definitions",
];

fn write_schema(out: &mut Vec<u8>, schema: &Map<String, Value>) {
    write_ordered_with(
        out,
        schema,
        SCHEMA_ORDER,
        &mut |out, key, value| match (key, value) {
            ("items" | "additionalProperties", Value::Object(inner)) => write_schema(out, inner),
            ("properties" | "$defs" | "definitions", Value::Object(map)) => {
                out.push(b'{');
                for (index, (name, inner)) in map.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    write_string(out, name);
                    out.push(b':');
                    match inner.as_object() {
                        Some(inner) => write_schema(out, inner),
                        None => write_json(out, inner),
                    }
                }
                out.push(b'}');
            }
            ("anyOf" | "oneOf" | "allOf", Value::Array(variants)) => {
                out.push(b'[');
                for (index, inner) in variants.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    match inner.as_object() {
                        Some(inner) => write_schema(out, inner),
                        None => write_json(out, inner),
                    }
                }
                out.push(b']');
            }
            _ => write_json(out, value),
        },
    );
}

/// TextControls (verbosity, format) and TextFormat (type, strict, schema,
/// name) in codex-api/src/common.rs order.
fn write_text_controls(out: &mut Vec<u8>, text: &Map<String, Value>) {
    write_ordered_with(
        out,
        text,
        &["verbosity", "format"],
        &mut |out, key, value| match (key, value) {
            ("format", Value::Object(format)) => {
                write_ordered(out, format, &["type", "strict", "schema", "name"]);
            }
            _ => write_json(out, value),
        },
    );
}

// ---- conversations ----

/// How long a completed conversation keeps its identifiers without a request,
/// and how many are kept.
const CONVERSATION_TTL: Duration = Duration::from_secs(24 * 3600);
const CONVERSATION_LIMIT: usize = 2048;

struct Conversation {
    thread_id: String,
    context_window_id: String,
    turn_id: String,
    turn_started_ms: i64,
    last_user: String,
    item_ids: HashMap<String, String>,
    used: Instant,
}

/// The identifiers Codex keeps for a thread, kept for each conversation an
/// app holds with the pool, so its turns look like one Codex thread: one
/// thread id, a turn id that changes only when the user speaks, and item ids
/// that stay the same when the history is sent again.
#[derive(Default)]
pub struct Conversations {
    entries: Mutex<HashMap<String, Conversation>>,
    logged: Mutex<HashSet<String>>,
}

fn digest(parts: &[&[u8]]) -> String {
    use base64::Engine as _;
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
        hasher.update([0]);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// What identifies an app's conversation: the session it names, if any, else
/// the start of its history (an app resends the whole conversation, and its
/// first item stays the same).
pub fn conversation_key(api_key: &str, headers: &HeaderMap, normalized: &Normalized) -> String {
    for name in ["session_id", "session-id", "thread-id", "x-session-id"] {
        let value = header_value(headers, name).trim();
        if !value.is_empty() {
            return digest(&[api_key.as_bytes(), b"session", value.as_bytes()]);
        }
    }
    let first = normalized
        .items
        .first()
        .map(|item| serde_json::to_vec(item).unwrap_or_default())
        .unwrap_or_default();
    digest(&[
        api_key.as_bytes(),
        normalized
            .instructions
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
        &first,
    ])
}

impl Conversations {
    /// The identifiers of this request's turn, creating the conversation on
    /// its first request.
    pub fn turn(&self, key: &str, normalized: &Normalized) -> TurnIds {
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !entries.contains_key(key) {
            entries.retain(|_, entry| now.duration_since(entry.used) < CONVERSATION_TTL);
            while entries.len() >= CONVERSATION_LIMIT {
                let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                entries.remove(&oldest);
            }
            entries.insert(
                key.to_string(),
                Conversation {
                    thread_id: Uuid::now_v7().to_string(),
                    context_window_id: Uuid::now_v7().to_string(),
                    turn_id: String::new(),
                    turn_started_ms: 0,
                    last_user: String::new(),
                    item_ids: HashMap::new(),
                    used: now,
                },
            );
        }
        let Some(entry) = entries.get_mut(key) else {
            return TurnIds {
                thread_id: Uuid::now_v7().to_string(),
                context_window_id: Uuid::now_v7().to_string(),
                turn_id: Uuid::now_v7().to_string(),
                turn_started_ms: chrono::Utc::now().timestamp_millis(),
                item_ids: Vec::new(),
            };
        };
        entry.used = now;
        // A turn starts when the user speaks; tool results answering the
        // model continue the same turn, as in Codex.
        let last_user = normalized
            .items
            .iter()
            .enumerate()
            .rev()
            .find(|(_, item)| {
                item.get("type").and_then(Value::as_str) == Some("message")
                    && item.get("role").and_then(Value::as_str) == Some("user")
            })
            .map(|(index, item)| item_digest(index, item))
            .unwrap_or_default();
        if entry.turn_id.is_empty() || last_user != entry.last_user {
            entry.turn_id = Uuid::now_v7().to_string();
            entry.turn_started_ms = chrono::Utc::now().timestamp_millis();
            entry.last_user = last_user;
        }
        let item_ids = normalized
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let prefix = id_prefix(item)?;
                let id = entry
                    .item_ids
                    .entry(item_digest(index, item))
                    .or_insert_with(|| format!("{prefix}{}", Uuid::now_v7()));
                Some(id.clone())
            })
            .collect();
        TurnIds {
            thread_id: entry.thread_id.clone(),
            context_window_id: entry.context_window_id.clone(),
            turn_id: entry.turn_id.clone(),
            turn_started_ms: entry.turn_started_ms,
            item_ids,
        }
    }

    /// Whether a log line with this key is new (bounded, so a stream of
    /// distinct names cannot grow it without limit).
    pub fn first_time(&self, key: &str) -> bool {
        let mut logged = self
            .logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if logged.contains(key) {
            return false;
        }
        if logged.len() < 512 {
            logged.insert(key.to_string());
        }
        true
    }
}

fn item_digest(index: usize, item: &Value) -> String {
    let encoded = serde_json::to_vec(item).unwrap_or_default();
    digest(&[index.to_string().as_bytes(), &encoded])
}

// ---- the pool's side ----

impl super::app::App {
    /// Checks an HTTP request before routing. Writes the refusal and returns
    /// None when it is not sent; otherwise returns whether it is an app's
    /// request that /v1 completes per attempt.
    pub fn gate_http_request(
        &self,
        request: &super::proxy::ProxyRequest,
        payload: &Map<String, Value>,
        writer: &mut dyn super::http_io::ResponseWriter,
        chat: bool,
        compact: bool,
    ) -> Option<bool> {
        let entry = if request.native {
            Entry::Native
        } else {
            Entry::V1
        };
        let identity = codex_identity(&request.headers);
        let checked = if request.native && !identity {
            Err(identity_refusal())
        } else if compact && !identity {
            // Only Codex compacts a conversation; the compact body is not
            // completed, so an app's would go out as the app wrote it.
            Err(Refusal::new(
                "user-agent",
                "is not in Codex's format, and only Codex compacts a conversation",
                "Send the whole conversation to /v1/responses instead.",
            ))
        } else if chat {
            check_chat(payload)
        } else {
            let body = if compact {
                Body::Compact
            } else {
                Body::Responses
            };
            check_responses(payload, entry, body, newer_than_pinned(&request.headers))
        };
        let api_key = super::server::request_api_key(&request.headers, &request.query);
        match checked {
            Err(refusal) => {
                self.log_refused_request(entry.label(false), &api_key, &refusal);
                super::http_io::write_json(
                    writer,
                    http::StatusCode::BAD_REQUEST,
                    &refusal.error_body(),
                );
                None
            }
            Ok(unknown) => {
                self.note_unrecognized_request_fields(entry.label(false), &unknown);
                // A compaction is Codex's own (its identity was required
                // above) and its body is not a Responses request, so it is
                // never completed.
                Some(
                    !request.native
                        && !compact
                        && (chat || !codex_shaped(&request.headers, payload)),
                )
            }
        }
    }

    /// One line per refusal, naming the client key by its fingerprint so the
    /// owner can tell which app it was.
    pub fn log_refused_request(&self, entry: &str, api_key: &str, refusal: &Refusal) {
        self.logger.printf(&format!(
            "request refused before upstream: entry={entry} key={} param={} reason=\"{}\"",
            super::hash::api_key_fingerprint(api_key),
            super::proxy::safe_header_value(&refusal.param),
            refusal.reason
        ));
    }

    /// Names the pool has never seen pass upstream; each distinct set is
    /// logged once, so a newer Codex's new field is found and recorded.
    pub fn note_unrecognized_request_fields(&self, entry: &str, unknown: &[String]) {
        if unknown.is_empty() {
            return;
        }
        let names = unknown.join(",");
        if self
            .foreign_conversations
            .first_time(&format!("unknown {entry} {names}"))
        {
            self.logger.printf(&format!(
                "request fields the pool does not know were passed upstream: entry={entry} fields={}",
                super::proxy::safe_header_value(&names)
            ));
        }
    }

    /// The Codex a completed request presents for this account: the one that
    /// last used it, or the last one seen at all, with the turn-metadata
    /// profile recorded from it; else the bundled CLI's agent and profile, so
    /// the user agent and the profile always belong to the same Codex.
    fn foreign_presentation(
        &self,
        account_id: &str,
    ) -> (super::state::ClientAgent, Map<String, Value>) {
        let inner = self.read();
        let agents = inner.state.client_agents();
        let recorded = [account_id, ""]
            .iter()
            .filter_map(|key| agents.get(*key))
            .find(|agent| !agent.user_agent.is_empty() && !agent.turn_profile.is_empty())
            .cloned();
        drop(inner);
        match recorded {
            Some(mut agent) => {
                if agent.originator.is_empty() {
                    agent.originator =
                        super::credentials::codex_originator_from_user_agent(&agent.user_agent);
                }
                let profile = agent.turn_profile.clone();
                (agent, profile)
            }
            None => (
                super::credentials::fallback_client_agent(),
                fallback_profile(),
            ),
        }
    }

    /// An app's /v1 request completed into the request a Codex
    /// custom-provider client sends, for this account: its body, the headers
    /// that replace the app's, and whether the app asked for no stream.
    pub fn complete_foreign_request(
        &self,
        candidate: &super::state::Account,
        body: &bytes::Bytes,
        inbound: &HeaderMap,
        model: &str,
        api_key: &str,
    ) -> Result<(bytes::Bytes, HeaderMap, bool), String> {
        let payload: Map<String, Value> = serde_json::from_slice(body)
            .map_err(|_| "request body must be a JSON object".to_string())?;
        let aggregate = payload.get("stream").and_then(Value::as_bool) != Some(true);
        let normalized = normalize(&payload);
        if !normalized.removed.is_empty() {
            let names = normalized.removed.join(",");
            if self
                .foreign_conversations
                .first_time(&format!("removed {names}"))
            {
                self.logger.printf(&format!(
                    "removed from an app's request before sending: fields={names}"
                ));
            }
        }
        let defaults = {
            let inner = self.read();
            let info = inner
                .codex_model_catalog(&[model.to_string()])
                .into_iter()
                .next();
            ModelDefaults {
                effort: info.as_ref().map_or_else(
                    || "medium".to_string(),
                    |info| info.default_reasoning_level.clone(),
                ),
                verbosity: info
                    .as_ref()
                    .filter(|info| info.support_verbosity)
                    .map(|info| info.default_verbosity.to_string()),
                parallel_tool_calls: info
                    .as_ref()
                    .is_none_or(|info| info.supports_parallel_tool_calls),
            }
        };
        let (agent, profile) = self.foreign_presentation(&candidate.id);
        let presented = Presented {
            user_agent: agent.user_agent,
            originator: agent.originator,
            beta_features: agent.beta_features,
            profile,
            installation_id: self
                .presented_installation_id(candidate)
                .unwrap_or_default(),
        };
        let key = conversation_key(api_key, inbound, &normalized);
        let turn = self.foreign_conversations.turn(&key, &normalized);
        let (body, headers) = complete(&normalized, model, &defaults, &presented, &turn);
        Ok((bytes::Bytes::from(body), headers, aggregate))
    }
}

#[cfg(test)]
#[path = "codex_request_tests.rs"]
mod tests;
