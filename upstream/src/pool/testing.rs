//! Shared fixtures for the pool's tests, mirroring the Go tests' helpers
//! (testApp, writeCodexDeviceAuth, fakeJWT). Fake data only.

use super::app::App;
use super::state::Account;
use super::time::Timestamp;
use super::time::hours;
use base64::Engine;
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
