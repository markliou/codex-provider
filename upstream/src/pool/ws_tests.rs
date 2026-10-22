//! The Go Responses WebSocket and native-endpoint tests (responses_ws_test.go,
//! native_codex_test.go), one by one. Each keeps its Go name in a comment;
//! the assertions are Go's.
//!
//! Go's fake stood in for codex-upstream's `/ws`; the transport is in-process
//! now, so the fake here is the Codex backend itself: a WebSocket server the
//! pool reaches with Codex's own connector. Where Go read the account from
//! X-Codex-Upstream-Identity, this fake reads it from ChatGPT-Account-ID
//! (acct-<id>), and an "unreachable" handshake is a connection dropped before
//! any answer, which is what the connector reports as unreachable.

use super::app::App;
use super::proxy_tests::codex_app;
use super::proxy_tests::serve;
use super::testing::TempDir;
use bytes::Bytes;
use futures::SinkExt;
use futures::StreamExt;
use http::HeaderMap;
use http::StatusCode;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Script = dyn Fn(&str) -> Vec<String> + Send + Sync;

#[derive(Default)]
struct Log {
    handshakes: Vec<HeaderMap>,
    paths: Vec<String>,
    messages: Vec<String>,
}

/// Go fakeCodexUpstream.
struct Behavior {
    /// identity -> handshake status
    refuse: HashMap<String, u16>,
    /// identity -> handshakes still to drop as unreachable
    unreachable: Mutex<HashMap<String, usize>>,
    /// The 101's extra headers (default: a turn state).
    handshake: Option<Vec<(&'static str, &'static str)>>,
    script: Box<Script>,
}

impl Behavior {
    fn scripted(script: impl Fn(&str) -> Vec<String> + Send + Sync + 'static) -> Self {
        Self {
            refuse: HashMap::new(),
            unreachable: Mutex::new(HashMap::new()),
            handshake: None,
            script: Box::new(script),
        }
    }
}

struct FakeCodexUpstream {
    url: String,
    log: Arc<Mutex<Log>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeCodexUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeCodexUpstream {
    async fn start(behavior: Behavior) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let url = format!(
            "http://{}",
            listener
                .local_addr()
                .unwrap_or_else(|error| panic!("{error}"))
        );
        let log = Arc::new(Mutex::new(Log::default()));
        let behavior = Arc::new(behavior);
        let shared = log.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (log, behavior) = (shared.clone(), behavior.clone());
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request| {
                        answer(request, log.clone(), behavior.clone())
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        Self { url, log, task }
    }

    fn identities(&self) -> Vec<String> {
        self.log
            .lock()
            .map(|log| log.handshakes.iter().map(identity).collect())
            .unwrap_or_default()
    }

    fn messages(&self) -> Vec<String> {
        self.log
            .lock()
            .map(|log| log.messages.clone())
            .unwrap_or_default()
    }

    fn handshake(&self, index: usize) -> HeaderMap {
        self.log
            .lock()
            .map(|log| log.handshakes[index].clone())
            .unwrap_or_default()
    }
}

fn identity(headers: &HeaderMap) -> String {
    let account = super::http_io::header_value(headers, "chatgpt-account-id");
    account.strip_prefix("acct-").unwrap_or(account).to_string()
}

type FakeBody = http_body_util::Empty<Bytes>;

async fn answer(
    mut request: hyper::Request<hyper::body::Incoming>,
    log: Arc<Mutex<Log>>,
    behavior: Arc<Behavior>,
) -> Result<hyper::Response<FakeBody>, std::io::Error> {
    let who = identity(request.headers());
    if let Ok(mut log) = log.lock() {
        log.handshakes.push(request.headers().clone());
        log.paths.push(request.uri().path().to_string());
    }
    let unreachable = behavior
        .unreachable
        .lock()
        .map(|mut left| match left.get_mut(&who) {
            Some(count) if *count > 0 => {
                *count -= 1;
                true
            }
            _ => false,
        })
        .unwrap_or(false);
    if unreachable {
        // hyper drops the connection without an answer.
        return Err(std::io::Error::other("unreachable"));
    }
    let mut response = hyper::Response::new(FakeBody::new());
    if let Some(status) = behavior.refuse.get(&who) {
        *response.status_mut() = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
        return Ok(response);
    }
    let key = super::http_io::header_value(request.headers(), "sec-websocket-key").to_string();
    let upgrade = hyper::upgrade::on(&mut request);
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert("upgrade", http::HeaderValue::from_static("websocket"));
    headers.insert("connection", http::HeaderValue::from_static("Upgrade"));
    if let Ok(accept) = http::HeaderValue::from_str(
        &tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes()),
    ) {
        headers.insert("sec-websocket-accept", accept);
    }
    let turn_state = format!("ts-ws-{who}");
    let extra: Vec<(String, String)> = match &behavior.handshake {
        Some(extra) => extra
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
        None => vec![("x-codex-turn-state".into(), turn_state)],
    };
    for (name, value) in extra {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(&value),
        ) {
            headers.insert(name, value);
        }
    }
    tokio::spawn(async move {
        let Ok(upgraded) = upgrade.await else {
            return;
        };
        let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
            hyper_util::rt::TokioIo::new(upgraded),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        while let Some(Ok(message)) = socket.next().await {
            let text = match message {
                Message::Text(text) => text.as_str().to_string(),
                Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Message::Close(_) => return,
                _ => continue,
            };
            if let Ok(mut log) = log.lock() {
                log.messages.push(text);
            }
            for event in (behavior.script)(&who) {
                if socket.send(Message::Text(event.into())).await.is_err() {
                    return;
                }
            }
        }
    });
    Ok(response)
}

