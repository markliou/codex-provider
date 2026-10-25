//! The Responses WebSocket, relayed account by account.
//!
//! A port of responses_ws.go. The Responses WebSocket is how a native Codex
//! signed in with ChatGPT talks to the Codex backend: one long-lived socket per
//! session, a generate=false prewarm that uploads tools and instructions once,
//! and then requests that carry only new input plus the
//! previous_response_id. That state lives on the upstream connection, which is
//! what spares the backend from re-reading the whole conversation on every
//! request, so the pool relays the socket instead of turning it back into
//! full HTTP requests.
//!
//! One client socket is bound to one account's upstream socket, opened with
//! Codex's own connector. Messages pass through byte for byte in both
//! directions.
//!
//! The account is chosen at the handshake, from the session, thread and model
//! the handshake names (X-Codex-Routing-Hint, else X-Codex-Turn-Metadata), and
//! the upstream socket is opened before the client's is accepted. That order
//! is what lets the client receive the upstream handshake's own headers, as a
//! native Codex does: Codex reads its turn state from the handshake response
//! alone on a socket (codex-api endpoint/responses_websocket.rs) and sends it
//! back in every later message's client_metadata, and it reads
//! x-reasoning-included there, which changes its token accounting and so when
//! it compacts. Accepting first and dialing at the first message left Codex
//! with neither, so its later messages differed from a native Codex's.
//!
//! When the next request belongs on another account (a lineage move, a
//! cooled-down account) or the account fails mid-connection, the pool closes
//! the client socket instead of switching upstream sockets underneath it:
//! incremental requests reference responses that exist only on the old
//! connection, and a native Codex answers a closed socket by reconnecting and
//! resending the request in full, which the pool then routes like any new
//! request. The one exception is the first request on a socket, which may
//! settle the account differently from the handshake's guess: nothing has gone
//! out on that upstream socket yet, so it is replaced before the request is
//! sent, rather than sending Codex to reconnect into the same guess.
//!
//! Each leg has one writer task fed by a channel, so the client reader, the
//! upstream relay and the ping ticker never hold a lock across a send.

use super::app::App;
use super::credentials::codex_client_version;
use super::http_io::Cancel;
use super::http_io::GoHeader;
use super::http_io::ResponseWriter;
use super::http_io::header_value;
use super::http_io::set_header;
use super::http_io::write_openai_error;
use super::installation_id;
use super::outcome::ThroughputMeasurement;
use super::proxy::parse_model;
use super::proxy::relayed_codex_request_header;
use super::proxy::safe_header_value;
use super::proxy::transient_overload_code;
use super::quota::extract_upstream_error_code;
use super::response_info::ProxyResponseInfo;
use super::response_info::response_info_from_sse_block;
use super::response_info::retryable_terminal_failure_class;
use super::routes::Request;
use super::routing::RequestFacts;
use super::routing::RoutingDecision;
use super::routing::metadata_object;
use super::routing::metadata_string;
use super::state::Account;
use super::stream::precommit_lifecycle_sse_block;
use super::time::Timestamp;
use super::time::seconds;
use super::tuning::OVERLOAD_SAME_ACCOUNT_RETRY_LIMIT;
use super::tuning::TRANSPORT_SAME_ACCOUNT_RETRY_LIMIT;
use crate::transport::WebSocketRefusal;
use bytes::Bytes;
use futures::SinkExt;
use futures::StreamExt;
use http::HeaderMap;
use http::StatusCode;
use serde_json::Map;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

/// Keeps the client leg alive between messages. That leg crosses the
/// internet to a remote Codex, through NATs and proxies that drop a silent
/// connection, while upstream can be silent for as long as a model reasons.
/// Pings are control frames Codex answers itself; they never reach upstream.
const WS_CLIENT_PING_INTERVAL: Duration = Duration::from_secs(20);
/// Bounds the lifecycle preamble held back from Codex.
const WS_PRECOMMIT_MAX_BLOCKS: usize = 64;
/// A turn state lives for one Codex turn; a day is far longer, and the bound
/// keeps the map small however many sockets come and go.
const TURN_STATE_ISSUER_TTL_HOURS: i64 = 24;
const TURN_STATE_ISSUER_MAX: usize = 4096;
/// The upstream handshake response headers a native Codex reads
/// (codex-api endpoint/responses_websocket.rs): turn state, whether reasoning
/// is included, and the serving model. X-Models-Etag joins them only on the
/// native endpoint, where the catalog is upstream's.
const WS_HANDSHAKE_HEADERS: &[&str] =
    &["x-codex-turn-state", "x-reasoning-included", "openai-model"];

enum Outgoing {
    Message(Message),
    Close(u16, String),
}

/// A message as relayed: text or binary, the opcode kept.
#[derive(Clone)]
enum Frame {
    Text(Bytes),
    Binary(Bytes),
}

impl Frame {
    fn into_message(self) -> Message {
        match self {
            Self::Text(bytes) => Message::Text(String::from_utf8_lossy(&bytes).into_owned().into()),
            Self::Binary(bytes) => Message::Binary(bytes),
        }
    }

