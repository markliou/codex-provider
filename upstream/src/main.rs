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
//! them.
//!
//! It is also the pool's front door (RUST_PORT.md step 3, `front`): with
//! `CODEX_FRONT_ADDR` set it accepts every client connection on the pool's
//! public address and relays it unchanged to the Go pool, which listens only
//! inside the container. That is a step of the move to Rust, not a second
//! router: until routing itself moves here (step 4), nothing in this process
//! picks an account or decides a retry, because two deciders would split
//! responsibility the way the retired CLIProxyAPI sidecar did.
//! `/classify` is no exception: it reports what a native Codex would make of
//! a failure, read with Codex's own code (codex_vendor::classify), and leaves
//! every retry and failover decision to the pool. Nor is `/own`: it writes the
//! pool's own calls (refresh, usage, account check, reset credits) as Codex
//! writes them (codex_vendor::pool::own_call), sends them, and hands upstream's
//! answer back; which call, for which account and when stay the pool's.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use codex_http_client::Request as CodexRequest;
use codex_http_client::RequestCompression;
use codex_vendor::pool::own_call;
use codex_vendor::pool::own_call::OwnCall;
use codex_vendor::transport::*;

mod front;
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
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

type ResponseBody = UnsyncBoxBody<Bytes, std::io::Error>;

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
    let order = header_str(&parts.headers, UPSTREAM_HEADER_ORDER).map(str::to_string);
    let (codex_request, after) = match build_request(
        parts.method.clone(),
        url,
        &parts.headers,
        body,
        compression,
        order.as_deref(),
    ) {
        Ok(built) => built,
        Err(error) => return Ok(plain(StatusCode::BAD_REQUEST, None, error)),
    };
    Ok(send(&transports, codex_request, &after).await)
}

/// The final hop of /forward and /own: Codex's request assembly (the body step
/// adds its content headers), the headers a client adds after it, and Codex's
/// client. Upstream's answer is streamed back as it arrives.
async fn send(
    transports: &Transports,
    codex_request: CodexRequest,
    after: &HeaderMap,
) -> Response<ResponseBody> {
    match send_request(transports, codex_request, after).await {
        Ok(upstream) => {
            let frames = upstream.body.map(|chunk| match chunk {
                Ok(bytes) => Ok(Frame::data(bytes)),
                Err(error) => Err(std::io::Error::other(error)),
            });
            let mut response = Response::new(BodyExt::boxed_unsync(StreamBody::new(frames)));
            *response.status_mut() = upstream.status;
            *response.headers_mut() = upstream.headers;
            response
        }
        Err(SendFailure::Timeout) => transport_failure("timeout", "upstream timeout".to_string()),
        Err(SendFailure::Connection(detail)) => transport_failure("connection", detail),
        Err(SendFailure::Prepare(detail)) => plain(StatusCode::BAD_REQUEST, None, detail),
    }
}

/// The pool's own calls (refresh, usage, account check, reset credits): the
/// pool posts which call and what it is sent with, and this process writes the
/// request the way a native Codex writes it (codex_vendor::pool::own_call) and
/// sends it as /forward does, answering with upstream's response. The pool
/// still reads that answer and owns every decision and all state.
async fn own(
    transports: Arc<Transports>,
    request: hyper::Request<Incoming>,
) -> Response<ResponseBody> {
    let body = match http_body_util::Limited::new(request.into_body(), OWN_BODY_LIMIT)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return plain(StatusCode::BAD_REQUEST, None, format!("own call: {error}"));
        }
    };
    let call: OwnCall = match serde_json::from_slice(&body) {
        Ok(call) => call,
        Err(error) => {
            return plain(StatusCode::BAD_REQUEST, None, format!("own call: {error}"));
        }
    };
    let composed = match own_call::compose(&call) {
        Ok(composed) => composed,
        Err(error) => {
            return plain(StatusCode::BAD_REQUEST, None, format!("own call: {error}"));
        }
    };
    let (codex_request, after) = match build_own_call(composed) {
        Ok(built) => built,
        Err(error) => {
            return plain(StatusCode::BAD_REQUEST, None, format!("own call: {error}"));
        }
    };
    send(&transports, codex_request, &after).await
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