/// What the real Codex backend sends on a socket before response.created,
/// captured from a live session.
const NATIVE_PREAMBLE: [&str; 2] = [
    r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":3}}}"#,
    r#"{"type":"codex.response.metadata","metadata":{}}"#,
];

fn completed_events(id: &str) -> Vec<String> {
    let mut events: Vec<String> = NATIVE_PREAMBLE
        .iter()
        .map(|event| (*event).to_string())
        .collect();
    events.extend([
        format!(r#"{{"type":"response.created","response":{{"id":"{id}"}}}}"#),
        r#"{"type":"response.output_text.delta","delta":"ok"}"#.to_string(),
        r#"{"type":"responsesapi.websocket_timing","timing":{}}"#.to_string(),
        format!(
            r#"{{"type":"response.completed","response":{{"id":"{id}","usage":{{"input_tokens":100,"input_tokens_details":{{"cached_tokens":80}},"output_tokens":5}}}}}}"#
        ),
    ]);
    events
}

fn failing_events(failure: &str) -> Vec<String> {
    let mut events: Vec<String> = NATIVE_PREAMBLE
        .iter()
        .map(|event| (*event).to_string())
        .collect();
    events.push(r#"{"type":"response.created","response":{"id":"resp_busy"}}"#.to_string());
    events.push(failure.to_string());
    events
}

struct Pool {
    app: Arc<App>,
    url: String,
    upstream: FakeCodexUpstream,
    _dir: TempDir,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Go wsTestPool: two device accounts whose Codex backend is the fake, served
/// on a real listener. `prepare` runs before the listener starts.
async fn ws_test_pool(behavior: Behavior, prepare: impl FnOnce(&App)) -> Pool {
    let upstream = FakeCodexUpstream::start(behavior).await;
    let (app, dir) = codex_app(
        &upstream.url,
        &[("first", "acct-first", 100), ("second", "acct-second", 10)],
    );
    prepare(&app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("{error}"));
    let served = app.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            tokio::spawn(super::listener::serve_connection(
                served.clone(),
                stream,
                peer,
                super::http_io::Cancel::default(),
            ));
        }
    });
    Pool {
        app,
        url: format!("ws://{address}/v1/responses"),
        upstream,
        _dir: dir,
        task,
    }
}

type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn dial(
    url: &str,
    headers: &[(&'static str, &str)],
) -> Result<(ClientSocket, HeaderMap), Option<hyper::Response<Option<Vec<u8>>>>> {
    let mut request = url
        .into_client_request()
        .unwrap_or_else(|error| panic!("{error}"));
    for (name, value) in headers {
        if let Ok(value) = http::HeaderValue::from_str(value) {
            request.headers_mut().insert(*name, value);
        }
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, response)) => Ok((socket, response.headers().clone())),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Err(Some(*response)),
        Err(_) => Err(None),
    }
}

/// Go dialPool.
async fn dial_pool(url: &str) -> ClientSocket {
    let headers = [
        ("authorization", "Bearer client-key"),
        ("openai-beta", "responses_websockets=2026-02-06"),
        ("session-id", "session-ws"),
        ("thread-id", "thread-ws"),
        (
            "x-codex-turn-metadata",
            r#"{"thread_id":"thread-ws","session_id":"session-ws"}"#,
        ),
        ("x-codex-turn-state", "stale-turn-state"),
        ("user-agent", "codex-tui/0.163.0 (Mac OS 26.0.0; arm64)"),
        ("originator", "Codex_Desktop"),
    ];
    match dial(url, &headers).await {
        Ok((socket, _)) => socket,
        Err(response) => panic!(
            "dial pool: {:?}",
            response.map(|response| response.status())
        ),
    }
}

async fn send(socket: &mut ClientSocket, message: &str) {
    if let Err(error) = socket.send(Message::Text(message.to_string().into())).await {
        panic!("socket closed: {error}");
    }
}

/// Go readUntilTerminal: the events up to a terminal one, or up to the
/// socket closing.
async fn read_until_terminal(socket: &mut ClientSocket) -> Vec<String> {
    let mut events = Vec::new();
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next()).await;
        let Ok(Some(Ok(message))) = next else {
            return events;
        };
        let text = match message {
            Message::Text(text) => text.as_str().to_string(),
            Message::Close(_) => return events,
            _ => continue,
        };
        let kind = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|event| event["type"].as_str().map(str::to_string))
            .unwrap_or_default();
        events.push(text);
        if matches!(
            kind.as_str(),
            "response.completed" | "response.failed" | "error"
        ) {
            return events;
        }
    }
}