    fn from_message(message: Message) -> Option<Self> {
        match message {
            Message::Text(text) => Some(Self::Text(Bytes::from(text.as_str().to_string()))),
            Message::Binary(bytes) => Some(Self::Binary(bytes)),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct Upstream {
    id: u64,
    tx: mpsc::UnboundedSender<Outgoing>,
}

impl Upstream {
    fn send(&self, frame: Frame) -> bool {
        self.tx
            .send(Outgoing::Message(frame.into_message()))
            .is_ok()
    }

    fn close(&self, code: u16, reason: &str) {
        let _ = self.tx.send(Outgoing::Close(code, reason.to_string()));
    }
}

struct WsResponse {
    route: RoutingDecision,
    model: String,
    account_id: String,
    warmup: bool,
    finished: AtomicBool,
    /// The fields below belong to the upstream relay. buffered holds the
    /// lifecycle preamble Codex has not been shown yet.
    state: Mutex<ResponseState>,
}

struct ResponseState {
    info: ProxyResponseInfo,
    throughput: Option<ThroughputMeasurement>,
    buffered: Vec<Bytes>,
    committed: bool,
}

struct SessionState {
    upstream: Option<Upstream>,
    account: Account,
    active: Option<Arc<WsResponse>>,
    /// The current upstream socket's handshake response.
    upstream_headers: HeaderMap,
    /// A request has gone out on the current upstream socket.
    sent: bool,
}

struct Session {
    app: Arc<App>,
    handshake: HeaderMap,
    native: bool,
    api_key: String,
    cancel: Cancel,
    client: mpsc::UnboundedSender<Outgoing>,
    closed: Cancel,
    state: Mutex<SessionState>,
    next_upstream: AtomicU64,
}

/// Why no upstream socket could be opened for a handshake.
struct Refusal {
    status: StatusCode,
    code: String,
    detail: String,
}

impl Session {
    fn state(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn send_client(&self, frame: Frame) -> bool {
        self.client
            .send(Outgoing::Message(frame.into_message()))
            .is_ok()
    }

    fn close_client(&self, code: u16, reason: &str) {
        let _ = self.client.send(Outgoing::Close(code, reason.to_string()));
    }

    fn facts(&self) -> RequestFacts<'_> {
        RequestFacts {
            headers: &self.handshake,
            native: self.native,
            websocket: true,
        }
    }
}

/// The model a handshake names: the routing hint a native Codex sends the
/// Codex backend, else the turn metadata every Codex sends, else the pool's
/// default, with the pool's alias applied for routing only.
fn ws_handshake_model(app: &App, headers: &HeaderMap) -> String {
    let mut model = String::new();
    for part in header_value(headers, "x-codex-routing-hint").split(';') {
        if let Some(value) = part.trim().strip_prefix("model=") {
            model = value.trim().to_string();
        }
    }
    if model.is_empty() {
        model = metadata_object(Some(&Value::String(
            header_value(headers, "x-codex-turn-metadata").to_string(),
        )))
        .map(|metadata| metadata_string(&metadata, &["model"]))
        .unwrap_or_default();
    }
    let inner = app.read();
    if model.is_empty() {
        model.clone_from(&inner.config.default_model);
    }
    let (model, _) = parse_model(&model);
    inner
        .config
        .model_aliases()
        .get(&model)
        .cloned()
        .unwrap_or(model)
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    header_value(headers, "upgrade").eq_ignore_ascii_case("websocket")
        && headers
            .get_all("connection")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
}

/// Serves GET /v1/responses upgrades (and the native endpoint's).
pub async fn handle_responses_websocket(
    app: &Arc<App>,
    mut request: Request,
    native: bool,
    writer: Box<dyn ResponseWriter>,
) {
    let mut writer = writer;
    let Some(upgrade) = request
        .upgrade
        .take()
        .filter(|_| is_websocket_upgrade(&request.headers))
    else {
        write_openai_error(
            writer.as_mut(),
            StatusCode::UPGRADE_REQUIRED,
            "websocket_unavailable",
            "this pool serves Responses over HTTP",
        );
        return;
    };
    let key = header_value(&request.headers, "sec-websocket-key")
        .trim()
        .to_string();
    if key.is_empty() || header_value(&request.headers, "sec-websocket-version") != "13" {
        write_openai_error(
            writer.as_mut(),
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid websocket handshake",
        );
        return;
    }
    // The socket carries Codex's own messages, which the pool forwards
    // without completing anything (codex_request.rs), so only a client that
    // presents itself as Codex may open one; an app talks HTTP to /v1, where
    // its request is completed.
    if !super::codex_request::codex_identity(&request.headers) {
        let refusal = if native {
            super::codex_request::identity_refusal()
        } else {
            super::codex_request::Refusal::new(
                "user-agent",
                "is not in Codex's format, and the Responses WebSocket only carries Codex's own requests",
                "Send HTTP requests to /v1/responses, which completes them into Codex requests.",
            )
        };
        let entry = if native {
            super::codex_request::Entry::Native
        } else {
            super::codex_request::Entry::V1
        };
        app.log_refused_request(
            entry.label(true),
            &super::server::request_api_key(&request.headers, &request.query),
            &refusal,
        );
        super::http_io::write_json(
            writer.as_mut(),
            StatusCode::BAD_REQUEST,
            &refusal.error_body(),
        );
        return;
    }
    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let session = Arc::new(Session {
        app: app.clone(),
        handshake: request.headers.clone(),
        native,
        api_key: super::server::request_api_key(&request.headers, &request.query),
        cancel: request.cancel.clone(),
        client: client_tx,
        closed: Cancel::default(),
        state: Mutex::new(SessionState {
            upstream: None,
            account: Account::default(),
            active: None,
            upstream_headers: HeaderMap::new(),
            sent: false,
        }),
        next_upstream: AtomicU64::new(1),
    });
    let model = ws_handshake_model(app, &request.headers);
    let mut route = app.routing_decision(&session.facts(), &Map::new(), &model, &session.api_key);
    // No account could take the socket: answer the handshake itself, as the
    // Codex backend does when it refuses one, so Codex maps the status (426
    // means HTTP, 429 and 5xx are retried) before any socket exists.
    let socket = match dial(&session, &mut route, &model).await {
        Ok(socket) => socket,
        Err(refusal) => {
            write_openai_error(
                writer.as_mut(),
                refusal.status,
                &refusal.code,
                &refusal.detail,
            );
            return;
        }
    };
    // Accept the client's socket with the upstream handshake's headers.
    let handshake_headers = session.state().upstream_headers.clone();
    let mut names: Vec<&str> = WS_HANDSHAKE_HEADERS.to_vec();
    if native {
        names.push("x-models-etag");
    }
    for name in names {
        let value = safe_header_value(header_value(&handshake_headers, name));
        if !value.is_empty() {
            set_header(writer.as_mut(), name, &value);
        }
    }
    set_header(writer.as_mut(), "upgrade", "websocket");
    set_header(writer.as_mut(), "connection", "Upgrade");
    set_header(
        writer.as_mut(),
        "sec-websocket-accept",
        &tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes()),
    );
    writer.write_header(StatusCode::SWITCHING_PROTOCOLS);
    drop(writer);
    let Ok(upgraded) = upgrade.await else {
        if let Some(upstream) = session.state().upstream.clone() {
            upstream.close(1000, "");
        }
        return;
    };
    // Neither leg's extension is the pool's: the client leg negotiates none,
    // the upstream leg is Codex's own connector with its deflate offer.
    let client = tokio_tungstenite::WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        Role::Server,
        None,
    )
    .await;
    let (client_sink, client_stream) = client.split();
    tokio::spawn(write_loop(client_sink, client_rx));
    start_relay(&session, socket);
    run(&session, client_stream).await;
}

async fn write_loop<S>(mut sink: S, mut rx: mpsc::UnboundedReceiver<Outgoing>)
where
    S: futures::Sink<Message> + Unpin,
{
    while let Some(outgoing) = rx.recv().await {
        match outgoing {
            Outgoing::Message(message) => {
                if sink.send(message).await.is_err() {
                    return;
                }
            }
            Outgoing::Close(code, reason) => {
                let _ = sink
                    .send(Message::Close(Some(CloseFrame {
                        code: CloseCode::from(code),
                        reason: reason.into(),
                    })))
                    .await;
                let _ = sink.close().await;
                return;
            }
        }
    }
    let _ = sink.close().await;
}

async fn run<S>(session: &Arc<Session>, mut client: S)
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let pings = {
        let client = session.client.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(WS_CLIENT_PING_INTERVAL);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if client
                    .send(Outgoing::Message(Message::Ping(Bytes::new())))
                    .is_err()
                {
                    return;
                }
            }
        })
    };
    loop {
        let next = tokio::select! {
            next = client.next() => next,
            () = session.closed.cancelled() => None,
        };
        let Some(Ok(message)) = next else {
            break;
        };
        let frame = match message {
            Message::Close(_) => break,
            other => match Frame::from_message(other) {
                Some(frame) => frame,
                None => continue,
            },
        };
        if let Frame::Text(text) = &frame
            && let Ok(payload) = serde_json::from_slice::<Map<String, Value>>(text)
            && payload.get("type").and_then(Value::as_str) == Some("response.create")
        {
            if !gate_response_create(session, &payload) {
                break;
            }
            if !start_response(session, text.clone(), payload).await {
                break;
            }
            continue;
        }
        // Anything else (an interrupt, a binary frame) belongs to the response
        // already running on the bound upstream socket.
        let upstream = session.state().upstream.clone();
        if upstream.is_none_or(|upstream| !upstream.send(frame)) {
            break;
        }
    }
    pings.abort();
    shutdown(session);
}

