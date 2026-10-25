use super::*;
use pretty_assertions::assert_eq;

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(object) => object,
        _ => Map::new(),
    }
}

/// A Responses request as Codex 0.164.0 writes it (wire-capture's native
/// scenario, trimmed).
fn codex_request() -> Map<String, Value> {
    object(serde_json::json!({
        "model": "gpt-test",
        "stream": true,
        "input": [{"type": "message", "id": "msg_1", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
        "tools": [{"type": "function", "name": "exec_command", "description": "", "strict": false, "parameters": {"type": "object"}}],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "reasoning": {"effort": "high"},
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "01a16cf6-f4be-7e62-9fca-ad570b73b218",
        "text": {"verbosity": "low"},
        "client_metadata": {"session_id": "01a16cf6-f4be-7e62-9fca-ad570b73b218"},
    }))
}

const CODEX_UA: &str =
    "codex_exec/0.164.0 (Alpine Linux 3.21.7; x86_64) unknown (codex_exec; 0.164.0)";

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(*name, value);
        }
    }
    headers
}

#[test]
fn a_codex_request_passes_on_both_entries() {
    for entry in [Entry::V1, Entry::Native] {
        assert_eq!(
            check_responses(&codex_request(), entry, Body::Responses, false),
            Ok(Vec::new())
        );
    }
}

#[test]
fn a_parameter_codex_never_sends_is_refused_with_its_name() {
    for (field, value) in [
        ("temperature", serde_json::json!(0.2)),
        ("max_output_tokens", serde_json::json!(100)),
        ("previous_response_id", serde_json::json!("resp_1")),
    ] {
        let mut request = codex_request();
        request.insert(field.to_string(), value);
        for entry in [Entry::V1, Entry::Native] {
            let refusal = check_responses(&request, entry, Body::Responses, false)
                .expect_err("a non-Codex parameter must be refused");
            assert_eq!(refusal.param, field);
            assert!(refusal.message().contains(field), "{}", refusal.message());
            assert_eq!(refusal.error_body()["error"]["code"], Refusal::CODE);
        }
    }
}

#[test]
fn a_name_never_seen_passes_and_is_reported() {
    let mut request = codex_request();
    request.insert("brand_new_codex_field".into(), Value::Bool(true));
    for entry in [Entry::V1, Entry::Native] {
        assert_eq!(
            check_responses(&request, entry, Body::Responses, false),
            Ok(vec!["brand_new_codex_field".to_string()])
        );
    }
}

#[test]
fn identity_fields_are_removed_on_v1_and_refused_on_the_native_endpoint() {
    let mut request = codex_request();
    request.insert("user".into(), Value::from("my-app"));
    assert_eq!(
        check_responses(&request, Entry::V1, Body::Responses, false),
        Ok(Vec::new())
    );
    assert_eq!(normalize(&request).removed, vec!["user".to_string()]);
    let refusal = check_responses(&request, Entry::Native, Body::Responses, false)
        .expect_err("the native endpoint rewrites nothing, so it refuses");
    assert_eq!(refusal.param, "user");
}

#[test]
fn a_missing_required_field_passes_only_for_a_newer_codex() {
    let mut request = codex_request();
    request.remove("include");
    assert!(check_responses(&request, Entry::Native, Body::Responses, false).is_err());
    assert_eq!(
        check_responses(&request, Entry::Native, Body::Responses, true),
        Ok(vec!["missing include".to_string()])
    );
    // /v1 completes it instead.
    assert_eq!(
        check_responses(&request, Entry::V1, Body::Responses, false),
        Ok(Vec::new())
    );
}

#[test]
fn tools_items_and_values_codex_never_sends_are_refused() {
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "tools",
            serde_json::json!([{"type": "file_search"}]),
            "tools: file_search",
        ),
        ("tool_choice", serde_json::json!("required"), "tool_choice"),
        (
            "include",
            serde_json::json!(["message.output_text.logprobs"]),
            "include: message.output_text.logprobs",
        ),
        (
            "input",
            serde_json::json!([{"type": "item_reference", "id": "x"}]),
            "input[0]: item_reference",
        ),
        (
            "input",
            serde_json::json!([{"type": "message", "role": "user", "content": [{"type": "input_file", "file_id": "f"}]}]),
            "input[0].content: input_file",
        ),
        (
            "text",
            serde_json::json!({"format": {"type": "json_object"}}),
            "text.format",
        ),
    ];
    for (field, value, param) in cases {
        let mut request = codex_request();
        request.insert(field.to_string(), value);
        let refusal = check_responses(&request, Entry::V1, Body::Responses, false)
            .expect_err("must be refused");
        assert_eq!(refusal.param, param);
    }
}

