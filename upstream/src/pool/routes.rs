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

/// What a matched pattern serves.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Handler {
    AdminPage,
    AdminAsset,
    Healthz,
    Status,
    Models,
    Responses,
    ResponsesWebSocket,
    Compact,
    Chat,
    Native,
    InFlight,
    PublicDashboard,
    PublicAccountAction,
    AdminLogin,
    AdminLogout,
    AdminState,
    AdminSettings,
    Accounts,
    AccountHealth,
    RefreshAllQuota,
    ResetPromptCacheWindow,
    Job,
    JobCancel,
    StickySessions,
    StickySessionDelete,
    AccountAction,
    AccountDelete,
}

/// How a pattern's path matches (Go 1.22 ServeMux).
#[derive(Clone, Copy)]
enum PathPattern {
    /// "/a/b": exactly this path; "/{$}" is Exact("/").
    Exact(&'static str),
    /// "/a/": this path and everything under it.
    Subtree(&'static str),
    /// "/k/{key}/backend-api/codex/{rest...}".
    Native,
}

impl PathPattern {
    fn matches(self, path: &str) -> bool {
        match self {
            Self::Exact(pattern) => path == pattern,
            Self::Subtree(prefix) => path.starts_with(prefix),
            Self::Native => native_parts(path).is_some(),
        }
    }

    /// Go's precedence for these patterns: an exact path beats any subtree,
    /// a longer subtree beats a shorter one.
    fn specificity(self) -> (u8, usize) {
        match self {
            Self::Exact(pattern) => (2, pattern.len()),
            Self::Native => (1, usize::MAX),
            Self::Subtree(prefix) => (1, prefix.len()),
        }
    }
}

/// The pool's route table (Go mux()). `None` as the method matches every
/// method; "GET" also matches HEAD.
///
/// Surface contract: one listener serves both machine clients and humans.
/// Provider routes stay under /v1 and require the client API key; the
/// control page stays at /admin (and the root alias), visible without a
/// password, while owner-only actions keep their session and CSRF checks.
/// The unauthenticated chrome deliberately uses low-key wording: do not
/// "clarify" the visible title or login copy into obvious Codex, pool or
/// provider terms without owner approval. That is only passive exposure
/// reduction; require_admin remains the security boundary.
const ROUTES: &[(Option<&str>, PathPattern, Handler)] = &[
    (Some("GET"), PathPattern::Exact("/"), Handler::AdminPage),
    (
        Some("GET"),
        PathPattern::Exact("/healthz"),
        Handler::Healthz,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/v1/codex-pool/status"),
        Handler::Status,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/v1/models"),
        Handler::Models,
    ),
    (
        Some("POST"),
        PathPattern::Exact("/v1/responses"),
        Handler::Responses,
    ),
    // A Responses WebSocket stays open across turns, so it is not counted as
    // in flight; each response running on it is.
    (
        Some("GET"),
        PathPattern::Exact("/v1/responses"),
        Handler::ResponsesWebSocket,
    ),
    (
        Some("POST"),
        PathPattern::Exact("/v1/responses/compact"),
        Handler::Compact,
    ),
    (
        Some("POST"),
        PathPattern::Exact("/v1/chat/completions"),
        Handler::Chat,
    ),
    (None, PathPattern::Native, Handler::Native),
    (
        Some("GET"),
        PathPattern::Exact("/internal/inflight"),
        Handler::InFlight,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/admin"),
        Handler::AdminPage,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/admin/assets/app.css"),
        Handler::AdminAsset,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/admin/assets/app.js"),
        Handler::AdminAsset,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/admin/assets/logo.svg"),
        Handler::AdminAsset,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/admin/manifest.webmanifest"),
        Handler::AdminAsset,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/admin/api/public-dashboard"),
        Handler::PublicDashboard,
    ),
    (
        Some("GET"),
        PathPattern::Subtree("/admin/api/public-dashboard/accounts/"),
        Handler::PublicAccountAction,
    ),
    (
        Some("POST"),
        PathPattern::Subtree("/admin/api/public-dashboard/accounts/"),
        Handler::PublicAccountAction,
    ),
    (
        Some("POST"),
        PathPattern::Exact("/admin/api/login"),
        Handler::AdminLogin,
    ),
    (
        Some("POST"),
        PathPattern::Exact("/admin/api/logout"),
        Handler::AdminLogout,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/admin/api/state"),
        Handler::AdminState,
    ),
    (
        Some("POST"),
        PathPattern::Exact("/admin/api/settings"),
        Handler::AdminSettings,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/admin/api/accounts"),
        Handler::Accounts,
    ),
    (
        Some("POST"),
        PathPattern::Exact("/admin/api/accounts"),
        Handler::Accounts,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/admin/api/accounts/health"),
        Handler::AccountHealth,
    ),
    (
        Some("POST"),
        PathPattern::Exact("/admin/api/accounts/quota/refresh-all"),
        Handler::RefreshAllQuota,
    ),
    (
        Some("POST"),
        PathPattern::Exact("/admin/api/cache/reset"),
        Handler::ResetPromptCacheWindow,
    ),
    (
        Some("GET"),
        PathPattern::Subtree("/admin/api/jobs/"),
        Handler::Job,
    ),
    (
        Some("POST"),
        PathPattern::Subtree("/admin/api/jobs/"),
        Handler::JobCancel,
    ),
    (
        Some("GET"),
        PathPattern::Exact("/admin/api/sticky-sessions"),
        Handler::StickySessions,
    ),
    (
        Some("DELETE"),
        PathPattern::Subtree("/admin/api/sticky-sessions/"),
        Handler::StickySessionDelete,
    ),
    (
        Some("POST"),
        PathPattern::Subtree("/admin/api/accounts/"),
        Handler::AccountAction,
    ),
    (
        Some("DELETE"),
        PathPattern::Subtree("/admin/api/accounts/"),
        Handler::AccountDelete,
    ),
];

fn method_matches(pattern: Option<&str>, method: &Method) -> bool {
    match pattern {
        None => true,
        Some("GET") => *method == Method::GET || *method == Method::HEAD,
        Some(pattern) => method.as_str() == pattern,
    }
}

/// The most specific pattern for the path and method.
fn find(method: &Method, path: &str) -> Option<Handler> {
    ROUTES
        .iter()
        .filter(|(pattern_method, pattern, _)| {
            method_matches(*pattern_method, method) && pattern.matches(path)
        })
        .max_by_key(|(_, pattern, _)| pattern.specificity())
        .map(|(_, _, handler)| *handler)
}

/// Whether the path matches a pattern exactly rather than as part of a
/// subtree (Go exactMatch).
fn exact_match(method: &Method, path: &str) -> bool {
    ROUTES.iter().any(|(pattern_method, pattern, _)| {
        method_matches(*pattern_method, method)
            && match pattern {
                PathPattern::Exact(exact) | PathPattern::Subtree(exact) => *exact == path,
                PathPattern::Native => false,
            }
    })
}

/// Methods some pattern serves for the path (or the path with a trailing
/// slash), sorted, with HEAD wherever GET is (Go matchingMethods).
fn matching_methods(path: &str) -> Vec<&'static str> {
    let mut methods: Vec<&'static str> = Vec::new();
    for candidate in [path.to_string(), format!("{path}/")] {
        if candidate != path && path.ends_with('/') {
            continue;
        }
        for (method, pattern, _) in ROUTES {
            if let Some(method) = method
                && pattern.matches(&candidate)
            {
                methods.push(method);
                if *method == "GET" {
                    methods.push("HEAD");
                }
            }
        }
    }
    methods.sort_unstable();
    methods.dedup();
    methods
}

/// Go's cleanPath: path.Clean, keeping a trailing slash.
fn clean_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    let rooted = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in rooted.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut cleaned = format!("/{}", parts.join("/"));
    if rooted.ends_with('/') && cleaned != "/" {
        cleaned.push('/');
    }
    cleaned
}