/// Checks a response.create message as Codex writes it (codex_request.rs).
/// Nothing on a socket is completed, so both entries hold it to the native
/// rules. A refused message gets the wrapped error event a Codex reads as a
/// 400, which it does not retry, and the socket closes; nothing reaches
/// upstream.
fn gate_response_create(session: &Arc<Session>, payload: &Map<String, Value>) -> bool {
    use super::codex_request::{Body, Entry, check_responses, newer_than_pinned};
    let entry = if session.native {
        Entry::Native
    } else {
        Entry::V1
    };
    // A /v1 socket is Codex as a custom provider; a field it leaves out is
    // logged rather than refused, as for a newer Codex.
    match check_responses(
        payload,
        Entry::Native,
        Body::WebSocket,
        !session.native || newer_than_pinned(&session.handshake),
    ) {
        Ok(unknown) => {
            session
                .app
                .note_unrecognized_request_fields(entry.label(true), &unknown);
            true
        }
        Err(refusal) => {
            session
                .app
                .log_refused_request(entry.label(true), &session.api_key, &refusal);
            if let Ok(text) = serde_json::to_string(&refusal.websocket_event()) {
                session.send_client(Frame::Text(Bytes::from(text)));
            }
            session.close_client(1008, "request refused");
            false
        }
    }
}

