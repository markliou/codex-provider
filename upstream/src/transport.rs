//! The final hop to upstream, with Codex's own HTTP and WebSocket clients:
//! the native header orders, the per-request client, the WebSocket connector
//! and the relay of an open socket.
//!
//! Used by the pool, which sends in process. It was first the loopback hop a
//! Go pool sent through (removed with the Go code); the rules came unchanged.

use bytes::Bytes;
use codex_http_client::EncodedJsonBody;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use codex_http_client::HttpTransport;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::Request as CodexRequest;
use codex_http_client::RequestBody;
use codex_http_client::RequestCompression;
use codex_http_client::ReqwestTransport;
use codex_http_client::TransportError;
use codex_websocket_client::WebSocketConnector;
use futures::SinkExt;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderName;
use http::Method;
use http::StatusCode;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::extensions::ExtensionsConfig;
use tokio_tungstenite::tungstenite::extensions::compression::deflate::DeflateConfig;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Control headers the pool sets for this hop. They are consumed here and are
/// never sent upstream.
pub const UPSTREAM_URL: &str = "x-codex-upstream-url";
// Names the pool account a request is for. Nothing is keyed on it: every
// request gets a fresh client (see Transports::client).
pub const UPSTREAM_IDENTITY: &str = "x-codex-upstream-identity";
pub const UPSTREAM_COMPRESSION: &str = "x-codex-upstream-compression";
// The wire order for a request that is not a Responses request: header names
// before the body step, `|`, then names after it (for example
// "user-agent,authorization,chatgpt-account-id|"). The pool's own backend
// calls and passthrough paths have native orders of their own (captured from
// a native Codex); the pool's HTTP hop cannot carry an order, so it names it.
pub const UPSTREAM_HEADER_ORDER: &str = "x-codex-upstream-header-order";
/// Set on a response this process generated itself because the upstream could
/// not be reached, so the pool treats it as a transport failure rather than an
/// upstream answer.
pub const UPSTREAM_ERROR: &str = "x-codex-upstream-error";

/// Header order of a native Codex 0.163.0 Responses request to the Codex
/// backend, captured on the wire. Provider headers and turn headers come first,
/// then `accept`; the body step adds `content-encoding` and `content-type`;
/// auth adds `authorization` and `chatgpt-account-id`; the client's default
/// headers add `originator` and `user-agent` last. Codex headers not listed
/// here keep their relative order after `thread-id`.
///
/// The subagent, memgen, attestation and responses-lite headers never appear
/// in a CLI capture (a subagent, a memory job, the Desktop app, a lite model
/// send them). Their places come from Codex's own assembly instead: core
/// client.rs build_responses_options extends the compatibility headers
/// (responses_metadata.rs: window id, turn metadata, parent thread, subagent;
/// then memgen), then inserts attestation and responses-lite, and the routing
/// hint is inserted after the options are built. Unlisted, they were written
/// after thread-id, where no native Codex puts them.
pub const LEADING: &[&str] = &[
    "version",
    "x-codex-beta-features",
    "x-codex-turn-state",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-codex-parent-thread-id",
    "x-openai-subagent",
    "x-openai-memgen-request",
    "x-oai-attestation",
    "x-openai-internal-codex-responses-lite",
    "x-codex-routing-hint",
    "x-client-request-id",
    "session-id",
    "thread-id",
];
pub const BEFORE_BODY: &[&str] = &["accept"];
pub const AFTER_BODY: &[&str] = &[
    "authorization",
    "chatgpt-account-id",
    "x-openai-fedramp",
    "originator",
    "user-agent",
];
/// Hop-by-hop and framing headers belong to each connection, and the body step
/// owns content headers, so none of them are copied from the pool's request.
pub const NOT_FORWARDED: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "content-length",
    "content-type",
    "content-encoding",
    "accept-encoding",
    UPSTREAM_URL,
    UPSTREAM_IDENTITY,
    UPSTREAM_COMPRESSION,
    UPSTREAM_HEADER_ORDER,
];