async fn wait_in_flight_zero(app: &App) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while app.in_flight.load(std::sync::atomic::Ordering::SeqCst) != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "in-flight count stuck"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn cooled(app: &App, id: &str) -> bool {
    super::proxy_tests::active_cooldown_count(app, id) > 0
}

// Go TestResponsesWebSocketRelaysCodexMessagesUnchanged: the socket is
// relayed, not rebuilt; prewarm and incremental requests reach upstream byte
// for byte over one upstream socket opened with the native handshake, and
// only real responses are recorded.
#[tokio::test]
async fn responses_websocket_relays_codex_messages_unchanged() {
    let pool = ws_test_pool(Behavior::scripted(|_| completed_events("resp_ws")), |_| {}).await;
    let mut socket = dial_pool(&pool.url).await;
    let prewarm = r#"{"type":"response.create","model":"gpt-test","generate":false,"input":[],"tools":[{"type":"function","name":"z_tool"}],"store":false,"stream":true}"#;
    let turn = r#"{"type":"response.create","model":"gpt-test","previous_response_id":"resp_ws","input":[{"type":"message","role":"user","content":"hi"}],"store":false,"stream":true}"#;
    for message in [prewarm, turn] {
        send(&mut socket, message).await;
        let events = read_until_terminal(&mut socket).await;
        assert!(
            events.len() == completed_events("x").len()
                && events
                    .last()
                    .is_some_and(|event| event.contains("response.completed")),
            "events not relayed: {events:?}"
        );
    }
    wait_in_flight_zero(&pool.app).await;
    assert_eq!(
        pool.upstream.identities().len(),
        1,
        "one client socket must use one upstream socket"
    );
    assert_eq!(
        pool.upstream.messages(),
        [prewarm, turn],
        "messages must reach upstream byte for byte"
    );
    let first = pool
        .app
        .read()
        .codex_auth(&pool.app.read().config.accounts()[0])
        .unwrap_or_else(|error| panic!("{error:?}"));
    let path = pool
        .upstream
        .log
        .lock()
        .map(|log| log.paths[0].clone())
        .unwrap_or_default();
    assert_eq!(path, "/backend-api/codex/responses");
    let handshake = pool.upstream.handshake(0);
    for (name, want) in [
        ("authorization", format!("Bearer {}", first.access_token)),
        ("chatgpt-account-id", "acct-first".to_string()),
        ("x-codex-routing-hint", "model=gpt-test".to_string()),
        ("version", "0.163.0".to_string()),
        ("openai-beta", "responses_websockets=2026-02-06".to_string()),
        ("session-id", "session-ws".to_string()),
        ("thread-id", "thread-ws".to_string()),
        ("originator", "Codex_Desktop".to_string()),
    ] {
        assert_eq!(
            super::http_io::header_value(&handshake, name),
            want,
            "upstream handshake {name}"
        );
    }
    assert_eq!(
        super::http_io::header_value(&handshake, "x-codex-turn-state"),
        "",
        "a native handshake carries no turn state"
    );
    let events = pool.app.read().state.routing_cache_events().to_vec();
    assert!(
        events.len() == 1 && events[0].cached_tokens == 80,
        "only the real response is recorded, with its usage: {events:?}"
    );
}

// Go TestResponsesWebSocketFailsOverARefusedHandshake.
#[tokio::test]
async fn responses_websocket_fails_over_a_refused_handshake() {
    let mut behavior = Behavior::scripted(|_| completed_events("resp_second"));
    behavior.refuse.insert("first".into(), 429);
    let pool = ws_test_pool(behavior, |_| {}).await;
    let mut socket = dial_pool(&pool.url).await;
    send(
        &mut socket,
        r#"{"type":"response.create","model":"gpt-test","input":[]}"#,
    )
    .await;
    assert_eq!(
        read_until_terminal(&mut socket).await.len(),
        completed_events("x").len()
    );
    wait_in_flight_zero(&pool.app).await;
    assert_eq!(
        pool.upstream.identities(),
        ["first", "second"],
        "expected failover to the second account"
    );
}

// Go TestResponsesWebSocketClosesAfterACapacityFailure: when an account's
// allowance is spent before any output, Codex is shown nothing: the socket
// closes, Codex reconnects and resends in full, and the cooled account is not
// chosen again.
#[tokio::test]
async fn responses_websocket_closes_after_a_capacity_failure() {
    for failure in [
        r#"{"type":"response.failed","response":{"id":"resp_busy","error":{"code":"rate_limit_exceeded","message":"limit"}}}"#,
        // The shape the Codex backend sends for a spent allowance: the kind is
        // in error.type and there is no code.
        r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"team","resets_at":1738888888}}"#,
    ] {
        let pool = ws_test_pool(Behavior::scripted(move |_| failing_events(failure)), |_| {}).await;
        let mut socket = dial_pool(&pool.url).await;
        send(
            &mut socket,
            r#"{"type":"response.create","model":"gpt-test","input":[]}"#,
        )
        .await;
        let events = read_until_terminal(&mut socket).await;
        assert!(
            events.is_empty(),
            "nothing of a failure before output may reach Codex: {events:?}"
        );
        wait_in_flight_zero(&pool.app).await;
        assert!(
            cooled(&pool.app, "first"),
            "the spent account must cool down so the reconnect goes elsewhere"
        );
    }
}

