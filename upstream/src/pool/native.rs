//! The native Codex endpoint: the Codex backend's own paths under a
//! key-bearing prefix.
//!
//! A port of native_codex.go. A Codex sends its full native request only to
//! its built-in OpenAI provider with ChatGPT auth and a base URL ending in
//! /backend-api/codex (core/src/client.rs uses_codex_backend,
//! model-provider-info supports_codex_backend_routes, model-provider/src/
//! provider.rs include_internal_metadata, which configuration cannot set). So
//! the pool serves those paths under
//!
//! ```text
//! openai_base_url = "https://<pool>/k/<pool-api-key>/backend-api/codex"
//! ```
//!
//! and Codex then sends exactly what it sends OpenAI, measured equal to a
//! native Codex by scripts/conformance/wire-capture.sh. The built-in
//! provider's headers cannot be configured, and Codex puts its own ChatGPT
//! login in Authorization, so the pool API key travels in the path. The key
//! is checked here and the request then runs through the same handlers as
//! /v1, with the client's own credential removed. The key must never reach a
//! log line; nothing in the pool logs request paths, and this file must keep
//! it that way.

use super::app::App;
use super::http_io::ResponseWriter;
use super::http_io::header_value;
use super::http_io::set_header;
use super::http_io::write_openai_error;
use super::installation_id;
use super::proxy::relayed_codex_request_header;
use super::routes::Request;
use super::routes::serve_provider;
use super::routing::RequestFacts;
use super::routing::telemetry_label;
use super::state::Account;
use super::stream::MAX_REQUEST_BODY;
use super::stream::SseReader;
use super::time::Timestamp;
use bytes::Bytes;
use http::HeaderMap;
use http::Method;
use http::StatusCode;
use std::io::Read;
use std::sync::Arc;

/// The other Codex-backend paths Codex calls on its provider base URL
/// (codex-api/src/endpoint: models, images, memories, search, realtime
/// calls). They are not conversation turns the pool routes, but a native Codex
/// makes them with the same login as the conversation they belong to, so one
/// that names its thread or session goes to the account that conversation is
/// on; only one that names none, like the models catalog, takes the account a
/// new session would.
const PASSTHROUGH_PATHS: &[&str] = &[
    "models",
    "images/generations",
    "images/edits",
    "memories/trace_summarize",
    "alpha/search",
    "realtime/calls",
];

/// What Codex reads from these answers (the models catalog's ETag) besides
/// what a routed response relays.
fn native_response_headers() -> Vec<&'static str> {
    let mut names = vec!["etag"];
    names.extend_from_slice(super::proxy::PROXY_RESPONSE_HEADER_ALLOWLIST);
    names
}

/// The catalog request's native order: provider headers, auth, then the
/// default client's originator and user-agent (codex_own.go
/// codexModelsHeaderOrder).
const CODEX_MODELS_HEADER_ORDER: &str =
    "version,authorization,chatgpt-account-id,originator,user-agent|";

/// The client headers the pool consumes or replaces on purpose rather than
/// relaying: framing and transport, the client's own credential, and the
/// WebSocket handshake.
const NATIVE_HANDLED_HEADERS: &[&str] = &[
    "authorization",
    "chatgpt-account-id",
    "content-type",
    "content-length",
    "content-encoding",
    "accept",
    "accept-encoding",
    "connection",
    "upgrade",
    "host",
    "te",
    "keep-alive",
    "via",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-protocol",
    "cookie",
];

/// The front door's peer header is consumed before handlers run; it is never
/// a client header.
const FRONT_PEER_HEADER: &str = "x-codex-pool-peer";

/// Bounds both the compressed and the decoded size, so a small body cannot
/// expand past the limit every other request is held to.
pub fn decode_zstd_request_body(body: &[u8]) -> Result<Vec<u8>, String> {
    if body.len() > MAX_REQUEST_BODY {
        return Err("request body too large".to_string());
    }
    let decoder = zstd::stream::read::Decoder::new(body).map_err(|error| error.to_string())?;
    let mut decoded = Vec::new();
    decoder
        .take(u64::try_from(MAX_REQUEST_BODY).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut decoded)
        .map_err(|error| error.to_string())?;
    if decoded.len() > MAX_REQUEST_BODY {
        return Err("decoded request body too large".to_string());
    }
    Ok(decoded)
}