/// Handshake header order of a native Codex 0.163.0 Responses WebSocket,
/// captured on the wire. The WebSocket library writes host, connection,
/// upgrade, version and key first and the extension offer last; Codex's own
/// headers sit between them in this order, and other Codex headers follow the
/// routing hint. A native handshake carries no turn state.
pub const WS_ORDER: &[&str] = &[
    "chatgpt-account-id",
    "authorization",
    "user-agent",
    "originator",
    "openai-beta",
    "version",
    "x-codex-beta-features",
    "x-client-request-id",
    "session-id",
    "thread-id",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    // Not in any capture; placed from core client.rs build_websocket_headers,
    // which adds the compatibility headers, then the routing hint, then
    // attestation and the timing opt-in.
    "x-codex-parent-thread-id",
    "x-openai-subagent",
    "x-openai-memgen-request",
    "x-codex-routing-hint",
    "x-oai-attestation",
    "x-responsesapi-include-timing-metrics",
];
pub const WS_NOT_FORWARDED: &[&str] = &[
    "host",
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-protocol",
    "x-codex-turn-state",
    "content-length",
    UPSTREAM_URL,
    UPSTREAM_IDENTITY,
    UPSTREAM_COMPRESSION,
];
/// Upstream handshake headers that belong to that connection, not to the
/// pool's leg, and are not copied onto the 101 the pool receives.
pub const WS_UPGRADE_HEADERS: &[&str] = &[
    "connection",
    "upgrade",
    "sec-websocket-accept",
    "sec-websocket-extensions",
    "sec-websocket-protocol",
    "content-length",
    "transfer-encoding",
];

pub struct Transports {
    // Codex's default factory (transport default proxy, no PSP cookie), from
    // which each WebSocket connection gets its own connector.
    factory: HttpClientFactory,
}

impl Transports {
    pub fn new() -> std::io::Result<Self> {
        let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
        // Fail at startup, not on the first socket, if Codex's TLS setup
        // (native roots, custom CA) cannot be built.
        WebSocketConnector::new(&factory).map_err(std::io::Error::other)?;
        Ok(Self { factory })
    }

    // A new connector per connection, as Codex builds one in every
    // ResponsesWebsocketClient::connect (codex-api endpoint/
    // responses_websocket.rs). A connector shared across connections keeps
    // OpenSSL's session cache, so a later socket resumed the earlier TLS
    // session and sent a pre_shared_key ClientHello no native Codex sends;
    // wire-capture.sh caught it once the pool opened a second socket.
    pub fn websocket(&self) -> std::io::Result<WebSocketConnector> {
        WebSocketConnector::new(&self.factory).map_err(std::io::Error::other)
    }

    // A new client per request, as Codex builds one for every Responses,
    // compaction, memories and realtime call (core/src/client.rs
    // build_api_transport -> login default_client create_client_for_route).
    // So each request opens its own connection with a full TLS handshake.
    // A client kept per account reused connections and resumed TLS sessions
    // (a pre_shared_key ClientHello), neither of which a native Codex does;
    // wire-capture.sh caught the resumption. No account's request can ride
    // another's connection either way. Cloudflare infrastructure cookies are
    // process-global in Codex itself (codex-http-client keeps only those and
    // the __oailb routing cookie, never account cookies), so they stay shared.
    pub fn client(&self) -> ReqwestTransport {
        // Codex's default client: Cloudflare cookie store, transport default
        // proxy and TLS, custom-CA fallback. Request logging is off because
        // these requests carry account credentials.
        #[allow(deprecated)]
        let client = HttpClientBuilder::new()
            .with_chatgpt_cloudflare_cookie_store()
            .without_request_logging()
            .build_with_transport_default_proxy_and_custom_ca_fallback();
        ReqwestTransport::from_http_client(client)
    }
}

/// Splits the pool's headers by an explicit order (see UPSTREAM_HEADER_ORDER).
/// Headers the order does not name are dropped: the pool names every header
/// such a request carries, so anything else is not part of the native request.
pub fn named_order_headers(incoming: &HeaderMap, order: &str) -> (HeaderMap, HeaderMap) {
    let (first, second) = order.split_once('|').unwrap_or((order, ""));
    let pick = |names: &str| {
        let mut headers = HeaderMap::new();
        for name in names
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let lower = name.to_ascii_lowercase();
            if NOT_FORWARDED.contains(&lower.as_str()) {
                continue;
            }
            copy_all(incoming, &lower, &mut headers);
        }
        headers
    };
    (pick(first), pick(second))
}

/// Splits the pool's headers into the three groups Codex builds a request
/// from, each in native order.
pub fn ordered_headers(incoming: &HeaderMap) -> (HeaderMap, HeaderMap) {
    let mut before = HeaderMap::new();
    let mut after = HeaderMap::new();
    let is_listed = |name: &str| {
        LEADING.contains(&name) || BEFORE_BODY.contains(&name) || AFTER_BODY.contains(&name)
    };
    for name in LEADING {
        copy_all(incoming, name, &mut before);
    }
    let mut seen: Vec<HeaderName> = Vec::new();
    for name in incoming.keys() {
        let lower = name.as_str();
        if is_listed(lower) || NOT_FORWARDED.contains(&lower) || seen.contains(name) {
            continue;
        }
        seen.push(name.clone());
        for value in incoming.get_all(name) {
            before.append(name.clone(), value.clone());
        }
    }
    for name in BEFORE_BODY {
        copy_all(incoming, name, &mut before);
    }
    for name in AFTER_BODY {
        copy_all(incoming, name, &mut after);
    }
    (before, after)
}