// An overload before content is never resent on the failed socket: a native
// Codex drops a socket on any failed response and resends on a new one
// (codex-api endpoint/responses_websocket.rs stream_request). The pool closes
// both sockets without showing the failure, the account stays uncooled inside
// the same-account schedule, and Codex's reconnect resends in full on the same
// account. Upstream sends the failed response's own trailing response.failed
// after the error event, as it did live; it must not be read as a second
// failure. (Replaces Go TestResponsesWebSocketKeepsTheSocketThroughAnOverload,
// which asserted the resend on the same socket.)
#[tokio::test]
async fn responses_websocket_hands_an_overload_to_codex_on_a_new_socket() {
    for code in ["server_is_overloaded", "server_error"] {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let pool = ws_test_pool(
            Behavior::scripted(move |_| {
                if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    let mut events = failing_events(&format!(
                        r#"{{"type":"error","status":500,"error":{{"code":"{code}","message":"busy"}}}}"#
                    ));
                    events.push(format!(
                        r#"{{"type":"response.failed","response":{{"id":"resp_busy","error":{{"code":"{code}","message":"busy"}}}}}}"#
                    ));
                    events
                } else {
                    completed_events("resp_ok")
                }
            }),
            |_| {},
        )
        .await;
        let turn = r#"{"type":"response.create","model":"gpt-test","previous_response_id":"resp_prev","input":[]}"#;
        let mut socket = dial_pool(&pool.url).await;
        send(&mut socket, turn).await;
        let events = read_until_terminal(&mut socket).await;
        assert!(
            events.is_empty(),
            "the {code} must not reach Codex; its socket closes instead: {events:?}"
        );
        wait_in_flight_zero(&pool.app).await;
        assert_eq!(
            pool.upstream.messages().len(),
            1,
            "nothing may be resent on the socket that failed"
        );
        assert!(
            !cooled(&pool.app, "first"),
            "an overload inside the same-account schedule must not cool the account"
        );
        // Codex reconnects and resends in full, as after any dropped socket.
        let full = r#"{"type":"response.create","model":"gpt-test","input":[{"type":"message","role":"user","content":[]}]}"#;
        let mut socket = dial_pool(&pool.url).await;
        send(&mut socket, full).await;
        let events = read_until_terminal(&mut socket).await;
        assert_eq!(events.len(), completed_events("x").len(), "{events:?}");
        wait_in_flight_zero(&pool.app).await;
        assert_eq!(
            pool.upstream.identities(),
            ["first", "first"],
            "the reconnect stays on the same account, on a new upstream socket"
        );
        assert_eq!(pool.upstream.messages(), [turn, full]);
    }
}

