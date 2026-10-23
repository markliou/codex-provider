//! Connections to the Rust pool: hyper requests in, the route table's
//! answers out.
//!
//! A port of the Go server's request plumbing (withFrontPeer, recovery,
//! http.Server with ReadHeaderTimeout). The handler runs on its own task and
//! writes through a `ChannelWriter`: the status and headers go out when it
//! first writes, and body chunks stream as they are written, so a long
//! Responses stream and the early keepalive writer reach the client as they
//! did from Go.
//!
//! Two connection facts the handlers need are set here. The remote address:
//! the peer, or `FRONT_PEER_HEADER` when the connection comes from loopback
//! (the removed front door relayed it so); a client's copy of that header is
//! always removed, or every client could claim to be loopback. And the client going
//! away: when hyper drops the response body before the handler finished, the
//! request's `Cancel` fires, as Go's request context is cancelled.

use super::app::App;
use super::http_io::Cancel;
use super::http_io::ChannelWriter;
use super::routes::Request;
use super::stream::MAX_REQUEST_BODY;
use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::StreamBody;
use hyper::body::Frame;
use hyper::body::Incoming;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// The header the removed front door used to pass on the client's address. A
/// client's copy is stripped from every request and believed only from a
/// loopback connection (SPEC.md 2.3), so no client can claim to be local.
pub const FRONT_PEER_HEADER: &str = "x-codex-pool-peer";

/// Go's http.Server ReadHeaderTimeout for the pool.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

pub type ResponseBody =
    StreamBody<futures::stream::BoxStream<'static, Result<Frame<Bytes>, Infallible>>>;

/// Serves one accepted connection until it closes. Upgrades (the Responses
/// WebSocket) are handed to the handler. When `shutdown` fires the connection
/// finishes the request it is serving and then closes, and an idle
/// keep-alive connection closes at once (Go http.Server.Shutdown).
pub async fn serve_connection(
    app: Arc<App>,
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    shutdown: Cancel,
) {
    serve_http1(
        stream,
        move |request| handle(app.clone(), request, peer),
        shutdown,
    )
    .await;
}

/// One HTTP/1 connection with the pool's header timeout and upgrades, closed
/// gracefully on `shutdown`. `handler` answers each request.
pub async fn serve_http1<F, Fut>(stream: tokio::net::TcpStream, handler: F, shutdown: Cancel)
where
    F: Fn(hyper::Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = hyper::Response<ResponseBody>> + Send + 'static,
{
    let handler = Arc::new(handler);
    let service = hyper::service::service_fn(move |request| {
        let handler = handler.clone();
        async move { Ok::<_, Infallible>(handler(request).await) }
    });
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
        .with_upgrades();
    tokio::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => return,
        () = shutdown.cancelled() => {}
    }
    connection.as_mut().graceful_shutdown();
    let _ = connection.await;
}

/// The remote address the handlers judge a request by (Go withFrontPeer):
/// the front door's peer header, believed only from loopback.
pub fn remote_address(headers: &mut http::HeaderMap, peer: SocketAddr) -> String {
    let claimed = headers
        .remove(FRONT_PEER_HEADER)
        .and_then(|value| value.to_str().ok().map(str::to_string));
    match claimed {
        Some(claimed) if peer.ip().is_loopback() && split_host_port(&claimed) => claimed,
        _ => peer.to_string(),
    }
}

/// Whether Go's net.SplitHostPort accepts the address.
fn split_host_port(address: &str) -> bool {
    if let Some(rest) = address.strip_prefix('[') {
        return rest
            .split_once(']')
            .is_some_and(|(_, after)| after.starts_with(':') && !after[1..].contains(':'));
    }
    address
        .rsplit_once(':')
        .is_some_and(|(host, _)| !host.contains(':') && !host.contains('[') && !host.contains(']'))
}

pub async fn handle(
    app: Arc<App>,
    mut request: hyper::Request<Incoming>,
    peer: SocketAddr,
) -> hyper::Response<ResponseBody> {
    let upgrade = hyper::upgrade::on(&mut request);
    let (mut parts, body) = request.into_parts();
    let remote_addr = remote_address(&mut parts.headers, peer);
    // The handlers read at most MAX_REQUEST_BODY + 1 bytes, so an oversized
    // body is still recognized as one.
    let limit = u64::try_from(MAX_REQUEST_BODY).unwrap_or(u64::MAX) + 1;
    let body = http_body_util::Limited::new(body, usize::try_from(limit).unwrap_or(usize::MAX))
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_else(|_| Bytes::from(vec![0; MAX_REQUEST_BODY + 1]));
    let cancel = Cancel::default();
    let inbound = Request {
        method: parts.method,
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or_default().to_string(),
        headers: parts.headers,
        body,
        remote_addr,
        cancel: cancel.clone(),
        upgrade: Some(upgrade),
    };
    let (writer, response) = ChannelWriter::new();
    tokio::spawn(async move {
        super::routes::serve(&app, inbound, Box::new(writer)).await;
    });
    let (status, headers) = response.head.await.unwrap_or((
        http::StatusCode::INTERNAL_SERVER_ERROR,
        http::HeaderMap::new(),
    ));
    // When hyper drops this body before the handler closed it, the client is
    // gone: cancel the request as Go's context is. A body the handler closed
    // ends normally and cancels nothing: a WebSocket handler closes its 101
    // response's body and goes on serving the socket.
    let guard = CancelOnDrop(Some(cancel));
    let chunks =
        futures::stream::unfold((response.body, guard), |(mut body, mut guard)| async move {
            match body.recv().await {
                Some(chunk) => Some((Ok(Frame::data(chunk)), (body, guard))),
                None => {
                    guard.disarm();
                    None
                }
            }
        });
    let mut answer = hyper::Response::new(StreamBody::new(futures::StreamExt::boxed(chunks)));
    *answer.status_mut() = status;
    *answer.headers_mut() = headers;
    answer
}

struct CancelOnDrop(Option<Cancel>);

impl CancelOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.cancel();
        }
    }
}