impl App {
    /// Logs, by name only and once per distinct set, the headers a
    /// native-endpoint client sent that the pool does not relay upstream. A
    /// native Codex's request must reach upstream whole, and the relay is an
    /// allowlist: x-oai-attestation, which only the Desktop sends, was once
    /// dropped this way without a trace. Values are never logged.
    pub fn note_unrelayed_native_headers(&self, headers: &HeaderMap) {
        let mut names: Vec<String> = headers
            .keys()
            .map(|name| name.as_str().to_string())
            .filter(|name| {
                !NATIVE_HANDLED_HEADERS.contains(&name.as_str())
                    && name != FRONT_PEER_HEADER
                    && !relayed_codex_request_header(name)
            })
            .collect();
        if names.is_empty() {
            return;
        }
        names.sort();
        names.dedup();
        let key = names.join(",");
        let seen = {
            let mut sets = self
                .unrelayed_header_sets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let seen = sets.contains(&key);
            if !seen && sets.len() < 256 {
                sets.insert(key.clone());
            }
            seen
        };
        if !seen {
            self.logger.printf(&format!(
                "native endpoint client headers not relayed upstream: {key} (originator {})",
                telemetry_label(header_value(headers, "originator"))
            ));
        }
    }

    /// The account a passthrough request belongs on: the lineage's or the
    /// thread's when the request names a conversation and that account can
    /// still serve, else the account a new session would take for the default
    /// model. It only reads routing state; it never creates a binding or a
    /// placement, since these requests are not turns.
    fn native_passthrough_account(&self, request: &Request) -> Option<Account> {
        let model = self.read().config.default_model.clone();
        let facts = RequestFacts {
            headers: &request.headers,
            native: true,
            websocket: false,
        };
        let api_key = super::server::request_api_key(&request.headers, &request.query);
        let route = self.routing_decision(&facts, &serde_json::Map::new(), &model, &api_key);
        let mut candidates = vec![route.lineage_account_id.clone()];
        if !route.identity.thread_id.is_empty() || !route.identity.session_id.is_empty() {
            candidates.push(self.sticky_session_account_id(&route.sticky_key));
        }
        let now = Timestamp::now();
        let inner = self.read();
        for id in candidates.iter().filter(|id| !id.is_empty()) {
            if let Some(item) = inner.account(id)
                && inner.usable(item, &model, now)
            {
                return Some(item.clone());
            }
        }
        inner.preferred_account(&model, &std::collections::HashMap::new(), now)
    }

    /// Sends one non-conversation request to the Codex backend with an
    /// account's credential, carrying the client's Codex headers as
    /// forward_once does for a conversation.
    async fn send_codex_backend(
        &self,
        item: &Account,
        auth: &super::metadata::CodexAuthInfo,
        request: &Request,
        target: &str,
        order: Option<&str>,
    ) -> Result<crate::transport::UpstreamResponse, crate::transport::SendFailure> {
        let inbound = &request.headers;
        let mut body = request.body.clone();
        self.note_client_agent(&item.id, inbound);
        self.note_account_activity(&item.id);
        // Whatever Codex request this is, the account presents its own
        // installation id.
        let installation = self.presented_installation_id(item);
        if let Some(id) = &installation
            && !body.is_empty()
        {
            let text = String::from_utf8_lossy(&body).into_owned();
            body = Bytes::from(installation_id::with_installation_id(&text, id).into_owned());
        }
        let mut headers = super::http_io::GoHeader::default();
        for name in ["Accept", "Content-Type", "If-None-Match"] {
            let value = header_value(inbound, name);
            if !value.is_empty() {
                headers.set(name, value);
            }
        }
        for (name, value) in inbound {
            if relayed_codex_request_header(name.as_str())
                && let Ok(value) = value.to_str()
                && !value.trim().is_empty()
            {
                headers.add(name.as_str(), value.trim());
            }
        }
        if let Some(id) = &installation {
            let metadata = headers.get("X-Codex-Turn-Metadata").to_string();
            if !metadata.is_empty() {
                headers.set(
                    "X-Codex-Turn-Metadata",
                    &installation_id::turn_metadata_with_installation_id(&metadata, id),
                );
            }
        }
        let account_id = if auth.account_id.is_empty() {
            item.account_id.clone()
        } else {
            auth.account_id.clone()
        };
        headers.set("Authorization", &format!("Bearer {}", auth.access_token));
        if !account_id.is_empty() {
            headers.set("ChatGPT-Account-ID", &account_id);
        }
        if auth.fedramp {
            headers.set("X-OpenAI-Fedramp", "true");
        }
        let (upstream, after) = crate::transport::build_request(
            request.method.clone(),
            target.to_string(),
            &headers.to_map(),
            body,
            codex_http_client::RequestCompression::None,
            order,
        )
        .map_err(crate::transport::SendFailure::Prepare)?;
        crate::transport::send_request(&self.transports, upstream, &after).await
    }
}