pub fn copy_all(from: &HeaderMap, name: &str, to: &mut HeaderMap) {
    let Ok(header) = HeaderName::from_bytes(name.as_bytes()) else {
        return;
    };
    for value in from.get_all(&header) {
        to.append(header.clone(), value.clone());
    }
}

pub fn response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream {
        match name.as_str() {
            "connection" | "keep-alive" | "transfer-encoding" | "content-length" | "trailer"
            | "upgrade" => {}
            _ => {
                headers.append(name.clone(), value.clone());
            }
        }
    }
    headers
}

/// Codex's Responses WebSocket configuration: default limits plus the
/// permessage-deflate offer.
pub fn websocket_config() -> WebSocketConfig {
    let mut extensions = ExtensionsConfig::default();
    extensions.permessage_deflate = Some(DeflateConfig::default());
    let mut config = WebSocketConfig::default();
    config.extensions = extensions;
    config
}

/// Names of the headers the WebSocket library puts into a client request
/// itself (`into_client_request`) and later removes to write them first.
pub const WS_LIBRARY_HEADERS: usize = 5;

/// Builds the header map to append to the library's client request so the
/// handshake is written in native wire order. The library removes its own
/// five headers before writing the rest, and each removal swaps the map's last
/// entry into the freed slot, so the last five appended headers come out
/// first and in reverse. A native Codex goes through the same writer; this
/// appends the wire order with its first five names rotated to the end,
/// reversed, so the writer restores it. Verified against a native capture.
pub fn ordered_websocket_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut names: Vec<HeaderName> = Vec::new();
    for name in WS_ORDER {
        if let Ok(header) = HeaderName::from_bytes(name.as_bytes())
            && incoming.contains_key(&header)
        {
            names.push(header);
        }
    }
    for name in incoming.keys() {
        let lower = name.as_str();
        if WS_ORDER.contains(&lower) || WS_NOT_FORWARDED.contains(&lower) || names.contains(name) {
            continue;
        }
        names.push(name.clone());
    }
    if names.len() >= WS_LIBRARY_HEADERS {
        let leading: Vec<HeaderName> = names.drain(..WS_LIBRARY_HEADERS).rev().collect();
        names.extend(leading);
    }
    let mut headers = HeaderMap::new();
    for name in names {
        for value in incoming.get_all(&name) {
            headers.append(name.clone(), value.clone());
        }
    }
    headers
}

pub async fn relay<L, U>(local: L, upstream: U)
where
    L: futures::Stream<Item = Result<Message, WsError>>
        + futures::Sink<Message, Error = WsError>
        + Unpin,
    U: futures::Stream<Item = Result<Message, WsError>>
        + futures::Sink<Message, Error = WsError>
        + Unpin,
{
    let (mut local_tx, mut local_rx) = local.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();
    let to_upstream = async {
        while let Some(Ok(message)) = local_rx.next().await {
            let closing = message.is_close();
            // Pings and pongs on the pool's leg are answered by that leg.
            if message.is_ping() || message.is_pong() {
                continue;
            }
            if upstream_tx.send(message).await.is_err() || closing {
                break;
            }
        }
        let _ = upstream_tx.close().await;
    };
    let to_local = async {
        while let Some(Ok(message)) = upstream_rx.next().await {
            let closing = message.is_close();
            if message.is_ping() || message.is_pong() {
                continue;
            }
            if local_tx.send(message).await.is_err() || closing {
                break;
            }
        }
        let _ = local_tx.close().await;
    };
    tokio::select! {
        () = to_upstream => {}
        () = to_local => {}
    }
}

/// Upstream's answer: status, its headers (hop-by-hop ones removed) and the
/// body as it arrives. A non-2xx answer is an answer too, with its body.
pub struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: crate::pool::stream::ByteStream,
}

/// Upstream could not be reached at all, as opposed to answering with an
/// error: the pool treats it as a transport failure and may fail over.
#[derive(Debug)]
pub enum SendFailure {
    Timeout,
    Connection(String),
    /// The request itself could not be assembled.
    Prepare(String),
}

impl std::fmt::Display for SendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => f.write_str("request timed out"),
            Self::Connection(detail) => write!(f, "connection failed: {detail}"),
            Self::Prepare(detail) => write!(f, "request could not be prepared: {detail}"),
        }
    }
}

