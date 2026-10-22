//! The pool's front door (RUST_PORT.md step 3).
//!
//! Every client connection (Codex CLI and Desktop, the admin UI, the public
//! dashboard, Caddy) is accepted here and relayed to the Go pool, which now
//! listens only inside the container. Nothing is decided here yet: requests
//! and answers pass through byte for byte, streams and WebSocket upgrades
//! included, so to a client the pool behaves exactly as before, and what the
//! pool sends upstream does not change either (the pool still hands its
//! upstream requests to `/forward` and `/ws` of this same process). Later steps
//! move routing here and shrink what is relayed.
//!
//! Two things the Go pool decides by the connection's address must keep
//! working although every connection to it now comes from loopback: the
//! loopback-only `/internal/inflight` the deploy script reads, and the admin
//! login lockout, keyed on the client (or, behind a trusted proxy, on its
//! `X-Forwarded-For`). So each relayed request carries the address this door
//! accepted it from in `PEER_HEADER`, overwriting anything a client sent, and
//! the pool takes that as the request's remote address (Go withFrontPeer).
//! `X-Forwarded-For` itself is passed through untouched: Caddy's entry is the
//! one the pool's trusted-proxy rule reads. Do not drop the peer header or let
//! a client's copy through: either would make every client look like loopback.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http::HeaderMap;
use http::HeaderValue;
use http::Request;
use http::Response;
use http::StatusCode;
use http::Uri;
use http::Version;
use http::header;
use http_body_util::BodyExt;
use http_body_util::Full;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::rt::TokioTimer;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

/// The address the front door accepted the request's connection from. Set
/// here on every relayed request; the pool believes it only from loopback.
pub const PEER_HEADER: &str = codex_vendor::pool::listener::FRONT_PEER_HEADER;

/// Go's http.Server ReadHeaderTimeout for the pool: a client that never
/// finishes its headers is dropped here as it was there.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

type Body = UnsyncBoxBody<Bytes, std::io::Error>;

/// Binds the public address. A Go-style `:port` means every IPv4 interface.
/// A non-loopback bind needs `CODEX_POOL_ALLOW_REMOTE_ADMIN=true`, the same
/// acknowledgement the pool demanded when it bound this address itself: the
/// admin UI is reachable wherever `/v1` is.
pub async fn bind(address: &str, allow_remote: bool) -> std::io::Result<TcpListener> {
    let address = match address.strip_prefix(':') {
        Some(port) => format!("0.0.0.0:{port}"),
        None => address.to_string(),
    };
    let listener = TcpListener::bind(&address).await?;
    if !allow_remote && !listener.local_addr()?.ip().is_loopback() {
        return Err(std::io::Error::other(
            "combined HTTP address must be loopback unless CODEX_POOL_ALLOW_REMOTE_ADMIN=true",
        ));
    }
    Ok(listener)
}

pub async fn serve(listener: TcpListener, pool: SocketAddr) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                // Out of descriptors and the like: wait instead of spinning.
                eprintln!("codex-upstream front: accept: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        tokio::spawn(async move {
            let service = service_fn(move |request| relay(pool, peer, request));
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

/// Relays one request to the pool over a connection of its own and streams
/// the answer back. An upgrade (the Responses WebSocket) is relayed by
/// splicing the two upgraded connections once the pool answers 101.
async fn relay(
    pool: SocketAddr,
    peer: SocketAddr,
    mut request: Request<Incoming>,
) -> Result<Response<Body>, Infallible> {
    let upgrade = is_upgrade(request.headers());
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut request));
    let (mut parts, body) = request.into_parts();
    let host = parts.headers.get(header::HOST).cloned();
    remove_hop_by_hop(&mut parts.headers, upgrade);
    if let Some(host) = host {
        parts.headers.insert(header::HOST, host);
    } else if let Ok(host) = HeaderValue::from_str(&pool.to_string()) {
        parts.headers.insert(header::HOST, host);
    }
    match HeaderValue::from_str(&peer.to_string()) {
        Ok(value) => {
            parts.headers.insert(PEER_HEADER, value);
        }
        Err(_) => {
            parts.headers.remove(PEER_HEADER);
        }
    }
    parts.uri = origin_form(&parts.uri);
    parts.version = Version::HTTP_11;

    let stream = match TcpStream::connect(pool).await {
        Ok(stream) => stream,
        Err(error) => return Ok(unavailable(format!("pool unreachable: {error}"))),
    };
    let _ = stream.set_nodelay(true);
    let (mut sender, connection) =
        match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
            Ok(handshake) => handshake,
            Err(error) => return Ok(unavailable(format!("pool handshake: {error}"))),
        };
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut response = match sender.send_request(Request::from_parts(parts, body)).await {
        Ok(response) => response,
        Err(error) => return Ok(unavailable(format!("pool request: {error}"))),
    };

    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client_upgrade) = client_upgrade {
            let pool_upgrade = hyper::upgrade::on(&mut response);
            tokio::spawn(async move {
                if let (Ok(client), Ok(pool)) = tokio::join!(client_upgrade, pool_upgrade) {
                    let _ = tokio::io::copy_bidirectional(
                        &mut TokioIo::new(client),
                        &mut TokioIo::new(pool),
                    )
                    .await;
                }
            });
        }
    } else {
        remove_hop_by_hop(response.headers_mut(), false);
    }
    Ok(response.map(|body| body.map_err(std::io::Error::other).boxed_unsync()))
}

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
        && headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