/// Largest failure the pool hands /classify: an error event or an HTTP error
/// body, never a whole stream.
const CLASSIFY_BODY_LIMIT: usize = 1 << 20;
/// An own call is a few short strings; anything larger is not one.
const OWN_BODY_LIMIT: usize = 64 << 10;

/// Codex's verdict on one upstream failure the pool met (codex_vendor::classify).
/// The pool posts {"source": "sse"|"ws"|"http", "data": <event text>} or, for
/// an HTTP answer, {"source": "http", "status", "headers": {name: value},
/// "body"}. Nothing is sent anywhere; the answer only reports what a native
/// Codex would conclude, and the pool decides what to do with it.
async fn classify(request: hyper::Request<Incoming>) -> Response<ResponseBody> {
    let body = match http_body_util::Limited::new(request.into_body(), CLASSIFY_BODY_LIMIT)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return plain(
                StatusCode::BAD_REQUEST,
                None,
                format!("classify body: {error}"),
            );
        }
    };
    let input: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(input) => input,
        Err(error) => {
            return plain(
                StatusCode::BAD_REQUEST,
                None,
                format!("classify body: {error}"),
            );
        }
    };
    let text = |name: &str| input.get(name).and_then(serde_json::Value::as_str);
    let verdict = match text("source") {
        Some("sse") => codex_vendor::classify::sse_event(text("data").unwrap_or_default()),
        Some("ws") => codex_vendor::classify::ws_message(text("data").unwrap_or_default()),
        Some("http") => {
            let status = input
                .get("status")
                .and_then(serde_json::Value::as_u64)
                .and_then(|status| u16::try_from(status).ok())
                .and_then(|status| StatusCode::from_u16(status).ok());
            let Some(status) = status else {
                return plain(StatusCode::BAD_REQUEST, None, "classify: missing status");
            };
            let mut headers = HeaderMap::new();
            if let Some(map) = input.get("headers").and_then(serde_json::Value::as_object) {
                for (name, value) in map {
                    if let (Ok(name), Some(Ok(value))) = (
                        HeaderName::from_bytes(name.as_bytes()),
                        value.as_str().map(HeaderValue::from_str),
                    ) {
                        headers.append(name, value);
                    }
                }
            }
            codex_vendor::classify::http_response(status, headers, text("body").map(str::to_string))
        }
        _ => return plain(StatusCode::BAD_REQUEST, None, "classify: unknown source"),
    };
    match serde_json::to_vec(&verdict) {
        Ok(json) => {
            let mut response = plain(StatusCode::OK, None, json);
            response.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            response
        }
        Err(error) => plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
            format!("classify: {error}"),
        ),
    }
}

async fn route(
    transports: Arc<Transports>,
    request: hyper::Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    if request.uri().path() == "/ws" {
        return Ok(websocket(transports, request).await);
    }
    if request.uri().path() == "/classify" && request.method() == Method::POST {
        return Ok(classify(request).await);
    }
    if request.uri().path() == "/own" && request.method() == Method::POST {
        return Ok(own(transports, request).await);
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
    // Bound before the transport answers, so the supervisor's readiness check
    // fails rather than passes when the public address cannot be taken.
    if let Ok(front_addr) = std::env::var("CODEX_FRONT_ADDR") {
        let pool: SocketAddr = std::env::var("CODEX_FRONT_POOL_ADDR")
            .map_err(|_| std::io::Error::other("CODEX_FRONT_ADDR needs CODEX_FRONT_POOL_ADDR"))?
            .parse()
            .map_err(std::io::Error::other)?;
        if !pool.ip().is_loopback() {
            return Err(std::io::Error::other(
                "CODEX_FRONT_POOL_ADDR must be a loopback address",
            ));
        }
        let allow_remote = std::env::var("CODEX_POOL_ALLOW_REMOTE_ADMIN").as_deref() == Ok("true");
        let front = front::bind(&front_addr, allow_remote).await?;
        eprintln!(
            "codex-upstream front door listening on {}, relaying to the pool at {pool}",
            front.local_addr()?
        );
        tokio::spawn(front::serve(front, pool));
    }
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