// Go TestResponsesWebSocketMovesOnWhenTheOverloadOutlastsTheSchedule: each
// overload closes the socket and Codex reconnects; past the same-account
// schedule the account cools and the next reconnect goes elsewhere.
#[tokio::test]
async fn responses_websocket_moves_on_when_the_overload_outlasts_the_schedule() {
    let pool = ws_test_pool(
        Behavior::scripted(|who| {
            if who == "first" {
                failing_events(r#"{"type":"response.failed","response":{"id":"resp_busy","error":{"code":"server_is_overloaded"}}}"#)
            } else {
                completed_events("resp_second")
            }
        }),
        |_| {},
    )
    .await;
    let turn = r#"{"type":"response.create","model":"gpt-test","input":[]}"#;
    let limit =
        usize::try_from(super::tuning::OVERLOAD_SAME_ACCOUNT_RETRY_LIMIT).unwrap_or_default();
    for attempt in 0..=limit {
        assert!(
            !cooled(&pool.app, "first"),
            "attempt {attempt} is still inside the same-account schedule"
        );
        let mut socket = dial_pool(&pool.url).await;
        send(&mut socket, turn).await;
        let events = read_until_terminal(&mut socket).await;
        assert!(
            events.is_empty(),
            "the overload must not reach Codex: {events:?}"
        );
        wait_in_flight_zero(&pool.app).await;
    }
    assert!(
        cooled(&pool.app, "first"),
        "the account that kept refusing must cool down"
    );
    let mut socket = dial_pool(&pool.url).await;
    send(&mut socket, turn).await;
    assert_eq!(
        read_until_terminal(&mut socket).await.len(),
        completed_events("x").len()
    );
    let mut identities = vec!["first"; limit + 1];
    identities.push("second");
    assert_eq!(
        pool.upstream.identities(),
        identities,
        "one upstream socket per attempt, then the next account"
    );
}

// Go TestResponsesWebSocketWithoutTransportAsksForHTTP: a request that cannot
// be upgraded is answered 426, which a native Codex treats as "use HTTP".
#[tokio::test]
async fn responses_websocket_without_an_upgrade_asks_for_http() {
    let account =
        super::proxy_tests::provider("provider", 100, "http://127.0.0.1:1/v1", "upstream-key");
    let (app, _dir) = super::proxy_tests::arc_app(vec![account]);
    let request = super::routes::Request::test(http::Method::GET, "/v1/responses", "")
        .with_header("authorization", "Bearer client-key")
        .with_header("upgrade", "websocket")
        .with_header("connection", "Upgrade");
    assert_eq!(serve(&app, request).await.code(), 426);
}

// Go TestResponsesWebSocketRelaysTheUpstreamHandshake: the upstream
// handshake's headers reach Codex, as they reach a native Codex, and the
// account's socket is open before the client's is accepted.
#[tokio::test]
async fn responses_websocket_relays_the_upstream_handshake() {
    let mut behavior = Behavior::scripted(|_| completed_events("resp_hs"));
    behavior.handshake = Some(vec![
        ("x-codex-turn-state", "ts-native"),
        ("x-reasoning-included", "true"),
        ("openai-model", "gpt-test"),
        ("x-models-etag", "catalog"),
    ]);
    let pool = ws_test_pool(behavior, |_| {}).await;
    let (_socket, headers) = dial(
        &pool.url,
        &[
            ("authorization", "Bearer client-key"),
            ("session-id", "session-hs"),
        ],
    )
    .await
    .unwrap_or_else(|_| panic!("dial"));
    for (name, want) in [
        ("x-codex-turn-state", "ts-native"),
        ("x-reasoning-included", "true"),
        ("openai-model", "gpt-test"),
    ] {
        assert_eq!(
            super::http_io::header_value(&headers, name),
            want,
            "client handshake {name}"
        );
    }
    // On /v1 the catalog is the pool's own, so upstream's ETag is withheld.
    assert_eq!(super::http_io::header_value(&headers, "x-models-etag"), "");
    assert_eq!(
        pool.upstream.identities().len(),
        1,
        "the upstream socket must be open before the client's is accepted"
    );
}

// Go TestResponsesWebSocketRefusesTheHandshakeWhenNoAccountCan.
#[tokio::test]
async fn responses_websocket_refuses_the_handshake_when_no_account_can() {
    let mut behavior = Behavior::scripted(|_| Vec::new());
    behavior.refuse.insert("first".into(), 429);
    behavior.refuse.insert("second".into(), 429);
    let pool = ws_test_pool(behavior, |_| {}).await;
    match dial(&pool.url, &[("authorization", "Bearer client-key")]).await {
        Err(Some(response)) => assert_ne!(response.status(), StatusCode::SWITCHING_PROTOCOLS),
        _ => panic!("the handshake must be refused before a socket exists"),
    }
}

// Go TestResponsesWebSocketMovesTheSocketForItsFirstRequest: the handshake
// can only guess the account; nothing has gone out on that upstream socket,
// so it is replaced without closing Codex's socket.
#[tokio::test]
async fn responses_websocket_moves_the_socket_for_its_first_request() {
    let pool = ws_test_pool(
        Behavior::scripted(|identity| completed_events(&format!("resp_{identity}"))),
        |app| {
            app.write().config.accounts_mut()[0].allowed_models = vec!["gpt-test".to_string()];
        },
    )
    .await;
    let mut socket = dial_pool(&pool.url).await;
    send(
        &mut socket,
        r#"{"type":"response.create","model":"gpt-other","input":[]}"#,
    )
    .await;
    let events = read_until_terminal(&mut socket).await;
    assert!(
        events.len() == completed_events("x").len() && events.concat().contains("resp_second"),
        "the first request must be served by the account that can serve it: {events:?}"
    );
    wait_in_flight_zero(&pool.app).await;
    assert_eq!(pool.upstream.identities(), ["first", "second"]);
    assert_eq!(pool.upstream.messages().len(), 1);
}

// Go TestResponsesWebSocketWithholdsAnotherAccountsTurnState.
#[tokio::test]
async fn responses_websocket_withholds_another_accounts_turn_state() {
    let pool = ws_test_pool(Behavior::scripted(|_| completed_events("resp_ts")), |app| {
        // The second account issued this one, on a socket the session left.
        app.note_turn_state_issuer("ts-from-another-account", "second");
    })
    .await;
    let mut socket = dial_pool(&pool.url).await;
    let own = r#"{"type":"response.create","model":"gpt-test","client_metadata":{"session_id":"s","x-codex-turn-state":"ts-ws-first","thread_id":"t"},"input":[]}"#;
    let foreign = r#"{"type":"response.create","model":"gpt-test","client_metadata":{"session_id":"s","x-codex-turn-state":"ts-from-another-account","thread_id":"t"},"input":[]}"#;
    for message in [own, foreign] {
        send(&mut socket, message).await;
        read_until_terminal(&mut socket).await;
    }
    wait_in_flight_zero(&pool.app).await;
    let messages = pool.upstream.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0], own,
        "the issuing account's turn state must pass byte for byte"
    );
    assert_eq!(
        messages[1],
        r#"{"type":"response.create","model":"gpt-test","client_metadata":{"session_id":"s","thread_id":"t"},"input":[]}"#,
        "another account's turn state must be removed and nothing else"
    );
}

