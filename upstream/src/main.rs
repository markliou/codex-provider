//! Loopback transport that sends the pool's Codex-backend requests the way a
//! native Codex CLI sends them.
//!
//! The pool still selects the account, owns its credential, and decides every
//! retry and failover. This process only performs the final hop with Codex's
//! own HTTP client crate, so the TLS handshake, HTTP/2 framing, Cloudflare
//! cookie handling, zstd request compression and header order are Codex's
//! rather than a Go client's. Keep it a transport: routing decisions here would
//! split responsibility with the pool the way the CLIProxyAPI sidecar did.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use codex_http_client::EncodedJsonBody;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpTransport;
use codex_http_client::ReqwestTransport;
use codex_http_client::Request as CodexRequest;
use codex_http_client::RequestBody;
use codex_http_client::RequestCompression;
use codex_http_client::TransportError;
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

/// Control headers the pool sets for this hop. They are consumed here and are
/// never sent upstream.
const UPSTREAM_URL: &str = "x-codex-upstream-url";
const UPSTREAM_IDENTITY: &str = "x-codex-upstream-identity";
const UPSTREAM_COMPRESSION: &str = "x-codex-upstream-compression";
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
const LEADING: &[&str] = &[
    "version",
    "x-codex-beta-features",
    "x-codex-turn-state",
    "x-codex-window-id",
    "x-codex-turn-metadata",
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
];

type ResponseBody = UnsyncBoxBody<Bytes, std::io::Error>;

#[derive(Default)]
struct Transports {
    // One client per upstream identity, as each native Codex process has its
    // own connection pool and Cloudflare cookie jar for its one account.
    // Sharing one would carry one account's cookies into another's requests.
    by_identity: Mutex<HashMap<String, ReqwestTransport>>,
}

impl Transports {
    fn for_identity(&self, identity: &str) -> ReqwestTransport {
        let mut clients = self.by_identity.lock().unwrap_or_else(|e| e.into_inner());
        clients
            .entry(identity.to_string())
            .or_insert_with(|| {
                // Codex's default client: Cloudflare cookie store, transport
                // default proxy and TLS, custom-CA fallback. Request logging is
                // off because these requests carry account credentials.
                #[allow(deprecated)]
                let client = HttpClientBuilder::new()
                    .with_chatgpt_cloudflare_cookie_store()
                    .without_request_logging()
                    .build_with_transport_default_proxy_and_custom_ca_fallback();
                ReqwestTransport::from_http_client(client)
            })
            .clone()
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok()).map(str::trim).filter(|value| !value.is_empty())
}

/// Splits the pool's headers into the three groups Codex builds a request
/// from, each in native order.
fn ordered_headers(incoming: &HeaderMap) -> (HeaderMap, HeaderMap) {
    let mut before = HeaderMap::new();
    let mut after = HeaderMap::new();
    let is_listed = |name: &str| LEADING.contains(&name) || BEFORE_BODY.contains(&name) || AFTER_BODY.contains(&name);
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

fn plain(status: StatusCode, headers: Option<HeaderMap>, body: impl Into<Bytes>) -> Response<ResponseBody> {
    let mut response = Response::new(Full::new(body.into()).map_err(|never: Infallible| match never {}).boxed_unsync());
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
            "connection" | "keep-alive" | "transfer-encoding" | "content-length" | "trailer" | "upgrade" => {}
            _ => {
                headers.append(name.clone(), value.clone());
            }
        }
    }
    headers
}

async fn forward(transports: Arc<Transports>, request: hyper::Request<Incoming>) -> Result<Response<ResponseBody>, Infallible> {
    let (parts, body) = request.into_parts();
    let Some(url) = header_str(&parts.headers, UPSTREAM_URL).map(str::to_string) else {
        return Ok(plain(StatusCode::BAD_REQUEST, None, "missing x-codex-upstream-url"));
    };
    let identity = header_str(&parts.headers, UPSTREAM_IDENTITY).unwrap_or("default").to_string();
    let compression = match header_str(&parts.headers, UPSTREAM_COMPRESSION) {
        Some("zstd") => RequestCompression::Zstd,
        _ => RequestCompression::None,
    };
    let body = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => return Ok(plain(StatusCode::BAD_REQUEST, None, format!("request body: {error}"))),
    };
    let (before, after) = ordered_headers(&parts.headers);

    let mut codex_request = CodexRequest::new(parts.method.clone(), url);
    codex_request.headers = before;
    // The pool's bytes are what the client sent, so they are encoded as-is:
    // re-serializing a parsed value would reorder keys. A body that is not
    // JSON cannot be compressed by Codex's request type and is sent raw.
    let is_json = parts.method != Method::GET && serde_json::from_slice::<&serde_json::value::RawValue>(&body).is_ok();
    if is_json {
        let raw: &serde_json::value::RawValue = match serde_json::from_slice(&body) {
            Ok(raw) => raw,
            Err(error) => return Ok(plain(StatusCode::BAD_REQUEST, None, format!("request body: {error}"))),
        };
        match EncodedJsonBody::encode(raw) {
            Ok(encoded) => {
                codex_request.body = Some(RequestBody::EncodedJson(encoded));
                codex_request.compression = compression;
            }
            Err(error) => return Ok(plain(StatusCode::BAD_REQUEST, None, format!("request body: {error}"))),
        }
    } else if !body.is_empty() {
        codex_request.body = Some(RequestBody::Raw(body));
    }
    let mut prepared = match codex_request.into_prepared() {
        Ok(prepared) => prepared,
        Err(error) => return Ok(plain(StatusCode::BAD_REQUEST, None, format!("prepare request: {error}"))),
    };
    for (name, value) in &after {
        prepared.headers.append(name.clone(), value.clone());
    }

    match transports.for_identity(&identity).stream(prepared).await {
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
        Err(TransportError::Http { status, headers, body, .. }) => Ok(plain(status, headers, body.unwrap_or_default())),
        Err(TransportError::Timeout) => Ok(transport_failure("timeout", "upstream timeout".to_string())),
        Err(error) => Ok(transport_failure("connection", error.to_string())),
    }
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
        return Err(std::io::Error::other("CODEX_UPSTREAM_ADDR must be a loopback address"));
    }
    let listener = TcpListener::bind(addr).await?;
    let transports = Arc::new(Transports::default());
    eprintln!("codex-upstream listening on {addr}");
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => accepted?,
            _ = tokio::signal::ctrl_c() => return Ok(()),
        };
        let transports = transports.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| forward(transports.clone(), request));
            let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
        });
    }
}