#[test]
fn what_v1_completes_is_refused_on_the_native_endpoint() {
    let cases: Vec<(&str, Value)> = vec![
        ("input", Value::from("hi")),
        ("stream", Value::Bool(false)),
        ("store", Value::Bool(true)),
        ("tools", serde_json::json!([{"type": "web_search_preview"}])),
        (
            "input",
            serde_json::json!([{"role": "system", "content": "be brief"}]),
        ),
    ];
    for (field, value) in cases {
        let mut request = codex_request();
        request.insert(field.to_string(), value.clone());
        assert!(
            check_responses(&request, Entry::V1, Body::Responses, false).is_ok(),
            "{field}={value} must pass on /v1"
        );
        assert!(
            check_responses(&request, Entry::Native, Body::Responses, false).is_err(),
            "{field}={value} must be refused on the native endpoint"
        );
    }
}

#[test]
fn a_websocket_message_may_continue_a_response() {
    let mut request = codex_request();
    request.insert("type".into(), Value::from("response.create"));
    request.insert("previous_response_id".into(), Value::from("resp_1"));
    request.insert("generate".into(), Value::Bool(false));
    assert_eq!(
        check_responses(&request, Entry::Native, Body::WebSocket, false),
        Ok(Vec::new())
    );
}

#[test]
fn only_codex_s_identity_format_counts_as_codex() {
    assert!(codex_identity(&headers(&[
        ("user-agent", CODEX_UA),
        ("originator", "codex_exec"),
    ])));
    assert!(codex_identity(&headers(&[
        (
            "user-agent",
            "codex-tui/0.164.0 (Ubuntu 22.4.0; x86_64) xterm-256color (codex-tui; 0.164.0)",
        ),
        ("originator", "codex-tui"),
    ])));
    // An app that calls itself Codex without Codex's format.
    for pairs in [
        &[("user-agent", "codex"), ("originator", "codex")][..],
        &[
            ("user-agent", "python-requests/2.31.0"),
            ("originator", "codex_exec"),
        ][..],
        &[("user-agent", CODEX_UA), ("originator", "my-app")][..],
        &[("user-agent", CODEX_UA)][..],
    ] {
        assert!(!codex_identity(&headers(pairs)), "{pairs:?}");
    }
}

#[test]
fn a_codex_custom_provider_request_is_not_completed() {
    let codex = headers(&[
        ("user-agent", CODEX_UA),
        ("originator", "codex_exec"),
        ("session-id", "01a16cf6-f4be-7e62-9fca-ad570b73b218"),
        ("thread-id", "01a16cf6-f4be-7e62-9fca-ad570b73b218"),
    ]);
    assert!(codex_shaped(&codex, &codex_request()));
    let mut app = codex_request();
    app.remove("client_metadata");
    assert!(!codex_shaped(&codex, &app));
    assert!(!codex_shaped(
        &headers(&[("user-agent", "my-app/1.0")]),
        &codex_request()
    ));
}