async fn start_response(
    session: &Arc<Session>,
    mut message: Bytes,
    mut payload: Map<String, Value>,
) -> bool {
    let app = &session.app;
    let mut requested = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if requested.is_empty() {
        requested = app.read().config.default_model.clone();
    }
    let (mut model, tier) = parse_model(&requested);
    if let Some(alias) = app.read().config.model_aliases().get(&model) {
        model.clone_from(alias);
    }
    // Only a model suffix or alias changes the message; everything else is
    // forwarded as Codex sent it.
    if model != requested || (!tier.is_empty() && tier != "none") {
        payload.insert("model".into(), Value::String(model.clone()));
        if !tier.is_empty() && tier != "none" {
            payload.insert("reasoning".into(), serde_json::json!({"effort": tier}));
        }
        message = Bytes::from(super::gojson::marshal(&Value::Object(payload.clone())));
    }
    let mut route = app.routing_decision(&session.facts(), &payload, &model, &session.api_key);
    let warmup = payload.get("generate") == Some(&Value::Bool(false));
    let (bound, sent) = {
        let state = session.state();
        (state.account.clone(), state.sent)
    };
    let candidate = app.select_account_for_route(&mut route, &model, &HashMap::new());
    if candidate
        .as_ref()
        .map_or(true, |candidate| candidate.id != bound.id)
    {
        if sent {
            // Nothing of this request reached upstream; Codex resends it in
            // full after reconnecting, and routing places it then.
            session.close_client(1012, "account changed");
            return false;
        }
        // The first request settled the account differently from the
        // handshake's guess, and nothing has gone out on this socket: move it.
        if !redial(session, &mut route, &model).await {
            return false;
        }
    }
    let bound = session.state().account.clone();
    app.note_session_placement(&route.balance_key, &bound.id);
    app.note_account_activity(&bound.id);
    message = gate_turn_state(session, message, &payload, &route, &bound.id);
    if let Some(id) = app.presented_installation_id(&bound) {
        let text = String::from_utf8_lossy(&message).into_owned();
        message = Bytes::from(installation_id::with_installation_id(&text, &id).into_owned());
    }
    let throughput = (!warmup).then(|| {
        let mut measurement = app.begin_throughput_measurement(true);
        measurement.account_id.clone_from(&bound.id);
        measurement
    });
    let response = Arc::new(WsResponse {
        route,
        model,
        account_id: bound.id,
        warmup,
        finished: AtomicBool::new(false),
        state: Mutex::new(ResponseState {
            info: ProxyResponseInfo::default(),
            throughput,
            buffered: Vec::new(),
            committed: false,
        }),
    });
    let upstream = {
        let mut state = session.state();
        state.active = Some(response.clone());
        state.sent = true;
        state.upstream.clone()
    };
    // A running response holds the drain and the deploy idle wait like an
    // HTTP request; an idle socket between turns does not.
    app.in_flight.fetch_add(1, Ordering::SeqCst);
    if upstream.is_none_or(|upstream| !upstream.send(Frame::Text(message))) {
        finish(session, &response, false, true, "upstream_send_failed");
        return false;
    }
    true
}

/// Binds the session to an account's upstream socket, failing over the same
/// way an HTTP request does before anything reaches upstream. It does not
/// start the relay; the caller does once the client's socket exists.
async fn dial(
    session: &Arc<Session>,
    route: &mut RoutingDecision,
    model: &str,
) -> Result<crate::transport::UpstreamSocket, Refusal> {
    let app = &session.app;
    let mut excluded: HashMap<String, bool> = HashMap::new();
    let mut refusal = Refusal {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "all_accounts_cooling_down".to_string(),
        detail: "no eligible upstream account".to_string(),
    };
    for _ in 0..app.proxy_attempt_limit() {
        let Ok(candidate) = app.select_account_for_route(route, model, &excluded) else {
            break;
        };
        excluded.insert(candidate.id.clone(), true);
        app.note_session_placement(&route.balance_key, &candidate.id);
        if !App::uses_codex_backend_directly(&candidate) {
            // API-key providers have no Codex WebSocket; their sessions use
            // HTTP.
            continue;
        }
        let mut result = dial_codex_websocket(app, &candidate, &session.handshake, model).await;
        // A socket that could not be opened at all is retried on the same
        // account before the lineage moves.
        let mut retry = 1;
        while matches!(result, Err(WsDialError::Unreachable))
            && retry <= TRANSPORT_SAME_ACCOUNT_RETRY_LIMIT
            && !session.cancel.is_cancelled()
        {
            app.logger.printf(&format!(
                "upstream socket unreachable for {} (try {retry} of {} on this account)",
                candidate.id,
                TRANSPORT_SAME_ACCOUNT_RETRY_LIMIT + 1
            ));
            session
                .cancel
                .sleep(app.tuning.transport_retry_backoff * (1 << (retry - 1)))
                .await;
            result = dial_codex_websocket(app, &candidate, &session.handshake, model).await;
            retry += 1;
        }
        match result {
            Ok((socket, handshake)) => {
                let turn_state = header_value(&handshake, "x-codex-turn-state").to_string();
                if !turn_state.is_empty() {
                    app.note_turn_state_issuer(&turn_state, &candidate.id);
                }
                let mut state = session.state();
                state.account = candidate;
                state.upstream_headers = handshake;
                state.sent = false;
                return Ok(socket);
            }
            Err(WsDialError::Unreachable) => {
                app.mark_failure(
                    &candidate.id,
                    model,
                    "upstream_transport_error",
                    seconds(30),
                );
                refusal = Refusal {
                    status: StatusCode::BAD_GATEWAY,
                    code: "bad_gateway".to_string(),
                    detail: "upstream websocket unreachable".to_string(),
                };
            }
            Err(WsDialError::Refused {
                status,
                headers,
                body,
            }) => {
                let mut code = extract_upstream_error_code(&body);
                if code.is_empty() {
                    code = "upstream_refused".to_string();
                }
                refusal = Refusal {
                    status,
                    code: code.clone(),
                    detail: "upstream refused the websocket".to_string(),
                };
                if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                    app.mark_account_auth_failure(&candidate.id, model, &code);
                } else if status == StatusCode::TOO_MANY_REQUESTS {
                    let wait = super::proxy::retry_after_or_default(
                        header_value(&headers, "retry-after"),
                        Duration::from_secs(60),
                    );
                    app.mark_failure(
                        &candidate.id,
                        model,
                        "rate_limited",
                        chrono::TimeDelta::from_std(wait).unwrap_or_default(),
                    );
                } else if status == StatusCode::UPGRADE_REQUIRED {
                    // Upstream wants HTTP, which is the backend's answer, not
                    // this account's: tell the client at once, as a native
                    // Codex hears it on its one handshake, rather than knocking
                    // with every account.
                    return Err(refusal);
                } else {
                    app.mark_failure(&candidate.id, model, "upstream_5xx", seconds(30));
                }
            }
        }
    }
    Err(refusal)
}

