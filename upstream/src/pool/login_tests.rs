//! The Go device-auth sign-in tests (main_test.go), one by one. Each keeps
//! its Go name in a comment; the assertions are Go's. Go put a stand-in
//! `codex` script on PATH; PATH is one per process and these tests run in
//! parallel, so the script's path goes in Settings::codex_cli instead.

use super::app::App;
use super::app::LoginJob;
use super::login::LoginStartError;
use super::metadata::CodexAuthInfo;
use super::proxy_tests::account;
use super::state::Account;
use super::state::AccountHealth;
use super::state::AccountQuota;
use super::state::Cooldown;
use super::state::PromptCacheStat;
use super::state::QuotaErrorInfo;
use super::state::QuotaSnapshot;
use super::state::QuotaWindow;
use super::state::ResponseBinding;
use super::state::RoutingCacheEvent;
use super::state::StickySession;
use super::state::ThreadBinding;
use super::testing::FakeResponse;
use super::testing::FakeServer;
use super::testing::TempDir;
use super::testing::device_account;
use super::testing::fake_jwt_claims;
use super::time::Timestamp;
use super::time::hours;
use super::time::minutes;
use std::sync::Arc;

/// A stand-in Codex CLI: `body` runs under `set -eu` with CODEX_HOME set.
pub(super) fn fake_codex(dir: &TempDir, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.path().join("codex");
    let script = format!("#!/bin/sh\nset -eu\nmkdir -p \"$CODEX_HOME\"\n{body}");
    std::fs::write(&path, script).unwrap_or_else(|error| panic!("{error}"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("{error}"));
    path.display().to_string()
}

pub(super) const PROMPT: &str =
    "printf '%s\\n' 'Open https://auth.openai.com/activate' 'ABCD-EFGH'\n";

pub(super) fn writes_auth(id_token: &str) -> String {
    format!(
        "{PROMPT}cat > \"$CODEX_HOME/auth.json\" <<EOF\n{{\"auth_mode\":\"chatgpt\",\"tokens\":{{\"id_token\":\"{id_token}\",\"access_token\":\"<access-token>\",\"refresh_token\":\"<refresh-token>\"}}}}\nEOF\n"
    )
}

pub(super) fn login_app(
    accounts: Vec<Account>,
    cli: &str,
    base_url: Option<&str>,
) -> (Arc<App>, TempDir) {
    let (cli, base) = (cli.to_string(), base_url.map(str::to_string));
    let (app, dir) = super::testing::test_app_with(accounts, |settings| {
        settings.codex_cli = cli;
        if let Some(base) = base {
            settings.codex_base_url = base;
        }
    });
    (Arc::new(app), dir)
}

pub(super) async fn wait_for_job(
    app: &App,
    job_id: &str,
    done: impl Fn(&LoginJob) -> bool,
) -> LoginJob {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let job = app.read().jobs.get(job_id).cloned().unwrap_or_default();
        if done(&job) {
            return job;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "login job did not finish: {job:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

pub(super) fn terminal(job: &LoginJob) -> bool {
    matches!(job.status.as_str(), "completed" | "failed" | "cancelled")
}

/// Go seedDeviceAuthIdentityHistory.
pub(super) fn seed_device_auth_identity_history(app: &App, account_id: &str, now: Timestamp) {
    let cache_key = format!("{account_id}:gpt-test:subagent");
    let stat = PromptCacheStat {
        account_id: account_id.into(),
        model_id: "gpt-test".into(),
        agent_kind: "subagent".into(),
        request_count: 8,
        input_tokens: 12_000,
        cached_tokens: 9_000,
        parent_affinity_hit_count: 4,
        parent_affinity_fallback_count: 1,
        lineage_failover_count: 2,
        cache_write_observed_request_count: 3,
        ..PromptCacheStat::default()
    };
    let mut inner = app.write();
    let state = &mut inner.state;
    state
        .prompt_cache_mut()
        .insert(cache_key.clone(), stat.clone());
    state.prompt_cache_baseline_mut().insert(cache_key, stat);
    state
        .prompt_cache_reset_at_by_account_mut()
        .insert(account_id.into(), now.add(-hours(1)));
    state.sticky_sessions_mut().insert(
        "gpt-test:thread:root".into(),
        StickySession {
            key: "gpt-test:thread:root".into(),
            model_id: "gpt-test".into(),
            account_id: account_id.into(),
            created_at: now.add(-hours(1)),
            last_success_at: now.add(-minutes(1)),
            ..StickySession::default()
        },
    );
    state.response_bindings_mut().insert(
        "resp-old".into(),
        ResponseBinding {
            response_id: "resp-old".into(),
            sticky_key: "gpt-test:thread:root".into(),
            model_id: "gpt-test".into(),
            account_id: account_id.into(),
            created_at: now.add(-hours(1)),
            expires_at: now.add(hours(1)),
            ..ResponseBinding::default()
        },
    );
    state.thread_bindings_mut().insert(
        "thread-old".into(),
        ThreadBinding {
            thread_id: "thread-old".into(),
            lineage_root_id: "thread-old".into(),
            model_id: "gpt-test".into(),
            account_id: account_id.into(),
            sticky_key: "gpt-test:thread:root".into(),
            created_at: now.add(-hours(1)),
            last_success_at: now.add(-minutes(1)),
            expires_at: now.add(hours(1)),
            ..ThreadBinding::default()
        },
    );
    state.routing_cache_events_mut().push(RoutingCacheEvent {
        timestamp: now.add(-minutes(1)),
        model_id: "gpt-test".into(),
        account_id: account_id.into(),
        agent_kind: "subagent".into(),
        routing_outcome: "sticky_reuse".into(),
        ..RoutingCacheEvent::default()
    });
    state.health_mut().insert(
        account_id.into(),
        AccountHealth {
            last_failure_reason: "invalid_token".into(),
            consecutive_failure: 2,
            ..AccountHealth::default()
        },
    );
    state.cooldowns_mut().insert(
        account_id.into(),
        vec![Cooldown {
            model_id: "gpt-test".into(),
            next_retry_at: now.add(hours(1)),
            reason: "auth_failed".into(),
            ..Cooldown::default()
        }],
    );
    state.quotas_mut().insert(
        account_id.into(),
        QuotaSnapshot {
            account_id: account_id.into(),
            quota: Some(AccountQuota {
                hourly: QuotaWindow {
                    present: true,
                    percentage: 72,
                    ..QuotaWindow::default()
                },
                ..AccountQuota::default()
            }),
            quota_error: Some(QuotaErrorInfo {
                code: "invalid_token".into(),
                message: "expired".into(),
                timestamp: now.add(-minutes(1)),
                ..QuotaErrorInfo::default()
            }),
            ..QuotaSnapshot::default()
        },
    );
}

pub(super) fn history_sizes(app: &App) -> (usize, usize, usize, usize, usize) {
    let inner = app.read();
    let state = &inner.state;
    (
        state.prompt_cache().len(),
        state.sticky_sessions().len(),
        state.response_bindings().len(),
        state.thread_bindings().len(),
        state.routing_cache_events().len(),
    )
}

fn returning_slot(id: &str) -> Account {
    Account {
        email: "old@example.test".into(),
        last_login_at: Timestamp::now().add(-hours(1)),
        ..device_account(id, "upstream-one", 0)
    }
}

// Go TestDeviceAuthPromptParsing.
#[test]
fn device_auth_prompt_parsing() {
    use super::login::parse_device_auth_prompt;
    use super::login::redact_login_output;
    assert_eq!(
        parse_device_auth_prompt("Open https://auth.openai.com/activate\nABCD-EFGH\n"),
        (
            "https://auth.openai.com/activate".to_string(),
            "ABCD-EFGH".to_string()
        )
    );
    let jwt = format!(
        "{}{}{}",
        "ey", "JhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.", "signaturevalue"
    );
    let redacted = redact_login_output(&format!(
        "Authorization: Bearer <secret-token>\naccess_token=<secret>\napi key: <secret>\nCookie: <secret>\n{jwt}"
    ));
    for forbidden in [
        "<secret-token>",
        "access_token=<secret>",
        "api key: <secret>",
        "Cookie: <secret>",
        "eyJhbGci",
    ] {
        assert!(
            !redacted.contains(forbidden),
            "login output was not redacted: {redacted}"
        );
    }
    assert!(
        redacted.matches("[REDACTED]").count() >= 5,
        "login output was not redacted: {redacted}"
    );
}

// Go TestDeviceAuthPromptOfThePinnedCodexParses.
#[test]
fn device_auth_prompt_of_the_pinned_codex_parses() {
    use super::login::parse_device_auth_prompt;
    use super::login::strip_ansi;
    let raw = include_str!("../../testdata/codex-device-auth-output.txt");
    assert_eq!(
        parse_device_auth_prompt(&strip_ansi(raw)),
        (
            "https://auth.openai.com/codex/device".to_string(),
            "WXYZ-12345".to_string()
        )
    );
}

// Go TestCodexLoginEnvDoesNotInheritServiceSecrets.
#[test]
fn codex_login_env_does_not_inherit_service_secrets() {
    let lookup = |name: &str| match name {
        "CODEX_POOL_API_KEY" => Some("client-secret".to_string()),
        "CODEX_POOL_ADMIN_PASSWORD_HASH" => Some("admin-secret".to_string()),
        "CODEX_POOL_UPSTREAM_API_KEY" => Some("upstream-secret".to_string()),
        "HTTPS_PROXY" => Some("http://proxy.example.test".to_string()),
        _ => None,
    };
    let env =
        super::login::codex_login_env(std::path::Path::new("/data/accounts/acct/.codex"), lookup);
    let joined = env
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "CODEX_POOL_API_KEY=",
        "CODEX_POOL_ADMIN_PASSWORD_HASH=",
        "CODEX_POOL_UPSTREAM_API_KEY=",
        "client-secret",
        "admin-secret",
        "upstream-secret",
    ] {
        assert!(
            !joined.contains(forbidden),
            "login env inherited secret {forbidden:?}: {joined}"
        );
    }
    for required in [
        "CODEX_HOME=/data/accounts/acct/.codex",
        "HOME=/data/accounts/acct",
        "HTTPS_PROXY=http://proxy.example.test",
    ] {
        assert!(
            joined.contains(required),
            "login env missing {required:?}: {joined}"
        );
    }
}

// Go TestDeviceAuthLoginJobLifecycle. The child's environment is the one
// codex_login_env builds; the pool's secrets cannot reach it because the
// child starts from an empty environment.
#[tokio::test]
async fn device_auth_login_job_lifecycle() {
    let usage = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":60},"secondary_window":null}}"#,
        )
    })
    .await;
    let bin = TempDir::new();
    let id_token = fake_jwt_claims(
        &serde_json::json!({"email": "user@example.test", "https://api.openai.com/auth": {"chatgpt_account_id": "acct-chatgpt", "chatgpt_account_name": "Acme Workspace"}}),
    );
    let cli = fake_codex(
        &bin,
        &format!("env > \"$CODEX_HOME/env.txt\"\n{}", writes_auth(&id_token)),
    );
    let slot = Account {
        label: "Login".into(),
        enabled: false,
        in_pool: false,
        pending_pool_activation: true,
        ..device_account("acct-login", "", 0)
    };
    let (app, _dir) = login_app(
        vec![slot],
        &cli,
        Some(&format!("{}/backend-api", usage.url)),
    );
    {
        let mut inner = app.write();
        let now = Timestamp::now();
        inner.state.health_mut().insert(
            "acct-login".into(),
            AccountHealth {
                last_failure_at: now.add(-minutes(1)),
                last_failure_reason: "old auth error".into(),
                consecutive_failure: 2,
                ..AccountHealth::default()
            },
        );
        inner.state.cooldowns_mut().insert(
            "acct-login".into(),
            vec![Cooldown {
                model_id: "gpt-test".into(),
                next_retry_at: now.add(hours(1)),
                reason: "old cooldown".into(),
                ..Cooldown::default()
            }],
        );
        inner.state.quotas_mut().insert(
            "acct-login".into(),
            QuotaSnapshot {
                account_id: "acct-login".into(),
                quota_error: Some(QuotaErrorInfo {
                    code: "old_error".into(),
                    message: "old quota error".into(),
                    timestamp: now.add(-minutes(1)),
                    ..QuotaErrorInfo::default()
                }),
                ..QuotaSnapshot::default()
            },
        );
    }
    let job = app
        .start_login_job("acct-login")
        .unwrap_or_else(|error| panic!("{error}"));
    let current = wait_for_job(&app, &job.id, terminal).await;
    assert_eq!(current.status, "completed", "{current:?}");
    assert!(
        current.verification_url == "https://auth.openai.com/activate"
            && current.user_code == "ABCD-EFGH",
        "job did not capture device prompt"
    );
    assert!(
        current.code_expires_at.sub(&Timestamp::now()) >= minutes(14),
        "job did not set a 15 minute code expiry"
    );
    assert!(
        !current.reauthentication && !current.history_reset,
        "first login was misclassified as reauthentication"
    );
    let item = account(&app, 0);
    assert!(
        item.email == "user@example.test"
            && item.account_id == "acct-chatgpt"
            && item.organization_name.is_empty(),
        "account metadata not updated: {item:?}"
    );
    assert!(
        item.enabled && item.in_pool && !item.pending_pool_activation,
        "login did not activate staged account"
    );
    assert!(
        !item.pending_auth_verification && item.pending_auth_expected_account_id.is_empty(),
        "completed login did not release the durable routing gate"
    );
    let inner = app.read();
    let snapshot = inner
        .state
        .quotas()
        .get("acct-login")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        snapshot.quota.as_ref().map(|quota| quota.hourly.percentage),
        Some(90),
        "quota was not refreshed before job completion"
    );
    assert!(
        snapshot.quota_error.is_none(),
        "login did not clear stale quota error"
    );
    let health = inner
        .state
        .health()
        .get("acct-login")
        .cloned()
        .unwrap_or_default();
    assert!(
        health.consecutive_failure == 0 && health.last_failure_reason.is_empty(),
        "login did not clear stale health error"
    );
    assert!(
        inner
            .state
            .cooldowns()
            .get("acct-login")
            .is_none_or(Vec::is_empty),
        "login did not clear stale cooldowns"
    );
    let env = std::fs::read_to_string(inner.account_codex_home("acct-login").join("env.txt"))
        .unwrap_or_default();
    assert!(
        env.contains("CODEX_HOME=") && !env.contains("CODEX_POOL_"),
        "device login child environment: {env}"
    );
}