// Go TestResponsesWebSocketRetriesAnUnreachableAccountBeforeMoving.
#[tokio::test]
async fn responses_websocket_retries_an_unreachable_account_before_moving() {
    let limit =
        usize::try_from(super::tuning::TRANSPORT_SAME_ACCOUNT_RETRY_LIMIT).unwrap_or_default();
    for (failures, want) in [
        (limit, vec!["first"; limit + 1]),
        (limit + 1, {
            let mut want = vec!["first"; limit + 1];
            want.push("second");
            want
        }),
    ] {
        let behavior = Behavior::scripted(|_| completed_events("resp_retry"));
        if let Ok(mut left) = behavior.unreachable.lock() {
            left.insert("first".into(), failures);
        }
        let pool = ws_test_pool(behavior, |_| {}).await;
        let mut socket = dial_pool(&pool.url).await;
        send(
            &mut socket,
            r#"{"type":"response.create","model":"gpt-test","input":[]}"#,
        )
        .await;
        assert_eq!(
            read_until_terminal(&mut socket).await.len(),
            completed_events("x").len()
        );
        wait_in_flight_zero(&pool.app).await;
        assert_eq!(pool.upstream.identities(), want, "{failures} failures");
    }
}

// Go TestResponsesWebSocketPassesUpgradeRequiredOnAtOnce.
#[tokio::test]
async fn responses_websocket_passes_upgrade_required_on_at_once() {
    let mut behavior = Behavior::scripted(|_| Vec::new());
    behavior.refuse.insert("first".into(), 426);
    behavior.refuse.insert("second".into(), 426);
    let pool = ws_test_pool(behavior, |_| {}).await;
    match dial(&pool.url, &[("authorization", "Bearer client-key")]).await {
        Err(Some(response)) => assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED),
        _ => panic!("handshake answer must be 426"),
    }
    assert_eq!(
        pool.upstream.identities().len(),
        1,
        "upstream must see the one handshake a native Codex makes"
    );
    assert!(
        pool.app
            .read()
            .state
            .cooldowns()
            .get("first")
            .is_none_or(Vec::is_empty),
        "a 426 must not cool the account"
    );
}

// Go TestNativeCodexEndpointServesTheResponsesWebSocket.
#[tokio::test]
async fn native_codex_endpoint_serves_the_responses_websocket() {
    let pool = ws_test_pool(
        Behavior::scripted(|_| completed_events("resp_ws_native")),
        |_| {},
    )
    .await;
    let native = pool
        .url
        .replace("/v1/responses", "/k/client-key/backend-api/codex/responses");
    let mut socket = dial_pool(&native).await;
    send(
        &mut socket,
        r#"{"type":"response.create","model":"gpt-test","input":[]}"#,
    )
    .await;
    assert_eq!(
        read_until_terminal(&mut socket).await.len(),
        completed_events("x").len()
    );
    wait_in_flight_zero(&pool.app).await;
    let events = pool.app.read().state.routing_cache_events().to_vec();
    assert_eq!(
        events.last().map(|event| event.client_entry.as_str()),
        Some("native/ws")
    );
}

// ---- the native endpoint over HTTP ----