/// Replaces the upstream socket for the first request on a client socket.
/// The old socket's relay sees it close and, no longer current, exits without
/// touching the client.
async fn redial(session: &Arc<Session>, route: &mut RoutingDecision, model: &str) -> bool {
    let old = session.state().upstream.take();
    if let Some(old) = old {
        old.close(1000, "");
    }
    match dial(session, route, model).await {
        Ok(socket) => {
            start_relay(session, socket);
            true
        }
        Err(refusal) => {
            // The wrapped error event is what the Codex backend itself sends
            // on a socket; Codex maps its status the way it maps an HTTP
            // status, so a 426 still means "fall back to HTTP" and a 429 or
            // 5xx is retried.
            let event = super::gojson::marshal(&serde_json::json!({
                "type": "error",
                "status": refusal.status.as_u16(),
                "error": {"code": refusal.code, "message": refusal.detail},
            }));
            session.send_client(Frame::Text(Bytes::from(event)));
            session.close_client(1011, "no upstream");
            false
        }
    }
}

/// A credential that cannot be refreshed counts as an unreachable socket,
/// as in Go's dialCodexWebSocket (an error, not a refusal).
enum WsDialError {
    Unreachable,
    Refused {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    },
}

/// Opens an account's upstream Responses socket with the handshake a native
/// Codex sends: the client's own Codex headers, the account's credential and
/// ChatGPT-Account-ID, and the routing hint and version a native Codex adds
/// for the Codex backend. A native handshake carries no turn state (core
/// client.rs build_websocket_headers). A 401 is answered like on HTTP:
/// refresh once and redial the same account.
async fn dial_codex_websocket(
    app: &App,
    candidate: &Account,
    client_headers: &HeaderMap,
    model: &str,
) -> Result<(crate::transport::UpstreamSocket, HeaderMap), WsDialError> {
    let auth = app
        .refreshed_codex_auth(candidate)
        .await
        .map_err(|_| WsDialError::Unreachable)?;
    match dial_codex_websocket_once(app, candidate, client_headers, model, &auth).await {
        Err(WsDialError::Refused {
            status,
            headers,
            body,
        }) if status == StatusCode::UNAUTHORIZED => {
            match app
                .refresh_codex_auth_after_rejection(candidate, &auth.access_token)
                .await
            {
                Ok(refreshed)
                    if !refreshed.access_token.is_empty()
                        && refreshed.access_token != auth.access_token =>
                {
                    dial_codex_websocket_once(app, candidate, client_headers, model, &refreshed)
                        .await
                }
                _ => Err(WsDialError::Refused {
                    status,
                    headers,
                    body,
                }),
            }
        }
        other => other,
    }
}