#[test]
fn chat_parameters_the_bridge_cannot_carry_are_refused() {
    let plain = object(serde_json::json!({
        "model": "gpt-test",
        "messages": [{"role": "system", "content": "be brief"}, {"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        "stream": true,
        "user": "my-app",
    }));
    assert_eq!(check_chat(&plain), Ok(Vec::new()));
    for (field, value, param) in [
        ("temperature", serde_json::json!(0.1), "temperature"),
        ("tools", serde_json::json!([]), "tools"),
        (
            "messages",
            serde_json::json!([{"role": "tool", "content": "x", "tool_call_id": "c"}]),
            "messages[0].role",
        ),
    ] {
        let mut request = plain.clone();
        request.insert(field.to_string(), value);
        assert_eq!(
            check_chat(&request).map_err(|refusal| refusal.param),
            Err(param.to_string())
        );
    }
}

#[test]
fn normalizing_writes_an_app_s_request_in_codex_s_vocabulary() {
    let request = object(serde_json::json!({
        "model": "gpt-test",
        "instructions": "be brief",
        "input": [
            {"role": "system", "content": "rules"},
            {"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image_url", "image_url": {"url": "data:x", "detail": "low"}}]},
            {"role": "assistant", "content": "hello"},
        ],
        "tools": [{"type": "web_search_preview"}],
        "text": {"format": {"type": "text"}},
        "stream_options": {"include_usage": true},
        "service_tier": "auto",
        "metadata": {"app": "my-app"},
        "prompt_cache_key": "my-app-cache",
        "client_metadata": {"app": "my-app"},
    }));
    let normalized = normalize(&request);
    assert_eq!(normalized.instructions.as_deref(), Some("be brief"));
    assert_eq!(
        Value::Array(normalized.items.clone()),
        serde_json::json!([
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}, {"type": "input_image", "image_url": "data:x", "detail": "low"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]},
        ])
    );
    assert_eq!(normalized.removed, vec!["metadata".to_string()]);
    assert_eq!(
        Value::Object(normalized.payload),
        serde_json::json!({"model": "gpt-test", "tools": [{"type": "web_search"}]})
    );
}

fn presented() -> Presented {
    Presented {
        user_agent: CODEX_UA.to_string(),
        originator: "codex_exec".to_string(),
        beta_features: "remote_compaction_v2".to_string(),
        profile: fallback_profile(),
        installation_id: "33333333-3333-4333-8333-333333333333".to_string(),
    }
}

fn defaults() -> ModelDefaults {
    ModelDefaults {
        effort: "medium".to_string(),
        verbosity: Some("low".to_string()),
        parallel_tool_calls: true,
    }
}

fn keys_in_order(body: &[u8]) -> Vec<String> {
    // serde_json's Map sorts keys, so read the order off the bytes.
    let text = String::from_utf8_lossy(body);
    let mut keys = Vec::new();
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
                if depth == 1 && text[index + 1..].starts_with(':') {
                    keys.push(text[start..index].to_string());
                }
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                start = index + 1;
            }
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            _ => {}
        }
    }
    keys
}

#[test]
fn a_completed_request_is_the_one_a_codex_custom_provider_client_sends() {
    let request = object(serde_json::json!({
        "model": "gpt-test",
        "instructions": "be brief",
        "input": "hi",
        "tools": [{"name": "lookup", "type": "function", "parameters": {"properties": {}, "type": "object"}}],
        "user": "my-app",
    }));
    let normalized = normalize(&request);
    let conversations = Conversations::default();
    let turn = conversations.turn("k", &normalized);
    let (body, headers) = complete(&normalized, "gpt-test", &defaults(), &presented(), &turn);
    // wire-capture's client scenario: Codex 0.164.0 as a custom provider.
    assert_eq!(
        keys_in_order(&body),
        vec![
            "model",
            "stream",
            "input",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning",
            "store",
            "include",
            "prompt_cache_key",
            "text",
            "client_metadata",
        ]
    );
    let parsed: Value = serde_json::from_slice(&body).unwrap_or_default();
    assert_eq!(
        parsed["prompt_cache_key"],
        Value::from(turn.thread_id.clone())
    );
    assert_eq!(parsed["reasoning"], serde_json::json!({"effort": "medium"}));
    assert_eq!(parsed["text"], serde_json::json!({"verbosity": "low"}));
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains("my-app"), "{text}");
    assert!(
        text.contains(
            r#"{"type":"function","name":"lookup","description":"","strict":false,"parameters":{"type":"object","properties":{}}}"#
        ),
        "{text}"
    );
    // The base instructions lead as a developer message with a v5 id.
    let namespace = Uuid::new_v5(&Uuid::NAMESPACE_OID, turn.thread_id.as_bytes());
    assert_eq!(
        parsed["input"][0],
        serde_json::json!({
            "type": "message",
            "id": format!("msg_{}", Uuid::new_v5(&namespace, b"be brief")),
            "role": "developer",
            "content": [{"type": "input_text", "text": "be brief"}],
        })
    );
    assert!(text.contains(r#"{"type":"message","id":"msg_"#), "{text}");
    let metadata = parsed["client_metadata"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut names: Vec<&String> = metadata.keys().collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "root_turn_id",
            "session_id",
            "thread_id",
            "turn_id",
            "x-codex-installation-id",
            "x-codex-turn-metadata",
            "x-codex-window-id",
        ]
    );
    for (name, value) in [
        ("user-agent", CODEX_UA.to_string()),
        ("originator", "codex_exec".to_string()),
        ("session-id", turn.thread_id.clone()),
        ("thread-id", turn.thread_id.clone()),
        ("x-client-request-id", turn.thread_id.clone()),
        ("x-codex-window-id", format!("{}:0", turn.thread_id)),
        ("accept", "text/event-stream".to_string()),
        ("x-codex-beta-features", "remote_compaction_v2".to_string()),
    ] {
        assert_eq!(header_value(&headers, name), value, "{name}");
    }
    let turn_metadata = header_value(&headers, "x-codex-turn-metadata");
    assert_eq!(turn_metadata, metadata["x-codex-turn-metadata"]);
    assert_eq!(
        keys_in_order(turn_metadata.as_bytes()),
        vec![
            "installation_id",
            "session_id",
            "thread_id",
            "agent_name",
            "turn_id",
            "window_id",
            "window_number",
            "context_window_id",
            "request_kind",
            "history_initialization",
            "root_turn_id",
            "thread_source",
            "turn_trigger",
            "sandbox",
            "sandbox_mode",
            "auto_review_enabled",
            "node_repl_auto_review_required",
            "node_repl_disabled",
            "turn_started_at_unix_ms",
            "analytics_enabled",
            "model",
            "reasoning_effort",
        ]
    );
}