// Go TestDeviceAuthLoginDoesNotStartDuplicateJob.
#[tokio::test]
async fn device_auth_login_does_not_start_duplicate_job() {
    let (app, _dir) = login_app(
        vec![device_account("acct-login", "", 0)],
        "/nonexistent/codex",
        None,
    );
    app.write().jobs.insert(
        "job-existing".into(),
        LoginJob {
            id: "job-existing".into(),
            account_id: "acct-login".into(),
            status: "waiting_for_user".into(),
            ..LoginJob::default()
        },
    );
    let job = app
        .start_login_job("acct-login")
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(job.id, "job-existing", "duplicate login created a new job");
}

// Go TestDeviceAuthLoginIsGlobalSingleFlight.
#[tokio::test]
async fn device_auth_login_is_global_single_flight() {
    let (app, _dir) = login_app(
        vec![
            device_account("acct-one", "", 0),
            device_account("acct-two", "", 0),
        ],
        "/nonexistent/codex",
        None,
    );
    app.write().jobs.insert(
        "job-existing".into(),
        LoginJob {
            id: "job-existing".into(),
            account_id: "acct-one".into(),
            status: "waiting_for_user".into(),
            ..LoginJob::default()
        },
    );
    assert_eq!(
        app.start_login_job("acct-two").err(),
        Some(LoginStartError::AnotherJobInProgress)
    );
    assert!(
        !account(&app, 1).pending_auth_verification,
        "rejected second login changed its durable routing gate"
    );
    if let Some(job) = app.write().jobs.get_mut("job-existing") {
        job.status = "cancelling".into();
    }
    assert_eq!(
        app.start_login_job("acct-two").err(),
        Some(LoginStartError::AnotherJobInProgress),
        "second login during process cancellation"
    );
}