async fn dial_codex_websocket_once(
    app: &App,
    candidate: &Account,
    client_headers: &HeaderMap,
    model: &str,
    auth: &super::metadata::CodexAuthInfo,
) -> Result<(crate::transport::UpstreamSocket, HeaderMap), WsDialError> {
    app.note_client_agent(&candidate.id, client_headers);
    app.note_account_activity(&candidate.id);
    let mut headers = GoHeader::default();
    for (name, value) in client_headers {
        if relayed_codex_request_header(name.as_str())
            && let Ok(value) = value.to_str()
            && !value.trim().is_empty()
        {
            headers.add(name.as_str(), value.trim());
        }
    }
    headers.del("X-Codex-Turn-State");
    // The socket is the account's, so its handshake and every message on it
    // present the account's installation id.
    if let Some(id) = app.presented_installation_id(candidate) {
        let metadata = headers.get("X-Codex-Turn-Metadata").to_string();
        if !metadata.is_empty() {
            headers.set(
                "X-Codex-Turn-Metadata",
                &installation_id::turn_metadata_with_installation_id(&metadata, &id),
            );
        }
    }
    let account_id = if auth.account_id.is_empty() {
        candidate.account_id.clone()
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
    // A native Codex names the model in its handshake's routing hint; a
    // client that sent none gets the handshake's model. The service tier is
    // only in the request body, so such a client's hint carries no tier.
    if header_value(client_headers, "x-codex-routing-hint")
        .trim()
        .is_empty()
        && !header_value(client_headers, "x-codex-guardian")
            .trim()
            .eq_ignore_ascii_case("reviewer")
        && !model.is_empty()
    {
        headers.set("X-Codex-Routing-Hint", &format!("model={model}"));
    }
    let version = codex_client_version(client_headers);
    if !version.is_empty() && headers.get("Version").is_empty() {
        headers.set("Version", &version);
    }
    let base = format!("{}/responses", app.settings.codex_responses_base_url());
    let url = match base.strip_prefix("http") {
        Some(rest) => format!("ws{rest}"),
        None => base,
    };
    crate::transport::connect_websocket(&app.transports, &url, &headers.to_map())
        .await
        .map_err(|refusal| match refusal {
            WebSocketRefusal::Http {
                status,
                headers,
                body,
            } => WsDialError::Refused {
                status,
                headers,
                body,
            },
            WebSocketRefusal::Connection(_) => WsDialError::Unreachable,
        })
}

/// Starts the writer and the relay for a newly bound upstream socket.
fn start_relay(session: &Arc<Session>, socket: crate::transport::UpstreamSocket) {
    let (sink, stream) = socket.split();
    let (tx, rx) = mpsc::unbounded_channel();
    let id = session.next_upstream.fetch_add(1, Ordering::SeqCst);
    let upstream = Upstream { id, tx };
    let account_id = {
        let mut state = session.state();
        state.upstream = Some(upstream.clone());
        state.account.id.clone()
    };
    tokio::spawn(write_loop(sink, rx));
    let session = session.clone();
    tokio::spawn(async move { relay_upstream(session, upstream, account_id, stream).await });
}

/// Forwards upstream events to Codex with the same pre-commit rule as the
/// HTTP stream. Lifecycle events are held until the first event carrying
/// content, because a native Codex does not retry an overload that arrives
/// without retry advice: it shows "Selected model is at capacity" and stops
/// the turn. A capacity, transient or authentication failure before content
/// is never shown to Codex and never retried on this socket: a native Codex
/// drops a socket on any failed response ("A terminal stream error should
/// reach the caller immediately", codex-api endpoint/responses_websocket.rs
/// stream_request) and its retry opens a new one and resends in full. A resend
/// on the failed socket was not what Codex sends: it went out while upstream
/// was still finishing the failed response, read that response's trailing
/// response.failed as its own, resent again, and upstream then closed the
/// socket, so one turn reached upstream four times instead of twice. The pool
/// closes the upstream socket at once and Codex's socket after the
/// same-account backoff, so Codex makes the retry itself. An overload inside
/// the same-account schedule leaves the account uncooled, and Codex's
/// reconnect lands on it again; past the schedule, or when the allowance or
/// credential failed, the account is cooled and routing moves the whole
/// lineage. A failure after content has reached Codex is relayed as it is.
async fn relay_upstream<S>(
    session: Arc<Session>,
    upstream: Upstream,
    account_id: String,
    mut stream: S,
) where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let app = session.app.clone();
    loop {
        let message = match stream.next().await {
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                upstream_gone(&session, &upstream);
                return;
            }
            Some(Ok(message)) => message,
        };
        let Some(frame) = Frame::from_message(message) else {
            continue;
        };
        let response = session.state().active.clone();
        if let Frame::Text(text) = &frame {
            app.note_event_turn_state(text, &account_id);
        }
        let text = match &frame {
            Frame::Text(text) => Some(text.clone()),
            Frame::Binary(_) => None,
        };
        let committed = response
            .as_ref()
            .is_none_or(|response| lock(&response.state).committed);
        let (Some(text), Some(response), false) = (text.clone(), response.clone(), committed)
        else {
            if !session.send_client(frame) {
                upstream.close(1000, "");
                return;
            }
            if let (Some(text), Some(response)) = (text, response)
                && !record(&session, &response, &text)
            {
                upstream.close(1000, "");
                return;
            }
            continue;
        };
        let block = format!("data: {}\n\n", String::from_utf8_lossy(&text));
        let mut info = response_info_from_sse_block(&block);
        mark_websocket_terminal(&mut info);
        {
            let mut state = lock(&response.state);
            if info.terminal_event.is_empty()
                && precommit_lifecycle_sse_block(&block)
                && state.buffered.len() < WS_PRECOMMIT_MAX_BLOCKS
            {
                state.info.merge(info);
                state.buffered.push(text.clone());
                continue;
            }
        }
        if !info.terminal_event.is_empty()
            && info.terminal_event != "response.completed"
            && (retryable_terminal_failure_class(&info.terminal_failure_class)
                || info.terminal_failure_class == "authentication")
        {
            // Some(n): the nth overload in a row on this account and model,
            // still inside the same-account schedule.
            let retry = transient_overload_code(&info.terminal_error_code)
                .then(|| app.note_ws_overload_drop(&response.account_id, &response.model))
                .filter(|drops| *drops <= OVERLOAD_SAME_ACCOUNT_RETRY_LIMIT);
            app.mark_retryable_terminal_response_failure(
                &response.account_id,
                &response.model,
                &info,
                retry.is_none(),
            );
            session.state().active = None;
            finish(
                &session,
                &response,
                false,
                true,
                "handed_to_codex_reconnect",
            );
            upstream.close(1000, "");
            if retry.is_some() {
                // The same-account backoff, so Codex's reconnect does not
                // reach upstream sooner than the overload's retry advice.
                if !session.closed.sleep(app.tuning.overload_retry_wait()).await {
                    return;
                }
            }
            session.close_client(1012, "upstream account unavailable");
            return;
        }
        // First content, or a failure that is the caller's own: release the
        // held preamble and stream from here on.
        let held = {
            let mut state = lock(&response.state);
            state.committed = true;
            std::mem::take(&mut state.buffered)
        };
        for message in held {
            if !session.send_client(Frame::Text(message)) {
                upstream.close(1000, "");
                return;
            }
        }
        if !session.send_client(Frame::Text(text.clone())) || !record(&session, &response, &text) {
            upstream.close(1000, "");
            return;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Accumulates a relayed event into the response and finishes it at its
/// terminal event. False when the socket was closed because the account must
/// not serve the next request.
fn record(session: &Arc<Session>, response: &Arc<WsResponse>, message: &[u8]) -> bool {
    let mut info =
        response_info_from_sse_block(&format!("data: {}\n\n", String::from_utf8_lossy(message)));
    mark_websocket_terminal(&mut info);
    let terminal = info.terminal_event.clone();
    let merged = {
        let mut state = lock(&response.state);
        state.info.merge(info);
        state.info.clone()
    };
    if terminal.is_empty() {
        return true;
    }
    {
        let mut state = session.state();
        if state
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, response))
        {
            state.active = None;
        }
    }
    let failed = terminal != "response.completed";
    let overloaded = failed && transient_overload_code(&merged.terminal_error_code);
    finish(session, response, true, !overloaded, "");
    if failed
        && !overloaded
        && (retryable_terminal_failure_class(&merged.terminal_failure_class)
            || merged.terminal_failure_class == "authentication")
    {
        // The failure reached Codex after output; the next request must not
        // land on this account's socket.
        session.close_client(1012, "upstream account unavailable");
        return false;
    }
    true
}

