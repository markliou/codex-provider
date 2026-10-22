//! The Go tests that drive the pool through its listener (main_test.go,
//! native_codex_test.go, responses_ws_test.go), one by one. Each keeps its Go
//! name in a comment; the assertions are Go's. Upstream is a FakeServer
//! reached through the real Codex transport.

use super::app::App;
use super::http_io::Recorded;
use super::http_io::Recorder;
use super::outcome::UPSTREAM_5XX_FAILOVER_AFTER;
use super::outcome::UPSTREAM_5XX_FAILURE_WINDOW_MINUTES;
use super::routes::Request;
use super::state::Account;
use super::state::AccountHealth;
use super::state::AccountQuota;
use super::state::QuotaSnapshot;
use super::state::QuotaWindow;
use super::state::StickySession;
use super::status::active_cooldowns;
use super::testing::FakeResponse;
use super::testing::FakeServer;
use super::testing::TempDir;
use super::testing::device_account;
use super::testing::provider_account;
use super::testing::test_app;
use super::testing::write_codex_device_auth;
use super::time::Timestamp;
use super::time::hours;
use super::time::minutes;
use super::time::seconds;
use http::Method;
use std::sync::Arc;

pub(super) const OK_RESPONSE: &str = r#"{"id":"resp_ok","object":"response","output":[]}"#;

pub(super) fn arc_app(accounts: Vec<Account>) -> (Arc<App>, TempDir) {
    let (app, dir) = test_app(accounts);
    (Arc::new(app), dir)
}

/// A POST with the client key, as most Go tests build it.
pub(super) fn post(target: &str, body: &str) -> Request {
    Request::test(Method::POST, target, body).with_header("authorization", "Bearer client-key")
}

pub(super) fn responses(body: &str, session: &str) -> Request {
    let request = post("/v1/responses", body);
    if session.is_empty() {
        request
    } else {
        request.with_header("x-codex-pool-session", session)
    }
}

/// Go a.publicMux().ServeHTTP(recorder, request).
pub(super) async fn serve(app: &Arc<App>, request: Request) -> Recorded {
    let recorder = Recorder::new();
    super::routes::serve(app, request, Box::new(recorder.clone())).await;
    recorder.result()
}

pub(super) fn provider(id: &str, priority: i64, url: &str, key: &str) -> Account {
    Account {
        upstream_base_url: url.to_string(),
        upstream_api_key: key.to_string(),
        ..provider_account(id, priority)
    }
}

pub(super) fn windows(hourly: Option<i64>, weekly: Option<i64>) -> AccountQuota {
    let window = |percentage: Option<i64>| QuotaWindow {
        percentage: percentage.unwrap_or_default(),
        present: percentage.is_some(),
        ..QuotaWindow::default()
    };
    AccountQuota {
        hourly: window(hourly),
        weekly: window(weekly),
        ..AccountQuota::default()
    }
}

pub(super) fn set_quota(app: &App, id: &str, plan_type: &str, quota: AccountQuota) {
    app.write().state.quotas_mut().insert(
        id.to_string(),
        QuotaSnapshot {
            account_id: id.to_string(),
            plan_type: plan_type.to_string(),
            quota: Some(quota),
            ..QuotaSnapshot::default()
        },
    );
}

pub(super) fn set_sticky(app: &App, key: &str, account_id: &str, last_success: Timestamp) {
    let now = Timestamp::now();
    app.write().state.sticky_sessions_mut().insert(
        key.to_string(),
        StickySession {
            key: key.to_string(),
            model_id: "gpt-test".to_string(),
            account_id: account_id.to_string(),
            created_at: now.add(-minutes(1)),
            last_success_at: last_success,
            expires_at: now.add(hours(1)),
            ..StickySession::default()
        },
    );
}

pub(super) fn sticky(app: &App, key: &str) -> StickySession {
    app.read()
        .state
        .sticky_sessions()
        .get(key)
        .cloned()
        .unwrap_or_default()
}

pub(super) fn health(app: &App, id: &str) -> AccountHealth {
    app.read()
        .state
        .health()
        .get(id)
        .cloned()
        .unwrap_or_default()
}

pub(super) fn active_cooldown_count(app: &App, id: &str) -> usize {
    let inner = app.read();
    active_cooldowns(
        inner
            .state
            .cooldowns()
            .get(id)
            .map_or(&[][..], Vec::as_slice),
        Timestamp::now(),
    )
    .len()
}

pub(super) fn status_response(status: u16, headers: &[(&str, &str)]) -> FakeResponse {
    FakeResponse {
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
        ..FakeResponse::status(status)
    }
}