/// Assembles a request as Codex's request type does: the headers set before
/// the body step in native order (`order` names them for a non-Responses
/// request, see UPSTREAM_HEADER_ORDER), the body step, and the headers a
/// client adds after it. The bytes are encoded as given: re-serializing a
/// parsed value would reorder keys. A body that is not JSON cannot be
/// compressed by Codex's request type and is sent raw.
pub fn build_request(
    method: Method,
    url: String,
    headers: &HeaderMap,
    body: Bytes,
    compression: RequestCompression,
    order: Option<&str>,
) -> Result<(CodexRequest, HeaderMap), String> {
    let (before, after) = match order {
        Some(order) => named_order_headers(headers, order),
        None => ordered_headers(headers),
    };
    let mut codex_request = CodexRequest::new(method.clone(), url);
    codex_request.headers = before;
    let is_json = method != Method::GET
        && serde_json::from_slice::<&serde_json::value::RawValue>(&body).is_ok();
    if is_json {
        let raw: &serde_json::value::RawValue =
            serde_json::from_slice(&body).map_err(|error| format!("request body: {error}"))?;
        let encoded =
            EncodedJsonBody::encode(raw).map_err(|error| format!("request body: {error}"))?;
        codex_request.body = Some(RequestBody::EncodedJson(encoded));
        codex_request.compression = compression;
    } else if !body.is_empty() {
        codex_request.body = Some(RequestBody::Raw(body));
    }
    Ok((codex_request, after))
}

/// Sends with Codex's client: Codex's request assembly (the body step adds
/// its content headers), the headers a client adds after it, and a fresh
/// client per request (Transports::client).
pub async fn send_request(
    transports: &Transports,
    codex_request: CodexRequest,
    after: &HeaderMap,
) -> Result<UpstreamResponse, SendFailure> {
    let mut prepared = codex_request
        .into_prepared()
        .map_err(|error| SendFailure::Prepare(format!("prepare request: {error}")))?;
    for (name, value) in after {
        prepared.headers.append(name.clone(), value.clone());
    }
    match transports.client().stream(prepared).await {
        Ok(streamed) => Ok(UpstreamResponse {
            status: streamed.status,
            headers: response_headers(&streamed.headers),
            body: Box::pin(
                streamed
                    .bytes
                    .map(|chunk| chunk.map_err(|error| error.to_string())),
            ),
        }),
        Err(TransportError::Http {
            status,
            headers,
            body,
            ..
        }) => Ok(UpstreamResponse {
            status,
            headers: headers.as_ref().map(response_headers).unwrap_or_default(),
            body: crate::pool::stream::byte_stream_from(body.unwrap_or_default()),
        }),
        Err(TransportError::Timeout) => Err(SendFailure::Timeout),
        Err(error) => Err(SendFailure::Connection(error.to_string())),
    }
}

/// Assembles one of the pool's own calls, as composed by own_call.rs: the
/// headers before the body step, the JSON body (the body step adds its
/// content-type), and the headers a client adds after it.
pub fn build_own_call(
    composed: crate::pool::own_call::Composed,
) -> Result<(CodexRequest, HeaderMap), String> {
    let mut codex_request = CodexRequest::new(composed.method, composed.url);
    codex_request.headers = composed.before_body;
    if let Some(json) = composed.json_body {
        let raw = serde_json::from_slice::<&serde_json::value::RawValue>(&json)
            .map_err(|error| error.to_string())?;
        let encoded = EncodedJsonBody::encode(raw).map_err(|error| error.to_string())?;
        codex_request.body = Some(RequestBody::EncodedJson(encoded));
    }
    Ok((codex_request, composed.after_body))
}

pub type UpstreamSocket = codex_websocket_client::WebSocketConnection;

/// How a WebSocket handshake to upstream ended without a socket.
pub enum WebSocketRefusal {
    /// Upstream answered the handshake with this status, headers and body.
    Http {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    },
    /// Upstream could not be reached.
    Connection(String),
}

/// Opens an upstream Responses WebSocket with Codex's own connector and its
/// permessage-deflate configuration, the handshake headers in native order.
/// Returns the socket and the upstream handshake's response headers.
pub async fn connect_websocket(
    transports: &Transports,
    url: &str,
    headers: &HeaderMap,
) -> Result<(UpstreamSocket, HeaderMap), WebSocketRefusal> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url
        .into_client_request()
        .map_err(|error| WebSocketRefusal::Connection(format!("upstream url: {error}")))?;
    request
        .headers_mut()
        .extend(ordered_websocket_headers(headers));
    let connector = transports
        .websocket()
        .map_err(|error| WebSocketRefusal::Connection(error.to_string()))?;
    match connector.connect(request, websocket_config()).await {
        Ok((socket, handshake)) => Ok((socket, handshake.headers().clone())),
        Err(WsError::Http(refused)) => {
            let (parts, body) = refused.into_parts();
            Err(WebSocketRefusal::Http {
                status: parts.status,
                headers: parts.headers,
                body: body.unwrap_or_default(),
            })
        }
        Err(error) => Err(WebSocketRefusal::Connection(error.to_string())),
    }
}