/// Records a response exactly once. `delivered` is false when the request
/// never reached upstream or the socket died before a terminal event, and
/// `reason` then names why for the interrupted-request log line;
/// `cool_down` false keeps an overload inside the same-account schedule from
/// cooling the account.
fn finish(
    session: &Arc<Session>,
    response: &Arc<WsResponse>,
    delivered: bool,
    cool_down: bool,
    reason: &'static str,
) {
    if response.finished.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = &session.app;
    if !response.warmup {
        // A prewarm is connection setup, not a model request.
        let (mut info, measurement) = {
            let mut state = lock(&response.state);
            (state.info.clone(), state.throughput.take())
        };
        info.completed_at = Timestamp::now();
        let mut measurement = measurement.unwrap_or_default();
        measurement.completed_at = info.completed_at;
        measurement.usage = info.usage.clone();
        measurement.model.clone_from(&response.model);
        if !delivered {
            measurement.cancelled = true;
            measurement.cancel_reason = reason;
            app.finish_throughput_measurement(&mut measurement);
        } else if info.terminal_event == "response.completed" {
            measurement.success = true;
            app.clear_ws_overload_drops(&response.account_id, &response.model);
            app.mark_success_with_measurement(
                &response.route,
                &response.model,
                &response.account_id,
                &info,
                Some(&mut measurement),
            );
        } else {
            app.mark_terminal_response_failure(
                &response.route,
                &response.model,
                &response.account_id,
                &info,
                Some(&mut measurement),
                cool_down,
            );
        }
    }
    // Released last, after the response is recorded, so the drain never sees
    // an idle pool while a result is still being saved.
    app.in_flight.fetch_sub(1, Ordering::SeqCst);
}

fn upstream_gone(session: &Arc<Session>, upstream: &Upstream) {
    let (response, account_id) = {
        let mut state = session.state();
        if state
            .upstream
            .as_ref()
            .is_none_or(|current| current.id != upstream.id)
        {
            // A socket replaced by redial: the session has already moved on.
            drop(state);
            upstream.close(1000, "");
            return;
        }
        (state.active.take(), state.account.id.clone())
    };
    if let Some(response) = response {
        // Recorded, but without a cooldown: a dropped socket is no evidence
        // against the account, and a native Codex reconnects to the same one.
        // A cooldown here sent Codex's immediate reconnect, and so the whole
        // lineage, to another account; a reconnect that cannot open a socket is
        // retried on this account and only then moves (see dial).
        session.app.mark_failure(
            &account_id,
            &response.model,
            "upstream_transport_error",
            chrono::TimeDelta::zero(),
        );
        finish(session, &response, false, true, "upstream_closed");
    }
    upstream.close(1000, "");
    session.close_client(1011, "upstream closed");
}

fn shutdown(session: &Arc<Session>) {
    session.closed.cancel();
    let (upstream, response) = {
        let mut state = session.state();
        (state.upstream.take(), state.active.take())
    };
    if let Some(response) = response {
        finish(session, &response, false, true, "client_closed");
    }
    if let Some(upstream) = upstream {
        upstream.close(1000, "");
    }
    let _ = session.client.send(Outgoing::Close(1000, String::new()));
}

