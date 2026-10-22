//! The pool's one listener: its route table and the provider handlers.
//!
//! A port of mux() in cmd/codex-pool/main.go. Surface contract: one listener
//! serves both machine clients and humans. Provider routes stay under /v1 and
//! require the client API key; the control page stays at /admin (and the
//! convenient root alias), while owner-only actions keep their session and
//! CSRF checks. Do not collapse these auth policies merely because the network
//! port is shared.
//!
//! Matching follows Go's ServeMux for the patterns registered here: a path
//! that matches with another method is answered 405 with Allow, an unknown
//! path 404, and GET patterns also serve HEAD.

use super::app::App;
use super::http_io::Cancel;
use super::http_io::ResponseWriter;
use super::http_io::set_header;
use super::http_io::write_json;
use super::http_io::write_openai_error;
use super::proxy::ProxyRequest;
use super::proxy::handle_proxy;
use super::proxy::parse_model;
use super::server::query_param;
use super::server::request_api_key;
use super::status::to_value;
use super::time::Timestamp;
use bytes::Bytes;
use http::HeaderMap;
use http::Method;
use http::StatusCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// One inbound request, read whole (the provider routes read at most
/// MAX_REQUEST_BODY + 1 bytes, so an oversized body is still recognized).
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    /// The client's address as the pool judges it (after the front door's
    /// peer header).
    pub remote_addr: String,
    pub cancel: Cancel,
    /// The connection, for a WebSocket upgrade.
    pub upgrade: Option<hyper::upgrade::OnUpgrade>,
}

impl Request {
    /// A request built in a test (Go httptest.NewRequest).
    pub fn test(method: Method, target: &str, body: &str) -> Self {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        Self {
            method,
            path: path.to_string(),
            query: query.to_string(),
            headers: HeaderMap::new(),
            body: Bytes::from(body.to_string()),
            remote_addr: "192.0.2.1:1234".to_string(),
            cancel: Cancel::default(),
            upgrade: None,
        }
    }

    pub fn with_header(mut self, name: &'static str, value: &str) -> Self {
        if let Ok(value) = http::HeaderValue::from_str(value) {
            self.headers.insert(name, value);
        }
        self
    }
}

const NATIVE_PREFIX: &str = "/k/";
const NATIVE_INFIX: &str = "/backend-api/codex/";

enum Route {
    Root,
    Healthz,
    Status,
    Models,
    Responses,
    ResponsesWebSocket,
    Compact,
    Chat,
    InFlight,
    Native { key: String, rest: String },
}

enum Miss {
    NotFound,
    MethodNotAllowed(&'static str),
}

fn route(method: &Method, path: &str) -> Result<Route, Miss> {
    let get = *method == Method::GET || *method == Method::HEAD;
    let post = *method == Method::POST;
    let only = |allowed: bool, route: Route, allow: &'static str| {
        if allowed {
            Ok(route)
        } else {
            Err(Miss::MethodNotAllowed(allow))
        }
    };
    match path {
        "/" => only(get, Route::Root, "GET, HEAD"),
        "/healthz" => only(get, Route::Healthz, "GET, HEAD"),
        "/v1/codex-pool/status" => only(get, Route::Status, "GET, HEAD"),
        "/v1/models" => only(get, Route::Models, "GET, HEAD"),
        "/v1/responses" if post => Ok(Route::Responses),
        "/v1/responses" if get => Ok(Route::ResponsesWebSocket),
        "/v1/responses" => Err(Miss::MethodNotAllowed("GET, HEAD, POST")),
        "/v1/responses/compact" => only(post, Route::Compact, "POST"),
        "/v1/chat/completions" => only(post, Route::Chat, "POST"),
        "/internal/inflight" => only(get, Route::InFlight, "GET, HEAD"),
        _ => {
            // "/k/{key}/backend-api/codex/{rest...}": the key is one segment.
            let rest = path.strip_prefix(NATIVE_PREFIX).ok_or(Miss::NotFound)?;
            let (key, rest) = rest.split_once('/').ok_or(Miss::NotFound)?;
            let rest = format!("/{rest}");
            let rest = rest
                .strip_prefix(NATIVE_INFIX.trim_end_matches('/'))
                .ok_or(Miss::NotFound)?;
            let rest = rest.strip_prefix('/').ok_or(Miss::NotFound)?;
            if key.is_empty() {
                return Err(Miss::NotFound);
            }
            Ok(Route::Native {
                key: key.to_string(),
                rest: rest.to_string(),
            })
        }
    }
}