/// Authenticates a native-endpoint request by the key in its path and hands
/// it to the /v1 handler it corresponds to. The client's own ChatGPT
/// credential and account id are dropped before anything reads the request:
/// the pool sends the selected account's credential instead, and the client's
/// must never be relayed, logged, or mistaken for a pool API key.
pub async fn handle_native(
    app: &Arc<App>,
    mut request: Request,
    key: String,
    rest: String,
    writer: Box<dyn ResponseWriter>,
) {
    let mut writer = writer;
    let responses = rest == "responses";
    let compact = rest == "responses/compact";
    if !app.valid_api_key(&key) {
        write_openai_error(
            writer.as_mut(),
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "invalid or missing API key",
        );
        return;
    }
    app.note_unrelayed_native_headers(&request.headers);
    request.headers.remove("authorization");
    request.headers.remove("chatgpt-account-id");
    if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {key}")) {
        request.headers.insert("authorization", value);
    }
    // A native Codex zstd-compresses its Responses requests to the Codex
    // backend (codex-api/src/endpoint/responses.rs); the handlers read JSON,
    // and the transport compresses again on the way out.
    if header_value(&request.headers, "content-encoding")
        .trim()
        .eq_ignore_ascii_case("zstd")
    {
        match decode_zstd_request_body(&request.body) {
            Ok(body) => request.body = Bytes::from(body),
            Err(_) => {
                write_openai_error(
                    writer.as_mut(),
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "request body is invalid or too large",
                );
                return;
            }
        }
        request.headers.remove("content-encoding");
        request.headers.remove("content-length");
    }
    if (responses || compact) && request.method == Method::POST {
        request.path = if compact {
            "/v1/responses/compact".into()
        } else {
            "/v1/responses".into()
        };
        serve_provider(app, request, true, writer, false).await;
        return;
    }
    if responses && request.method == Method::GET {
        // As on /v1, the socket is not in flight; each response running on
        // it is.
        request.path = "/v1/responses".into();
        super::responses_ws::handle_responses_websocket(app, request, true, writer).await;
        return;
    }
    handle_native_passthrough(app, request, rest, writer.as_mut()).await;
}

async fn handle_native_passthrough(
    app: &Arc<App>,
    request: Request,
    rest: String,
    writer: &mut dyn ResponseWriter,
) {
    let rest = rest.trim_matches('/').to_string();
    if !PASSTHROUGH_PATHS.contains(&rest.as_str()) {
        write_openai_error(
            writer,
            StatusCode::NOT_FOUND,
            "not_found",
            "this Codex backend path is not served by the pool",
        );
        return;
    }
    if request.body.len() > MAX_REQUEST_BODY {
        write_openai_error(
            writer,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "request body is invalid or too large",
        );
        return;
    }
    let Some(item) = app
        .native_passthrough_account(&request)
        .filter(App::uses_codex_backend_directly)
    else {
        write_openai_error(
            writer,
            StatusCode::SERVICE_UNAVAILABLE,
            "no_available_account",
            "no account can serve this request",
        );
        return;
    };
    let mut target = format!("{}/{rest}", app.settings.codex_responses_base_url());
    if !request.query.is_empty() {
        target.push('?');
        target.push_str(&request.query);
    }
    let Ok(auth) = app.refreshed_codex_auth(&item).await else {
        write_openai_error(
            writer,
            StatusCode::SERVICE_UNAVAILABLE,
            "account_auth_failed",
            "account credential is unavailable",
        );
        return;
    };
    // The catalog request has a native order of its own; the others share the
    // Responses request's order, which the transport applies by default.
    let order = (rest == "models").then_some(CODEX_MODELS_HEADER_ORDER);
    let mut response = app
        .send_codex_backend(&item, &auth, &request, &target, order)
        .await;
    // On 401 refresh once and retry the same account, as a native Codex does.
    if let Ok(answer) = &response
        && answer.status == StatusCode::UNAUTHORIZED
    {
        response = match app
            .refresh_codex_auth_after_rejection(&item, &auth.access_token)
            .await
        {
            Ok(refreshed) => {
                app.send_codex_backend(&item, &refreshed, &request, &target, order)
                    .await
            }
            Err(error) => Err(crate::transport::SendFailure::Connection(error.message)),
        };
    }
    let Ok(response) = response else {
        write_openai_error(
            writer,
            StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            "upstream request failed",
        );
        return;
    };
    for name in native_response_headers() {
        let value = header_value(&response.headers, name);
        if !value.is_empty() {
            set_header(writer, name, value);
        }
    }
    writer.write_header(response.status);
    let mut reader = SseReader::new(response.body);
    loop {
        let (chunk, end) = reader.read_block().await;
        if !chunk.is_empty() {
            writer.write(&chunk);
            writer.flush();
        }
        if end.is_some() {
            break;
        }
    }
}