// Go TestNativeCodexEndpointSendsTheClientRequestWithThePoolAccount: the key
// in the path is the only way in, the client's own ChatGPT credential and
// account never go upstream, the account's do, and the body Codex compressed
// reaches upstream as the JSON Codex wrote, with Codex's own headers relayed
// rather than doubled.
#[tokio::test]
async fn native_codex_endpoint_sends_the_client_request_with_the_pool_account() {
    use super::testing::FakeResponse;
    use super::testing::FakeServer;
    // image_gen is the namespace tool a native Codex sends its own backend;
    // the custom-provider path strips it, the native endpoint must not. Keys
    // are deliberately unsorted so a re-encode shows.
    const RAW: &str = r#"{"model":"gpt-test","tools":[{"type":"namespace","name":"image_gen","tools":[{"type":"function","name":"imagegen","parameters":{"type":"object"}}]}],"stream":true,"store":false,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}],"internal_chat_message_metadata_passthrough":{"turn_id":"t"}}],"client_metadata":{"guardian_credits_requested":"true","mcp_attribution":"{\"status\":\"none\"}"},"prompt_cache_key":"thread-native"}"#;
    let upstream = FakeServer::start(|_| {
        let mut response = FakeResponse::json(
            200,
            r#"{"id":"resp_native","object":"response","output":[]}"#,
        );
        response
            .headers
            .push(("x-models-etag".into(), "catalog-v2".into()));
        response
    })
    .await;
    let (app, _dir) = codex_app(&upstream.url, &[("device", "acct-pool", 100)]);
    let auth = app
        .read()
        .codex_auth(&app.read().config.accounts()[0])
        .unwrap_or_else(|error| panic!("{error:?}"));
    let compressed = zstd::encode_all(RAW.as_bytes(), 0).unwrap_or_default();
    let send = |key: &str| {
        let mut request = super::routes::Request::test(
            http::Method::POST,
            &format!("/k/{key}/backend-api/codex/responses"),
            "",
        );
        request.body = Bytes::from(compressed.clone());
        request
            .with_header("content-encoding", "zstd")
            .with_header("authorization", "Bearer <client-chatgpt-login>")
            .with_header("chatgpt-account-id", "acct-client")
            .with_header("x-openai-account-routing-override", "us")
            .with_header("x-codex-routing-hint", "model=gpt-test;tier=priority")
            .with_header("version", "0.163.0")
            .with_header("session-id", "session-native")
            .with_header("originator", "codex_exec")
    };
    let refused = serve(&app, send("wrong-key")).await;
    assert!(
        refused.code() == 401 && upstream.count() == 0,
        "a wrong path key must be refused before anything goes upstream"
    );
    let result = serve(&app, send("client-key")).await;
    assert_eq!(
        result.code(),
        200,
        "native endpoint: {}",
        result.body_string()
    );
    assert_eq!(
        result.header("x-models-etag"),
        "catalog-v2",
        "a native client refetches its catalog on a new X-Models-Etag"
    );
    // Local telemetry says the request came through the native endpoint.
    let events = app.read().state.routing_cache_events().to_vec();
    assert_eq!(
        events.last().map(|event| event.client_entry.as_str()),
        Some("native/http")
    );
    let sent = upstream.requests().pop().unwrap_or_default();
    assert_eq!(sent.path, "/backend-api/codex/responses");
    let body = zstd::decode_all(sent.body.as_slice()).unwrap_or_default();
    assert_eq!(
        String::from_utf8_lossy(&body),
        RAW,
        "the JSON Codex compressed must reach upstream byte for byte"
    );
    for (name, want) in [
        ("authorization", format!("Bearer {}", auth.access_token)),
        ("chatgpt-account-id", "acct-pool".to_string()),
        (
            "x-codex-routing-hint",
            "model=gpt-test;tier=priority".to_string(),
        ),
        ("version", "0.163.0".to_string()),
        ("session-id", "session-native".to_string()),
        ("originator", "codex_exec".to_string()),
    ] {
        let values: Vec<&str> = sent
            .headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert_eq!(
            values,
            [want.as_str()],
            "upstream {name} must be exactly one value"
        );
    }
    assert_eq!(
        sent.header("x-openai-account-routing-override"),
        "",
        "the client account's workspace routing must not follow the request"
    );
    assert!(
        !sent
            .header("authorization")
            .contains("client-chatgpt-login"),
        "the client's own ChatGPT login reached upstream"
    );
}