// Go TestDeviceAuthLoginStartSaveFailureStaysBlocked.
#[tokio::test]
async fn device_auth_login_start_save_failure_stays_blocked() {
    let (app, dir) = login_app(
        vec![device_account("acct-login", "upstream-one", 0)],
        "/nonexistent/codex",
        None,
    );
    std::fs::create_dir(dir.path().join("state").join("runtime.json"))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        app.start_login_job("acct-login").is_err(),
        "login start succeeded when runtime state could not be written"
    );
    let item = account(&app, 0);
    assert!(
        item.pending_auth_verification && item.pending_auth_expected_account_id == "upstream-one",
        "partial start save released the in-memory routing gate"
    );
    assert!(
        app.read().jobs.is_empty(),
        "login process started after pending state save failed"
    );
    let persisted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("config.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    let slot = &persisted["accounts"][0];
    assert!(
        slot["pendingAuthVerification"] == true
            && slot["pendingAuthExpectedAccountId"] == "upstream-one",
        "config-only partial save did not stay fail-closed: {slot}"
    );
}

// Go TestCompletedAuthVerificationSaveWritesRuntimeBeforeGateConfig.
#[test]
fn completed_auth_verification_save_writes_runtime_before_gate_config() {
    let (app, dir) =
        super::testing::test_app(vec![device_account("acct-login", "upstream-two", 0)]);
    app.write().state.request_count = 73;
    std::fs::create_dir(dir.path().join("config.json")).unwrap_or_else(|error| panic!("{error}"));
    assert!(
        app.write().save_completed_auth_verification().is_err(),
        "completed verification save wrote over a config directory"
    );
    let persisted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("state").join("runtime.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    assert_eq!(
        persisted["requestCount"], 73,
        "runtime state was not persisted before the gate config write"
    );
}