/// Hop-by-hop headers belong to each connection (RFC 9110 7.6.1), including
/// any the Connection header names. An upgrade keeps Connection and Upgrade:
/// they are the request the pool answers with 101.
fn remove_hop_by_hop(headers: &mut HeaderMap, upgrade: bool) {
    let named: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();
    for name in named {
        if upgrade && name == "upgrade" {
            continue;
        }
        headers.remove(name.as_str());
    }
    for name in [
        "keep-alive",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
    if !upgrade {
        headers.remove(header::CONNECTION);
        headers.remove(header::UPGRADE);
    }
}

fn origin_form(uri: &Uri) -> Uri {
    uri.path_and_query()
        .map(http::uri::PathAndQuery::as_str)
        .unwrap_or("/")
        .parse()
        .unwrap_or_else(|_| Uri::from_static("/"))
}

fn unavailable(detail: String) -> Response<Body> {
    eprintln!("codex-upstream front: {detail}");
    let mut response = Response::new(
        Full::new(Bytes::from_static(b"pool unavailable\n"))
            .map_err(|never: Infallible| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    // A stand-in for the Go pool: answers each request with what it received.
    async fn echo_pool() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{error}"));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let service = service_fn(|request: Request<Incoming>| async move {
                        if is_upgrade(request.headers()) {
                            let mut request = request;
                            let upgraded = hyper::upgrade::on(&mut request);
                            tokio::spawn(async move {
                                if let Ok(upgraded) = upgraded.await {
                                    let mut io = TokioIo::new(upgraded);
                                    let mut buffer = [0u8; 5];
                                    if io.read_exact(&mut buffer).await.is_ok() {
                                        let _ = io.write_all(&buffer).await;
                                    }
                                }
                            });
                            let mut response = Response::new(Full::new(Bytes::new()));
                            *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                            response
                                .headers_mut()
                                .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
                            response
                                .headers_mut()
                                .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
                            return Ok::<_, Infallible>(response);
                        }
                        let (parts, body) = request.into_parts();
                        let body = body
                            .collect()
                            .await
                            .map(http_body_util::Collected::to_bytes)
                            .unwrap_or_default();
                        let mut seen = format!("{} {}\n", parts.method, parts.uri);
                        for (name, value) in &parts.headers {
                            seen.push_str(&format!(
                                "{name}: {}\n",
                                value.to_str().unwrap_or_default()
                            ));
                        }
                        seen.push('\n');
                        seen.push_str(&String::from_utf8_lossy(&body));
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(seen))))
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        address
    }

    async fn front(pool: SocketAddr) -> SocketAddr {
        let listener = bind("127.0.0.1:0", false)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{error}"));
        tokio::spawn(serve(listener, pool));
        address
    }

    async fn exchange(front: SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(front)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        stream
            .write_all(raw.as_bytes())
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer).await;
        String::from_utf8_lossy(&answer).into_owned()
    }

    #[tokio::test]
    async fn relays_the_request_and_names_the_real_peer() {
        let front = front(echo_pool().await).await;
        let answer = exchange(
            front,
            "POST /v1/responses?x=1 HTTP/1.1\r\nHost: codex-api.example.test\r\n\
             Authorization: Bearer test-key\r\nX-Forwarded-For: 203.0.113.9\r\n\
             X-Codex-Pool-Peer: 10.0.0.1:1\r\nKeep-Alive: timeout=5\r\n\
             Content-Length: 11\r\nConnection: close\r\n\r\n{\"a\":\"b c\"}",
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.contains("POST /v1/responses?x=1\n"), "{answer}");
        assert!(
            answer.contains("host: codex-api.example.test\n"),
            "{answer}"
        );
        assert!(
            answer.contains("authorization: Bearer test-key\n"),
            "{answer}"
        );
        // Caddy's forwarded address is the pool's to judge, so it passes as is.
        assert!(
            answer.contains("x-forwarded-for: 203.0.113.9\n"),
            "{answer}"
        );
        // The peer is the connection's, never a client's claim.
        assert!(!answer.contains("10.0.0.1:1"), "{answer}");
        assert!(answer.contains("x-codex-pool-peer: 127.0.0.1:"), "{answer}");
        assert!(!answer.contains("keep-alive"), "{answer}");
        assert!(answer.ends_with("{\"a\":\"b c\"}"), "{answer}");
    }

    #[tokio::test]
    async fn splices_an_upgrade() {
        let front = front(echo_pool().await).await;
        let mut stream = TcpStream::connect(front)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        stream
            .write_all(
                b"GET /v1/responses HTTP/1.1\r\nHost: pool\r\nConnection: Upgrade\r\n\
                  Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream
                .read_exact(&mut byte)
                .await
                .unwrap_or_else(|error| panic!("{error}"));
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 101"), "{head}");
        assert!(head.contains("upgrade: websocket"), "{head}");
        stream
            .write_all(b"hello")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let mut echoed = [0u8; 5];
        stream
            .read_exact(&mut echoed)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(&echoed, b"hello");
    }

    #[tokio::test]
    async fn answers_502_when_the_pool_is_down() {
        let closed = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let pool = closed
            .local_addr()
            .unwrap_or_else(|error| panic!("{error}"));
        drop(closed);
        let answer = exchange(
            front(pool).await,
            "GET /healthz HTTP/1.1\r\nHost: pool\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 502"), "{answer}");
    }

    #[tokio::test]
    async fn refuses_a_remote_bind_without_the_acknowledgement() {
        assert!(bind("0.0.0.0:0", false).await.is_err());
        assert!(bind(":0", true).await.is_ok());
    }
}