/// "/k/{key}/backend-api/codex/{rest...}": the key and the rest.
fn native_parts(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix(NATIVE_PREFIX)?;
    let (key, rest) = rest.split_once('/')?;
    let rest = format!("/{rest}");
    let rest = rest.strip_prefix(NATIVE_INFIX.trim_end_matches('/'))?;
    let rest = rest.strip_prefix('/')?;
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), rest.to_string()))
}

fn not_found(writer: &mut dyn ResponseWriter) {
    set_header(writer, "content-type", "text/plain; charset=utf-8");
    set_header(writer, "x-content-type-options", "nosniff");
    writer.write_header(StatusCode::NOT_FOUND);
    writer.write(b"404 page not found\n");
}

/// Go http.Redirect with 301 Moved Permanently.
fn redirect(writer: &mut dyn ResponseWriter, method: &Method, path: &str, query: &str) {
    let location = if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    };
    set_header(writer, "location", &location);
    if *method == Method::GET || *method == Method::HEAD {
        set_header(writer, "content-type", "text/html; charset=utf-8");
    }
    writer.write_header(StatusCode::MOVED_PERMANENTLY);
    if *method == Method::GET {
        let escaped = location
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&#34;")
            .replace('\'', "&#39;");
        writer.write(format!("<a href=\"{escaped}\">Moved Permanently</a>.\n\n").as_bytes());
    }
}

