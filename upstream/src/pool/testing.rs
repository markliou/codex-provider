//! Shared fixtures for the pool's tests, mirroring the Go tests' helpers
//! (testApp, writeCodexDeviceAuth, fakeJWT). Fake data only.

use super::app::App;
use super::state::Account;
use super::time::Timestamp;
use super::time::hours;
use base64::Engine;
use futures::StreamExt;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;

/// A temporary directory removed when dropped (Go t.TempDir).
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("codex-pool-test-{}", super::crypto::random_id()));
        if let Err(error) = std::fs::create_dir_all(&path) {
            panic!("temp dir: {error}");
        }
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Default for TempDir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Go testApp. Keep the TempDir alive as long as the app.
pub fn test_app(accounts: Vec<Account>) -> (App, TempDir) {
    let dir = TempDir::new();
    let app = App::for_test(dir.path(), accounts);
    (app, dir)
}

pub fn test_app_with(
    accounts: Vec<Account>,
    configure: impl FnOnce(&mut super::settings::Settings),
) -> (App, TempDir) {
    let dir = TempDir::new();
    let app = App::for_test_with(dir.path(), accounts, configure);
    (app, dir)
}

/// An API-key provider slot (Go tests' {ID, Enabled, InPool, Priority,
/// UpstreamBaseURL, UpstreamAPIKey}).
pub fn provider_account(id: &str, priority: i64) -> Account {
    Account {
        id: id.to_string(),
        enabled: true,
        in_pool: true,
        priority,
        upstream_base_url: format!("https://{id}.example.test"),
        upstream_api_key: id.to_string(),
        ..Account::default()
    }
}

pub fn fake_jwt_claims(claims: &Value) -> String {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        engine.encode(br#"{"alg":"none"}"#),
        engine.encode(claims.to_string())
    )
}

pub fn fake_jwt(exp: Timestamp) -> String {
    fake_jwt_claims(&json!({"exp": exp.unix()}))
}

/// Go writeCodexDeviceAuth: a Team credential for the upstream account.
pub fn write_codex_device_auth(
    app: &App,
    account_id: &str,
    upstream_account_id: &str,
    email: &str,
) {
    let home = app.read().account_codex_home(account_id);
    if let Err(error) = std::fs::create_dir_all(&home) {
        panic!("{error}");
    }
    let id_token = fake_jwt_claims(&json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": upstream_account_id,
            "chatgpt_account_name": "Workspace",
            "chatgpt_plan_type": "team",
        },
    }));
    let auth = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": fake_jwt(Timestamp::now().add(hours(1))),
            "refresh_token": "<refresh-token>",
            "account_id": upstream_account_id,
        },
    });
    if let Err(error) = std::fs::write(home.join("auth.json"), auth.to_string()) {
        panic!("{error}");
    }
}

pub fn device_account(id: &str, upstream_account_id: &str, priority: i64) -> Account {
    Account {
        id: id.to_string(),
        account_id: upstream_account_id.to_string(),
        auth_type: "codex_device_auth".to_string(),
        enabled: true,
        in_pool: true,
        priority,
        ..Account::default()
    }
}

// ---- a fake upstream (Go httptest.NewServer) ----

#[derive(Clone, Debug, Default)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    /// In wire order.
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }

    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

pub struct FakeResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Chunks sent in order, each after its delay.
    pub chunks: Vec<(std::time::Duration, bytes::Bytes)>,
}

impl FakeResponse {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            chunks: vec![(
                std::time::Duration::ZERO,
                bytes::Bytes::from(body.to_string()),
            )],
        }
    }

    pub fn sse(blocks: &[&str]) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            chunks: blocks
                .iter()
                .map(|block| {
                    (
                        std::time::Duration::ZERO,
                        bytes::Bytes::from(block.to_string()),
                    )
                })
                .collect(),
        }
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            chunks: Vec::new(),
        }
    }
}

type Handler = dyn Fn(&RecordedRequest) -> FakeResponse + Send + Sync;

pub struct FakeServer {
    pub url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<RecordedRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeServer {
    pub async fn start(
        handler: impl Fn(&RecordedRequest) -> FakeResponse + Send + Sync + 'static,
    ) -> Self {
        use http_body_util::BodyExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let url = format!(
            "http://{}",
            listener
                .local_addr()
                .unwrap_or_else(|error| panic!("{error}"))
        );
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler: std::sync::Arc<Handler> = std::sync::Arc::new(handler);
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = handler.clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(
                        move |request: hyper::Request<hyper::body::Incoming>| {
                            let handler = handler.clone();
                            let recorded = recorded.clone();
                            async move {
                                let (parts, body) = request.into_parts();
                                let body = body
                                    .collect()
                                    .await
                                    .map(http_body_util::Collected::to_bytes)
                                    .unwrap_or_default();
                                let request = RecordedRequest {
                                    method: parts.method.to_string(),
                                    path: parts.uri.path().to_string(),
                                    query: parts.uri.query().unwrap_or_default().to_string(),
                                    headers: parts.headers,
                                    body: body.to_vec(),
                                };
                                let reply = handler(&request);
                                if let Ok(mut all) = recorded.lock() {
                                    all.push(request);
                                }
                                let frames = futures::stream::iter(reply.chunks).then(
                                    |(delay, chunk)| async move {
                                        if !delay.is_zero() {
                                            tokio::time::sleep(delay).await;
                                        }
                                        Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(
                                            chunk,
                                        ))
                                    },
                                );
                                let mut response =
                                    hyper::Response::new(http_body_util::StreamBody::new(frames));
                                *response.status_mut() = http::StatusCode::from_u16(reply.status)
                                    .unwrap_or(http::StatusCode::OK);
                                for (name, value) in reply.headers {
                                    if let (Ok(name), Ok(value)) = (
                                        http::HeaderName::from_bytes(name.as_bytes()),
                                        http::HeaderValue::from_str(&value),
                                    ) {
                                        response.headers_mut().append(name, value);
                                    }
                                }
                                Ok::<_, std::convert::Infallible>(response)
                            }
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .map(|all| all.clone())
            .unwrap_or_default()
    }

    pub fn count(&self) -> usize {
        self.requests.lock().map(|all| all.len()).unwrap_or(0)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Go writeTestCodexAuthTokens.
pub fn write_test_codex_auth_tokens(
    app: &App,
    account_id: &str,
    access_token: &str,
    refresh_token: &str,
) {
    let home = app.read().account_codex_home(account_id);
    if let Err(error) = std::fs::create_dir_all(&home) {
        panic!("{error}");
    }
    let auth = json!({"auth_mode": "chatgpt", "tokens": {"id_token": "", "access_token": access_token, "refresh_token": refresh_token}});
    if let Err(error) = std::fs::write(home.join("auth.json"), auth.to_string()) {
        panic!("{error}");
    }
}

/// Go `a.logger = log.New(&logged, "", 0)`: the app logs into the returned
/// lines.
pub fn capture_logs(mut app: App) -> (App, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let (logger, lines) = super::log::Logger::capture();
    let logger = std::sync::Arc::new(logger);
    app.logger = logger.clone();
    app.write().logger = logger;
    (app, lines)
}
