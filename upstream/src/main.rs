//! Loopback transport that sends the pool's Codex-backend requests the way a
//! native Codex CLI sends them.
//!
//! The pool still selects the account, owns its credential, and decides every
//! retry and failover. This process only performs the final hop with Codex's
//! own HTTP client crate, so the TLS handshake, HTTP/2 framing, Cloudflare
//! cookie handling, zstd request compression and header order are Codex's
//! rather than a Go client's. The Responses WebSocket is relayed the same way:
//! the upstream connection is opened with Codex's own WebSocket connector and
//! its permessage-deflate configuration, and messages pass through unchanged,
//! so prewarm and incremental requests reach upstream exactly as Codex sent
//! them. Keep it a transport: routing decisions here would split
//! responsibility with the pool the way the retired CLIProxyAPI sidecar did.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

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
use http::HeaderValue;
use http::Method;
use http::Response;
use http::StatusCode;
use http_body_util::BodyExt;
use http_body_util::Full;
use http_body_util::StreamBody;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::Frame;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::extensions::ExtensionsConfig;
use tokio_tungstenite::tungstenite::extensions::compression::deflate::DeflateConfig;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Control headers the pool sets for this hop. They are consumed here and are
/// never sent upstream.
const UPSTREAM_URL: &str = "x-codex-upstream-url";
// Names the pool account a request is for. Nothing is keyed on it: every
// request gets a fresh client (see Transports::client).
const UPSTREAM_IDENTITY: &str = "x-codex-upstream-identity";
const UPSTREAM_COMPRESSION: &str = "x-codex-upstream-compression";
// The wire order for a request that is not a Responses request: header names
// before the body step, `|`, then names after it (for example
// "user-agent,authorization,chatgpt-account-id|"). The pool's own backend
// calls and passthrough paths have native orders of their own (captured from
// a native Codex); the pool's HTTP hop cannot carry an order, so it names it.
const UPSTREAM_HEADER_ORDER: &str = "x-codex-upstream-header-order";
/// Set on a response this process generated itself because the upstream could
/// not be reached, so the pool treats it as a transport failure rather than an
/// upstream answer.
const UPSTREAM_ERROR: &str = "x-codex-upstream-error";

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
const LEADING: &[&str] = &[
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
const BEFORE_BODY: &[&str] = &["accept"];
const AFTER_BODY: &[&str] = &[
    "authorization",
    "chatgpt-account-id",
    "x-openai-fedramp",
    "originator",
    "user-agent",
];
/// Hop-by-hop and framing headers belong to each connection, and the body step
/// owns content headers, so none of them are copied from the pool's request.
const NOT_FORWARDED: &[&str] = &[
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
const WS_ORDER: &[&str] = &[
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
const WS_NOT_FORWARDED: &[&str] = &[
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
const WS_UPGRADE_HEADERS: &[&str] = &[
    "connection",
    "upgrade",
    "sec-websocket-accept",
    "sec-websocket-extensions",
    "sec-websocket-protocol",
    "content-length",
    "transfer-encoding",
];

type ResponseBody = UnsyncBoxBody<Bytes, std::io::Error>;

struct Transports {
    // Codex's default factory (transport default proxy, no PSP cookie), from
    // which each WebSocket connection gets its own connector.
    factory: HttpClientFactory,
}

impl Transports {
    fn new() -> std::io::Result<Self> {
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
    fn websocket(&self) -> std::io::Result<WebSocketConnector> {
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
    fn client(&self) -> ReqwestTransport {
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
fn named_order_headers(incoming: &HeaderMap, order: &str) -> (HeaderMap, HeaderMap) {
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

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Splits the pool's headers into the three groups Codex builds a request
/// from, each in native order.
fn ordered_headers(incoming: &HeaderMap) -> (HeaderMap, HeaderMap) {
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

fn copy_all(from: &HeaderMap, name: &str, to: &mut HeaderMap) {
    let Ok(header) = HeaderName::from_bytes(name.as_bytes()) else {
        return;
    };
    for value in from.get_all(&header) {
        to.append(header.clone(), value.clone());
    }
}

fn plain(
    status: StatusCode,
    headers: Option<HeaderMap>,
    body: impl Into<Bytes>,
) -> Response<ResponseBody> {
    let mut response = Response::new(
        Full::new(body.into())
            .map_err(|never: Infallible| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    if let Some(headers) = headers {
        *response.headers_mut() = response_headers(&headers);
    }
    response
}

fn transport_failure(kind: &str, detail: String) -> Response<ResponseBody> {
    let mut response = plain(StatusCode::BAD_GATEWAY, None, detail);
    if let Ok(value) = HeaderValue::from_str(kind) {
        response.headers_mut().insert(UPSTREAM_ERROR, value);
    }
    response
}

fn response_headers(upstream: &HeaderMap) -> HeaderMap {
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

async fn forward(
    transports: Arc<Transports>,
    request: hyper::Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    let (parts, body) = request.into_parts();
    let Some(url) = header_str(&parts.headers, UPSTREAM_URL).map(str::to_string) else {
        return Ok(plain(
            StatusCode::BAD_REQUEST,
            None,
            "missing x-codex-upstream-url",
        ));
    };
    let compression = match header_str(&parts.headers, UPSTREAM_COMPRESSION) {
        Some("zstd") => RequestCompression::Zstd,
        _ => RequestCompression::None,
    };
    let body = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return Ok(plain(
                StatusCode::BAD_REQUEST,
                None,
                format!("request body: {error}"),
            ));
        }
    };
    let (before, after) = match header_str(&parts.headers, UPSTREAM_HEADER_ORDER) {
        Some(order) => named_order_headers(&parts.headers, order),
        None => ordered_headers(&parts.headers),
    };

    let mut codex_request = CodexRequest::new(parts.method.clone(), url);
    codex_request.headers = before;
    // The pool's bytes are what the client sent, so they are encoded as-is:
    // re-serializing a parsed value would reorder keys. A body that is not
    // JSON cannot be compressed by Codex's request type and is sent raw.
    let is_json = parts.method != Method::GET
        && serde_json::from_slice::<&serde_json::value::RawValue>(&body).is_ok();
    if is_json {
        let raw: &serde_json::value::RawValue = match serde_json::from_slice(&body) {
            Ok(raw) => raw,
            Err(error) => {
                return Ok(plain(
                    StatusCode::BAD_REQUEST,
                    None,
                    format!("request body: {error}"),
                ));
            }
        };
        match EncodedJsonBody::encode(raw) {
            Ok(encoded) => {
                codex_request.body = Some(RequestBody::EncodedJson(encoded));
                codex_request.compression = compression;
            }
            Err(error) => {
                return Ok(plain(
                    StatusCode::BAD_REQUEST,
                    None,
                    format!("request body: {error}"),
                ));
            }
        }
    } else if !body.is_empty() {
        codex_request.body = Some(RequestBody::Raw(body));
    }
    let mut prepared = match codex_request.into_prepared() {
        Ok(prepared) => prepared,
        Err(error) => {
            return Ok(plain(
                StatusCode::BAD_REQUEST,
                None,
                format!("prepare request: {error}"),
            ));
        }
    };
    for (name, value) in &after {
        prepared.headers.append(name.clone(), value.clone());
    }

    match transports.client().stream(prepared).await {
        Ok(streamed) => {
            let frames = streamed.bytes.map(|chunk| match chunk {
                Ok(bytes) => Ok(Frame::data(bytes)),
                Err(error) => Err(std::io::Error::other(error.to_string())),
            });
            let mut response = Response::new(BodyExt::boxed_unsync(StreamBody::new(frames)));
            *response.status_mut() = streamed.status;
            *response.headers_mut() = response_headers(&streamed.headers);
            Ok(response)
        }
        Err(TransportError::Http {
            status,
            headers,
            body,
            ..
        }) => Ok(plain(status, headers, body.unwrap_or_default())),
        Err(TransportError::Timeout) => {
            Ok(transport_failure("timeout", "upstream timeout".to_string()))
        }
        Err(error) => Ok(transport_failure("connection", error.to_string())),
    }
}

/// Codex's Responses WebSocket configuration: default limits plus the
/// permessage-deflate offer.
fn websocket_config() -> WebSocketConfig {
    let mut extensions = ExtensionsConfig::default();
    extensions.permessage_deflate = Some(DeflateConfig::default());
    let mut config = WebSocketConfig::default();
    config.extensions = extensions;
    config
}

/// Names of the headers the WebSocket library puts into a client request
/// itself (`into_client_request`) and later removes to write them first.
const WS_LIBRARY_HEADERS: usize = 5;

/// Builds the header map to append to the library's client request so the
/// handshake is written in native wire order. The library removes its own
/// five headers before writing the rest, and each removal swaps the map's last
/// entry into the freed slot, so the last five appended headers come out
/// first and in reverse. A native Codex goes through the same writer; this
/// appends the wire order with its first five names rotated to the end,
/// reversed, so the writer restores it. Verified against a native capture.
fn ordered_websocket_headers(incoming: &HeaderMap) -> HeaderMap {
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

/// Opens the upstream Responses WebSocket for one pool connection and, once it
/// is up, accepts the pool's upgrade and relays messages both ways unchanged.
/// An upstream handshake refusal is answered as the same HTTP status, headers
/// and body without upgrading, so the pool can fail over before the client
/// has sent anything upstream.
async fn websocket(
    transports: Arc<Transports>,
    mut request: hyper::Request<Incoming>,
) -> Response<ResponseBody> {
    let Some(url) = header_str(request.headers(), UPSTREAM_URL).map(str::to_string) else {
        return plain(
            StatusCode::BAD_REQUEST,
            None,
            "missing x-codex-upstream-url",
        );
    };
    let Some(key) = header_str(request.headers(), "sec-websocket-key").map(str::to_string) else {
        return plain(StatusCode::BAD_REQUEST, None, "missing sec-websocket-key");
    };
    let mut upstream_request = match url.as_str().into_client_request() {
        Ok(upstream_request) => upstream_request,
        Err(error) => {
            return plain(
                StatusCode::BAD_REQUEST,
                None,
                format!("upstream url: {error}"),
            );
        }
    };
    upstream_request
        .headers_mut()
        .extend(ordered_websocket_headers(request.headers()));
    let connector = match transports.websocket() {
        Ok(connector) => connector,
        Err(error) => return transport_failure("connection", error.to_string()),
    };
    let (upstream, handshake) = match connector
        .connect(upstream_request, websocket_config())
        .await
    {
        Ok(connected) => connected,
        Err(WsError::Http(refused)) => {
            let (parts, body) = refused.into_parts();
            return plain(parts.status, Some(parts.headers), body.unwrap_or_default());
        }
        Err(error) => return transport_failure("connection", error.to_string()),
    };

    let mut response = plain(StatusCode::SWITCHING_PROTOCOLS, None, Bytes::new());
    for (name, value) in handshake.headers() {
        if !WS_UPGRADE_HEADERS.contains(&name.as_str()) {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONNECTION,
        HeaderValue::from_static("upgrade"),
    );
    headers.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
    if let Ok(accept) = HeaderValue::from_str(&derive_accept_key(key.as_bytes())) {
        headers.insert(http::header::SEC_WEBSOCKET_ACCEPT, accept);
    }
    tokio::spawn(async move {
        let Ok(upgraded) = hyper::upgrade::on(&mut request).await else {
            return;
        };
        // The pool's leg is loopback and uncompressed; only the upstream leg
        // negotiates Codex's deflate extension.
        let local =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
        relay(local, upstream).await;
    });
    response
}

async fn relay<L, U>(local: L, upstream: U)
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

async fn route(
    transports: Arc<Transports>,
    request: hyper::Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    if request.uri().path() == "/ws" {
        return Ok(websocket(transports, request).await);
    }
    forward(transports, request).await
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let addr: SocketAddr = std::env::var("CODEX_UPSTREAM_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8320".to_string())
        .parse()
        .map_err(std::io::Error::other)?;
    // Only the pool in the same container may use this hop; it holds no
    // credentials of its own, but it sends whatever it is handed.
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::other(
            "CODEX_UPSTREAM_ADDR must be a loopback address",
        ));
    }
    let listener = TcpListener::bind(addr).await?;
    let transports = Arc::new(Transports::new()?);
    eprintln!("codex-upstream listening on {addr}");
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = tokio::signal::ctrl_c() => return Ok(()),
        };
        let transports = transports.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| route(transports.clone(), request));
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}