/// Answers one request. A panic in a handler is answered 500, as Go's
/// recovery middleware does, when nothing was written yet.
pub async fn serve(app: &Arc<App>, request: Request, writer: Box<dyn ResponseWriter>) {
    let mut writer = writer;
    let route = match route(&request.method, &request.path) {
        Ok(route) => route,
        Err(Miss::NotFound) => {
            set_header(writer.as_mut(), "content-type", "text/plain; charset=utf-8");
            set_header(writer.as_mut(), "x-content-type-options", "nosniff");
            writer.write_header(StatusCode::NOT_FOUND);
            writer.write(b"404 page not found\n");
            return;
        }
        Err(Miss::MethodNotAllowed(allow)) => {
            set_header(writer.as_mut(), "allow", allow);
            set_header(writer.as_mut(), "content-type", "text/plain; charset=utf-8");
            set_header(writer.as_mut(), "x-content-type-options", "nosniff");
            writer.write_header(StatusCode::METHOD_NOT_ALLOWED);
            writer.write(b"Method Not Allowed\n");
            return;
        }
    };
    let api_key_ok =
        |request: &Request| app.valid_api_key(&request_api_key(&request.headers, &request.query));
    match route {
        Route::Root => super::admin::handle_admin_page(writer.as_mut()),
        Route::InFlight => handle_in_flight(app, &request, writer.as_mut()),
        Route::Native { key, rest } => {
            super::native::handle_native(app, request, key, rest, writer).await
        }
        _ if !api_key_ok(&request) => {
            write_openai_error(
                writer.as_mut(),
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "invalid or missing API key",
            );
        }
        Route::Healthz => write_json(
            writer.as_mut(),
            StatusCode::OK,
            &serde_json::json!({"ok": true}),
        ),
        Route::Status => handle_current_status(app, &request, writer.as_mut()),
        Route::Models => handle_models(app, &request, writer.as_mut()),
        Route::ResponsesWebSocket => {
            super::responses_ws::handle_responses_websocket(app, request, false, writer).await
        }
        Route::Responses | Route::Compact | Route::Chat => {
            let chat = matches!(route, Route::Chat);
            serve_provider(app, request, false, writer, chat).await;
        }
    }
}

/// A provider request (responses, compact, chat), counted in flight for the
/// drain and the deploy idle wait. Only provider routes are counted: the
/// dashboard and admin API are short and must not hold a deploy back.
pub async fn serve_provider(
    app: &Arc<App>,
    request: Request,
    native: bool,
    writer: Box<dyn ResponseWriter>,
    chat: bool,
) {
    app.in_flight.fetch_add(1, Ordering::SeqCst);
    let proxy = ProxyRequest {
        path: request.path,
        query: request.query,
        headers: request.headers,
        body: request.body,
        native,
        cancel: request.cancel,
    };
    handle_proxy(app, proxy, writer, chat).await;
    app.in_flight.fetch_sub(1, Ordering::SeqCst);
}

/// Reports the in-flight provider request count to the deploy script, which
/// polls it from inside the container to restart only when the pool is idle.
/// It answers loopback callers only: external traffic reaches the pool from a
/// non-loopback address, so this count, which reveals current usage, is never
/// served to the network.
fn handle_in_flight(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    if !super::settings::is_loopback_address(&request.remote_addr)
        || request.remote_addr.starts_with("localhost")
    {
        set_header(writer, "content-type", "text/plain; charset=utf-8");
        set_header(writer, "x-content-type-options", "nosniff");
        writer.write_header(StatusCode::NOT_FOUND);
        writer.write(b"404 page not found\n");
        return;
    }
    write_json(
        writer,
        StatusCode::OK,
        &serde_json::json!({"inFlight": app.in_flight.load(Ordering::SeqCst)}),
    );
}