/// Answers one request. A panic in a handler is answered 500 by the
/// listener when nothing was written yet (Go's recovery middleware).
pub async fn serve(app: &Arc<App>, request: Request, writer: Box<dyn ResponseWriter>) {
    let mut writer = writer;
    if request.method != Method::CONNECT {
        let cleaned = clean_path(&request.path);
        if cleaned != request.path {
            return redirect(writer.as_mut(), &request.method, &cleaned, &request.query);
        }
    }
    let handler = match find(&request.method, &request.path) {
        Some(handler) => handler,
        None => {
            // A subtree root named without its trailing slash is redirected
            // to it.
            let slashed = format!("{}/", request.path);
            if !request.path.ends_with('/')
                && !exact_match(&request.method, &request.path)
                && exact_match(&request.method, &slashed)
            {
                return redirect(writer.as_mut(), &request.method, &slashed, &request.query);
            }
            let allowed = matching_methods(&request.path);
            if allowed.is_empty() {
                return not_found(writer.as_mut());
            }
            set_header(writer.as_mut(), "allow", &allowed.join(", "));
            set_header(writer.as_mut(), "content-type", "text/plain; charset=utf-8");
            set_header(writer.as_mut(), "x-content-type-options", "nosniff");
            writer.write_header(StatusCode::METHOD_NOT_ALLOWED);
            writer.write(b"Method Not Allowed\n");
            return;
        }
    };
    let api_key_ok =
        |request: &Request| app.valid_api_key(&request_api_key(&request.headers, &request.query));
    let unauthorized = |writer: &mut dyn ResponseWriter| {
        write_openai_error(
            writer,
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "invalid or missing API key",
        )
    };
    use super::admin_api as admin;
    match handler {
        Handler::AdminPage => super::admin::handle_admin_page(writer.as_mut()),
        Handler::AdminAsset => {
            super::admin::handle_admin_asset(writer.as_mut(), &request.path);
        }
        Handler::InFlight => handle_in_flight(app, &request, writer.as_mut()),
        Handler::Native => {
            let Some((key, rest)) = native_parts(&request.path) else {
                return not_found(writer.as_mut());
            };
            super::native::handle_native(app, request, key, rest, writer).await;
        }
        Handler::Healthz
        | Handler::Status
        | Handler::Models
        | Handler::Responses
        | Handler::ResponsesWebSocket
        | Handler::Compact
        | Handler::Chat
            if !api_key_ok(&request) =>
        {
            unauthorized(writer.as_mut());
        }
        Handler::Healthz => write_json(
            writer.as_mut(),
            StatusCode::OK,
            &serde_json::json!({"ok": true}),
        ),
        Handler::Status => handle_current_status(app, &request, writer.as_mut()),
        Handler::Models => handle_models(app, &request, writer.as_mut()),
        Handler::ResponsesWebSocket => {
            super::responses_ws::handle_responses_websocket(app, request, false, writer).await
        }
        Handler::Responses | Handler::Compact | Handler::Chat => {
            serve_provider(app, request, false, writer, handler == Handler::Chat).await;
        }
        Handler::PublicDashboard => admin::handle_public_dashboard(app, writer.as_mut()),
        Handler::PublicAccountAction => {
            admin::handle_public_account_action(app, &request, writer.as_mut())
        }
        Handler::AdminLogin => admin::handle_admin_login(app, &request, writer.as_mut()),
        _ if !admin::require_admin(app, &request, writer.as_mut()) => {}
        Handler::AdminLogout => admin::handle_admin_logout(app, &request, writer.as_mut()),
        Handler::AdminState => admin::handle_admin_state(app, writer.as_mut()),
        Handler::AdminSettings => admin::handle_admin_settings(app, &request, writer.as_mut()),
        Handler::Accounts => admin::handle_accounts(app, &request, writer.as_mut()),
        Handler::AccountHealth => admin::handle_account_health(app, writer.as_mut()),
        Handler::RefreshAllQuota => admin::handle_refresh_all_quota(app, writer.as_mut()).await,
        Handler::ResetPromptCacheWindow => {
            admin::handle_reset_prompt_cache_window(app, writer.as_mut())
        }
        Handler::Job => admin::handle_job(app, &request, writer.as_mut()),
        Handler::JobCancel => admin::handle_job_cancel(app, &request, writer.as_mut()),
        Handler::StickySessions => admin::handle_sticky_sessions(app, writer.as_mut()),
        Handler::StickySessionDelete => {
            admin::handle_sticky_session_delete(app, &request, writer.as_mut())
        }
        Handler::AccountAction => {
            admin::handle_account_action(app, &request, writer.as_mut()).await
        }
        Handler::AccountDelete => admin::handle_account_delete(app, &request, writer.as_mut()),
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