#[test]
fn a_conversation_keeps_its_thread_and_starts_a_turn_only_when_the_user_speaks() {
    let conversations = Conversations::default();
    let first = normalize(&object(serde_json::json!({
        "input": [{"role": "user", "content": "hi"}],
    })));
    let one = conversations.turn("k", &first);
    let tool_round = normalize(&object(serde_json::json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "c1", "name": "lookup", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "42"},
        ],
    })));
    let two = conversations.turn("k", &tool_round);
    assert_eq!(two.thread_id, one.thread_id);
    assert_eq!(two.turn_id, one.turn_id, "a tool result continues the turn");
    assert_eq!(
        two.item_ids[0], one.item_ids[0],
        "a resent item keeps its id"
    );
    assert_eq!(two.item_ids[1], None, "upstream names a function call");
    assert!(
        two.item_ids[2]
            .as_deref()
            .is_some_and(|id| id.starts_with("fco_"))
    );
    let next = normalize(&object(serde_json::json!({
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "c1", "name": "lookup", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "42"},
            {"role": "user", "content": "thanks"},
        ],
    })));
    let three = conversations.turn("k", &next);
    assert_eq!(three.thread_id, one.thread_id);
    assert_ne!(three.turn_id, one.turn_id, "the user spoke again");
    assert_eq!(three.item_ids[2], two.item_ids[2]);
    assert_ne!(conversations.turn("other", &first).thread_id, one.thread_id);
}

#[test]
fn a_newer_codex_is_recognized_from_its_user_agent() {
    let agent = |version: &str| {
        headers(&[(
            "user-agent",
            &format!("codex_exec/{version} (Linux 6; x86_64) unknown"),
        )])
    };
    assert!(newer_than_pinned(&agent("0.999.0")));
    assert!(!newer_than_pinned(&agent(PINNED_CODEX_VERSION)));
    assert!(!newer_than_pinned(&agent("0.100.0")));
    assert!(!newer_than_pinned(&headers(&[(
        "user-agent",
        "my-app/9.9.9"
    )])));
}

#[test]
fn the_profile_is_read_from_codex_s_turn_metadata() {
    let metadata = r#"{"installation_id":"i","session_id":"s","agent_name":"/root","turn_trigger":"exec","sandbox":"none","workspaces":{"/home/x":{}},"model":"m"}"#;
    assert_eq!(
        Value::Object(profile_from_turn_metadata(metadata)),
        serde_json::json!({"agent_name": "/root", "turn_trigger": "exec", "sandbox": "none"})
    );
}