/// Applies the HTTP turn-state rule (turn_state_for_attempt) to a socket,
/// where Codex carries the turn state in the message's client_metadata rather
/// than a header: only the account that issued it may receive it. After the
/// session moved accounts, Codex still holds the old account's token for the
/// turn, and a native Codex never presents one account's turn token to
/// another, so it is removed from the message, leaving every other byte as
/// Codex wrote it.
fn gate_turn_state(
    session: &Arc<Session>,
    message: Bytes,
    payload: &Map<String, Value>,
    route: &RoutingDecision,
    bound: &str,
) -> Bytes {
    let value = payload
        .get("client_metadata")
        .and_then(|metadata| metadata.get("x-codex-turn-state"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if value.is_empty() {
        return message;
    }
    let mut issuer = session.app.turn_state_issuer(&value);
    if issuer.is_empty() {
        issuer = session.app.sticky_session_account_id(&route.sticky_key);
    }
    if issuer == bound {
        return message;
    }
    without_client_metadata_field(&message, payload, "x-codex-turn-state", &value)
}

/// Removes one client_metadata field from a message without re-encoding the
/// rest. Codex writes compact JSON, so the field is a literal "key":"value"
/// pair; only if that pair cannot be found is the message re-encoded.
pub fn without_client_metadata_field(
    message: &[u8],
    payload: &Map<String, Value>,
    key: &str,
    value: &str,
) -> Bytes {
    let pair = format!(
        "{}:{}",
        serde_json::Value::String(key.to_string()),
        serde_json::Value::String(value.to_string())
    );
    for candidate in [format!(",{pair}"), format!("{pair},"), pair] {
        let Some(index) = find(message, candidate.as_bytes()) else {
            continue;
        };
        let mut edited = message[..index].to_vec();
        edited.extend_from_slice(&message[index + candidate.len()..]);
        if let Ok(check) = serde_json::from_slice::<Map<String, Value>>(&edited)
            && let Some(metadata) = check.get("client_metadata").and_then(Value::as_object)
            && !metadata.contains_key(key)
        {
            return Bytes::from(edited);
        }
    }
    let mut payload = payload.clone();
    if let Some(Value::Object(metadata)) = payload.get_mut("client_metadata") {
        metadata.remove(key);
    }
    serde_json::to_vec(&payload)
        .map(Bytes::from)
        .unwrap_or_else(|_| Bytes::copy_from_slice(message))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Records that a terminal failure came on the Responses WebSocket, which
/// Codex reads differently from SSE (a wrapped error first).
fn mark_websocket_terminal(info: &mut ProxyResponseInfo) {
    if !info.terminal_data.is_empty() {
        info.terminal_source = "ws".to_string();
    }
}

impl App {
    /// Counts an overload that closed a socket before content, per account
    /// and model, and returns the count. Each one hands the retry to Codex's
    /// reconnect, a new socket, so the same-account schedule is kept here
    /// rather than on the socket; the count past the schedule cools the
    /// account and starts over.
    pub fn note_ws_overload_drop(&self, account_id: &str, model: &str) -> u32 {
        let mut drops = self
            .ws_overload_drops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = format!("{account_id}\n{model}");
        let count = drops.entry(key.clone()).or_insert(0);
        *count += 1;
        let count = *count;
        if count > OVERLOAD_SAME_ACCOUNT_RETRY_LIMIT {
            drops.remove(&key);
        }
        count
    }

    /// A completed response ends the account's run of overloads.
    pub fn clear_ws_overload_drops(&self, account_id: &str, model: &str) {
        self.ws_overload_drops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&format!("{account_id}\n{model}"));
    }

    pub fn note_turn_state_issuer(&self, value: &str, account_id: &str) {
        if value.is_empty() || account_id.is_empty() {
            return;
        }
        let now = Timestamp::now();
        let ttl = super::time::hours(TURN_STATE_ISSUER_TTL_HOURS);
        let mut issuers = self
            .turn_state_issuers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if issuers.len() >= TURN_STATE_ISSUER_MAX {
            issuers.retain(|_, (_, at)| now.sub(at) <= ttl);
            while issuers.len() >= TURN_STATE_ISSUER_MAX {
                let Some(key) = issuers.keys().next().cloned() else {
                    break;
                };
                issuers.remove(&key);
            }
        }
        issuers.insert(value.to_string(), (account_id.to_string(), now));
    }

    pub fn turn_state_issuer(&self, value: &str) -> String {
        let issuers = self
            .turn_state_issuers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match issuers.get(value) {
            Some((account_id, at))
                if Timestamp::now().sub(at) <= super::time::hours(TURN_STATE_ISSUER_TTL_HOURS) =>
            {
                account_id.clone()
            }
            _ => String::new(),
        }
    }

    /// Records a turn state an upstream event carries in its headers, so a
    /// later message presenting it is known to be this account's.
    pub fn note_event_turn_state(&self, message: &[u8], account_id: &str) {
        if find(message, b"x-codex-turn-state").is_none() {
            return;
        }
        let Ok(event) = serde_json::from_slice::<Map<String, Value>>(message) else {
            return;
        };
        if let Some(headers) = event.get("headers").and_then(Value::as_object) {
            for (name, value) in headers {
                if let Some(text) = value.as_str()
                    && name.eq_ignore_ascii_case("x-codex-turn-state")
                {
                    self.note_turn_state_issuer(text, account_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_one_metadata_field_in_place() {
        let message = br#"{"type":"response.create","client_metadata":{"a":"1","x-codex-turn-state":"tok","b":"2"},"input":[]}"#;
        let payload: Map<String, Value> = serde_json::from_slice(message).unwrap_or_default();
        let edited = without_client_metadata_field(message, &payload, "x-codex-turn-state", "tok");
        assert_eq!(
            String::from_utf8_lossy(&edited),
            r#"{"type":"response.create","client_metadata":{"a":"1","b":"2"},"input":[]}"#
        );
    }
}