// Go TestResponsesProxyTranslatesModelAndUsesStickySession.
#[tokio::test]
async fn responses_proxy_translates_model_and_uses_sticky_session() {
    let upstream = FakeServer::start(|request| {
        assert_eq!(request.path, "/v1/responses", "unexpected upstream path");
        assert_eq!(
            request.header("authorization"),
            "Bearer upstream-key",
            "upstream API key was not forwarded"
        );
        let body = request.json();
        assert_eq!(body["model"], "gpt-test", "unexpected model");
        assert_eq!(
            body["reasoning"]["effort"], "high",
            "reasoning not translated: {}",
            body["reasoning"]
        );
        FakeResponse::json(200, r#"{"id":"resp_test","object":"response","output":[]}"#)
    })
    .await;
    let (app, _dir) = arc_app(vec![provider(
        "one",
        100,
        &format!("{}/v1", upstream.url),
        "upstream-key",
    )]);
    for _ in 0..2 {
        let result = serve(
            &app,
            responses(r#"{"model":"alias(high)","input":"hello"}"#, "session-a"),
        )
        .await;
        assert_eq!(
            result.code(),
            200,
            "proxy returned: {}",
            result.body_string()
        );
    }
    assert_eq!(upstream.count(), 2);
    let session = sticky(&app, "gpt-test:session-a");
    assert_eq!(session.account_id, "one", "sticky session was not saved");
    assert!(
        !session.expires_at.is_zero() && session.expires_at.sub(&Timestamp::now()) >= hours(23),
        "sticky session expiry was not refreshed: {session:?}"
    );
}

// Go TestFailoverAfterRateLimit.
#[tokio::test]
async fn failover_after_rate_limit() {
    let first = FakeServer::start(|_| status_response(429, &[("retry-after", "1")])).await;
    let second = FakeServer::start(|_| FakeResponse::json(200, OK_RESPONSE)).await;
    let (app, _dir) = arc_app(vec![
        provider("first", 100, &first.url, "one"),
        provider("second", 10, &second.url, "two"),
    ]);
    let result = serve(
        &app,
        responses(r#"{"model":"gpt-test","input":"hello"}"#, "failover"),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "failover returned: {}",
        result.body_string()
    );
    assert_eq!(sticky(&app, "gpt-test:failover").account_id, "second");
    assert_eq!(
        app.read()
            .state
            .cooldowns()
            .get("first")
            .map_or(0, Vec::len),
        1,
        "missing cooldown"
    );
    assert_eq!(health(&app, "first").last_failure_reason, "rate_limited");
}

// Go TestResponsesProxyUsesCodexDeviceAuth.
#[tokio::test]
async fn responses_proxy_uses_codex_device_auth() {
    let upstream = FakeServer::start(|request| {
        assert_eq!(
            request.path, "/backend-api/responses",
            "unexpected upstream path"
        );
        assert_eq!(request.header("authorization"), "Bearer <access-token>");
        assert_eq!(
            request.header("chatgpt-account-id"),
            "acct-chatgpt",
            "missing ChatGPT account header"
        );
        FakeResponse::json(200, r#"{"id":"resp_test","object":"response","output":[]}"#)
    })
    .await;
    let account = Account {
        upstream_base_url: format!("{}/backend-api", upstream.url),
        ..device_account("codex-one", "", 100)
    };
    let (app, _dir) = arc_app(vec![account]);
    let home = app.read().account_codex_home("codex-one");
    std::fs::create_dir_all(&home).unwrap_or_else(|error| panic!("{error}"));
    let auth = r#"{"auth_mode":"chatgpt","tokens":{"access_token":"<access-token>","refresh_token":"<refresh-token>","account_id":"acct-chatgpt"}}"#;
    std::fs::write(home.join("auth.json"), auth).unwrap_or_else(|error| panic!("{error}"));
    let result = serve(
        &app,
        responses(r#"{"model":"gpt-test","input":"hello"}"#, ""),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "proxy returned: {}",
        result.body_string()
    );
}

// Go TestCurrentStatusReturnsSessionAccountQuota.
#[tokio::test]
async fn current_status_returns_session_account_quota() {
    let account = Account {
        label: "private@example.test · Team".into(),
        email: "private@example.test".into(),
        account_id: "chatgpt-private-id".into(),
        organization_name: "Private private@example.test".into(),
        plan_type: "team".into(),
        ..provider(
            "private-account-id",
            100,
            "https://upstream.example.test/v1",
            "upstream-secret-value",
        )
    };
    let (app, _dir) = arc_app(vec![account]);
    let now = Timestamp::now();
    set_sticky(&app, "gpt-test:session-a", "private-account-id", now);
    app.write().state.quotas_mut().insert(
        "private-account-id".into(),
        QuotaSnapshot {
            account_id: "private-account-id".into(),
            organization_name: "Private private@example.test".into(),
            plan_type: "team".into(),
            quota: Some(windows(Some(72), Some(44))),
            usage_updated_at: now,
            ..QuotaSnapshot::default()
        },
    );
    let request = Request::test(Method::GET, "/v1/codex-pool/status?model=alias", "")
        .with_header("authorization", "Bearer client-key")
        .with_header("x-codex-pool-session", "session-a");
    let result = serve(&app, request).await;
    assert_eq!(
        result.code(),
        200,
        "status returned: {}",
        result.body_string()
    );
    let body = result.body_string();
    for expected in [
        r#""model":"gpt-test""#,
        r#""displayName":"pr***te@example.test""#,
        r#""planDisplayName":"Team · Private pr***te@example.test""#,
        r#""percentage":72"#,
        r#""percentage":44"#,
    ] {
        assert!(
            body.contains(expected),
            "status body missing {expected}: {body}"
        );
    }
    for forbidden in [
        "private-account-id",
        "private@example.test",
        "chatgpt-private-id",
        "upstream.example.test",
        "upstream-secret-value",
    ] {
        assert!(
            !body.contains(forbidden),
            "status body exposed {forbidden:?}: {body}"
        );
    }
}

// Go TestUnifiedRootAndProtectedProviderEndpoints (the provider half; the
// admin page alias joins with the admin API in step 4.5).
#[tokio::test]
async fn unified_root_and_protected_provider_endpoints() {
    let (app, _dir) = arc_app(Vec::new());
    let root = serve(&app, Request::test(Method::GET, "/", "")).await;
    assert_eq!(root.code(), 200);
    let body = root.body_string();
    assert!(
        body.contains(r#"id="dashboard-view""#) && body.contains(r#"id="login-view""#),
        "combined root did not serve dashboard shell"
    );
    assert_eq!(
        serve(&app, Request::test(Method::GET, "/healthz", ""))
            .await
            .code(),
        401
    );
    let health = Request::test(Method::GET, "/healthz", "")
        .with_header("authorization", "Bearer client-key");
    assert_eq!(serve(&app, health).await.code(), 200);
}

// Go TestInFlightEndpointIsLoopbackOnly.
#[tokio::test]
async fn in_flight_endpoint_is_loopback_only() {
    let (app, _dir) = arc_app(Vec::new());
    app.in_flight.store(3, std::sync::atomic::Ordering::SeqCst);
    for (remote, want) in [
        ("127.0.0.1:41000", 200),
        ("[::1]:41000", 200),
        ("192.168.3.1:41000", 404),
        ("172.21.0.1:41000", 404),
        ("garbage", 404),
    ] {
        let mut request = Request::test(Method::GET, "/internal/inflight", "");
        request.remote_addr = remote.to_string();
        let result = serve(&app, request).await;
        assert_eq!(result.code(), want, "{remote}");
        if want == 200 {
            assert!(
                result.body_string().contains(r#""inFlight":3"#),
                "{remote}: {}",
                result.body_string()
            );
        }
    }
}

// Go TestUpstreamFailureAfterSelectionDoesNotReturnNoEligible503.
#[tokio::test]
async fn upstream_failure_after_selection_does_not_return_no_eligible_503() {
    let pro = FakeServer::start(|_| FakeResponse::status(500)).await;
    let team = Account {
        plan_type: "team".into(),
        upstream_base_url: "http://127.0.0.1:1".into(),
        ..device_account("team-empty", "upstream-team", 100)
    };
    let pro_account = Account {
        plan_type: "pro".into(),
        upstream_base_url: pro.url.clone(),
        ..device_account("pro-only", "upstream-pro", 90)
    };
    let (app, _dir) = arc_app(vec![team, pro_account]);
    app.write().preserve_pro_quota = true;
    set_quota(&app, "team-empty", "team", windows(Some(0), Some(0)));
    set_quota(&app, "pro-only", "pro", windows(Some(96), Some(98)));
    write_codex_device_auth(&app, "team-empty", "upstream-team", "team@example.test");
    write_codex_device_auth(&app, "pro-only", "upstream-pro", "pro@example.test");
    let result = serve(
        &app,
        responses(r#"{"model":"gpt-test","input":"hello"}"#, "single-pro-5xx"),
    )
    .await;
    assert_eq!(
        result.code(),
        502,
        "single eligible upstream failure: {}",
        result.body_string()
    );
    let body = result.body_string();
    assert!(
        !body.contains("no eligible account") && !body.contains("all_accounts_cooling_down"),
        "upstream failure was reported as pool exhaustion: {body}"
    );
    assert_eq!(pro.count(), 1);
    assert_eq!(
        active_cooldown_count(&app, "pro-only"),
        0,
        "single eligible upstream 5xx without Retry-After should not cool down"
    );
    assert_eq!(health(&app, "pro-only").last_failure_reason, "upstream_5xx");
}

// Go TestTransientUpstream5xxPreservesStickyAccount.
#[tokio::test]
async fn transient_upstream_5xx_preserves_sticky_account() {
    let sticky_server = FakeServer::start(|_| FakeResponse::status(500)).await;
    let backup = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_backup","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = arc_app(vec![
        provider("sticky", 100, &sticky_server.url, "sticky"),
        provider("backup", 10, &backup.url, "backup"),
    ]);
    let now = Timestamp::now();
    set_sticky(
        &app,
        "gpt-test:transient-5xx",
        "sticky",
        now.add(-minutes(1)),
    );
    app.write().state.health_mut().insert(
        "sticky".into(),
        AccountHealth {
            consecutive_failure: UPSTREAM_5XX_FAILOVER_AFTER - 1,
            last_failure_reason: "upstream_5xx".into(),
            last_failure_at: now
                .add(-minutes(UPSTREAM_5XX_FAILURE_WINDOW_MINUTES))
                .add(-seconds(1)),
            ..AccountHealth::default()
        },
    );
    let result = serve(
        &app,
        responses(r#"{"model":"gpt-test","input":"hello"}"#, "transient-5xx"),
    )
    .await;
    assert_eq!(
        result.code(),
        502,
        "transient 5xx: {}",
        result.body_string()
    );
    assert_eq!((sticky_server.count(), backup.count()), (1, 0));
    assert_eq!(
        sticky(&app, "gpt-test:transient-5xx").account_id,
        "sticky",
        "transient 5xx changed sticky account"
    );
    assert_eq!(
        active_cooldown_count(&app, "sticky"),
        0,
        "transient 5xx should not cool down sticky account"
    );
    assert_eq!(
        health(&app, "sticky").consecutive_failure,
        1,
        "stale 5xx failures were not reset"
    );
}

// Go TestRepeatedUpstream5xxCanFailoverAfterThreshold.
#[tokio::test]
async fn repeated_upstream_5xx_can_failover_after_threshold() {
    let sticky_server = FakeServer::start(|_| FakeResponse::status(500)).await;
    let backup = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_backup","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = arc_app(vec![
        provider("sticky", 100, &sticky_server.url, "sticky"),
        provider("backup", 10, &backup.url, "backup"),
    ]);
    set_sticky(
        &app,
        "gpt-test:repeated-5xx",
        "sticky",
        Timestamp::now().add(-minutes(1)),
    );
    for attempt in 1..=UPSTREAM_5XX_FAILOVER_AFTER {
        let result = serve(
            &app,
            responses(r#"{"model":"gpt-test","input":"hello"}"#, "repeated-5xx"),
        )
        .await;
        let want = if attempt < UPSTREAM_5XX_FAILOVER_AFTER {
            502
        } else {
            200
        };
        assert_eq!(
            result.code(),
            want,
            "attempt {attempt}: {}",
            result.body_string()
        );
    }
    assert_eq!(
        sticky_server.count(),
        usize::try_from(UPSTREAM_5XX_FAILOVER_AFTER).unwrap_or_default()
    );
    assert_eq!(backup.count(), 1);
    let session = sticky(&app, "gpt-test:repeated-5xx");
    assert_eq!(
        (session.account_id.as_str(), session.failover_from.as_str()),
        ("backup", "sticky")
    );
    assert_eq!(
        active_cooldown_count(&app, "sticky"),
        1,
        "repeated 5xx should cool down failed sticky account"
    );
}

// Go TestStickySessionWithExhaustedQuotaSnapshotReselects.
#[tokio::test]
async fn sticky_session_with_exhausted_quota_snapshot_reselects() {
    let first = FakeServer::start(|_| FakeResponse::status(500)).await;
    let second = FakeServer::start(|_| FakeResponse::json(200, OK_RESPONSE)).await;
    let (app, _dir) = arc_app(vec![
        provider("first", 100, &first.url, "one"),
        provider("second", 10, &second.url, "two"),
    ]);
    set_sticky(
        &app,
        "gpt-test:quota-session",
        "first",
        Timestamp::now().add(-minutes(1)),
    );
    set_quota(&app, "first", "", windows(Some(0), None));
    set_quota(&app, "second", "", windows(Some(80), None));
    let result = serve(
        &app,
        responses(r#"{"model":"gpt-test","input":"hello"}"#, "quota-session"),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "quota snapshot failover: {}",
        result.body_string()
    );
    assert_eq!(first.count(), 0, "exhausted sticky account was called");
    assert_eq!(second.count(), 1);
    let session = sticky(&app, "gpt-test:quota-session");
    assert_eq!(
        (session.account_id.as_str(), session.failover_from.as_str()),
        ("second", "first")
    );
}

// Go TestFailoverTriesAllConfiguredAccounts.
#[tokio::test]
async fn failover_tries_all_configured_accounts() {
    let limited = || FakeServer::start(|_| status_response(429, &[("retry-after", "1")]));
    let first = limited().await;
    let second = limited().await;
    let third = limited().await;
    let fourth = FakeServer::start(|_| FakeResponse::json(200, OK_RESPONSE)).await;
    let (app, _dir) = arc_app(vec![
        provider("first", 400, &first.url, "one"),
        provider("second", 300, &second.url, "two"),
        provider("third", 200, &third.url, "three"),
        provider("fourth", 100, &fourth.url, "four"),
    ]);
    let result = serve(
        &app,
        responses(r#"{"model":"gpt-test","input":"hello"}"#, "all-accounts"),
    )
    .await;
    assert_eq!(result.code(), 200, "failover: {}", result.body_string());
    for (id, server) in [
        ("first", &first),
        ("second", &second),
        ("third", &third),
        ("fourth", &fourth),
    ] {
        assert_eq!(server.count(), 1, "account {id}");
    }
    assert_eq!(sticky(&app, "gpt-test:all-accounts").account_id, "fourth");
    let inner = app.read();
    let (requests, successes) =
        inner
            .throughput_buckets
            .iter()
            .fold((0, 0), |(requests, successes), bucket| {
                (
                    requests + bucket.request_count,
                    successes + bucket.success_count,
                )
            });
    assert_eq!(
        (requests, successes, inner.active_proxy_requests),
        (1, 1, 0),
        "failover attempts were counted as client requests"
    );
}

// Go TestZeroQuotaAccountIsNotSelected.
#[tokio::test]
async fn zero_quota_account_is_not_selected() {
    let first = FakeServer::start(|_| FakeResponse::status(500)).await;
    let second = FakeServer::start(|_| FakeResponse::json(200, OK_RESPONSE)).await;
    let empty = Account {
        remaining_quota: Some(0),
        ..provider("empty", 100, &first.url, "empty")
    };
    let (app, _dir) = arc_app(vec![empty, provider("ready", 10, &second.url, "ready")]);
    let result = serve(
        &app,
        responses(r#"{"model":"gpt-test","input":"hello"}"#, ""),
    )
    .await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    assert_eq!(first.count(), 0, "zero-quota account was selected");
}

// Go TestResponsesProxyAddsCurrentAccountHeaders.
#[tokio::test]
async fn responses_proxy_adds_current_account_headers() {
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(200, r#"{"id":"resp_test","object":"response","output":[]}"#)
    })
    .await;
    let account = Account {
        label: "private@example.test · Team".into(),
        email: "private@example.test".into(),
        organization_name: "Acme Workspace".into(),
        plan_type: "team".into(),
        ..provider(
            "private-account-id",
            100,
            &format!("{}/v1", upstream.url),
            "upstream-key",
        )
    };
    let (app, _dir) = arc_app(vec![account]);
    app.write().state.quotas_mut().insert(
        "private-account-id".into(),
        QuotaSnapshot {
            account_id: "private-account-id".into(),
            organization_name: "Acme Workspace".into(),
            plan_type: "team".into(),
            quota: Some(windows(Some(72), Some(44))),
            usage_updated_at: Timestamp::now(),
            ..QuotaSnapshot::default()
        },
    );
    let result = serve(
        &app,
        responses(r#"{"model":"gpt-test","input":"hello"}"#, "session-a"),
    )
    .await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    assert_eq!(
        result.header("x-codex-pool-account"),
        "pr***te@example.test"
    );
    assert_eq!(result.header("x-codex-pool-quota-hourly-remaining"), "72");
    assert_eq!(result.header("x-codex-pool-quota-weekly-remaining"), "44");
    for forbidden in ["private-account-id", "private@example.test"] {
        for (name, value) in &result.headers {
            assert!(
                !value.to_str().unwrap_or_default().contains(forbidden),
                "response header {name} exposed {forbidden:?}"
            );
        }
    }
}

// ---- streaming: the pre-commit window and in-request retries ----

const STREAM_BODY: &str = r#"{"model":"gpt-test","input":"hello","stream":true}"#;

/// Go's {ID, AuthType: "provider_api_key", UpstreamBaseURL} slot, no key.
pub(super) fn api_slot(id: &str, label: &str, priority: i64, url: &str) -> Account {
    Account {
        label: label.to_string(),
        auth_type: "provider_api_key".to_string(),
        upstream_api_key: String::new(),
        upstream_base_url: url.to_string(),
        ..provider_account(id, priority)
    }
}

/// One SSE block: `event: <type>\ndata: <json>\n\n`.
pub(super) fn sse(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

pub(super) fn created(id: &str) -> String {
    sse(
        "response.created",
        &format!(r#"{{"type":"response.created","response":{{"id":"{id}"}}}}"#),
    )
}

pub(super) fn delta(text: &str) -> String {
    sse(
        "response.output_text.delta",
        &format!(r#"{{"type":"response.output_text.delta","delta":"{text}"}}"#),
    )
}

pub(super) fn completed(id: &str, usage: &str) -> String {
    let usage = if usage.is_empty() {
        String::new()
    } else {
        format!(r#","usage":{usage}"#)
    };
    sse(
        "response.completed",
        &format!(r#"{{"type":"response.completed","response":{{"id":"{id}"{usage}}}}}"#),
    )
}

pub(super) fn failed(id: &str, error: &str, usage: &str) -> String {
    let usage = if usage.is_empty() {
        String::new()
    } else {
        format!(r#","usage":{usage}"#)
    };
    sse(
        "response.failed",
        &format!(
            r#"{{"type":"response.failed","response":{{"id":"{id}","error":{error}{usage}}}}}"#
        ),
    )
}

pub(super) fn sse_response(blocks: &[String]) -> FakeResponse {
    let blocks: Vec<&str> = blocks.iter().map(String::as_str).collect();
    FakeResponse::sse(&blocks)
}

/// A handler that answers by how many requests it has seen (1-based), like
/// Go's `hits++` closures.
pub(super) fn counting(
    answer: impl Fn(usize) -> FakeResponse + Send + Sync + 'static,
) -> impl Fn(&super::testing::RecordedRequest) -> FakeResponse + Send + Sync + 'static {
    let hits = std::sync::atomic::AtomicUsize::new(0);
    move |_| answer(hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
}

fn counters(app: &App) -> (u64, u64, u64, u64) {
    let inner = app.read();
    let state = &inner.state;
    (
        state.request_count,
        state.success_count,
        state.failure_count,
        state.upstream_response_failed_count,
    )
}

fn events(app: &App) -> Vec<super::state::RoutingCacheEvent> {
    app.read().state.routing_cache_events().to_vec()
}

fn cooldown_reasons(app: &App, id: &str) -> Vec<String> {
    app.read()
        .state
        .cooldowns()
        .get(id)
        .map(|values| values.iter().map(|item| item.reason.clone()).collect())
        .unwrap_or_default()
}

fn prompt_cache(app: &App, key: &str) -> Option<super::state::PromptCacheStat> {
    app.read().state.prompt_cache().get(key).cloned()
}

const CAPACITY_USAGE: &str =
    r#"{"input_tokens":2048,"output_tokens":7,"input_tokens_details":{"cached_tokens":1024}}"#;

// Go TestEarlyCapacityStreamingFailureRetriesBeforeCommit.
#[tokio::test]
async fn early_capacity_streaming_failure_retries_before_commit() {
    let first = FakeServer::start(|_| {
        sse_response(&[
            created("resp_failed"),
            sse(
                "response.in_progress",
                r#"{"type":"response.in_progress","response":{"id":"resp_failed"}}"#,
            ),
            failed(
                "resp_failed",
                r#"{"code":"rate_limit_exceeded","message":"secret upstream detail"}"#,
                CAPACITY_USAGE,
            ),
        ])
    })
    .await;
    let second = FakeServer::start(|_| {
        sse_response(&[
            created("resp_second"),
            delta("recovered"),
            completed("resp_second", r#"{"input_tokens":4096,"output_tokens":12,"input_tokens_details":{"cached_tokens":2048}}"#),
        ])
    })
    .await;
    let (app, _dir) = arc_app(vec![
        api_slot("first", "Primary", 100, &first.url),
        api_slot("second", "Backup", 10, &second.url),
    ]);
    let result = serve(&app, responses(STREAM_BODY, "failed-stream")).await;
    let body = result.body_string();
    assert!(
        result.code() == 200 && body.contains("recovered") && body.contains("resp_second"),
        "backup SSE was not forwarded: {body}"
    );
    assert!(
        !body.contains("resp_failed")
            && !body.contains("response.failed")
            && !body.contains("secret upstream detail"),
        "abandoned pre-commit stream leaked downstream: {body}"
    );
    assert_eq!((first.count(), second.count()), (1, 1));
    assert_eq!(
        result.header("x-codex-pool-account"),
        "Backup",
        "response headers describe abandoned account"
    );
    assert_eq!(counters(&app), (1, 1, 1, 1));
    let (first_health, second_health) = (health(&app, "first"), health(&app, "second"));
    assert!(
        first_health.last_success_at.is_zero()
            && first_health.consecutive_failure == 1
            && !second_health.last_success_at.is_zero()
    );
    {
        let inner = app.read();
        assert_eq!(inner.state.sticky_sessions().len(), 1);
        assert_eq!(inner.state.response_bindings().len(), 1);
        assert_eq!(
            inner
                .state
                .response_bindings()
                .get("resp_second")
                .map(|binding| binding.account_id.as_str()),
            Some("second")
        );
        assert_eq!(
            inner.state.cooldowns().get("first").map_or(0, Vec::len),
            1,
            "capacity terminal failure did not set cooldown"
        );
    }
    assert!(
        prompt_cache(&app, "first:gpt-test:main").is_none(),
        "abandoned attempt created request-level cache stats"
    );
    let stat = prompt_cache(&app, "second:gpt-test:main").unwrap_or_default();
    assert_eq!(
        (
            stat.input_tokens,
            stat.cached_tokens,
            stat.routing_failover_count
        ),
        (4096, 2048, 1)
    );
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].routing_outcome, "stream_capacity_failover");
    assert_eq!(events[0].failover_from_account_id, "first");
    assert_eq!(events[0].terminal_event, "response.completed");
}

// Go TestServerErrorStreamFailsOverToAnotherIdentity.
#[tokio::test]
async fn server_error_stream_fails_over_to_another_identity() {
    let first = FakeServer::start(|_| {
        sse_response(&[
            created("resp_failed"),
            failed(
                "resp_failed",
                r#"{"code":"server_error","message":"secret upstream detail"}"#,
                "",
            ),
        ])
    })
    .await;
    let second = FakeServer::start(|_| {
        sse_response(&[
            created("resp_second"),
            delta("recovered"),
            completed("resp_second", ""),
        ])
    })
    .await;
    let (app, _dir) = test_app(vec![
        api_slot("first", "", 100, &first.url),
        api_slot("second", "", 90, &second.url),
    ]);
    let app = Arc::new(fast_overload(app));
    let result = serve(&app, post("/v1/responses", STREAM_BODY)).await;
    let body = result.body_string();
    assert!(
        body.contains("recovered"),
        "a server-error stream was not retried elsewhere: {body}"
    );
    assert!(
        !body.contains("server_error") && !body.contains("secret upstream detail"),
        "the abandoned failure leaked downstream: {body}"
    );
    assert_eq!(
        (first.count(), second.count()),
        (
            usize::try_from(super::tuning::OVERLOAD_SAME_ACCOUNT_RETRY_LIMIT).unwrap_or_default()
                + 1,
            1
        ),
        "the fault must be retried on its account, then fail over once"
    );
    // The outcome must name the cause. Reporting this as a capacity failover
    // would send an operator looking at quota for a server fault.
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].routing_outcome, "stream_transient_failover");
    // A server fault needs a moment, not a spent allowance: it earns the
    // longer 5xx cooldown rather than the short default.
    assert_eq!(
        cooldown_reasons(&app, "first"),
        vec!["server_error".to_string()]
    );
}

/// The same-account overload backoff is a second in production; tests pace
/// it down as responses_ws_test.go does.
pub(super) fn fast_overload(mut app: App) -> App {
    app.tuning.overload_retry_backoff = std::time::Duration::from_millis(1);
    app.tuning.transport_retry_backoff = std::time::Duration::from_millis(1);
    app
}

// Go TestSingleAccountServerErrorStreamRetriesTheSameAccount.
#[tokio::test]
async fn single_account_server_error_stream_retries_the_same_account() {
    let upstream = FakeServer::start(counting(|hits| {
        if hits >= 3 {
            sse_response(&[
                created("resp_only"),
                delta("recovered"),
                completed("resp_only", ""),
            ])
        } else {
            sse_response(&[
                created("resp_only"),
                failed("resp_only", r#"{"code":"internal_server_error"}"#, ""),
            ])
        }
    }))
    .await;
    let (app, _dir) = test_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let app = Arc::new(fast_overload(app));
    let result = serve(&app, post("/v1/responses", STREAM_BODY)).await;
    assert!(
        result.body_string().contains("recovered") && upstream.count() == 3,
        "hits={} body={}",
        upstream.count(),
        result.body_string()
    );
    // The in-place retries must leave no cooldown: one would make the only
    // account unselectable for its own retry.
    assert!(
        cooldown_reasons(&app, "only").is_empty(),
        "an in-request retry left a cooldown"
    );
}

// Go TestUnrecognizedTerminalCodeIsStillNotRetried.
#[tokio::test]
async fn unrecognized_terminal_code_is_still_not_retried() {
    let first = FakeServer::start(|_| {
        sse_response(&[
            created("resp_odd"),
            failed("resp_odd", r#"{"code":"some_new_condition"}"#, ""),
        ])
    })
    .await;
    let second = FakeServer::start(|_| sse_response(&[completed("resp_second", "")])).await;
    let (app, _dir) = arc_app(vec![
        api_slot("first", "", 100, &first.url),
        api_slot("second", "", 90, &second.url),
    ]);
    let result = serve(&app, post("/v1/responses", STREAM_BODY)).await;
    assert_eq!(
        (first.count(), second.count()),
        (1, 0),
        "an unrecognized code was retried"
    );
    assert!(
        result.body_string().contains("some_new_condition"),
        "the unrecognized failure was not forwarded verbatim"
    );
    // An unknown terminal failure still must not touch account health.
    assert!(cooldown_reasons(&app, "first").is_empty());
    assert_eq!(health(&app, "first").consecutive_failure, 0);
}

// Go TestCommittedStreamingResponseFailedDoesNotRetry.
#[tokio::test]
async fn committed_streaming_response_failed_does_not_retry() {
    let first = FakeServer::start(|_| {
        sse_response(&[
            created("resp_failed"),
            delta("visible"),
            failed(
                "resp_failed",
                r#"{"code":"rate_limit_exceeded","message":"secret upstream detail"}"#,
                CAPACITY_USAGE,
            ),
        ])
    })
    .await;
    let second =
        FakeServer::start(|_| FakeResponse::json(200, r#"{"id":"resp_second","output":[]}"#)).await;
    let (app, _dir) = arc_app(vec![
        api_slot("first", "", 100, &first.url),
        api_slot("second", "", 10, &second.url),
    ]);
    let result = serve(&app, responses(STREAM_BODY, "committed-failed-stream")).await;
    let body = result.body_string();
    assert!(
        result.code() == 200 && body.contains("visible") && body.contains("response.failed"),
        "committed failed SSE was not forwarded: {body}"
    );
    assert_eq!(
        (first.count(), second.count()),
        (1, 0),
        "proxy retried after semantic stream commit"
    );
    assert_eq!(counters(&app), (1, 0, 1, 1));
    let first_health = health(&app, "first");
    assert!(first_health.last_success_at.is_zero() && first_health.consecutive_failure == 1);
    {
        let inner = app.read();
        assert!(
            inner.state.sticky_sessions().is_empty()
                && inner.state.response_bindings().is_empty()
                && inner.state.thread_bindings().is_empty()
        );
    }
    assert_eq!(
        cooldown_reasons(&app, "first").len(),
        1,
        "capacity terminal failure did not set cooldown"
    );
    let stat = prompt_cache(&app, "first:gpt-test:main").unwrap_or_default();
    assert_eq!(
        (stat.input_tokens, stat.cached_tokens),
        (2048, 1024),
        "failed stream usage was not preserved"
    );
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert_eq!(
        (
            events[0].terminal_failure_class.as_str(),
            events[0].terminal_error_code.as_str()
        ),
        ("capacity", "rate_limit_exceeded")
    );
}

// Go TestEarlyCapacityStreamingFailureWithoutFallbackPreservesUpstreamSSE.
#[tokio::test]
async fn early_capacity_streaming_failure_without_fallback_preserves_upstream_sse() {
    let upstream = FakeServer::start(|_| {
        sse_response(&[
            created("resp_only"),
            failed(
                "resp_only",
                r#"{"code":"rate_limit_exceeded","message":"selected model is at capacity"}"#,
                "",
            ),
        ])
    })
    .await;
    let (app, _dir) = arc_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let limit = usize::try_from(app.tuning.capacity_retry_limit).unwrap_or_default();
    let result = serve(&app, post("/v1/responses", STREAM_BODY)).await;
    // With no other identity to fail over to, the same account is retried
    // behind a backoff before the caller ever sees the upstream refusal. The
    // refusal must still be preserved verbatim once that budget is spent.
    let body = result.body_string();
    assert!(
        upstream.count() == 1 + limit
            && result.code() == 200
            && body.contains("resp_only")
            && body.contains("selected model is at capacity"),
        "single-account capacity failure was not preserved: hits={} body={body}",
        upstream.count()
    );
    // One client request, but every failed upstream attempt is a real
    // upstream failure and is counted as such.
    let attempts = u64::try_from(1 + limit).unwrap_or_default();
    assert_eq!(counters(&app), (1, 0, attempts, attempts));
    // Exactly one cooldown: the final terminal failure owns it.
    assert_eq!(
        cooldown_reasons(&app, "only").len(),
        1,
        "in-place capacity retries changed cooldown accounting"
    );
}

fn busy_then_recovered(
    first: Vec<String>,
) -> impl Fn(&super::testing::RecordedRequest) -> FakeResponse + Send + Sync + 'static {
    counting(move |hits| {
        if hits == 1 {
            sse_response(&first)
        } else {
            sse_response(&[
                created("resp_ok"),
                delta("recovered"),
                completed(
                    "resp_ok",
                    r#"{"input_tokens":4096,"output_tokens":9,"input_tokens_details":{"cached_tokens":2048}}"#,
                ),
            ])
        }
    })
}

// Go TestEarlyCapacityStreamingFailureRetriesSameAccountWithoutFallback.
#[tokio::test]
async fn early_capacity_streaming_failure_retries_same_account_without_fallback() {
    let upstream = FakeServer::start(busy_then_recovered(vec![
        created("resp_busy"),
        failed(
            "resp_busy",
            r#"{"code":"server_is_overloaded","message":"selected model is at capacity"}"#,
            "",
        ),
    ]))
    .await;
    let (app, _dir) = test_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let app = Arc::new(fast_overload(app));
    let result = serve(&app, post("/v1/responses", STREAM_BODY)).await;
    let body = result.body_string();
    assert!(
        upstream.count() == 2 && result.code() == 200 && body.contains("recovered"),
        "not retried: {body}"
    );
    // The abandoned pre-commit prefix must never reach the caller.
    assert!(
        !body.contains("resp_busy") && !body.contains("at capacity"),
        "abandoned capacity prefix leaked downstream: {body}"
    );
    assert_eq!(counters(&app), (1, 1, 1, 1));
    assert!(
        cooldown_reasons(&app, "only").is_empty(),
        "recovered capacity retry left a cooldown"
    );
}

// Go TestErrorEventBeforeCapacityFailureKeepsRetryWindowOpen.
#[tokio::test]
async fn error_event_before_capacity_failure_keeps_retry_window_open() {
    let upstream = FakeServer::start(busy_then_recovered(vec![
        created("resp_busy"),
        sse("error", r#"{"type":"error","code":"server_is_overloaded","message":"Selected model is at capacity"}"#),
        failed("resp_busy", r#"{"code":"server_is_overloaded","message":"Selected model is at capacity"}"#, ""),
    ]))
    .await;
    let (app, _dir) = test_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let app = Arc::new(fast_overload(app));
    let result = serve(&app, post("/v1/responses", STREAM_BODY)).await;
    let body = result.body_string();
    assert!(
        upstream.count() == 2 && result.code() == 200 && body.contains("recovered"),
        "not retried: {body}"
    );
    assert!(
        !body.contains("resp_busy") && !body.contains("at capacity"),
        "abandoned error/failure prefix leaked downstream: {body}"
    );
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert_ne!(
        events[0].precommit_close_reason, "error",
        "bare error event still closed the retry window"
    );
}

// Go TestBufferedErrorEventIsForwardedWhenStreamContinues.
#[tokio::test]
async fn buffered_error_event_is_forwarded_when_stream_continues() {
    let upstream = FakeServer::start(|_| {
        sse_response(&[
            created("resp_warn"),
            sse(
                "error",
                r#"{"type":"error","code":"upstream_warning","message":"non-fatal notice"}"#,
            ),
            delta("still streaming"),
            completed("resp_warn", ""),
        ])
    })
    .await;
    let (app, _dir) = arc_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let result = serve(&app, post("/v1/responses", STREAM_BODY)).await;
    let body = result.body_string();
    assert!(
        result.code() == 200
            && body.contains("non-fatal notice")
            && body.contains("still streaming"),
        "dropped: {body}"
    );
    assert!(
        body.find("non-fatal notice") < body.find("still streaming"),
        "buffered error event was forwarded out of order: {body}"
    );
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert!(
        events[0].precommit_committed
            && events[0].precommit_close_reason == "response.output_text.delta"
    );
}

// Go TestStandaloneErrorEventIsNotRecordedAsSuccess.
#[tokio::test]
async fn standalone_error_event_is_not_recorded_as_success() {
    let upstream = FakeServer::start(|_| {
        sse_response(&[
            created("resp_error"),
            sse(
                "error",
                r#"{"type":"error","code":"invalid_prompt","message":"request rejected"}"#,
            ),
        ])
    })
    .await;
    let (app, _dir) = arc_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let result = serve(&app, responses(STREAM_BODY, "standalone-error")).await;
    assert!(
        result.code() == 200 && result.body_string().contains("request rejected"),
        "not forwarded: {}",
        result.body_string()
    );
    let (requests, successes, failures, _) = counters(&app);
    assert_eq!((requests, successes, failures), (1, 0, 1));
    {
        let inner = app.read();
        assert!(
            inner.state.sticky_sessions().is_empty() && inner.state.response_bindings().is_empty()
        );
    }
    assert!(health(&app, "only").last_success_at.is_zero());
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert_eq!(
        (
            events[0].terminal_event.as_str(),
            events[0].terminal_failure_class.as_str()
        ),
        ("error", "request")
    );
}

// Go TestStandaloneCapacityErrorRetriesAtEOF.
#[tokio::test]
async fn standalone_capacity_error_retries_at_eof() {
    let upstream = FakeServer::start(counting(|hits| {
        if hits == 1 {
            sse_response(&[sse(
                "error",
                r#"{"type":"error","code":"server_is_overloaded","message":"at capacity"}"#,
            )])
        } else {
            sse_response(&[
                created("resp_ok"),
                delta("recovered"),
                completed("resp_ok", ""),
            ])
        }
    }))
    .await;
    let (app, _dir) = test_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let app = Arc::new(fast_overload(app));
    let result = serve(&app, post("/v1/responses", STREAM_BODY)).await;
    let body = result.body_string();
    assert!(
        upstream.count() == 2 && result.code() == 200 && body.contains("recovered"),
        "not retried: {body}"
    );
    assert!(
        !body.contains("at capacity"),
        "abandoned standalone error leaked downstream: {body}"
    );
    let (requests, successes, failures, _) = counters(&app);
    assert_eq!((requests, successes, failures), (1, 1, 1));
}

fn in_progress(id: &str) -> String {
    sse(
        "response.in_progress",
        &format!(r#"{{"type":"response.in_progress","response":{{"id":"{id}"}}}}"#),
    )
}

fn overloaded(id: &str) -> String {
    failed(id, r#"{"code":"server_is_overloaded"}"#, "")
}

fn at_capacity(id: &str) -> String {
    failed(
        id,
        r#"{"code":"server_is_overloaded","message":"Selected model is at capacity"}"#,
        "",
    )
}

/// The same preamble every time, then a failure on the first request and
/// output on later ones.
fn preamble_then(
    preamble: Vec<String>,
    failure: String,
    id: &'static str,
) -> impl Fn(&super::testing::RecordedRequest) -> FakeResponse + Send + Sync + 'static {
    counting(move |hits| {
        let mut blocks = preamble.clone();
        if hits == 1 {
            blocks.push(failure.clone());
        } else {
            blocks.push(delta("recovered"));
            blocks.push(completed(id, ""));
        }
        sse_response(&blocks)
    })
}

async fn only_slot(server: &FakeServer) -> (Arc<App>, TempDir) {
    let (app, dir) = test_app(vec![api_slot("only", "", 100, &server.url)]);
    (Arc::new(fast_overload(app)), dir)
}

// Go TestCapacityFailureAfterLongLifecyclePreambleStillRetries.
#[tokio::test]
async fn capacity_failure_after_long_lifecycle_preamble_still_retries() {
    let mut preamble = vec![created("resp_wait")];
    preamble.extend((0..20).map(|_| in_progress("resp_wait")));
    let upstream = FakeServer::start(preamble_then(
        preamble,
        at_capacity("resp_wait"),
        "resp_wait",
    ))
    .await;
    let (app, _dir) = only_slot(&upstream).await;
    let body = serve(&app, post("/v1/responses", STREAM_BODY))
        .await
        .body_string();
    assert!(
        upstream.count() == 2 && body.contains("recovered"),
        "not retried: hits={} body={body}",
        upstream.count()
    );
    assert!(
        !body.contains("at capacity"),
        "abandoned capacity prefix leaked downstream: {body}"
    );
}

// Go TestKeepaliveCommentsDoNotSpendThePrecommitBudget.
#[tokio::test]
async fn keepalive_comments_do_not_spend_the_precommit_budget() {
    let mut preamble = vec![created("resp_ka")];
    preamble.extend((0..200).map(|_| ": keepalive\n\n".to_string()));
    let upstream =
        FakeServer::start(preamble_then(preamble, overloaded("resp_ka"), "resp_ka")).await;
    let (app, _dir) = only_slot(&upstream).await;
    let body = serve(&app, post("/v1/responses", STREAM_BODY))
        .await
        .body_string();
    assert!(
        upstream.count() == 2 && body.contains("recovered"),
        "keepalives closed the retry window: hits={}",
        upstream.count()
    );
}

// Go TestPrecommitBoundStillCommitsAndRecordsDepth.
#[tokio::test]
async fn precommit_bound_still_commits_and_records_depth() {
    use super::stream::STREAMING_PRECOMMIT_MAX_BLOCKS;
    let upstream = FakeServer::start(|_| {
        let mut blocks: Vec<String> = (0..STREAMING_PRECOMMIT_MAX_BLOCKS + 5)
            .map(|_| in_progress("resp_long"))
            .collect();
        blocks.push(overloaded("resp_long"));
        sse_response(&blocks)
    })
    .await;
    let (app, _dir) = only_slot(&upstream).await;
    serve(&app, post("/v1/responses", STREAM_BODY)).await;
    assert_eq!(
        upstream.count(),
        1,
        "a committed stream must never be retried"
    );
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].precommit_close_reason, "bounds");
    assert!(
        events[0].precommit_blocks
            > i64::try_from(STREAMING_PRECOMMIT_MAX_BLOCKS).unwrap_or_default()
    );
}

// Go TestLargeContextCapacityFailureStillRetries.
#[tokio::test]
async fn large_context_capacity_failure_still_retries() {
    // Well past the fixed 64 KiB part of the bound on its own.
    let context = "x".repeat(200 << 10);
    let hits = std::sync::atomic::AtomicUsize::new(0);
    let upstream = FakeServer::start(move |request| {
        let hits = hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let echo = request.json()["input"].to_string();
        // Upstream echoes the request in the created event, as the Responses
        // API does.
        let mut blocks = vec![sse(
            "response.created",
            &format!(
                r#"{{"type":"response.created","response":{{"id":"resp_big","input":{echo}}}}}"#
            ),
        )];
        if hits == 1 {
            blocks.push(at_capacity("resp_big"));
        } else {
            blocks.push(delta("recovered"));
            blocks.push(completed("resp_big", ""));
        }
        sse_response(&blocks)
    })
    .await;
    let (app, _dir) = only_slot(&upstream).await;
    let body =
        serde_json::json!({"model": "gpt-test", "input": context, "stream": true}).to_string();
    let result = serve(&app, post("/v1/responses", &body))
        .await
        .body_string();
    assert!(
        upstream.count() == 2 && result.contains("recovered"),
        "large-context capacity failure was not retried: hits={}",
        upstream.count()
    );
    assert!(
        !result.contains("at capacity"),
        "abandoned capacity prefix leaked downstream"
    );
    let events = events(&app);
    assert!(
        events.len() == 1 && events[0].precommit_close_reason != "bounds",
        "large context still closed the window on bounds"
    );
}

// Go TestPrecommitBoundsClosureRecordsBytes.
#[tokio::test]
async fn precommit_bounds_closure_records_bytes() {
    use super::stream::MAX_REQUEST_BODY;
    use super::stream::PRECOMMIT_REQUEST_ECHO_FACTOR;
    use super::stream::STREAMING_PRECOMMIT_MAX_BYTES;
    use super::stream::precommit_byte_limit;
    assert_eq!(precommit_byte_limit(0), STREAMING_PRECOMMIT_MAX_BYTES);
    assert_eq!(
        precommit_byte_limit(1000),
        STREAMING_PRECOMMIT_MAX_BYTES + 1000 * PRECOMMIT_REQUEST_ECHO_FACTOR
    );
    // The ceiling keeps the buffer the same order as the request body already
    // held.
    assert_eq!(precommit_byte_limit(MAX_REQUEST_BODY), MAX_REQUEST_BODY);
    let huge = "y".repeat(STREAMING_PRECOMMIT_MAX_BYTES + 1024);
    let upstream = FakeServer::start(move |_| {
        // One created event larger than the fixed bound with a tiny request
        // behind it, so the byte bound trips on a single block.
        sse_response(&[
            sse("response.created", &format!(r#"{{"type":"response.created","response":{{"id":"resp_huge","pad":"{huge}"}}}}"#)),
            overloaded("resp_huge"),
        ])
    })
    .await;
    let (app, _dir) = only_slot(&upstream).await;
    serve(
        &app,
        post(
            "/v1/responses",
            r#"{"model":"gpt-test","input":"hi","stream":true}"#,
        ),
    )
    .await;
    let events = events(&app);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert!(
        event.precommit_close_reason == "bounds"
            && event.precommit_blocks == 1
            && event.precommit_bytes
                > i64::try_from(STREAMING_PRECOMMIT_MAX_BYTES).unwrap_or_default(),
        "bounds closure did not record a single oversized block: {event:?}"
    );
}

// Go TestNamedKeepaliveEventsKeepTheRetryWindowOpen.
#[tokio::test]
async fn named_keepalive_events_keep_the_retry_window_open() {
    let mut preamble = vec![created("resp_ka")];
    for _ in 0..super::stream::STREAMING_PRECOMMIT_MAX_BLOCKS + 10 {
        preamble.push(sse("keepalive", r#"{"type":"keepalive"}"#));
        preamble.push(sse("ping", "{}"));
    }
    let upstream =
        FakeServer::start(preamble_then(preamble, overloaded("resp_ka"), "resp_ka")).await;
    let (app, _dir) = only_slot(&upstream).await;
    let body = serve(&app, post("/v1/responses", STREAM_BODY))
        .await
        .body_string();
    assert!(
        upstream.count() == 2 && body.contains("recovered"),
        "named keepalives closed the retry window"
    );
    let events = events(&app);
    assert!(events.len() == 1 && events[0].precommit_close_reason != "keepalive");
}

// Go TestResponseMetadataKeepsTheRetryWindowOpen.
#[tokio::test]
async fn response_metadata_keeps_the_retry_window_open() {
    let upstream = FakeServer::start(counting(|hits| {
        let mut blocks = vec![
            created("resp_meta"),
            in_progress("resp_meta"),
            sse(
                "response.metadata",
                &format!(r#"{{"type":"response.metadata","attempt":{hits}}}"#),
            ),
        ];
        if hits == 1 {
            blocks.push(overloaded("resp_meta"));
        } else {
            blocks.push(delta("recovered"));
            blocks.push(completed("resp_meta", ""));
        }
        sse_response(&blocks)
    }))
    .await;
    let (app, _dir) = only_slot(&upstream).await;
    let body = serve(&app, post("/v1/responses", STREAM_BODY))
        .await
        .body_string();
    assert!(
        upstream.count() == 2 && body.contains("recovered"),
        "response.metadata closed the retry window: {body}"
    );
    assert!(
        !body.contains(r#""attempt":1"#) && body.contains(r#""attempt":2"#),
        "client must get only the successful attempt's metadata: {body}"
    );
    assert!(
        !body.contains("server_is_overloaded"),
        "retried capacity failure leaked to the client: {body}"
    );
}

// Go TestCommittedCapacityFailureRecordsPrecommitCloseReason.
#[tokio::test]
async fn committed_capacity_failure_records_precommit_close_reason() {
    let upstream = FakeServer::start(|_| {
        sse_response(&[
            created("resp_c"),
            delta("visible"),
            failed(
                "resp_c",
                r#"{"code":"server_is_overloaded","message":"at capacity"}"#,
                "",
            ),
        ])
    })
    .await;
    let backup =
        FakeServer::start(|_| FakeResponse::json(200, r#"{"id":"resp_backup","output":[]}"#)).await;
    let (app, _dir) = arc_app(vec![
        api_slot("first", "", 100, &upstream.url),
        api_slot("second", "", 10, &backup.url),
    ]);
    serve(&app, post("/v1/responses", STREAM_BODY)).await;
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].terminal_failure_class, "capacity");
    assert!(
        events[0].precommit_committed
            && events[0].precommit_close_reason == "response.output_text.delta"
    );
}

// Go TestUncommittedCapacityFailureRecordsOpenPrecommitWindow.
#[tokio::test]
async fn uncommitted_capacity_failure_records_open_precommit_window() {
    let upstream = FakeServer::start(|_| {
        sse_response(&[
            created("resp_u"),
            failed(
                "resp_u",
                r#"{"code":"server_is_overloaded","message":"at capacity"}"#,
                "",
            ),
        ])
    })
    .await;
    let (app, _dir) = only_slot(&upstream).await;
    serve(&app, post("/v1/responses", STREAM_BODY)).await;
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert!(!events[0].precommit_committed && events[0].precommit_close_reason.is_empty());
}

// Go TestRequestSpecificStreamingFailureDoesNotPenalizeAccount.
#[tokio::test]
async fn request_specific_streaming_failure_does_not_penalize_account() {
    let upstream = FakeServer::start(|_| {
        sse_response(&[failed(
            "resp_context",
            r#"{"code":"context_length_exceeded","message":"too long"}"#,
            "",
        )])
    })
    .await;
    let (app, _dir) = arc_app(vec![api_slot("provider", "", 100, &upstream.url)]);
    let result = serve(&app, post("/v1/responses", STREAM_BODY)).await;
    assert_eq!(result.code(), 200);
    let provider_health = health(&app, "provider");
    assert!(
        provider_health.consecutive_failure == 0 && provider_health.last_failure_at.is_zero(),
        "penalized: {provider_health:?}"
    );
    assert!(cooldown_reasons(&app, "provider").is_empty());
    assert_eq!(counters(&app).2, 1);
}

// ---- capacity and rate-limit retries across accounts ----

fn rate_limited(retry_after: Option<&str>) -> FakeResponse {
    let mut response = FakeResponse::json(429, r#"{"error":{"code":"rate_limit_exceeded"}}"#);
    if let Some(value) = retry_after {
        response.headers.push(("retry-after".into(), value.into()));
    }
    response
}

type Visits = Arc<std::sync::Mutex<Vec<String>>>;

fn visits(log: &Visits) -> Vec<String> {
    log.lock().map(|order| order.clone()).unwrap_or_default()
}

/// A capacity-refusing stream that recovers from the given visit on (0:
/// never), recording each visit in `log`.
fn refusing(
    id: &'static str,
    succeed_from_visit: usize,
    log: Visits,
    preamble: Vec<String>,
) -> impl Fn(&super::testing::RecordedRequest) -> FakeResponse + Send + Sync + 'static {
    let count = std::sync::atomic::AtomicUsize::new(0);
    move |_| {
        let visit = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if let Ok(mut order) = log.lock() {
            order.push(id.to_string());
        }
        let mut blocks = vec![created("resp_x")];
        blocks.extend(preamble.iter().cloned());
        if succeed_from_visit > 0 && visit >= succeed_from_visit {
            blocks.push(delta("recovered"));
            blocks.push(completed("resp_x", ""));
        } else {
            blocks.push(overloaded("resp_x"));
        }
        sse_response(&blocks)
    }
}

// Go TestCapacityRetryRestoresEveryRefusedIdentity.
#[tokio::test]
async fn capacity_retry_restores_every_refused_identity() {
    let log: Visits = Arc::default();
    // The first account recovers only on its second visit, which it can never
    // get if a retry round leaves it excluded.
    let first = FakeServer::start(refusing("first", 2, log.clone(), Vec::new())).await;
    let second = FakeServer::start(refusing("second", 0, log.clone(), Vec::new())).await;
    let (app, _dir) = test_app(vec![
        api_slot("first", "", 100, &first.url),
        api_slot("second", "", 90, &second.url),
    ]);
    let app = Arc::new(fast_overload(app));
    let body = serve(&app, post("/v1/responses", STREAM_BODY))
        .await
        .body_string();
    assert!(
        body.contains("recovered"),
        "a recovered identity was never retried: {:?} {body}",
        visits(&log)
    );
    assert!(
        first.count() >= 2,
        "the first identity was not revisited after the wait"
    );
    assert!(
        !body.contains("server_is_overloaded"),
        "an abandoned capacity prefix leaked downstream: {body}"
    );
}

// Go TestSingleAccountRateLimitRetriesTheSameAccount.
#[tokio::test]
async fn single_account_rate_limit_retries_the_same_account() {
    // Recovers only on the third visit, which it can never get without an
    // in-request retry of the same account.
    let upstream = FakeServer::start(counting(|hits| {
        if hits < 3 {
            FakeResponse::json(
                429,
                r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#,
            )
        } else {
            FakeResponse::json(200, r#"{"id":"resp_recovered","output":[]}"#)
        }
    }))
    .await;
    let (app, _dir) = arc_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#),
    )
    .await;
    assert!(
        result.code() == 200 && result.body_string().contains("resp_recovered"),
        "not retried: {}",
        result.body_string()
    );
    assert_eq!(upstream.count(), 3);
    // The retried rounds must leave no cooldown behind.
    assert!(
        cooldown_reasons(&app, "only").is_empty(),
        "an in-request retry left a cooldown"
    );
}

// Go TestSingleAccountRateLimitStopsAtTheRetryBudget.
#[tokio::test]
async fn single_account_rate_limit_stops_at_the_retry_budget() {
    let upstream = FakeServer::start(|_| rate_limited(None)).await;
    let (app, _dir) = arc_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let limit = usize::try_from(app.tuning.capacity_retry_limit).unwrap_or_default();
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#),
    )
    .await;
    assert_eq!(upstream.count(), 1 + limit);
    assert_ne!(result.code(), 200, "a spent budget was reported as success");
    // The last attempt owns the cooldown.
    assert_eq!(cooldown_reasons(&app, "only").len(), 1);
}

// Go TestSingleAccountRateLimitHonorsALongRetryAfter.
#[tokio::test]
async fn single_account_rate_limit_honors_a_long_retry_after() {
    let upstream = FakeServer::start(|_| rate_limited(Some("3600"))).await;
    let (app, _dir) = arc_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#),
    )
    .await;
    assert_eq!(
        upstream.count(),
        1,
        "an hour-long Retry-After was waited out in-request"
    );
    assert_ne!(result.code(), 200);
    assert_eq!(
        cooldown_reasons(&app, "only"),
        vec!["rate_limited".to_string()]
    );
}

// Go TestRateLimitPrefersAFallbackOverWaiting.
#[tokio::test]
async fn rate_limit_prefers_a_fallback_over_waiting() {
    let limited = FakeServer::start(|_| rate_limited(None)).await;
    let healthy =
        FakeServer::start(|_| FakeResponse::json(200, r#"{"id":"resp_backup","output":[]}"#)).await;
    let (app, _dir) = arc_app(vec![
        api_slot("limited", "", 100, &limited.url),
        api_slot("healthy", "", 90, &healthy.url),
    ]);
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#),
    )
    .await;
    assert!(
        result.code() == 200 && result.body_string().contains("resp_backup"),
        "{}",
        result.body_string()
    );
    // The rate-limited identity is visited once and then left alone.
    assert_eq!((limited.count(), healthy.count()), (1, 1));
    assert_eq!(cooldown_reasons(&app, "limited").len(), 1);
}

// Go TestCapacityRefusalSweepsEveryAccountBeforeWaiting.
#[tokio::test]
async fn capacity_refusal_sweeps_every_account_before_waiting() {
    let log: Visits = Arc::default();
    // The refusal arrives after upstream has announced an output item, which
    // is where production actually falls over.
    let item = vec![sse(
        "response.output_item.added",
        r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"item_1","type":"reasoning","summary":[]}}"#,
    )];
    let first = FakeServer::start(refusing("first", 0, log.clone(), item.clone())).await;
    let second = FakeServer::start(refusing("second", 0, log.clone(), item.clone())).await;
    // The third identity recovers only on its second visit, which it can
    // reach only after a full sweep and one wait.
    let third = FakeServer::start(refusing("third", 2, log.clone(), item)).await;
    let (app, _dir) = test_app(vec![
        api_slot("first", "", 100, &first.url),
        api_slot("second", "", 90, &second.url),
        api_slot("third", "", 80, &third.url),
    ]);
    let app = Arc::new(fast_overload(app));
    let body = serve(&app, post("/v1/responses", STREAM_BODY))
        .await
        .body_string();
    assert!(
        body.contains("recovered"),
        "the sweep never reached a recovered identity: {:?}",
        visits(&log)
    );
    // The bound account first spends its same-account overload schedule; then
    // every other identity is tried once, in priority order, before the second
    // visit to any of them: that is a sweep, not a per-account wait loop.
    assert_eq!(
        visits(&log),
        [
            "first", "first", "first", "first", "second", "third", "first", "second", "third"
        ]
    );
    assert!(
        !body.contains("server_is_overloaded"),
        "an abandoned refusal leaked downstream: {body}"
    );
}

// Go TestUnroutableModelIsAClientErrorNotAnAccountFailure.
#[tokio::test]
async fn unroutable_model_is_a_client_error_not_an_account_failure() {
    use super::quota::gateway_unknown_model;
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            502,
            r#"{"error":{"message":"unknown provider for model codex-pool-acct/gpt-retired","type":"server_error","code":"internal_server_error"}}"#,
        )
    })
    .await;
    let other =
        FakeServer::start(|_| FakeResponse::json(200, r#"{"id":"resp_ok","output":[]}"#)).await;
    let (app, _dir) = arc_app(vec![
        api_slot("first", "", 100, &upstream.url),
        api_slot("second", "", 90, &other.url),
    ]);
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#),
    )
    .await;
    assert!(
        result.code() == 404 && result.body_string().contains("model_not_found"),
        "{}",
        result.body_string()
    );
    // No sweep: no other identity can serve a model the gateway does not know.
    assert_eq!(upstream.count(), 1);
    assert_eq!(health(&app, "first").consecutive_failure, 0);
    assert!(cooldown_reasons(&app, "first").is_empty());
    // A genuine 5xx must still be treated as an upstream failure.
    assert!(gateway_unknown_model(
        br#"{"error":{"message":"unknown provider for model x"}}"#
    ));
    for body in [
        r#"{"error":{"message":"Our servers are currently overloaded."}}"#,
        r#"{"error":{"code":"internal_server_error"}}"#,
        "not json",
    ] {
        assert!(!gateway_unknown_model(body.as_bytes()), "misread: {body}");
    }
}

// Go TestTerminalFailureIsLoggedWithoutPersistingTheMessage.
#[tokio::test]
async fn terminal_failure_is_logged_without_persisting_the_message() {
    let upstream = FakeServer::start(|_| {
        sse_response(&[
            created("resp_bad"),
            delta("visible"),
            failed(
                "resp_bad",
                r#"{"code":"server_error","message":"An error occurred.\nPlease include the request ID 52cb6f95-f64b-48dc-a68e-aeb7e6e9b870 in your message."}"#,
                "",
            ),
        ])
    })
    .await;
    let (app, _dir) = test_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let (app, logged) = super::testing::capture_logs(app);
    let app = Arc::new(app);
    serve(
        &app,
        post("/v1/responses", STREAM_BODY).with_header("x-request-id", "req-abc"),
    )
    .await;
    let lines = logged.lock().map(|lines| lines.clone()).unwrap_or_default();
    let line = lines
        .iter()
        .find(|line| line.contains("upstream terminal failure:"))
        .cloned()
        .unwrap_or_default();
    for want in [
        "account=only",
        "model=gpt-test",
        "code=server_error",
        "class=transient",
        "request=req-abc",
        // The upstream request id is what support asks for.
        "52cb6f95-f64b-48dc-a68e-aeb7e6e9b870",
    ] {
        assert!(line.contains(want), "log line missing {want:?}: {lines:?}");
    }
    // Bounded and stripped: a newline in upstream prose must not break the
    // line into something that reads as a separate log entry.
    assert!(
        !line.trim_end_matches('\n').contains('\n'),
        "upstream prose split the log line: {line:?}"
    );
    // State is the boundary the message must not cross.
    let persisted = serde_json::to_string(&app.read().state).unwrap_or_default();
    for forbidden in ["52cb6f95", "An error occurred"] {
        assert!(
            !persisted.contains(forbidden),
            "upstream message reached persisted state"
        );
    }
    let events = events(&app);
    assert!(events.len() == 1 && events[0].terminal_error_code == "server_error");
}

// Go TestRetriedTerminalFailureIsLoggedAsRetried.
#[tokio::test]
async fn retried_terminal_failure_is_logged_as_retried() {
    let upstream = FakeServer::start(counting(|hits| {
        if hits >= 2 {
            sse_response(&[created("resp_only"), completed("resp_only", "")])
        } else {
            sse_response(&[
                created("resp_only"),
                failed(
                    "resp_only",
                    r#"{"code":"server_error","message":"transient fault"}"#,
                    "",
                ),
            ])
        }
    }))
    .await;
    let (app, _dir) = test_app(vec![api_slot("only", "", 100, &upstream.url)]);
    let (app, logged) = super::testing::capture_logs(fast_overload(app));
    let app = Arc::new(app);
    let result = serve(&app, post("/v1/responses", STREAM_BODY)).await;
    assert!(
        result.code() == 200 && upstream.count() == 2,
        "the retry did not recover"
    );
    let all = logged
        .lock()
        .map(|lines| lines.join("\n"))
        .unwrap_or_default();
    assert!(
        all.contains("disposition=retried"),
        "a recovered attempt was not logged as retried: {all}"
    );
    assert!(
        !all.contains("disposition=final"),
        "a recovered request logged a final failure: {all}"
    );
}

// Go TestProxyClientCancelDoesNotMarkTransportFailure.
#[tokio::test]
async fn proxy_client_cancel_does_not_mark_transport_failure() {
    let upstream = FakeServer::start(|_| FakeResponse::status(204)).await;
    let account = Account {
        auth_type: "provider_api_key".into(),
        ..provider("provider", 0, &upstream.url, "provider-key")
    };
    let (app, _dir) = arc_app(vec![account]);
    let request = post("/v1/responses", r#"{"model":"gpt-test","input":"ping"}"#);
    request.cancel.cancel();
    serve(&app, request).await;
    assert_eq!(upstream.count(), 0, "cancelled request reached upstream");
    assert_eq!(
        counters(&app).2,
        0,
        "cancelled request incremented failure count"
    );
    let provider_health = health(&app, "provider");
    assert!(
        provider_health.last_failure_reason.is_empty() && provider_health.consecutive_failure == 0
    );
    assert!(cooldown_reasons(&app, "provider").is_empty());
}