// Go TestNativeCodexEndpointPassesModelsThroughWithAPoolAccount.
#[tokio::test]
async fn native_codex_endpoint_passes_models_through_with_a_pool_account() {
    use super::testing::FakeResponse;
    use super::testing::FakeServer;
    let upstream = FakeServer::start(|_| {
        let mut response = FakeResponse::json(200, r#"{"models":[]}"#);
        response
            .headers
            .push(("etag".into(), r#""models-v1""#.into()));
        response
            .headers
            .push(("set-cookie".into(), "upstream=private".into()));
        response
    })
    .await;
    let (app, _dir) = codex_app(&upstream.url, &[("device", "acct-pool", 100)]);
    let auth = app
        .read()
        .codex_auth(&app.read().config.accounts()[0])
        .unwrap_or_else(|error| panic!("{error:?}"));
    let request = super::routes::Request::test(
        http::Method::GET,
        "/k/client-key/backend-api/codex/models?client_version=0.163.0",
        "",
    )
    .with_header("authorization", "Bearer <client-chatgpt-login>");
    let result = serve(&app, request).await;
    assert!(
        result.code() == 200 && result.body_string() == r#"{"models":[]}"#,
        "models: {}",
        result.body_string()
    );
    let sent = upstream.requests().pop().unwrap_or_default();
    assert_eq!(
        format!("{}?{}", sent.path, sent.query),
        "/backend-api/codex/models?client_version=0.163.0"
    );
    assert_eq!(
        sent.header("authorization"),
        format!("Bearer {}", auth.access_token)
    );
    assert_eq!(sent.header("chatgpt-account-id"), "acct-pool");
    assert_eq!(
        result.header("etag"),
        r#""models-v1""#,
        "Codex caches the catalog by its ETag"
    );
    assert_eq!(
        result.header("set-cookie"),
        "",
        "upstream cookies belong to the pool account's session"
    );
    let unknown = super::routes::Request::test(
        http::Method::GET,
        "/k/client-key/backend-api/codex/wham/usage",
        "",
    );
    assert_eq!(
        serve(&app, unknown).await.code(),
        404,
        "a path the pool does not serve must be refused"
    );
}

// Go TestNativeEndpointReportsHeadersItDoesNotRelay: a native client's header
// the relay does not carry is reported by name, once.
#[test]
fn native_endpoint_reports_headers_it_does_not_relay() {
    let (app, _dir) = super::testing::test_app(Vec::new());
    let (app, logged) = super::testing::capture_logs(app);
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("authorization", "Bearer <client-login>"),
        ("x-codex-turn-metadata", "{}"),
        ("x-oai-attestation", "<attestation>"),
        ("x-some-future-header", "<value>"),
        ("originator", "Codex_Desktop"),
    ] {
        headers.insert(name, http::HeaderValue::from_static(value));
    }
    app.note_unrelayed_native_headers(&headers);
    app.note_unrelayed_native_headers(&headers);
    let out = logged
        .lock()
        .map(|lines| lines.join("\n"))
        .unwrap_or_default();
    assert!(
        out.matches("not relayed").count() == 1 && out.contains("x-some-future-header"),
        "log = {out:?}"
    );
    for leaked in [
        "<value>",
        "<client-login>",
        "<attestation>",
        "x-oai-attestation",
        "x-codex-turn-metadata",
        "authorization",
    ] {
        assert!(
            !out.contains(leaked),
            "the report must name only unrelayed headers and never a value: {out:?}"
        );
    }
}

// ---- the account's installation id (installation_id_test.go) ----

const CLIENT_INSTALLATION_ID: &str = "11111111-1111-4111-8111-111111111111";

fn client_turn_metadata() -> String {
    format!(
        r#"{{"installation_id":"{CLIENT_INSTALLATION_ID}","session_id":"01a15f63-427f-7fa2-884a-01ebe2c60a47","thread_id":"01a15f63-427f-7fa2-884a-01ebe2c60a47","request_kind":"turn"}}"#
    )
}

/// A Responses body whose client_metadata has the keys a native Codex
/// 0.163.0 sends, and whose input quotes the client's installation id, which
/// must not change.
fn client_metadata_body() -> String {
    let escaped = client_turn_metadata().replace('"', "\\\"");
    format!(
        r#"{{"model":"gpt-test","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"my id is {CLIENT_INSTALLATION_ID}"}}]}}],"stream":true,"store":false,"client_metadata":{{"x-codex-window-id":"01a15f63-427f-7fa2-884a-01ebe2c60a47:0","x-codex-installation-id":"{CLIENT_INSTALLATION_ID}","x-codex-turn-metadata":"{escaped}"}},"prompt_cache_key":"thread-native"}}"#
    )
}

// Go TestNativeEndpointPresentsTheAccountInstallationID.
#[tokio::test]
async fn native_endpoint_presents_the_account_installation_id() {
    use super::installation_id::with_installation_id;
    use super::testing::FakeResponse;
    use super::testing::FakeServer;
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_native","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = codex_app(&upstream.url, &[("device", "acct-pool", 100)]);
    let request = super::routes::Request::test(
        http::Method::POST,
        "/k/client-key/backend-api/codex/responses",
        &client_metadata_body(),
    )
    .with_header("content-type", "application/json")
    .with_header("x-codex-turn-metadata", &client_turn_metadata());
    let result = serve(&app, request).await;
    assert_eq!(
        result.code(),
        200,
        "native endpoint: {}",
        result.body_string()
    );
    let item = app.read().config.accounts()[0].clone();
    let id = app.presented_installation_id(&item).unwrap_or_default();
    let sent = upstream.requests().pop().unwrap_or_default();
    let body = String::from_utf8_lossy(&zstd::decode_all(sent.body.as_slice()).unwrap_or_default())
        .into_owned();
    assert_eq!(
        body,
        with_installation_id(&client_metadata_body(), &id),
        "upstream body"
    );
    assert_eq!(
        sent.header("x-codex-turn-metadata"),
        client_turn_metadata().replace(CLIENT_INSTALLATION_ID, &id)
    );
}

// Go TestWebSocketPresentsTheAccountInstallationID.
#[tokio::test]
async fn websocket_presents_the_account_installation_id() {
    use super::installation_id::with_installation_id;
    let pool = ws_test_pool(
        Behavior::scripted(|_| completed_events("resp_ws_id")),
        |_| {},
    )
    .await;
    let metadata = client_turn_metadata();
    let (mut socket, _) = dial(
        &pool.url,
        &[
            ("authorization", "Bearer client-key"),
            ("openai-beta", "responses_websockets=2026-02-06"),
            ("x-codex-turn-metadata", &metadata),
        ],
    )
    .await
    .unwrap_or_else(|_| panic!("dial pool"));
    let message =
        client_metadata_body().replacen(r#"{"model""#, r#"{"type":"response.create","model""#, 1);
    send(&mut socket, &message).await;
    read_until_terminal(&mut socket).await;
    wait_in_flight_zero(&pool.app).await;
    let identities = pool.upstream.identities();
    let bound_id = identities.last().cloned().unwrap_or_default();
    let accounts = pool.app.read().config.accounts().to_vec();
    let bound = accounts
        .iter()
        .find(|item| item.id == bound_id)
        .cloned()
        .unwrap_or_default();
    let id = pool
        .app
        .presented_installation_id(&bound)
        .unwrap_or_default();
    let handshake = pool.upstream.handshake(identities.len() - 1);
    assert_eq!(
        super::http_io::header_value(&handshake, "x-codex-turn-metadata"),
        metadata.replace(CLIENT_INSTALLATION_ID, &id)
    );
    assert_eq!(
        pool.upstream.messages().last().cloned().unwrap_or_default(),
        with_installation_id(&message, &id)
    );
    let other = accounts
        .iter()
        .find(|item| item.id != bound.id)
        .cloned()
        .unwrap_or_default();
    assert_ne!(
        pool.app
            .presented_installation_id(&other)
            .unwrap_or_default(),
        id,
        "two accounts share an installation id"
    );
}