// Go TestDeviceAuthLoginFailsClosedWhenIdentityCannotBeVerified.
#[tokio::test]
async fn device_auth_login_fails_closed_when_identity_cannot_be_verified() {
    let bin = TempDir::new();
    let cli = fake_codex(&bin, &format!("{PROMPT}rm -f \"$CODEX_HOME/auth.json\"\n"));
    let (app, _dir) = login_app(vec![returning_slot("acct-login")], &cli, None);
    seed_device_auth_identity_history(&app, "acct-login", Timestamp::now());
    let job = app
        .start_login_job("acct-login")
        .unwrap_or_else(|error| panic!("{error}"));
    let current = wait_for_job(&app, &job.id, terminal).await;
    assert!(
        current.status == "failed"
            && current.reauthentication
            && !current.history_reset
            && current.error.contains("verify"),
        "{current:?}"
    );
    let (cache, sticky, _, threads, _) = history_sizes(&app);
    assert_eq!(
        (cache, sticky, threads),
        (1, 1, 1),
        "failed verification mutated prior history"
    );
    let item = account(&app, 0);
    assert!(
        item.pending_auth_verification && item.pending_auth_expected_account_id == "upstream-one",
        "failed verification released the durable routing gate"
    );
}

// Go TestDeviceAuthLoginSameIdentityPreservesHistory.
#[tokio::test]
async fn device_auth_login_same_identity_preserves_history() {
    let usage = FakeServer::start(|_| {
        FakeResponse::json(200, r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":60}}}"#)
    })
    .await;
    let bin = TempDir::new();
    let id_token = fake_jwt_claims(
        &serde_json::json!({"email": "new@example.test", "https://api.openai.com/auth": {"chatgpt_account_id": "upstream-one", "chatgpt_account_name": "Personal Account", "chatgpt_plan_type": "plus"}}),
    );
    let cli = fake_codex(&bin, &writes_auth(&id_token));
    let (app, _dir) = login_app(
        vec![returning_slot("acct-login")],
        &cli,
        Some(&format!("{}/backend-api", usage.url)),
    );
    seed_device_auth_identity_history(&app, "acct-login", Timestamp::now());
    let job = app
        .start_login_job("acct-login")
        .unwrap_or_else(|error| panic!("{error}"));
    let current = wait_for_job(&app, &job.id, terminal).await;
    assert!(
        current.status == "completed" && current.reauthentication && !current.history_reset,
        "same-account repair result = {current:?}"
    );
    let item = account(&app, 0);
    assert!(
        item.account_id == "upstream-one"
            && !item.pending_auth_verification
            && item.pending_auth_expected_account_id.is_empty(),
        "same-account repair did not release its durable gate"
    );
    assert_eq!(
        history_sizes(&app),
        (1, 1, 1, 1, 1),
        "same-account repair discarded history"
    );
}

// Go TestDeviceAuthLoginRechecksIdentityAfterQuotaMetadata.
#[tokio::test]
async fn device_auth_login_rechecks_identity_after_quota_metadata() {
    let usage = FakeServer::start(|request| match request.path.as_str() {
        "/backend-api/wham/usage" => FakeResponse::json(200, r#"{"plan_type":"team","rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_after_seconds":60}}}"#),
        "/backend-api/wham/accounts/check" => FakeResponse::json(
            200,
            r#"{"accounts":[{"id":"upstream-two","name":"Different Workspace","plan_type":"team","structure":"workspace"}],"account_ordering":["upstream-two"]}"#,
        ),
        _ => FakeResponse::status(404),
    })
    .await;
    let bin = TempDir::new();
    let id_token = fake_jwt_claims(
        &serde_json::json!({"email": "new@example.test", "https://api.openai.com/auth": {"chatgpt_account_id": "upstream-one", "chatgpt_plan_type": "team"}}),
    );
    let cli = fake_codex(&bin, &writes_auth(&id_token));
    let (app, _dir) = login_app(
        vec![returning_slot("acct-login")],
        &cli,
        Some(&format!("{}/backend-api", usage.url)),
    );
    seed_device_auth_identity_history(&app, "acct-login", Timestamp::now());
    let job = app
        .start_login_job("acct-login")
        .unwrap_or_else(|error| panic!("{error}"));
    let current = wait_for_job(&app, &job.id, terminal).await;
    assert!(
        current.status == "completed" && current.reauthentication && current.history_reset,
        "metadata identity change was not reported: {current:?}"
    );
    let item = account(&app, 0);
    assert!(
        item.account_id == "upstream-two"
            && !item.pending_auth_verification
            && item.pending_auth_expected_account_id.is_empty(),
        "metadata identity change was not finalized safely: {item:?}"
    );
    assert_eq!(
        history_sizes(&app),
        (0, 0, 0, 0, 0),
        "late identity change retained prior history"
    );
    assert!(
        !app.read().state.quotas().contains_key("acct-login"),
        "late identity change retained a quota snapshot"
    );
}

// Go TestDeviceAuthLoginJobCancel.
#[tokio::test]
async fn device_auth_login_job_cancel() {
    let bin = TempDir::new();
    let cli = fake_codex(&bin, &format!("{PROMPT}while true; do sleep 1; done\n"));
    let (app, _dir) = login_app(vec![device_account("acct-cancel", "", 0)], &cli, None);
    let job = app
        .start_login_job("acct-cancel")
        .unwrap_or_else(|error| panic!("{error}"));
    wait_for_job(&app, &job.id, |job| job.status == "waiting_for_user").await;
    let session = super::admin_tests::admin_session(&app);
    let cancel = super::proxy_tests::serve(
        &app,
        super::admin_tests::admin_request(
            &session,
            http::Method::POST,
            &format!("/admin/api/jobs/{}/cancel", job.id),
            "",
        ),
    )
    .await;
    assert_eq!(cancel.code(), 200, "cancel: {}", cancel.body_string());
    let current = wait_for_job(&app, &job.id, |job| job.status == "cancelled").await;
    assert!(
        current.error.is_empty(),
        "cancelled job retained error: {current:?}"
    );
    assert!(
        account(&app, 0).pending_auth_verification,
        "cancelled login released the durable routing gate"
    );
}

// Go TestAccountHasPriorIdentityRecognizesLegacyRoutingHistory.
#[test]
fn account_has_prior_identity_recognizes_legacy_routing_history() {
    let (app, _dir) = super::testing::test_app(vec![device_account("legacy-slot", "", 0)]);
    seed_device_auth_identity_history(&app, "legacy-slot", Timestamp::now());
    let item = account(&app, 0);
    assert!(
        app.read().has_prior_identity(&item),
        "legacy slot with identity-scoped history was misclassified as a first login"
    );
}

// Go TestCompletedDeviceAuthSameIdentityPreservesCacheAffinityAndRoutes.
#[test]
fn completed_device_auth_same_identity_preserves_cache_affinity_and_routes() {
    let now = Timestamp::now();
    let slot = Account {
        label: "Old label".into(),
        email: "old@example.test".into(),
        organization_name: "Old workspace".into(),
        plan_type: "plus".into(),
        last_login_at: now.add(-hours(24)),
        ..device_account("slot-one", "upstream-one", 0)
    };
    let (app, _dir) = super::testing::test_app(vec![slot]);
    seed_device_auth_identity_history(&app, "slot-one", now);
    let reset_at = app
        .read()
        .state
        .prompt_cache_reset_at_by_account()
        .get("slot-one")
        .copied();
    let auth = CodexAuthInfo {
        account_id: "upstream-one".into(),
        email: "new@example.test".into(),
        organization_name: "Renamed workspace".into(),
        plan_type: "pro".into(),
        plan_limit: "pro".into(),
        ..CodexAuthInfo::default()
    };
    let reset =
        app.write()
            .apply_completed_device_auth("slot-one", &auth, "upstream-one", true, now);
    assert!(
        !reset,
        "same upstream account reset identity-scoped history"
    );
    let inner = app.read();
    let cache = inner
        .state
        .prompt_cache()
        .get("slot-one:gpt-test:subagent")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (
            cache.parent_affinity_hit_count,
            cache.parent_affinity_fallback_count,
            cache.lineage_failover_count,
            cache.cached_tokens
        ),
        (4, 1, 2, 9_000),
        "same-account cache/affinity history changed"
    );
    assert_eq!(
        inner
            .state
            .prompt_cache_baseline()
            .get("slot-one:gpt-test:subagent")
            .map(|stat| stat.request_count),
        Some(8)
    );
    assert_eq!(
        inner
            .state
            .prompt_cache_reset_at_by_account()
            .get("slot-one")
            .copied(),
        reset_at,
        "same-account reset window changed"
    );
    drop(inner);
    assert_eq!(
        history_sizes(&app),
        (1, 1, 1, 1, 1),
        "same-account routes were cleared"
    );
    let inner = app.read();
    let health = inner
        .state
        .health()
        .get("slot-one")
        .cloned()
        .unwrap_or_default();
    assert!(
        health.consecutive_failure == 0 && health.last_failure_reason.is_empty(),
        "reauthentication did not clear transient health error"
    );
    assert!(
        inner
            .state
            .cooldowns()
            .get("slot-one")
            .is_none_or(Vec::is_empty),
        "reauthentication did not clear transient cooldown"
    );
    let quota = inner
        .state
        .quotas()
        .get("slot-one")
        .cloned()
        .unwrap_or_default();
    assert!(
        quota
            .quota
            .as_ref()
            .is_some_and(|quota| quota.hourly.percentage == 72)
            && quota.quota_error.is_none(),
        "reauthentication did not keep the quota while clearing its error"
    );
    let item = inner.config.accounts()[0].clone();
    assert!(
        item.account_id == "upstream-one"
            && item.email == "new@example.test"
            && item.organization_name == "Renamed workspace"
            && item.plan_type == "pro"
            && item.last_login_at == now,
        "same-account metadata was not refreshed: {item:?}"
    );
}

// Go TestCompletedDeviceAuthChangedOrUnverifiableIdentityResetsHistory.
#[test]
fn completed_device_auth_changed_or_unverifiable_identity_resets_history() {
    let now = Timestamp::now();
    for (name, expected, current) in [
        ("changed", "upstream-one", "upstream-two"),
        ("missing current id", "upstream-one", ""),
        ("legacy identity cannot be verified", "", "upstream-one"),
    ] {
        let slot = Account {
            email: "old@example.test".into(),
            last_login_at: now.add(-hours(24)),
            ..device_account("slot-one", expected, 0)
        };
        let (app, _dir) = super::testing::test_app(vec![slot]);
        seed_device_auth_identity_history(&app, "slot-one", now);
        let auth = CodexAuthInfo {
            account_id: current.into(),
            email: "new@example.test".into(),
            plan_type: "plus".into(),
            ..CodexAuthInfo::default()
        };
        let reset = app
            .write()
            .apply_completed_device_auth("slot-one", &auth, expected, true, now);
        assert!(
            reset,
            "{name}: a different or unverifiable upstream identity inherited old history"
        );
        let inner = app.read();
        assert!(
            inner.state.prompt_cache().is_empty()
                && inner.state.prompt_cache_baseline().is_empty()
                && inner.state.prompt_cache_reset_at_by_account().is_empty(),
            "{name}: identity change retained cache history"
        );
        drop(inner);
        assert_eq!(
            history_sizes(&app),
            (0, 0, 0, 0, 0),
            "{name}: identity change retained routes"
        );
        assert!(
            !app.read().state.quotas().contains_key("slot-one"),
            "{name}: identity change retained old quota"
        );
        assert_eq!(
            account(&app, 0).account_id,
            current,
            "{name}: new upstream identity not stored"
        );
    }
}

// Go TestDeviceAuthLoginInProgressPausesRoutingWithoutClearingAffinity.
#[test]
fn device_auth_login_in_progress_pauses_routing_without_clearing_affinity() {
    let now = Timestamp::now();
    let slot = Account {
        last_login_at: now.add(-hours(1)),
        ..device_account("slot-one", "upstream-one", 0)
    };
    let (app, _dir) = super::testing::test_app(vec![slot]);
    {
        let mut inner = app.write();
        inner.state.sticky_sessions_mut().insert(
            "gpt-test:thread:root".into(),
            StickySession {
                key: "gpt-test:thread:root".into(),
                model_id: "gpt-test".into(),
                account_id: "slot-one".into(),
                created_at: now.add(-hours(1)),
                last_success_at: now.add(-minutes(1)),
                ..StickySession::default()
            },
        );
        inner.jobs.insert(
            "job-login".into(),
            LoginJob {
                id: "job-login".into(),
                account_id: "slot-one".into(),
                status: "waiting_for_user".into(),
                reauthentication: true,
                ..LoginJob::default()
            },
        );
    }
    let item = account(&app, 0);
    let inner = app.read();
    assert!(
        !inner.has_usable_auth(&item),
        "account remained auth-eligible while its credential file was being rewritten"
    );
    assert!(
        !inner.usable(&item, "gpt-test", now),
        "account remained routable while sign-in repair was active"
    );
    let (status, reason) = inner.account_status(&item, now);
    assert!(
        status == "authenticating" && reason.contains("repair"),
        "active repair status = {status}/{reason}"
    );
    assert_eq!(
        inner.dashboard_summary(now).get("authenticating").copied(),
        Some(1),
        "dashboard did not count active repair"
    );
    assert_eq!(
        inner.state.sticky_sessions().len(),
        1,
        "routing pause cleared sticky affinity"
    );
    let health = inner.account_health_item(&item, now);
    let job = &health["loginJob"];
    assert!(
        job["jobId"] == "job-login"
            && job["status"] == "waiting_for_user"
            && job["reauthentication"] == true,
        "management health did not expose the active login job: {job}"
    );
}

// Go TestPersistedAuthVerificationGateSurvivesRestart.
#[test]
fn persisted_auth_verification_gate_survives_restart() {
    let now = Timestamp::now();
    let slot = Account {
        pending_auth_verification: true,
        pending_auth_expected_account_id: "upstream-one".into(),
        last_login_at: now.add(-hours(1)),
        ..device_account("slot-one", "upstream-one", 0)
    };
    let (app, dir) = super::testing::test_app(vec![slot]);
    super::testing::write_codex_device_auth(&app, "slot-one", "upstream-one", "same@example.test");
    app.write().save().unwrap_or_else(|error| panic!("{error}"));
    let restarted = App::load(
        super::settings::Settings::for_test(dir.path().to_path_buf()),
        super::tuning::Tuning::default(),
        super::log::Logger::discard(),
        Arc::new(crate::transport::Transports::new().unwrap_or_else(|error| panic!("{error}"))),
        |_| None,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let item = restarted.read().config.accounts()[0].clone();
    assert!(
        item.pending_auth_verification && item.pending_auth_expected_account_id == "upstream-one",
        "pending repair marker was not persisted"
    );
    let inner = restarted.read();
    assert!(
        !inner.has_usable_auth(&item) && !inner.usable(&item, "gpt-test", now),
        "restart released an unfinished sign-in repair into routing"
    );
    let (status, reason) = inner.account_status(&item, now);
    assert!(
        status == "missing_auth" && reason.contains("completed"),
        "persisted repair status = {status}/{reason}"
    );
    let public = inner.public_dashboard_account(&item, 0, now);
    assert_eq!(
        public["poolAction"], "repair",
        "public dashboard action during persisted repair"
    );
    assert_eq!(
        public["poolActionLabel"], "Repair",
        "public dashboard repair label"
    );
    assert_eq!(
        super::dashboard::public_pool_label(&item),
        "Verification pending"
    );
}