pub fn resolve_model(app: &App, requested: &str) -> String {
    let inner = app.read();
    let requested = if requested.trim().is_empty() {
        inner.config.default_model.clone()
    } else {
        requested.to_string()
    };
    let (model, _) = parse_model(&requested);
    inner
        .config
        .model_aliases()
        .get(&model)
        .cloned()
        .unwrap_or(model)
}

fn current_status_sticky_key(request: &Request, model: &str) -> Option<(String, &'static str)> {
    [
        (
            super::http_io::header_value(&request.headers, "x-codex-pool-session").to_string(),
            "session",
        ),
        (query_param(&request.query, "session"), "session"),
        (
            super::http_io::header_value(&request.headers, "x-codex-pool-project").to_string(),
            "project",
        ),
        (query_param(&request.query, "project"), "project"),
    ]
    .into_iter()
    .find(|(value, _)| !value.trim().is_empty())
    .map(|(value, source)| (format!("{model}:{}", value.trim()), source))
}

fn handle_current_status(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    let model = resolve_model(app, &query_param(&request.query, "model"));
    let now = Timestamp::now();
    let inner = app.read();
    let (session, scope) = match current_status_sticky_key(request, &model) {
        Some((key, scope)) => (
            inner
                .state
                .sticky_sessions()
                .get(&key)
                .filter(|session| !inner.sticky_session_expired(session, now))
                .cloned(),
            scope,
        ),
        None => (inner.latest_sticky_session(&model, now), "latest"),
    };
    let Some(session) = session else {
        drop(inner);
        write_openai_error(
            writer,
            StatusCode::NOT_FOUND,
            "current_account_not_found",
            "no current account is bound to the requested model/session",
        );
        return;
    };
    let Some(index) = inner.account_index(&session.account_id) else {
        drop(inner);
        write_openai_error(
            writer,
            StatusCode::NOT_FOUND,
            "current_account_not_found",
            "current account is no longer configured",
        );
        return;
    };
    let item = inner.config.accounts()[index].clone();
    let expires_at = inner.sticky_expires_at(&session);
    let account = inner.current_account_status(&item, index, now);
    drop(inner);
    let mut response = serde_json::json!({
        "ok": true,
        "model": model,
        "scope": {"type": scope, "lastSuccessAt": session.last_success_at, "expiresAt": expires_at},
        "account": account,
    });
    if scope == "latest" {
        response["warning"] = serde_json::Value::String(
            "No session or project was provided; returning the most recent active sticky session for this model.".to_string(),
        );
    }
    write_json(writer, StatusCode::OK, &response);
}

fn handle_models(app: &App, request: &Request, writer: &mut dyn ResponseWriter) {
    let inner = app.read();
    let models = inner.models();
    if !query_param(&request.query, "client_version").is_empty() {
        // Codex decodes this endpoint with its remote-model schema, not the
        // loose OpenAI model-list shape below. Keep reasoning effort as
        // structured model capability metadata and collapse legacy
        // `(high)`-style aliases to one canonical model. Omitting required
        // capability fields makes the model manager retry during app-server
        // startup and can starve unrelated MCP app initialization until it
        // times out.
        let catalog = to_value(&inner.codex_model_catalog(&models));
        drop(inner);
        write_json(
            writer,
            StatusCode::OK,
            &serde_json::json!({"models": catalog}),
        );
        return;
    }
    drop(inner);
    let items: Vec<serde_json::Value> = models
        .iter()
        .map(|model| serde_json::json!({"id": model, "object": "model", "created": 0, "owned_by": "codex-pool"}))
        .collect();
    write_json(
        writer,
        StatusCode::OK,
        &serde_json::json!({"object": "list", "data": items}),
    );
}
