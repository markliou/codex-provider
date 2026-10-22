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

// ---- the Codex backend, reached directly with an account's credential ----

/// Go's device-auth slots with `a.codexBaseURL = <server>/backend-api`.
pub(super) fn codex_app(server_url: &str, slots: &[(&str, &str, i64)]) -> (Arc<App>, TempDir) {
    let accounts = slots
        .iter()
        .map(|(id, upstream, priority)| device_account(id, upstream, *priority))
        .collect();
    let base = format!("{server_url}/backend-api");
    let (app, dir) =
        super::testing::test_app_with(accounts, |settings| settings.codex_base_url = base);
    for (id, upstream, _) in slots {
        write_codex_device_auth(&app, id, upstream, &format!("{id}@example.test"));
    }
    (Arc::new(fast_overload(app)), dir)
}

fn account(app: &App, index: usize) -> Account {
    app.read().config.accounts()[index].clone()
}

// Go TestDirectCodexGatewaySendsTheNativeCodexRequest.
#[tokio::test]
async fn direct_codex_gateway_sends_the_native_codex_request() {
    const RAW: &str = r#"{"model":"gpt-test","stream":true,"input":[{"type":"message","role":"user","content":"hi"}],"tools":[{"type":"function","name":"f","parameters":{"type":"object","properties":{"z":{"type":"string"},"a":{"type":"string"}}}}],"store":false,"prompt_cache_key":"thread-native","n":1.50}"#;
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_native","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = codex_app(&upstream.url, &[("device", "acct-up", 100)]);
    let auth = app
        .read()
        .codex_auth(&account(&app, 0))
        .unwrap_or_else(|error| panic!("{error:?}"));
    let request = post("/v1/responses", RAW)
        .with_header("cookie", "client-cookie")
        .with_header("session-id", "session-native")
        .with_header("thread-id", "thread-native")
        .with_header("x-codex-window-id", "window-native")
        .with_header("x-openai-memgen-request", "true")
        .with_header(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        )
        .with_header("x-codex-pool-session", "pool-only")
        .with_header("x-codex-thread-id", "pool-only-thread")
        .with_header("x-openai-fedramp", "true");
    let result = serve(&app, request).await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    let sent = upstream.requests().pop().unwrap_or_default();
    assert_eq!(sent.path, "/backend-api/codex/responses");
    assert_eq!(sent.header("content-encoding"), "zstd");
    let body = zstd::decode_all(sent.body.as_slice()).unwrap_or_default();
    assert_eq!(
        String::from_utf8_lossy(&body),
        RAW,
        "an unchanged request must reach upstream byte for byte"
    );
    for (name, want) in [
        ("authorization", format!("Bearer {}", auth.access_token)),
        ("chatgpt-account-id", "acct-up".to_string()),
        ("x-codex-routing-hint", "model=gpt-test".to_string()),
        ("session-id", "session-native".to_string()),
        ("thread-id", "thread-native".to_string()),
        ("x-codex-window-id", "window-native".to_string()),
        ("x-openai-memgen-request", "true".to_string()),
        (
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_string(),
        ),
    ] {
        assert_eq!(sent.header(name), want, "upstream {name}");
    }
    for name in [
        "cookie",
        "x-codex-pool-session",
        "x-codex-thread-id",
        "x-openai-fedramp",
    ] {
        assert_eq!(sent.header(name), "", "{name} must not reach upstream");
    }
}

// Go TestDirectCodexGatewayAnswersNonStreamingClientsFromTheStream.
#[tokio::test]
async fn direct_codex_gateway_answers_non_streaming_clients_from_the_stream() {
    let upstream = FakeServer::start(|request| {
        if request.header("chatgpt-account-id") == "acct-first" {
            return sse_response(&[created("resp_busy"), failed("resp_busy", r#"{"code":"server_is_overloaded","message":"busy"}"#, "")]);
        }
        let item = r#"{"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"assembled"}]}"#;
        sse_response(&[
            created("resp_ok"),
            sse("response.output_item.done", &format!(r#"{{"type":"response.output_item.done","output_index":0,"item":{item}}}"#)),
            sse(
                "response.completed",
                r#"{"type":"response.completed","response":{"id":"resp_ok","object":"response","status":"completed","output":[]}}"#,
            ),
        ])
    })
    .await;
    let (app, _dir) = codex_app(
        &upstream.url,
        &[("first", "acct-first", 100), ("second", "acct-second", 10)],
    );
    let result = serve(
        &app,
        post(
            "/v1/responses",
            r#"{"model":"gpt-test","input":"hi","max_output_tokens":50}"#,
        ),
    )
    .await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    let first_bodies: Vec<String> = upstream
        .requests()
        .iter()
        .filter(|request| request.header("chatgpt-account-id") == "acct-first")
        .map(|request| {
            String::from_utf8_lossy(&zstd::decode_all(request.body.as_slice()).unwrap_or_default())
                .into_owned()
        })
        .collect();
    assert_eq!(first_bodies.len(), 1, "first account attempts");
    for body in &first_bodies {
        assert!(
            body.contains(r#""stream":true"#)
                && body.contains(r#""store":false"#)
                && !body.contains("max_output_tokens"),
            "{body}"
        );
    }
    let reply: serde_json::Value = serde_json::from_slice(&result.body)
        .unwrap_or_else(|error| panic!("one JSON response: {error} {}", result.body_string()));
    assert_eq!(reply["id"], "resp_ok");
    assert!(
        result.body_string().contains("assembled"),
        "the output must be rebuilt from the stream"
    );
}

// Go TestDirectCodexGatewaySendsCompactionToTheCompactEndpoint.
#[tokio::test]
async fn direct_codex_gateway_sends_compaction_to_the_compact_endpoint() {
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_compact","object":"response.compaction","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = codex_app(&upstream.url, &[("device", "acct-up", 100)]);
    const RAW: &str = r#"{"model":"gpt-test","input":[{"type":"message","role":"user","content":"hi"}],"instructions":"","prompt_cache_key":"k"}"#;
    let result = serve(&app, post("/v1/responses/compact", RAW)).await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    let sent = upstream.requests().pop().unwrap_or_default();
    assert_eq!(
        (sent.path.as_str(), sent.body_string().as_str()),
        ("/backend-api/codex/responses/compact", RAW)
    );
}

// Go TestDirectCodexGatewaySendsThroughCodexUpstream. The transport is
// in-process now, so the hop is the request upstream receives: the first
// account's endpoint refuses connections, is retried on that account
// (TRANSPORT_SAME_ACCOUNT_RETRY_LIMIT), and only then fails over.
#[tokio::test]
async fn direct_codex_gateway_sends_through_codex_upstream() {
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_via_transport","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = codex_app(
        &upstream.url,
        &[("first", "acct-first", 100), ("second", "acct-second", 10)],
    );
    app.write().config.accounts_mut()[0].upstream_base_url =
        "http://127.0.0.1:1/backend-api/codex".into();
    let (app, logged) = {
        let app = Arc::try_unwrap(app).unwrap_or_else(|_| panic!("shared app"));
        let (app, logged) = super::testing::capture_logs(app);
        (Arc::new(app), logged)
    };
    let second = app
        .read()
        .codex_auth(&account(&app, 1))
        .unwrap_or_else(|error| panic!("{error:?}"));
    let request = post(
        "/v1/responses",
        r#"{"model":"gpt-test","stream":true,"store":false,"input":[]}"#,
    )
    .with_header(
        "user-agent",
        "codex-tui/0.163.0 (Mac OS 26.0.0; arm64) iTerm.app/3.6.6 (codex-tui; 0.163.0)",
    );
    let result = serve(&app, request).await;
    assert!(
        result.code() == 200 && result.body_string().contains("resp_via_transport"),
        "{}",
        result.body_string()
    );
    let retries = logged
        .lock()
        .map(|lines| {
            lines
                .iter()
                .filter(|line| line.contains("upstream unreachable for first"))
                .count()
        })
        .unwrap_or_default();
    assert_eq!(
        retries,
        usize::try_from(super::tuning::TRANSPORT_SAME_ACCOUNT_RETRY_LIMIT).unwrap_or_default(),
        "retries must stay on the first account"
    );
    let sent = upstream.requests();
    assert_eq!(sent.len(), 1);
    let hop = &sent[0];
    assert_eq!(hop.path, "/backend-api/codex/responses");
    assert_eq!(hop.header("content-encoding"), "zstd");
    assert_eq!(hop.header("version"), "0.163.0");
    assert_eq!(hop.header("x-codex-routing-hint"), "model=gpt-test");
    assert_eq!(
        hop.header("authorization"),
        format!("Bearer {}", second.access_token)
    );
}

// Go TestCodexBackendStreamWithoutContentTypeIsStillAStream.
#[tokio::test]
async fn codex_backend_stream_without_content_type_is_still_a_stream() {
    let upstream = FakeServer::start(|request| {
        let blocks = if request.header("chatgpt-account-id") == "acct-first" {
            vec![created("resp_busy"), overloaded("resp_busy")]
        } else {
            vec![
                created("resp_ok"),
                delta("OK"),
                completed("resp_ok", r#"{"input_tokens":50,"input_tokens_details":{"cached_tokens":40},"output_tokens":2}"#),
            ]
        };
        FakeResponse {
            headers: Vec::new(),
            ..sse_response(&blocks)
        }
    })
    .await;
    let (app, _dir) = codex_app(
        &upstream.url,
        &[("first", "acct-first", 100), ("second", "acct-second", 10)],
    );
    let body = serve(
        &app,
        post(
            "/v1/responses",
            r#"{"model":"gpt-test","stream":true,"store":false,"input":[]}"#,
        ),
    )
    .await
    .body_string();
    assert!(
        !body.contains("server_is_overloaded") && body.contains("resp_ok"),
        "the overload before output must be retried on the next account: {body}"
    );
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert_eq!(
        (
            events[0].terminal_event.as_str(),
            events[0].cached_tokens,
            events[0].account_id.as_str()
        ),
        ("response.completed", 40, "second")
    );
}

// Go TestPoolReencodesTheBodyOnlyWhenItChangesTheRequest.
#[tokio::test]
async fn pool_reencodes_the_body_only_when_it_changes_the_request() {
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_changed","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = arc_app(vec![provider(
        "provider",
        100,
        &format!("{}/v1", upstream.url),
        "upstream-key",
    )]);
    const RAW: &str = r#"{"model":"gpt-test(high)","input":"hi","prompt_cache_key":"k"}"#;
    let result = serve(&app, post("/v1/responses", RAW)).await;
    assert_eq!(result.code(), 200);
    let body = upstream.requests().pop().unwrap_or_default().body_string();
    assert!(
        body != RAW
            && body.contains(r#""effort":"high""#)
            && body.contains(r#""model":"gpt-test""#),
        "{body}"
    );
}

// Go TestDirectCodexGatewayRefreshesOnceOnUnauthorizedAndRetriesTheSameAccount.
#[tokio::test]
async fn direct_codex_gateway_refreshes_once_on_unauthorized_and_retries_the_same_account() {
    let fresh = super::testing::fake_jwt_claims(
        &serde_json::json!({"exp": Timestamp::now().add(hours(1)).unix(), "n": 2}),
    );
    let refresh_reply =
        format!(r#"{{"access_token":"{fresh}","refresh_token":"<rotated-refresh>"}}"#);
    let refresh = FakeServer::start(move |_| FakeResponse::json(200, &refresh_reply)).await;
    let expected = format!("Bearer {fresh}");
    let upstream = FakeServer::start(move |request| {
        if request.header("chatgpt-account-id") != "acct-second"
            && request.header("authorization") != expected
        {
            return FakeResponse::json(401, r#"{"error":{"code":"token_expired"}}"#);
        }
        FakeResponse::json(
            200,
            r#"{"id":"resp_refreshed","object":"response","output":[]}"#,
        )
    })
    .await;
    let accounts = vec![
        device_account("first", "acct-first", 100),
        device_account("second", "acct-second", 10),
    ];
    let (base, refresh_url) = (format!("{}/backend-api", upstream.url), refresh.url.clone());
    let (app, _dir) = super::testing::test_app_with(accounts, |settings| {
        settings.codex_base_url = base;
        settings.refresh_url = refresh_url;
    });
    write_codex_device_auth(&app, "first", "acct-first", "first@example.test");
    write_codex_device_auth(&app, "second", "acct-second", "second@example.test");
    let app = Arc::new(app);
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hi"}"#),
    )
    .await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    let by_account = |id: &str| {
        upstream
            .requests()
            .iter()
            .filter(|request| request.header("chatgpt-account-id") == id)
            .count()
    };
    assert_eq!(
        (
            refresh.count(),
            by_account("acct-first"),
            by_account("acct-second")
        ),
        (1, 2, 0)
    );
    let file = app
        .read()
        .read_codex_auth_file(&account(&app, 0))
        .unwrap_or_else(|error| panic!("{error:?}"));
    let tokens = file.tokens.unwrap_or_default();
    assert!(
        tokens.access_token == fresh && tokens.refresh_token == "<rotated-refresh>",
        "the refreshed credential must be persisted for the next request"
    );
}

// Go TestOverloadRetriesTheSameAccountBeforeMovingTheLineage.
#[tokio::test]
async fn overload_retries_the_same_account_before_moving_the_lineage() {
    // server_error is the server's fault too; a native Codex retries it.
    for code in ["server_is_overloaded", "server_error"] {
        same_account_retry_before_moving(code).await;
    }
}

async fn same_account_retry_before_moving(code: &'static str) {
    use super::tuning::OVERLOAD_SAME_ACCOUNT_RETRY_LIMIT;
    let run = |failures: usize| async move {
        let first_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = first_hits.clone();
        let upstream = FakeServer::start(move |request| {
            let mut blocks = vec![created("resp_x")];
            if request.header("chatgpt-account-id") == "acct-first"
                && counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < failures
            {
                blocks.push(failed("resp_x", &format!(r#"{{"code":"{code}"}}"#), ""));
            } else {
                blocks.push(delta("ok"));
                blocks.push(completed("resp_x", ""));
            }
            sse_response(&blocks)
        })
        .await;
        let (app, dir) = codex_app(
            &upstream.url,
            &[("first", "acct-first", 100), ("second", "acct-second", 10)],
        );
        let request = post(
            "/v1/responses",
            r#"{"model":"gpt-test","stream":true,"store":false,"input":[]}"#,
        )
        .with_header("x-codex-turn-metadata", r#"{"thread_id":"thread-main"}"#);
        let body = serve(&app, request).await.body_string();
        assert!(
            !body.contains(code) && body.contains("response.completed"),
            "the {code} must be retried before output: {body}"
        );
        let second_hits = upstream
            .requests()
            .iter()
            .filter(|request| request.header("chatgpt-account-id") == "acct-second")
            .count();
        (
            first_hits.load(std::sync::atomic::Ordering::SeqCst),
            second_hits,
            app,
            dir,
        )
    };
    let lineage = |app: &App| {
        app.read()
            .lineage_account_id("thread-main", Timestamp::now())
            .unwrap_or_default()
    };

    let (first, second, app, _dir) = run(2).await;
    assert_eq!(
        (first, second),
        (3, 0),
        "a passing overload must be retried on the same account"
    );
    assert_eq!(
        active_cooldown_count(&app, "first"),
        0,
        "an overload inside the same-account schedule must not cool the account"
    );
    assert_eq!(
        lineage(&app),
        "first",
        "the lineage must stay on its account"
    );

    let limit = usize::try_from(OVERLOAD_SAME_ACCOUNT_RETRY_LIMIT).unwrap_or_default();
    let (first, second, app, _dir) = run(limit + 1).await;
    assert_eq!(
        (first, second),
        (limit + 1, 1),
        "only an overload that outlasts the schedule moves on"
    );
    assert!(
        active_cooldown_count(&app, "first") > 0,
        "the account that kept refusing must cool down"
    );
    assert_eq!(
        lineage(&app),
        "second",
        "the whole lineage moves with the request"
    );
}

// ---- what of the client's request reaches upstream ----

// Go TestResponsesProxyAddsCodexMetadataHeaders.
#[tokio::test]
async fn responses_proxy_adds_codex_metadata_headers() {
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(200, r#"{"id":"resp_test","object":"response","output":[]}"#)
    })
    .await;
    let account = Account {
        upstream_base_url: format!("{}/backend-api", upstream.url),
        ..device_account("codex-meta", "acct-from-metadata", 100)
    };
    let (app, _dir) = arc_app(vec![account]);
    let home = app.read().account_codex_home("codex-meta");
    std::fs::create_dir_all(&home).unwrap_or_else(|error| panic!("{error}"));
    let id_token = super::testing::fake_jwt_claims(
        &serde_json::json!({"https://api.openai.com/auth": {"chatgpt_account_is_fedramp": true}}),
    );
    let auth = serde_json::json!({"auth_mode": "chatgpt", "tokens": {"id_token": id_token, "access_token": "<access-token>", "refresh_token": "<refresh-token>"}});
    std::fs::write(home.join("auth.json"), auth.to_string())
        .unwrap_or_else(|error| panic!("{error}"));
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#),
    )
    .await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    let sent = upstream.requests().pop().unwrap_or_default();
    assert_eq!(sent.header("authorization"), "Bearer <access-token>");
    assert_eq!(
        sent.header("chatgpt-account-id"),
        "acct-from-metadata",
        "missing account id from account metadata"
    );
    assert_eq!(
        sent.header("x-openai-fedramp"),
        "true",
        "missing fedramp header"
    );
}

// Go TestResponsesProxyForwardsOnlyCodexMetadataHeaderAllowlist.
#[tokio::test]
async fn responses_proxy_forwards_only_codex_metadata_header_allowlist() {
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_headers","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = arc_app(vec![provider(
        "provider",
        100,
        &format!("{}/v1", upstream.url),
        "upstream-key",
    )]);
    let relayed = [
        ("x-codex-parent-thread-id", "parent-thread"),
        ("x-openai-subagent", "collab_spawn"),
        ("x-codex-turn-metadata", r#"{"thread_id":"child-thread"}"#),
        ("x-codex-window-id", "window-a"),
        ("x-codex-installation-id", "installation-a"),
        ("session-id", "session-a"),
        ("session_id", "legacy-session-a"),
        ("thread-id", "child-thread"),
        ("x-client-request-id", "request-a"),
        ("originator", "codex_exec"),
        ("user-agent", "codex_exec/0.141.0 (Debian 12.0.0; x86_64)"),
        ("x-codex-beta-features", "remote_compaction_v2"),
    ];
    let mut request = post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#)
        .with_header("cookie", "client-cookie")
        .with_header("x-unrelated-client-header", "do-not-forward")
        .with_header("x-codex-thread-id", "child-thread");
    for (name, value) in relayed {
        request = request.with_header(name, value);
    }
    let result = serve(&app, request).await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    let sent = upstream.requests().pop().unwrap_or_default();
    for (name, want) in relayed {
        assert_eq!(sent.header(name), want, "{name}");
    }
    assert_eq!(
        sent.header("authorization"),
        "Bearer upstream-key",
        "client authorization leaked or upstream auth missing"
    );
    for forbidden in [
        "cookie",
        "x-unrelated-client-header",
        "x-codex-thread-id",
        "x-codex-routing-hint",
    ] {
        assert_eq!(
            sent.header(forbidden),
            "",
            "non-allowlisted header {forbidden} leaked upstream"
        );
    }
}

// Go TestClientFingerprintIsRecordedLocallyAndNeverSentUpstream.
#[tokio::test]
async fn client_fingerprint_is_recorded_locally_and_never_sent_upstream() {
    const USER_AGENT: &str = "codex_exec/0.141.0 (Debian 12.0.0; x86_64) xterm";
    const TURN_METADATA: &str = r#"{"thread_id":"thread-a","request_kind":"memory consolidation"}"#;
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_fingerprint","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = arc_app(vec![provider(
        "provider",
        100,
        &format!("{}/v1", upstream.url),
        "upstream-key",
    )]);
    let request = post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#)
        .with_header("originator", "codex_exec")
        .with_header("user-agent", USER_AGENT)
        .with_header("x-codex-turn-metadata", TURN_METADATA);
    let result = serve(&app, request).await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    // Upstream must see the client's own values verbatim, not the shortened
    // telemetry labels, and no diagnostic field may reach the body.
    let sent = upstream.requests().pop().unwrap_or_default();
    assert_eq!(sent.header("user-agent"), USER_AGENT);
    assert_eq!(sent.header("x-codex-turn-metadata"), TURN_METADATA);
    let body = sent.body_string();
    for leaked in [
        "clientOriginator",
        "clientUserAgent",
        "clientRequestKind",
        "memory_consolidation",
    ] {
        assert!(
            !body.contains(leaked),
            "diagnostic {leaked:?} leaked into upstream body: {body}"
        );
        for (name, value) in &sent.headers {
            assert!(
                !format!("{name}{}", value.to_str().unwrap_or_default()).contains(leaked),
                "diagnostic {leaked:?} leaked into upstream header {name}"
            );
        }
    }
    let events = events(&app);
    assert_eq!(events.len(), 1);
    assert_eq!(
        (
            events[0].client_originator.as_str(),
            events[0].client_user_agent.as_str(),
            events[0].client_request_kind.as_str()
        ),
        ("codex_exec", "codex_exec/0.141.0", "memory_consolidation")
    );
}

// Go TestTurnStateIsRelayedOnlyToTheAccountThatIssuedIt.
#[tokio::test]
async fn turn_state_is_relayed_only_to_the_account_that_issued_it() {
    let first = FakeServer::start(|_| status_response(429, &[("retry-after", "60")])).await;
    let second = FakeServer::start(|_| {
        let mut response = FakeResponse::json(
            200,
            r#"{"id":"resp_turn_state","object":"response","output":[]}"#,
        );
        for (name, value) in [
            ("x-codex-turn-state", "ts-second"),
            ("openai-model", "gpt-test"),
            ("x-reasoning-included", "true"),
            ("x-models-etag", "upstream-etag"),
            ("x-codex-primary-used-percent", "40"),
        ] {
            response.headers.push((name.into(), value.into()));
        }
        response
    })
    .await;
    let (app, _dir) = arc_app(vec![
        Account {
            auth_type: "provider_api_key".into(),
            ..provider("first", 100, &first.url, "first-key")
        },
        Account {
            auth_type: "provider_api_key".into(),
            ..provider("second", 10, &second.url, "second-key")
        },
    ]);
    set_sticky(&app, "gpt-test:thread:thread-ts", "first", Timestamp::now());
    let send = |turn_state: &'static str| {
        let app = app.clone();
        async move {
            let request = post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#)
                .with_header("x-codex-turn-metadata", r#"{"thread_id":"thread-ts"}"#)
                .with_header("x-codex-turn-state", turn_state);
            let result = serve(&app, request).await;
            assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
            result
        }
    };
    let saw = |server: &FakeServer| {
        server
            .requests()
            .iter()
            .map(|request| request.header("x-codex-turn-state").to_string())
            .collect::<Vec<_>>()
    };
    let last_turn_state = || {
        events(&app)
            .last()
            .map(|event| event.client_turn_state.clone())
            .unwrap_or_default()
    };

    let result = send("ts-first").await;
    assert_eq!(
        saw(&first),
        ["ts-first"],
        "issuing account must get its turn state"
    );
    assert_eq!(
        saw(&second),
        [""],
        "failover account must not get another account's turn state"
    );
    assert_eq!(last_turn_state(), "withheld");
    for (name, want) in [
        ("x-codex-turn-state", "ts-second"),
        ("openai-model", "gpt-test"),
        ("x-reasoning-included", "true"),
    ] {
        assert_eq!(result.header(name), want, "response {name}");
    }
    for name in ["x-models-etag", "x-codex-primary-used-percent"] {
        assert_eq!(
            result.header(name),
            "",
            "response header {name} must not be relayed"
        );
    }
    // The success moved the thread to the second account, so the turn state
    // it issued now goes back to it.
    send("ts-second").await;
    assert_eq!(
        saw(&second),
        ["", "ts-second"],
        "issuing account must get its own turn state back"
    );
    assert_eq!(last_turn_state(), "relayed");
}

// Go TestTurnStateWithoutKnownIssuerIsWithheldWithoutTouchingTheInboundRequest.
#[test]
fn turn_state_without_known_issuer_is_withheld_without_touching_the_inbound_request() {
    use super::proxy::turn_state_for_attempt;
    let mut inbound = http::HeaderMap::new();
    inbound.insert("x-codex-turn-state", http::HeaderValue::from_static("ts"));
    let (forwarded, disposition) = turn_state_for_attempt(&inbound, "", "any");
    assert_eq!(disposition, "withheld");
    assert!(forwarded.get("x-codex-turn-state").is_none());
    assert_eq!(
        inbound
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok()),
        Some("ts"),
        "withholding must not mutate the inbound headers"
    );
    assert_eq!(
        turn_state_for_attempt(&http::HeaderMap::new(), "a", "a").1,
        ""
    );
}

// Go TestResponsesProxyDropsFunctionToolsConflictingWithHostedTools.
#[tokio::test]
async fn responses_proxy_drops_function_tools_conflicting_with_hosted_tools() {
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_tools","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = arc_app(vec![provider(
        "one",
        100,
        &format!("{}/v1", upstream.url),
        "upstream-key",
    )]);
    let request = r#"{"model":"gpt-test","input":"hello","tools":[{"type":"image_generation"},{"type":"function","name":"image_gen.imagegen","parameters":{}},{"type":"function","name":"shell","parameters":{}}]}"#;
    let result = serve(&app, post("/v1/responses", request)).await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    let tools = upstream.requests().pop().unwrap_or_default().json()["tools"].clone();
    let tools = tools.as_array().cloned().unwrap_or_default();
    assert_eq!(
        tools.len(),
        2,
        "expected hosted tool plus unrelated function: {tools:?}"
    );
    assert_eq!(
        tools[0]["type"], "image_generation",
        "hosted tool was not preserved"
    );
    assert_eq!(
        tools[1]["name"], "shell",
        "unrelated function tool was not preserved"
    );
}

// Go TestResponsesProxyInjectsPromptCacheControlsAndTracksUsage.
#[tokio::test]
async fn responses_proxy_injects_prompt_cache_controls_and_tracks_usage() {
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_cache","object":"response","output":[],"usage":{"input_tokens":2006,"input_tokens_details":{"cached_tokens":1920}}}"#,
        )
    })
    .await;
    let accounts = vec![provider(
        "one",
        100,
        &format!("{}/v1", upstream.url),
        "upstream-key",
    )];
    let (app, _dir) = super::testing::test_app_with(accounts, |settings| {
        settings.prompt_cache_retention = "24h".into()
    });
    let app = Arc::new(app);
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#)
            .with_header("x-codex-pool-project", "repo-main"),
    )
    .await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    let body = upstream.requests().pop().unwrap_or_default().json();
    let key = body["prompt_cache_key"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        key.starts_with("cp_"),
        "prompt_cache_key was not auto-generated: {}",
        body["prompt_cache_key"]
    );
    assert_eq!(body["prompt_cache_retention"], "24h");
    assert_ne!(
        key, "repo-main",
        "raw project id was sent as prompt_cache_key"
    );
    let inner = app.read();
    let binding = inner
        .state
        .response_bindings()
        .get("resp_cache")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (binding.account_id.as_str(), binding.sticky_key.as_str()),
        ("one", "gpt-test:repo-main")
    );
    let stat = inner
        .state
        .prompt_cache()
        .get("one:gpt-test:main")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (stat.request_count, stat.input_tokens, stat.cached_tokens),
        (1, 2006, 1920)
    );
}

// ---- chat completions, routing events and device-auth failover ----

fn write_auth(app: &App, id: &str, auth: &str) {
    let home = app.read().account_codex_home(id);
    std::fs::create_dir_all(&home).unwrap_or_else(|error| panic!("{error}"));
    std::fs::write(home.join("auth.json"), auth).unwrap_or_else(|error| panic!("{error}"));
}

fn device_at(id: &str, upstream_account: &str, priority: i64, url: &str) -> Account {
    Account {
        upstream_base_url: url.to_string(),
        ..device_account(id, upstream_account, priority)
    }
}

// Go TestChatCompletionConversionPreservesPromptCacheControls.
#[test]
fn chat_completion_conversion_preserves_prompt_cache_controls() {
    let (app, _dir) = test_app(Vec::new());
    let candidate = Account {
        id: "one".into(),
        upstream_base_url: "https://upstream.example.test/v1".into(),
        wire_api: "responses".into(),
        ..Account::default()
    };
    let body = bytes::Bytes::from_static(
        br#"{"model":"gpt-test","messages":[{"role":"system","content":"static"},{"role":"user","content":"dynamic"}],"prompt_cache_key":"cache-a","prompt_cache_retention":"24h","stream":true}"#,
    );
    let (endpoint, outbound, convert) = app
        .prepare_upstream_request(&candidate, &body, true)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        endpoint == "https://upstream.example.test/v1/responses" && convert,
        "endpoint={endpoint} convert={convert}"
    );
    let converted: serde_json::Value = serde_json::from_slice(&outbound).unwrap_or_default();
    assert_eq!(converted["prompt_cache_key"], "cache-a");
    assert_eq!(converted["prompt_cache_retention"], "24h");
    assert!(
        converted["input"].is_array(),
        "messages were not converted to input: {}",
        converted["input"]
    );
}

// Go TestStickyFallbackUsesChatMessagesAndAPIKey.
#[test]
fn sticky_fallback_uses_chat_messages_and_api_key() {
    let (app, _dir) = test_app(Vec::new());
    let headers = http::HeaderMap::new();
    let facts = super::routing::RequestFacts {
        headers: &headers,
        native: false,
        websocket: false,
    };
    let payload = |text: &str| {
        serde_json::json!({"messages": [{"role": "system", "content": "static"}, {"role": "user", "content": text}]})
            .as_object()
            .cloned()
            .unwrap_or_default()
    };
    let key_a = app
        .routing_decision(&facts, &payload("one"), "gpt-test", "client-key")
        .sticky_key;
    let key_b = app
        .routing_decision(&facts, &payload("two"), "gpt-test", "client-key")
        .sticky_key;
    let key_other = app
        .routing_decision(&facts, &payload("one"), "gpt-test", "other-client-key")
        .sticky_key;
    assert_ne!(
        key_a, key_b,
        "different chat messages produced same sticky fallback key"
    );
    assert_ne!(
        key_a, key_other,
        "different API keys produced same sticky fallback key"
    );
    assert!(
        !key_a.contains("client-key") && !key_a.contains("static") && !key_a.contains("one"),
        "sticky fallback key leaked raw request data: {key_a}"
    );
}

// Go TestRoutingCacheEventsCorrelateStickyReuseAndFailover.
#[tokio::test]
async fn routing_cache_events_correlate_sticky_reuse_and_failover() {
    let first = FakeServer::start(counting(|hits| {
        if hits == 3 {
            return status_response(429, &[("retry-after", "1")]);
        }
        let cached = (hits - 1) * 1500;
        let written = 400 - (hits - 1) * 300;
        FakeResponse::json(
            200,
            &format!(
                r#"{{"id":"resp_primary_{hits}","object":"response","output":[],"usage":{{"input_tokens":2000,"input_tokens_details":{{"cached_tokens":{cached},"cache_write_tokens":{written}}}}}}}"#
            ),
        )
    }))
    .await;
    let second = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_secondary","object":"response","output":[],"usage":{"input_tokens":2400,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":1800}}}"#,
        )
    })
    .await;
    let (app, _dir) = arc_app(vec![
        Account {
            label: "Primary".into(),
            ..provider("private-primary-id", 100, &first.url, "first-key")
        },
        Account {
            label: "Secondary".into(),
            ..provider("private-secondary-id", 10, &second.url, "second-key")
        },
    ]);
    for _ in 0..3 {
        let result = serve(
            &app,
            responses(
                r#"{"model":"gpt-test","input":"hello"}"#,
                "raw-sticky-session",
            ),
        )
        .await;
        assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    }
    let events = events(&app);
    let outcomes: Vec<&str> = events
        .iter()
        .map(|event| event.routing_outcome.as_str())
        .collect();
    assert_eq!(
        outcomes,
        [
            "new_route_assignment",
            "sticky_reuse",
            "rate_limit_failover"
        ]
    );
    let failover = &events[2];
    assert!(
        failover.account_id == "private-secondary-id"
            && failover.failover_from_account_id == "private-primary-id"
            && failover.cache_write_tokens == Some(1800)
            && failover.cold_cache_eligible,
        "failover cache correlation = {failover:?}"
    );
    let views = serde_json::to_string(&app.read().routing_cache_event_views(Timestamp::now()))
        .unwrap_or_default();
    for leaked in [
        "private-primary-id",
        "private-secondary-id",
        "raw-sticky-session",
    ] {
        assert!(
            !views.contains(leaked),
            "routing cache browser view leaked identifiers: {views}"
        );
    }
    for label in ["Primary", "Secondary"] {
        assert!(
            views.contains(label),
            "routing cache browser view omitted {label}: {views}"
        );
    }
}

// Go TestDeviceAuthFailoverAfterRateLimit.
#[tokio::test]
async fn device_auth_failover_after_rate_limit() {
    let first = FakeServer::start(|_| status_response(429, &[("retry-after", "1")])).await;
    let second = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_device_failover","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = arc_app(vec![
        device_at("device-first", "acct-first", 100, &first.url),
        device_at("device-second", "acct-second", 10, &second.url),
    ]);
    write_auth(
        &app,
        "device-first",
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"<first-access-token>"}}"#,
    );
    write_auth(
        &app,
        "device-second",
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"<second-access-token>"}}"#,
    );
    let result = serve(
        &app,
        responses(r#"{"model":"gpt-test","input":"hello"}"#, "device-failover"),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "device-auth failover: {}",
        result.body_string()
    );
    assert_eq!((first.count(), second.count()), (1, 1));
    let (sent_first, sent_second) = (
        first.requests().pop().unwrap_or_default(),
        second.requests().pop().unwrap_or_default(),
    );
    assert_eq!(
        (
            sent_first.header("authorization"),
            sent_first.header("chatgpt-account-id")
        ),
        ("Bearer <first-access-token>", "acct-first")
    );
    assert_eq!(
        (
            sent_second.header("authorization"),
            sent_second.header("chatgpt-account-id")
        ),
        ("Bearer <second-access-token>", "acct-second")
    );
    assert_eq!(
        sticky(&app, "gpt-test:device-failover").account_id,
        "device-second"
    );
    assert_eq!(
        health(&app, "device-first").last_failure_reason,
        "rate_limited"
    );
}

// Go TestDeviceAuthFailoverAfterRefreshTokenRevoked.
#[tokio::test]
async fn device_auth_failover_after_refresh_token_revoked() {
    let refresh = FakeServer::start(|_| FakeResponse::status(400)).await;
    let first = FakeServer::start(|_| FakeResponse::status(500)).await;
    let second = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_refresh_failover","object":"response","output":[]}"#,
        )
    })
    .await;
    let accounts = vec![
        device_at("device-first", "", 100, &first.url),
        device_at("device-second", "", 10, &second.url),
    ];
    let refresh_url = refresh.url.clone();
    let (app, _dir) =
        super::testing::test_app_with(accounts, |settings| settings.refresh_url = refresh_url);
    let app = Arc::new(app);
    let expired = super::testing::fake_jwt(Timestamp::now().add(-minutes(1)));
    write_auth(
        &app,
        "device-first",
        &format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{expired}","refresh_token":"<revoked-refresh-token>"}}}}"#
        ),
    );
    write_auth(
        &app,
        "device-second",
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"<second-access-token>"}}"#,
    );
    let result = serve(
        &app,
        responses(
            r#"{"model":"gpt-test","input":"hello"}"#,
            "refresh-auth-failover",
        ),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "refresh auth failover: {}",
        result.body_string()
    );
    assert_eq!((first.count(), second.count()), (0, 1));
    assert_eq!(
        second
            .requests()
            .pop()
            .unwrap_or_default()
            .header("authorization"),
        "Bearer <second-access-token>"
    );
    assert_eq!(
        sticky(&app, "gpt-test:refresh-auth-failover").account_id,
        "device-second"
    );
    let snapshot = app
        .read()
        .state
        .quotas()
        .get("device-first")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        snapshot
            .quota_error
            .map(|error| error.code)
            .unwrap_or_default(),
        "account_auth_failed",
        "refresh auth failure did not mark first account unavailable"
    );
}

// Go TestDeviceAuthZeroQuotaAccountIsNotSelected.
#[tokio::test]
async fn device_auth_zero_quota_account_is_not_selected() {
    let empty = FakeServer::start(|_| FakeResponse::status(500)).await;
    let ready = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_device_ready","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = arc_app(vec![
        Account {
            remaining_quota: Some(0),
            ..device_at("device-empty", "", 100, &empty.url)
        },
        device_at("device-ready", "", 10, &ready.url),
    ]);
    write_auth(
        &app,
        "device-ready",
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"<ready-access-token>"}}"#,
    );
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "device-auth zero quota routing: {}",
        result.body_string()
    );
    assert_eq!(
        empty.count(),
        0,
        "zero-quota device-auth account was called"
    );
    assert_eq!(
        ready
            .requests()
            .pop()
            .unwrap_or_default()
            .header("authorization"),
        "Bearer <ready-access-token>"
    );
}

// Go TestTransientQuotaRefreshErrorDoesNotBlockProFailover.
#[tokio::test]
async fn transient_quota_refresh_error_does_not_block_pro_failover() {
    let plus = FakeServer::start(|_| FakeResponse::status(500)).await;
    let pro = FakeServer::start(|_| {
        FakeResponse::json(
            200,
            r#"{"id":"resp_pro_fallback","object":"response","output":[]}"#,
        )
    })
    .await;
    let (app, _dir) = arc_app(vec![
        Account {
            plan_type: "plus".into(),
            ..provider("plus", 100, &plus.url, "plus")
        },
        Account {
            plan_type: "pro".into(),
            plan_limit: "20x".into(),
            ..provider("pro", 100, &pro.url, "pro")
        },
    ]);
    app.write().preserve_pro_quota = true;
    set_quota(&app, "plus", "plus", windows(Some(0), Some(80)));
    app.write().state.quotas_mut().insert(
        "pro".into(),
        QuotaSnapshot {
            account_id: "pro".into(),
            plan_type: "pro".into(),
            quota: Some(windows(Some(98), Some(100))),
            quota_error: Some(super::state::QuotaErrorInfo {
                code: "upstream_status".into(),
                message: "quota API returned status 500".into(),
                timestamp: Timestamp::now(),
                ..super::state::QuotaErrorInfo::default()
            }),
            ..QuotaSnapshot::default()
        },
    );
    let result = serve(
        &app,
        responses(
            r#"{"model":"gpt-test","input":"hello"}"#,
            "plus-to-pro-after-quota-poll-500",
        ),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "transient quota error caused failover response: {}",
        result.body_string()
    );
    assert_eq!((plus.count(), pro.count()), (0, 1));
    assert_eq!(
        sticky(&app, "gpt-test:plus-to-pro-after-quota-poll-500").account_id,
        "pro"
    );
    let item = account(&app, 1);
    let (status, reason) = app.read().account_status(&item, Timestamp::now());
    assert_eq!(
        status, "ready",
        "transient quota error made Pro dashboard unavailable: {reason}"
    );
}

// ---- duplicate upstream identities ----

fn planned_device(
    id: &str,
    upstream_account: &str,
    plan_type: &str,
    priority: i64,
    url: &str,
) -> Account {
    Account {
        plan_type: plan_type.to_string(),
        ..device_at(id, upstream_account, priority, url)
    }
}

fn ok_named(
    id: &'static str,
) -> impl Fn(&super::testing::RecordedRequest) -> FakeResponse + Send + Sync + 'static {
    move |_| {
        FakeResponse::json(
            200,
            &format!(r#"{{"id":"{id}","object":"response","output":[]}}"#),
        )
    }
}

fn quota_error(code: &str) -> QuotaSnapshot {
    QuotaSnapshot {
        quota_error: Some(super::state::QuotaErrorInfo {
            code: code.into(),
            message: "credential unavailable".into(),
            timestamp: Timestamp::now(),
            ..super::state::QuotaErrorInfo::default()
        }),
        ..QuotaSnapshot::default()
    }
}

fn set_snapshot(app: &App, id: &str, plan_type: &str, mut snapshot: QuotaSnapshot) {
    snapshot.account_id = id.to_string();
    snapshot.plan_type = plan_type.to_string();
    app.write()
        .state
        .quotas_mut()
        .insert(id.to_string(), snapshot);
}

fn write_device_auths(app: &App, slots: &[(&str, &str, &str)]) {
    for (id, upstream_account, email) in slots {
        write_codex_device_auth(app, id, upstream_account, email);
    }
}

// Go TestDuplicateUpstreamAccountsAreNotFailoverCapacity: two local slots for
// one upstream ChatGPT account must not become immediate retry targets inside
// the same request. The next request may elect a healthy sibling as the single
// representative.
#[tokio::test]
async fn duplicate_upstream_accounts_are_not_failover_capacity() {
    let primary = FakeServer::start(|_| status_response(429, &[("retry-after", "1")])).await;
    let duplicate = FakeServer::start(|_| FakeResponse::status(200)).await;
    let backup = FakeServer::start(ok_named("resp_duplicate_guard")).await;
    let (app, _dir) = arc_app(vec![
        device_at("slot-primary", "upstream-shared", 100, &primary.url),
        device_at("slot-duplicate", "upstream-shared", 90, &duplicate.url),
        device_at("slot-backup", "upstream-backup", 10, &backup.url),
    ]);
    write_device_auths(
        &app,
        &[
            ("slot-primary", "upstream-shared", "shared@example.test"),
            ("slot-duplicate", "upstream-shared", "shared@example.test"),
            ("slot-backup", "upstream-backup", "backup@example.test"),
        ],
    );
    let result = serve(
        &app,
        responses(
            r#"{"model":"gpt-test","input":"hello"}"#,
            "duplicate-upstream",
        ),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "duplicate upstream guard: {}",
        result.body_string()
    );
    assert_eq!(
        (primary.count(), duplicate.count(), backup.count()),
        (1, 0, 1)
    );
    assert_eq!(
        sticky(&app, "gpt-test:duplicate-upstream").account_id,
        "slot-backup"
    );
    let item = account(&app, 1);
    let (status, reason) = app.read().account_status(&item, Timestamp::now());
    assert_eq!(
        status, "ready",
        "duplicate sibling did not become next-request representative: {reason}"
    );
}

// Go TestWorkspaceMembersAreSeparateRoutingIdentities.
#[tokio::test]
async fn workspace_members_are_separate_routing_identities() {
    let spent = FakeServer::start(|_| status_response(429, &[("retry-after", "1")])).await;
    let member = FakeServer::start(ok_named("resp_member")).await;
    let other = FakeServer::start(ok_named("resp_other")).await;
    let (app, _dir) = arc_app(vec![
        device_at("slot-spent", "workspace-shared", 100, &spent.url),
        device_at("slot-member", "workspace-shared", 90, &member.url),
        device_at("slot-other", "workspace-other", 10, &other.url),
    ]);
    write_device_auths(
        &app,
        &[
            (
                "slot-spent",
                "workspace-shared",
                "first-member@example.test",
            ),
            (
                "slot-member",
                "workspace-shared",
                "second-member@example.test",
            ),
            ("slot-other", "workspace-other", "other@example.test"),
        ],
    );
    {
        let inner = app.read();
        let (spent_item, member_item) = (
            inner.config.accounts()[0].clone(),
            inner.config.accounts()[1].clone(),
        );
        assert_ne!(
            inner.upstream_identity_key(&spent_item),
            inner.upstream_identity_key(&member_item),
            "two members of one workspace share a routing identity"
        );
        let (status, _) = inner.account_status(&member_item, Timestamp::now());
        assert_eq!(
            status, "ready",
            "a second workspace member must be routable, not a standby duplicate"
        );
    }
    let result = serve(
        &app,
        responses(
            r#"{"model":"gpt-test","input":"hello"}"#,
            "workspace-members",
        ),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "workspace member failover: {}",
        result.body_string()
    );
    assert_eq!((spent.count(), member.count(), other.count()), (1, 1, 0));
}

/// Two Team slots of one upstream identity and a Pro backup, Pro preserved.
/// Each test expects only the duplicate to be reached, so the primary's and
/// Pro's answers never matter.
async fn team_pair_and_pro(duplicate_id: &'static str) -> (Arc<App>, TempDir, [FakeServer; 3]) {
    let primary = FakeServer::start(|_| FakeResponse::status(200)).await;
    let duplicate = FakeServer::start(ok_named(duplicate_id)).await;
    let pro = FakeServer::start(|_| FakeResponse::status(200)).await;
    let (app, dir) = arc_app(vec![
        planned_device("team-primary", "upstream-team", "team", 100, &primary.url),
        planned_device(
            "team-duplicate",
            "upstream-team",
            "team",
            90,
            &duplicate.url,
        ),
        planned_device("pro-backup", "upstream-pro", "pro", 100, &pro.url),
    ]);
    app.write().preserve_pro_quota = true;
    write_device_auths(
        &app,
        &[
            ("team-primary", "upstream-team", "team@example.test"),
            ("team-duplicate", "upstream-team", "team@example.test"),
            ("pro-backup", "upstream-pro", "pro@example.test"),
        ],
    );
    (app, dir, [primary, duplicate, pro])
}

fn counts(servers: &[FakeServer; 3]) -> (usize, usize, usize) {
    (servers[0].count(), servers[1].count(), servers[2].count())
}

// Go TestDuplicateUpstreamPositiveQuotaCredentialRepresentsIdentityBeforePro:
// the representative must come from the credential copy with positive quota,
// not from the stale zero-quota slot that happens to sort first.
#[tokio::test]
async fn duplicate_upstream_positive_quota_credential_represents_identity_before_pro() {
    let (app, _dir, servers) = team_pair_and_pro("resp_team_duplicate").await;
    app.write().config.preserve_pro_quota = Some(true);
    set_quota(&app, "team-primary", "team", windows(Some(99), Some(0)));
    set_quota(&app, "team-duplicate", "team", windows(Some(99), Some(61)));
    set_quota(&app, "pro-backup", "pro", windows(Some(97), Some(11)));
    let result = serve(
        &app,
        responses(r#"{"model":"gpt-test","input":"hello"}"#, "team-before-pro"),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "same-identity quota routing: {}",
        result.body_string()
    );
    assert_eq!(counts(&servers), (0, 1, 0));
    assert_eq!(
        sticky(&app, "gpt-test:team-before-pro").account_id,
        "team-duplicate"
    );
}

// Go TestDuplicateUpstreamCoolingRepresentativeUsesHealthyCredentialBeforePro.
#[tokio::test]
async fn duplicate_upstream_cooling_representative_uses_healthy_credential_before_pro() {
    let (app, _dir, servers) = team_pair_and_pro("resp_team_cooldown_duplicate").await;
    app.write().state.cooldowns_mut().insert(
        "team-primary".into(),
        vec![super::state::Cooldown {
            model_id: "gpt-test".into(),
            next_retry_at: Timestamp::now().add(minutes(1)),
            reason: "rate_limited".into(),
            ..super::state::Cooldown::default()
        }],
    );
    set_quota(&app, "team-primary", "team", windows(Some(80), Some(80)));
    set_quota(&app, "team-duplicate", "team", windows(Some(99), Some(61)));
    set_quota(&app, "pro-backup", "pro", windows(Some(97), Some(11)));
    let result = serve(
        &app,
        responses(
            r#"{"model":"gpt-test","input":"hello"}"#,
            "cooling-team-before-pro",
        ),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "cooldown duplicate routing: {}",
        result.body_string()
    );
    assert_eq!(counts(&servers), (0, 1, 0));
    assert_eq!(
        sticky(&app, "gpt-test:cooling-team-before-pro").account_id,
        "team-duplicate"
    );
}

// Go TestDuplicateUpstreamHealthyCredentialCanRepresentIdentity: an auth
// error means the local credential copy is unavailable, not that the shared
// upstream identity should be abandoned for Pro.
#[tokio::test]
async fn duplicate_upstream_healthy_credential_can_represent_identity() {
    let (app, _dir, servers) = team_pair_and_pro("resp_team_credential_copy").await;
    set_snapshot(
        &app,
        "team-primary",
        "team",
        quota_error("token_invalidated"),
    );
    set_quota(&app, "team-duplicate", "team", windows(Some(99), Some(61)));
    set_quota(&app, "pro-backup", "pro", windows(Some(97), Some(11)));
    let result = serve(
        &app,
        responses(
            r#"{"model":"gpt-test","input":"hello"}"#,
            "healthy-copy-before-pro",
        ),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "healthy duplicate credential routing: {}",
        result.body_string()
    );
    assert_eq!(counts(&servers), (0, 1, 0));
    assert_eq!(
        sticky(&app, "gpt-test:healthy-copy-before-pro").account_id,
        "team-duplicate"
    );
}

// Go TestErroredDuplicateManualQuotaDoesNotKeepZeroQuotaRepresentativeEligible:
// a stale manual quota hint on an errored duplicate must not keep a
// zero-quota representative selectable.
#[tokio::test]
async fn errored_duplicate_manual_quota_does_not_keep_zero_quota_representative_eligible() {
    let team = FakeServer::start(|_| FakeResponse::status(429)).await;
    let duplicate = FakeServer::start(|_| FakeResponse::status(429)).await;
    let pro = FakeServer::start(ok_named("resp_pro_backup")).await;
    let mut duplicate_slot = planned_device(
        "team-duplicate",
        "upstream-team",
        "team",
        90,
        &duplicate.url,
    );
    duplicate_slot.remaining_quota = Some(1);
    let (app, _dir) = arc_app(vec![
        planned_device("team-primary", "upstream-team", "team", 100, &team.url),
        duplicate_slot,
        planned_device("pro-backup", "upstream-pro", "pro", 100, &pro.url),
    ]);
    app.write().preserve_pro_quota = true;
    set_quota(&app, "team-primary", "team", windows(Some(0), Some(37)));
    set_snapshot(
        &app,
        "team-duplicate",
        "team",
        quota_error("token_invalidated"),
    );
    set_quota(&app, "pro-backup", "pro", windows(Some(93), Some(99)));
    write_device_auths(
        &app,
        &[
            ("team-primary", "upstream-team", "team@example.test"),
            ("team-duplicate", "upstream-team", "team@example.test"),
            ("pro-backup", "upstream-pro", "pro@example.test"),
        ],
    );
    let result = serve(
        &app,
        responses(
            r#"{"model":"gpt-test","input":"hello"}"#,
            "errored-duplicate-quota-hint",
        ),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "errored duplicate quota hint routing: {}",
        result.body_string()
    );
    assert_eq!((team.count(), duplicate.count(), pro.count()), (0, 0, 1));
    assert_eq!(
        sticky(&app, "gpt-test:errored-duplicate-quota-hint").account_id,
        "pro-backup"
    );
}

// ---- the request body and headers the pool composes ----

// Go TestParseModel.
#[test]
fn parse_model_reads_tier_suffixes() {
    use super::proxy::parse_model;
    for (input, model, tier) in [
        ("gpt-5.4(high)", "gpt-5.4", "high"),
        ("gpt-5.4(none)", "gpt-5.4", "none"),
        ("gpt-5.6-sol(ultra)", "gpt-5.6-sol", "ultra"),
        ("gpt-5.6-sol(max)", "gpt-5.6-sol", "max"),
        ("gpt-5.4(unknown)", "gpt-5.4(unknown)", ""),
        ("gpt-5.4", "gpt-5.4", ""),
    ] {
        assert_eq!(
            parse_model(input),
            (model.to_string(), tier.to_string()),
            "parse_model({input:?})"
        );
    }
}

fn object(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    value.as_object().cloned().unwrap_or_default()
}

// Go TestDropHostedToolConflicts.
#[test]
fn drop_hosted_tool_conflicts_cases() {
    use super::proxy::drop_hosted_tool_conflicts;
    let image = serde_json::json!({"type": "function", "name": "image_gen.imagegen"});
    let shell = serde_json::json!({"type": "function", "name": "shell"});
    // image_gen is implicitly hosted upstream, so the twin must be dropped
    // even when the request declares no hosted tool at all.
    let mut implicit = object(serde_json::json!({"tools": [image, shell]}));
    drop_hosted_tool_conflicts(&mut implicit);
    assert_eq!(
        implicit["tools"].as_array().map(Vec::len),
        Some(1),
        "implicitly reserved image_gen twin was not dropped"
    );
    let mut hosted = object(serde_json::json!({"tools": [
        {"type": "web_search_preview"},
        {"type": "function", "name": "web_search"},
        {"type": "function", "name": "web_search.search"},
        shell,
    ]}));
    drop_hosted_tool_conflicts(&mut hosted);
    let tools = hosted["tools"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        tools.len(),
        2,
        "expected hosted web search plus shell: {tools:?}"
    );
    assert_eq!(tools[1]["name"], "shell", "wrong function tool survived");
    // web_search is only reserved when the hosted tool is present, so a bare
    // function named web_search must pass through untouched.
    let mut bare =
        object(serde_json::json!({"tools": [{"type": "function", "name": "web_search"}]}));
    drop_hosted_tool_conflicts(&mut bare);
    assert_eq!(bare["tools"].as_array().map(Vec::len), Some(1));
    let mut missing = object(serde_json::json!({"input": "hello"}));
    drop_hosted_tool_conflicts(&mut missing);
    assert!(
        !missing.contains_key("tools"),
        "tools must not be created when absent"
    );
}

// Go TestDropHostedToolConflictsFiltersAdditionalToolsItems: Codex 0.144+
// declares tools in an additional_tools input item, and image_gen ships as a
// namespace tool that upstream flattens to image_gen.imagegen.
#[test]
fn drop_hosted_tool_conflicts_filters_additional_tools_items() {
    let mut payload = object(serde_json::json!({"input": [
        {"type": "additional_tools", "role": "developer", "tools": [
            {"type": "custom", "name": "exec"},
            {"type": "namespace", "name": "image_gen", "tools": [{"type": "function", "name": "imagegen"}]},
            {"type": "namespace", "name": "collaboration", "tools": [{"type": "function", "name": "spawn_agent"}]},
            {"type": "function", "name": "image_gen.imagegen"},
        ]},
        {"type": "message", "role": "user", "content": "hello"},
    ]}));
    super::proxy::drop_hosted_tool_conflicts(&mut payload);
    let input = payload["input"].as_array().cloned().unwrap_or_default();
    let tools = input[0]["tools"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        tools.len(),
        2,
        "expected exec and collaboration to survive: {tools:?}"
    );
    assert_eq!(tools[0]["name"], "exec");
    assert_eq!(tools[1]["name"], "collaboration");
    assert_eq!(
        input[1]["type"], "message",
        "non-tool input items must pass through"
    );
}

// Go TestCodexBackendRequestBodyChangesOnlyForeignRequests.
#[test]
fn codex_backend_request_body_changes_only_foreign_requests() {
    use super::proxy::codex_backend_request_body;
    const ENDPOINT: &str = "https://h/backend-api/codex/responses";
    let native = bytes::Bytes::from_static(br#"{"model":"m","stream":true,"store":false,"input":[{"type":"message","role":"developer","content":"x"}],"service_tier":"flex"}"#);
    let (got, aggregate) = codex_backend_request_body(ENDPOINT, &native);
    assert!(
        !aggregate && got == native,
        "a native Codex request must pass byte for byte"
    );
    let foreign = bytes::Bytes::from_static(
        br#"{"model":"m","input":"hello","max_output_tokens":10,"temperature":0.2,"top_p":1,"user":"u","previous_response_id":"r","prompt_cache_retention":"24h"}"#,
    );
    let (got, aggregate) = codex_backend_request_body(ENDPOINT, &foreign);
    assert!(
        aggregate,
        "a non-streaming request must be assembled from the stream"
    );
    let payload: serde_json::Value = serde_json::from_slice(&got).unwrap_or_default();
    assert!(
        payload["stream"] == true && payload["store"] == false,
        "the Codex backend needs stream=true and store=false"
    );
    for field in [
        "max_output_tokens",
        "temperature",
        "top_p",
        "user",
        "previous_response_id",
        "prompt_cache_retention",
    ] {
        assert!(
            payload.get(field).is_none(),
            "unsupported {field} must be removed"
        );
    }
    let text = String::from_utf8_lossy(&got);
    assert!(
        payload["input"].as_array().map(Vec::len) == Some(1) && text.contains(r#""text":"hello""#),
        "string input must become a user message: {text}"
    );
    let system = bytes::Bytes::from_static(br#"{"model":"m","stream":true,"store":false,"input":[{"type":"message","role":"system","content":"rules"}]}"#);
    let (got, aggregate) = codex_backend_request_body(ENDPOINT, &system);
    assert!(
        !aggregate && String::from_utf8_lossy(&got).contains(r#""role":"developer""#),
        "the system role must become developer"
    );
    let compact = bytes::Bytes::from_static(br#"{"model":"m","input":[]}"#);
    let (got, aggregate) =
        codex_backend_request_body("https://h/backend-api/codex/responses/compact", &compact);
    assert!(
        !aggregate && got == compact,
        "compaction must be forwarded as sent"
    );
}

// Go TestCodexClientVersionComesOnlyFromACodexUserAgent.
#[test]
fn codex_client_version_comes_only_from_a_codex_user_agent() {
    use super::credentials::codex_client_version;
    for (agent, want) in [
        (
            "codex_exec/0.163.0 (Alpine Linux 3.21.7; x86_64) unknown (codex_exec; 0.163.0)",
            "0.163.0",
        ),
        ("codex-tui/0.163.0 (Mac OS 26.0.0; arm64)", "0.163.0"),
        ("Python-urllib/3.12", ""),
        ("", ""),
    ] {
        let mut headers = http::HeaderMap::new();
        if let Ok(value) = http::HeaderValue::from_str(agent) {
            headers.insert("user-agent", value);
        }
        assert_eq!(
            codex_client_version(&headers),
            want,
            "version for {agent:?}"
        );
    }
    // Go also tried "codex_exec/0.163.0\r\nX-Injected: 1"; an http::HeaderValue
    // cannot hold a CR or LF at all, so that value cannot reach this function.
    assert!(http::HeaderValue::from_str("codex_exec/0.163.0\r\nX-Injected: 1").is_err());
}

// Go TestRoutingHintFollowsNativeCodexRules.
#[test]
fn routing_hint_follows_native_codex_rules() {
    use super::proxy::codex_routing_hint;
    let headers = |pairs: &[(&'static str, &'static str)]| {
        let mut map = http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, http::HeaderValue::from_static(value));
        }
        map
    };
    for (name, body, inbound, want) in [
        (
            "model only",
            r#"{"model":"gpt-x"}"#,
            headers(&[]),
            "model=gpt-x",
        ),
        (
            "service tier",
            r#"{"model":"gpt-x","service_tier":"priority"}"#,
            headers(&[]),
            "model=gpt-x;tier=priority",
        ),
        (
            "guardian reviewer gets none",
            r#"{"model":"gpt-x"}"#,
            headers(&[("x-codex-guardian", "reviewer")]),
            "",
        ),
        (
            "client hint wins",
            r#"{"model":"gpt-x"}"#,
            headers(&[("x-codex-routing-hint", "model=other")]),
            "",
        ),
        ("no model", r#"{"input":"hi"}"#, headers(&[]), ""),
    ] {
        assert_eq!(
            codex_routing_hint(body.as_bytes(), &inbound),
            want,
            "{name}"
        );
    }
}

// Go TestExtendedReasoningRequiresFamilyBoundary.
#[test]
fn extended_reasoning_requires_family_boundary() {
    use super::catalog::codex_extended_reasoning_model;
    for model in [
        "gpt-6",
        "gpt-6-astra",
        "gpt-6-sol",
        "gpt-6-luna",
        "gpt-6.1",
        "gpt-6.1-sol",
        "gpt-5.6",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
    ] {
        assert!(
            codex_extended_reasoning_model(model),
            "{model:?} is a documented extended-tier family member"
        );
    }
    for model in [
        "gpt-60-legacy",
        "gpt-6preview",
        "gpt-5.61",
        "gpt-5.6x",
        "gpt-5.5",
        "gpt-5.2-codex",
        "gpt-61",
        "gpt-6.10-sol",
        "gpt-6.1x",
    ] {
        assert!(
            !codex_extended_reasoning_model(model),
            "{model:?} must not inherit the extended tiers"
        );
    }
}

// Go TestMissingCodexAuthClassifiesWithoutRetry.
#[test]
fn missing_codex_auth_classifies_without_retry() {
    let (app, _dir) = test_app(vec![device_account("missing-fast", "", 0)]);
    let item = account(&app, 0);
    let start = std::time::Instant::now();
    let result = app.read().codex_auth(&item);
    assert!(
        matches!(result, Err(super::app::AuthError::Missing(_))),
        "missing auth must be classified as missing"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_millis(200),
        "missing auth classification must not wait for retries"
    );
}

// ---- the stream writers: pre-commit hold, early headers, keepalives ----

use super::http_io::ResponseWriter;
use super::stream::EarlyStreamWriter;
use super::stream::SseReader;
use super::stream::copy_streaming_proxy_response_with_precommit;
use super::stream::precommit_byte_limit;
use super::tuning::Tuning;
use std::time::Duration;
use std::time::Instant;

/// Go io.Pipe: an upstream body fed piece by piece.
fn pipe() -> (tokio::sync::mpsc::UnboundedSender<&'static str>, SseReader) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok(bytes::Bytes::from_static(chunk.as_bytes())), rx))
    });
    (tx, SseReader::new(Box::pin(stream)))
}

fn reader_of(text: String) -> SseReader {
    SseReader::new(super::stream::byte_stream_from(text))
}

async fn wait_for(mut done: impl FnMut() -> bool, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while !done() {
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    true
}

fn keepalive_tuning(interval: Duration) -> Tuning {
    Tuning {
        stream_keepalive_interval: interval,
        stream_keepalive_tick: interval / 3,
        ..Tuning::default()
    }
}

// Go TestCopyStreamingProxyResponseFlushesSSE.
#[tokio::test]
async fn copy_streaming_proxy_response_flushes_sse() {
    let mut recorder = Recorder::new();
    let mut reader = reader_of(completed("resp_flushed", ""));
    let info = super::stream::copy_streaming_proxy_response(
        &mut recorder,
        &mut reader,
        &Tuning::default(),
    )
    .await;
    assert!(
        recorder.result().flushed,
        "SSE proxy buffered the response without flushing"
    );
    assert_eq!(info.response_id, "resp_flushed");
}

// Go TestStreamingPrecommitBlockLimitCommitsBeforeCapacityFailure.
#[tokio::test]
async fn streaming_precommit_block_limit_commits_before_capacity_failure() {
    use super::stream::STREAMING_PRECOMMIT_MAX_BLOCKS;
    let mut text: String = (0..=STREAMING_PRECOMMIT_MAX_BLOCKS)
        .map(|index| in_progress(&format!("resp_{index}")))
        .collect();
    text.push_str(&overloaded("resp_failed"));
    let mut recorder = Recorder::new();
    let mut commits = 0;
    let mut before = |writer: &mut dyn ResponseWriter| {
        commits += 1;
        writer.write_header(http::StatusCode::OK);
    };
    let result = copy_streaming_proxy_response_with_precommit(
        &mut recorder,
        &mut reader_of(text),
        precommit_byte_limit(0),
        &Tuning::default(),
        Some(&mut before),
    )
    .await;
    assert!(
        !result.retryable_terminal_failure,
        "capacity failure remained retryable after precommit block limit"
    );
    let recorded = recorder.result();
    assert!(
        commits == 1 && recorded.flushed && recorded.body_string().contains("response.failed"),
        "bounded precommit stream was not committed once"
    );
    assert!(result.info.precommit_committed && result.info.precommit_close_reason == "bounds");
}

// Go TestShellEventsKeepTheRetryWindowOpen.
#[test]
fn shell_events_keep_the_retry_window_open() {
    use super::stream::precommit_lifecycle_sse_block;
    let shell = |kind: &str| format!("event: {kind}\ndata: {{\"type\":\"{kind}\"}}\n\n");
    for kind in [
        "response.created",
        "response.in_progress",
        "response.queued",
        "response.output_item.added",
        "response.content_part.added",
        "response.reasoning_summary_part.added",
    ] {
        assert!(
            precommit_lifecycle_sse_block(&shell(kind)),
            "{kind} must keep the retry window open"
        );
    }
    // Content, and the completion of an item that carries content, still
    // commit: a retry after either would duplicate what the caller saw.
    for kind in [
        "response.output_text.delta",
        "response.reasoning_summary_text.delta",
        "response.function_call_arguments.delta",
        "response.output_item.done",
        "response.content_part.done",
        "response.custom_tool_call_input.delta",
    ] {
        assert!(
            !precommit_lifecycle_sse_block(&shell(kind)),
            "{kind} must close the retry window"
        );
    }
}

// Go TestPrecommitReleasesHeadersWhenUpstreamStaysSilent: a plain writer
// takes the timed commit.
#[tokio::test]
async fn precommit_releases_headers_when_upstream_stays_silent() {
    const PREAMBLE: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_hold\"}}\n\nevent: response.in_progress\ndata: {\"type\":\"response.in_progress\"}\n\nevent: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"reasoning\"}}\n\n";
    const REST: &str = "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_hold\"}}\n\n";
    let tuning = Tuning {
        unmanaged_precommit_commit_after: Duration::from_millis(50),
        ..Tuning::default()
    };
    let (tx, mut reader) = pipe();
    let mut recorder = Recorder::new();
    let committed: Arc<std::sync::Mutex<Option<Instant>>> = Arc::default();
    let start = Instant::now();
    let mark = committed.clone();
    let mut before = move |_: &mut dyn ResponseWriter| {
        if let Ok(mut at) = mark.lock() {
            *at = Some(Instant::now());
        }
    };
    let feeder = async {
        let _ = tx.send(PREAMBLE);
        // Upstream now "reasons" in silence. The headers must go out anyway.
        let released = wait_for(
            || committed.lock().is_ok_and(|at| at.is_some()),
            Duration::from_secs(2),
        )
        .await;
        assert!(
            released,
            "headers were still withheld while upstream was silent"
        );
        let held = committed
            .lock()
            .ok()
            .and_then(|at| *at)
            .map(|at| at - start)
            .unwrap_or_default();
        assert!(
            held >= Duration::from_millis(40) && held <= Duration::from_secs(2),
            "headers released after {held:?}"
        );
        let _ = tx.send(REST);
        drop(tx);
    };
    let copy = copy_streaming_proxy_response_with_precommit(
        &mut recorder,
        &mut reader,
        precommit_byte_limit(0),
        &tuning,
        Some(&mut before),
    );
    let (result, ()) = tokio::join!(copy, feeder);
    assert!(
        !result.retryable_terminal_failure,
        "a released stream must not be reported as retryable"
    );
    assert!(
        result.info.precommit_close_reason == "hold_timeout" && result.info.precommit_hold_ms >= 40,
        "{:?}",
        result.info.precommit_close_reason
    );
    assert_eq!(
        recorder.result().body_string(),
        format!("{PREAMBLE}{REST}"),
        "stream was altered"
    );
}

// Go TestEarlyHeadersKeepRetryWindowOpenForLateCapacityRefusal.
#[tokio::test]
async fn early_headers_keep_retry_window_open_for_late_capacity_refusal() {
    let tuning = Tuning {
        precommit_max_hold: Duration::from_millis(30),
        ..keepalive_tuning(Duration::from_millis(20))
    };
    let client = Recorder::new();
    let mut early =
        EarlyStreamWriter::new(Box::new(client.clone()), tuning.precommit_max_hold, &tuning);
    let (tx, mut reader) = pipe();
    let feeder = async {
        let _ = tx.send("event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_q\"}}\n\nevent: response.in_progress\ndata: {\"type\":\"response.in_progress\"}\n\n");
        assert!(
            wait_for(|| client.result().status.is_some(), Duration::from_secs(2)).await,
            "headers were not sent early while upstream queued the request"
        );
        tokio::time::sleep(Duration::from_millis(80)).await; // let a few keepalives through
        let _ = tx.send("event: error\ndata: {\"type\":\"error\",\"code\":\"server_is_overloaded\"}\n\nevent: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_q\",\"error\":{\"code\":\"server_is_overloaded\"}}}\n\n");
        drop(tx);
    };
    let mut before = |writer: &mut dyn ResponseWriter| writer.write_header(http::StatusCode::OK);
    let copy = copy_streaming_proxy_response_with_precommit(
        &mut early,
        &mut reader,
        precommit_byte_limit(0),
        &tuning,
        Some(&mut before),
    );
    let (result, ()) = tokio::join!(copy, feeder);
    early.finish().await;
    assert!(
        result.retryable_terminal_failure,
        "a late capacity refusal must stay retryable after early headers"
    );
    let recorded = client.result();
    assert!(
        recorded.code() == 200 && recorded.header("content-type") == "text/event-stream",
        "early headers"
    );
    let body = recorded.body_string();
    assert!(
        !body.contains("data:") && body.matches(": keepalive").count() >= 2,
        "only keepalive comments may reach the client before a retry: {body:?}"
    );
}

// Go TestEarlyHeadersConvertLaterErrorResponseIntoStreamFailure.
#[tokio::test]
async fn early_headers_convert_later_error_response_into_stream_failure() {
    let tuning = Tuning::default();
    let client = Recorder::new();
    let mut early =
        EarlyStreamWriter::new(Box::new(client.clone()), Duration::from_millis(1), &tuning);
    assert!(wait_for(|| client.result().status.is_some(), Duration::from_secs(2)).await);
    super::http_io::write_openai_error(
        &mut early,
        http::StatusCode::SERVICE_UNAVAILABLE,
        "all_accounts_cooling_down",
        "every account is cooling down",
    );
    early.finish().await;
    let recorded = client.result();
    let body = recorded.body_string();
    assert_eq!(recorded.code(), 200);
    assert!(
        body.contains("event: response.failed")
            && body.contains(r#""code":"all_accounts_cooling_down""#)
            && body.contains("every account is cooling down"),
        "error was not converted into a stream failure: {body:?}"
    );
    // "invalid_request_error" is only in write_openai_error's raw JSON body.
    assert!(
        !body.contains("invalid_request_error"),
        "raw JSON error body leaked into the stream"
    );
}

// Go TestEarlyWriterIsTransparentWhenResponseStartsInTime.
#[tokio::test]
async fn early_writer_is_transparent_when_response_starts_in_time() {
    let client = Recorder::new();
    let mut early = EarlyStreamWriter::new(
        Box::new(client.clone()),
        Duration::from_secs(3600),
        &Tuning::default(),
    );
    super::http_io::set_header(&mut early, "content-type", "text/event-stream");
    early.write_header(http::StatusCode::OK);
    early.write(b"event: response.created\ndata: {}\n\n");
    early.finish().await;
    assert_eq!(
        client.result().body_string(),
        "event: response.created\ndata: {}\n\n"
    );
    assert!(
        early.headers_sent_early().is_none(),
        "headers were reported early for a prompt response"
    );
}

// Go TestKeepalivesCoverSilenceAfterRealOutput.
#[tokio::test]
async fn keepalives_cover_silence_after_real_output() {
    let tuning = keepalive_tuning(Duration::from_millis(30));
    let client = Recorder::new();
    let mut early =
        EarlyStreamWriter::new(Box::new(client.clone()), Duration::from_secs(3600), &tuning);
    super::http_io::set_header(&mut early, "content-type", "text/event-stream");
    early.write_header(http::StatusCode::OK);
    early.write(b"event: response.created\ndata: {}\n\n");
    tokio::time::sleep(Duration::from_millis(150)).await; // the model "thinks"
    early.write(b"event: response.completed\ndata: {}\n\n");
    early.finish().await;
    let body = client.result().body_string();
    assert!(
        body.starts_with("event: response.created\ndata: {}\n\n: keepalive\n\n"),
        "no keepalive during the silence after real output: {body:?}"
    );
    assert!(
        body.ends_with("event: response.completed\ndata: {}\n\n"),
        "stream was altered: {body:?}"
    );
    for block in body.trim_end_matches("\n\n").split("\n\n") {
        assert!(
            block == ": keepalive" || block.starts_with("event: "),
            "a keepalive landed inside an event: {body:?}"
        );
    }
}

// Go TestKeepalivesNeverEnterANonSSEResponse.
#[tokio::test]
async fn keepalives_never_enter_a_non_sse_response() {
    let tuning = keepalive_tuning(Duration::from_millis(10));
    let client = Recorder::new();
    let mut early =
        EarlyStreamWriter::new(Box::new(client.clone()), Duration::from_secs(3600), &tuning);
    super::http_io::write_openai_error(
        &mut early,
        http::StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "slow down",
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    early.finish().await;
    let recorded = client.result();
    assert!(
        !recorded.body_string().contains("keepalive") && recorded.code() == 429,
        "status {} body {:?}",
        recorded.code(),
        recorded.body_string()
    );
}

// ---- failure classes, cooldown pricing and Codex's verdict ----

// Go TestTerminalFailureClassesAndCooldownPricing.
#[test]
fn terminal_failure_classes_and_cooldown_pricing() {
    use super::outcome::UPSTREAM_5XX_COOLDOWN_SECONDS;
    use super::outcome::retryable_terminal_cooldown;
    use super::response_info::retryable_terminal_failure_class;
    use super::response_info::stream_retry_failover_outcome;
    use super::response_info::terminal_failure_class;
    for (code, want) in [
        ("server_error", "transient"),
        ("internal_server_error", "transient"),
        ("server_is_overloaded", "capacity"),
        ("rate_limit_exceeded", "capacity"),
        ("usage_limit_reached", "capacity"),
        ("invalid_prompt", "request"),
        ("unauthorized", "authentication"),
        ("some_new_condition", "unknown"),
    ] {
        assert_eq!(
            terminal_failure_class("response.failed", code),
            want,
            "terminal_failure_class({code:?})"
        );
    }
    for class in ["capacity", "transient"] {
        assert!(
            retryable_terminal_failure_class(class),
            "{class:?} must be retryable"
        );
    }
    for class in ["unknown", "request", "authentication", "incomplete", ""] {
        assert!(
            !retryable_terminal_failure_class(class),
            "{class:?} must not be retryable"
        );
    }
    // A server fault needs a moment, not a spent allowance, so it is priced
    // like overload rather than getting the short default.
    for reason in [
        "server_error",
        "internal_server_error",
        "server_is_overloaded",
        "slow_down",
    ] {
        assert_eq!(
            retryable_terminal_cooldown(reason),
            seconds(UPSTREAM_5XX_COOLDOWN_SECONDS),
            "{reason}"
        );
    }
    assert_eq!(
        retryable_terminal_cooldown("rate_limit_exceeded"),
        seconds(30)
    );
    // A spent allowance stays out until the quota refresh has seen it.
    let longest_poll =
        chrono::TimeDelta::from_std(super::quota_schedule::MAX_INTERVAL).unwrap_or_default();
    for reason in [
        "usage_limit_reached",
        "usage_not_included",
        "insufficient_quota",
    ] {
        assert_eq!(
            retryable_terminal_cooldown(reason),
            longest_poll,
            "{reason}"
        );
    }
    // The outcome must name the cause an operator has to act on.
    assert_eq!(
        stream_retry_failover_outcome("transient"),
        "stream_transient_failover"
    );
    assert_eq!(
        stream_retry_failover_outcome("capacity"),
        "stream_capacity_failover"
    );
}

fn cooled_only(reasons: &[(&str, chrono::TimeDelta)]) -> impl FnOnce(&App) {
    let cooldowns: Vec<super::state::Cooldown> = reasons
        .iter()
        .map(|(reason, wait)| super::state::Cooldown {
            model_id: "gpt-test".into(),
            next_retry_at: Timestamp::now().add(*wait),
            reason: (*reason).into(),
            ..super::state::Cooldown::default()
        })
        .collect();
    move |app: &App| {
        app.write()
            .state
            .cooldowns_mut()
            .insert("only".into(), cooldowns);
    }
}

async fn only_with_cooldowns(cool: impl FnOnce(&App)) -> (Recorded, usize) {
    let upstream =
        FakeServer::start(|_| FakeResponse::json(200, r#"{"id":"resp_ok","output":[]}"#)).await;
    let (app, _dir) = arc_app(vec![api_slot("only", "", 100, &upstream.url)]);
    cool(&app);
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#),
    )
    .await;
    let hits = upstream.count();
    (result, hits)
}

// Go TestOverloadCooldownIsSoftenedAsLastResort.
#[tokio::test]
async fn overload_cooldown_is_softened_as_last_resort() {
    let (result, hits) =
        only_with_cooldowns(cooled_only(&[("server_is_overloaded", seconds(30))])).await;
    assert!(
        result.code() == 200 && hits == 1,
        "overload cooldown was not softened: {}",
        result.body_string()
    );
    assert!(!result.body_string().contains("all_accounts_cooling_down"));
}

// Go TestRateLimitCooldownIsNotSoftened.
#[tokio::test]
async fn rate_limit_cooldown_is_not_softened() {
    let (result, hits) = only_with_cooldowns(cooled_only(&[("rate_limited", seconds(30))])).await;
    assert!(
        hits == 0 && result.code() != 200,
        "rate-limit cooldown was bypassed"
    );
}

// Go TestMixedCooldownKeepsAccountBlocked.
#[tokio::test]
async fn mixed_cooldown_keeps_account_blocked() {
    let (result, hits) = only_with_cooldowns(cooled_only(&[
        ("server_is_overloaded", seconds(5)),
        ("rate_limited", minutes(10)),
    ]))
    .await;
    assert!(
        hits == 0 && result.code() != 200,
        "mixed cooldown was bypassed"
    );
}

// Go TestCompletedResponseKeepsNoTerminalData.
#[test]
fn completed_response_keeps_no_terminal_data() {
    let info = super::response_info::response_info_from_sse_block(
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_ok\"}}\n\n",
    );
    assert!(
        info.terminal_event == "response.completed" && info.terminal_data.is_empty(),
        "{info:?}"
    );
}

// Go TestHTTPFailureLogCarriesCodexVerdict. Go asked a fake classifier over
// HTTP; the classifier is Codex's own code in-process now, so the verdict in
// the line is Codex's real one for this answer.
#[test]
fn http_failure_log_carries_codex_verdict() {
    let (app, _dir) = test_app(Vec::new());
    let (app, logged) = super::testing::capture_logs(app);
    let mut headers = http::HeaderMap::new();
    headers.insert("retry-after", http::HeaderValue::from_static("5"));
    let body = br#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#;
    app.log_codex_verdict_for_http(
        "acct",
        "gpt-test",
        http::StatusCode::TOO_MANY_REQUESTS,
        &headers,
        body,
        "rate_limited",
    );
    let line = logged
        .lock()
        .map(|lines| lines.join("\n"))
        .unwrap_or_default();
    assert_eq!(
        line,
        "upstream http failure: account=acct model=gpt-test status=429 pool=rate_limited code=usage_limit_reached codex=usage_limit_reached/Transport retry=no"
    );
}

// Go TestTerminalFailureLogCarriesCodexVerdict.
#[test]
fn terminal_failure_log_carries_codex_verdict() {
    use super::response_info::response_info_from_sse_block;
    let (app, _dir) = test_app(Vec::new());
    let (app, logged) = super::testing::capture_logs(app);
    let event = r#"{"type":"response.failed","response":{"id":"resp_1","error":{"code":"server_error","message":"boom"}}}"#;
    let sse_info =
        response_info_from_sse_block(&format!("event: response.failed\ndata: {event}\n\n"));
    assert!(
        sse_info.terminal_data == event && sse_info.terminal_source.is_empty(),
        "SSE terminal data"
    );
    app.log_terminal_response_failure("acct", "gpt-test", &sse_info, false);
    let mut ws_info = response_info_from_sse_block(&format!("data: {event}\n\n"));
    ws_info.terminal_source = "ws".into();
    app.log_terminal_response_failure("acct", "gpt-test", &ws_info, true);
    let lines = logged.lock().map(|lines| lines.clone()).unwrap_or_default();
    assert_eq!(lines.len(), 2);
    for line in &lines {
        assert!(
            line.contains("class=transient") && line.ends_with("codex=stream/Retryable retry=yes"),
            "log line lacks the pool class or Codex's verdict: {line}"
        );
    }
}

// Go TestUnrecognizedUpstreamFailuresAreCounted (the stored counter; Go read
// it through the dashboard window).
#[test]
fn unrecognized_upstream_failures_are_counted() {
    let (app, _dir) = test_app(vec![device_account("first", "", 0)]);
    for payload in [
        r#"{"type":"response.failed","response":{"error":{"code":"brand_new_condition"}}}"#,
        r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","code":"websocket_connection_limit_reached"}}"#,
    ] {
        let info =
            super::response_info::response_info_from_sse_block(&format!("data: {payload}\n\n"));
        app.mark_terminal_response_failure(
            &super::routing::RoutingDecision::default(),
            "gpt-test",
            "first",
            &info,
            None,
            true,
        );
    }
    let total: u64 = app
        .read()
        .state
        .prompt_cache()
        .values()
        .map(|stat| stat.unrecognized_failure_count)
        .sum();
    assert_eq!(total, 1, "the unknown code counts, not the passthrough one");
}

// Go TestCodexUpstreamShapesClassifyAsCodexReadsThem: every recorded Codex
// failure shape is terminal with its class and code, and every lifecycle
// shape stays before the commit point.
#[test]
fn codex_upstream_shapes_classify_as_codex_reads_them() {
    use super::response_info::response_info_from_sse_block;
    use super::stream::precommit_lifecycle_sse_block;
    let raw = include_str!("../../../cmd/codex-pool/testdata/codex-upstream-failures.json");
    let fixture: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();
    let failures = fixture["failures"].as_array().cloned().unwrap_or_default();
    let lifecycle = fixture["lifecycle"].as_array().cloned().unwrap_or_default();
    assert!(
        !failures.is_empty() && !lifecycle.is_empty(),
        "empty shape fixture"
    );
    for shape in &failures {
        let name = shape["name"].as_str().unwrap_or_default();
        assert!(
            !shape["source"].as_str().unwrap_or_default().is_empty()
                && !shape["marker"].as_str().unwrap_or_default().is_empty(),
            "{name}: every shape must name the Codex source it was taken from"
        );
        let info = response_info_from_sse_block(&format!("data: {}\n\n", shape["event"]));
        assert!(
            !info.terminal_event.is_empty(),
            "{name}: a failure Codex treats as terminal was not terminal"
        );
        assert_eq!(
            (
                info.terminal_failure_class.as_str(),
                info.terminal_error_code.as_str()
            ),
            (
                shape["class"].as_str().unwrap_or_default(),
                shape["code"].as_str().unwrap_or_default()
            ),
            "{name}"
        );
    }
    for shape in &lifecycle {
        let name = shape["name"].as_str().unwrap_or_default();
        let block = format!("data: {}\n\n", shape["event"]);
        assert!(
            response_info_from_sse_block(&block)
                .terminal_event
                .is_empty(),
            "lifecycle {name} read as terminal"
        );
        assert!(
            precommit_lifecycle_sse_block(&block),
            "lifecycle {name} must stay before the commit point"
        );
    }
}

// ---- large and agent payloads pass through unchanged ----

// Go TestResponsesProxyPreservesLargeMCPToolPayloadAndStreamingEvents.
#[tokio::test]
async fn responses_proxy_preserves_large_mcp_tool_payload_and_streaming_events() {
    const TOOL_COUNT: usize = 320;
    const LONG_TOOL_NAME: &str = "mcp__apps_runtime__workspace_agents__upsert_agent_application_configuration_with_extended_name";
    let upstream = FakeServer::start(counting(|round| {
        let (first, id, input, cached, written) = if round == 1 {
            (
                sse(
                    "response.output_item.added",
                    &format!(
                        r#"{{"type":"response.output_item.added","item":{{"type":"function_call","call_id":"call_mcp_long_1","name":"{LONG_TOOL_NAME}","arguments":"{{\"enabled\":true}}"}}}}"#
                    ),
                ),
                "resp_mcp_large",
                4096,
                3072,
                512,
            )
        } else {
            (delta("saved"), "resp_mcp_large_2", 5120, 4096, 256)
        };
        sse_response(&[
            first,
            completed(
                id,
                &format!(r#"{{"input_tokens":{input},"input_tokens_details":{{"cached_tokens":{cached},"cache_write_tokens":{written}}}}}"#),
            ),
        ])
    }))
    .await;
    let (app, _dir) = arc_app(vec![provider(
        "provider",
        100,
        &upstream.url,
        "provider-key",
    )]);
    let description = "schema-detail-".repeat(200);
    let tools: Vec<serde_json::Value> = (0..TOOL_COUNT)
        .map(|index| {
            let name = if index == 0 { LONG_TOOL_NAME.to_string() } else { format!("mcp__apps_runtime__tool_{index:03}") };
            serde_json::json!({"type": "function", "function": {"name": name, "parameters": {"type": "object", "properties": {"configuration": {"type": "string", "description": description}}}}})
        })
        .collect();
    let proxy = |payload: serde_json::Value| {
        let app = app.clone();
        async move {
            let result = serve(&app, post("/v1/responses", &payload.to_string())).await;
            assert_eq!(
                result.code(),
                200,
                "large MCP proxy: {}",
                result.body_string()
            );
            result.body_string()
        }
    };
    let first = proxy(
        serde_json::json!({"model": "gpt-test", "input": "hello", "stream": true, "tools": tools}),
    )
    .await;
    for expected in [
        LONG_TOOL_NAME,
        "call_mcp_long_1",
        "response.output_item.added",
        "response.completed",
    ] {
        assert!(
            first.contains(expected),
            "streaming response lost {expected:?}"
        );
    }
    let second = proxy(serde_json::json!({
        "model": "gpt-test",
        "previous_response_id": "resp_mcp_large",
        "input": [{"type": "function_call_output", "call_id": "call_mcp_long_1", "output": r#"{"saved":true}"#}],
        "stream": true,
        "tools": tools,
    }))
    .await;
    for expected in [
        "response.output_text.delta",
        "saved",
        "resp_mcp_large_2",
        "response.completed",
    ] {
        assert!(
            second.contains(expected),
            "second streaming round lost {expected:?}"
        );
    }
    let sent = upstream.requests();
    assert_eq!(sent.len(), 2, "upstream rounds");
    for (round, request) in sent.iter().enumerate() {
        let payload = request.json();
        let tools = payload["tools"].as_array().cloned().unwrap_or_default();
        assert_eq!(tools.len(), TOOL_COUNT, "forwarded tools");
        assert_eq!(
            tools[0]["function"]["name"], LONG_TOOL_NAME,
            "long MCP tool name changed"
        );
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["configuration"]["description"],
            description,
            "large MCP JSON Schema was truncated or changed"
        );
        if round == 1 {
            assert_eq!(payload["previous_response_id"], "resp_mcp_large");
            let input = payload["input"].as_array().cloned().unwrap_or_default();
            assert_eq!(input.len(), 1);
            assert!(
                input[0]["type"] == "function_call_output"
                    && input[0]["call_id"] == "call_mcp_long_1"
                    && input[0]["output"] == r#"{"saved":true}"#,
                "function call output changed: {}",
                input[0]
            );
        }
    }
    let stat = app
        .read()
        .state
        .prompt_cache()
        .get("provider:gpt-test:main")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (
            stat.input_tokens,
            stat.cached_tokens,
            stat.cache_write_tokens,
            stat.cache_write_observed_request_count,
            stat.request_count
        ),
        (9216, 7168, 768, 2, 2),
        "streaming usage was not recorded"
    );
}

// Go TestResponsesProxyPreservesSpawnAgentForkTurnsSchemaArgumentsAndHistory.
#[tokio::test]
async fn responses_proxy_preserves_spawn_agent_fork_turns_schema_arguments_and_history() {
    let spawn_tool = serde_json::json!({
        "type": "function",
        "name": "spawn_agent",
        "description": "Spawn a child agent",
        "parameters": {
            "type": "object",
            "properties": {
                "message": {"type": "string"},
                "task_name": {"type": "string"},
                "fork_turns": {"type": "string", "description": "none, all, or a positive integer string"},
            },
            "required": ["message", "task_name"],
            "additionalProperties": false,
        },
    });
    let child_history = serde_json::json!([
        {"role": "developer", "content": "shared instructions"},
        {"role": "user", "content": "parent turn one"},
        {"role": "assistant", "content": "parent answer one"},
        {"role": "user", "content": "child task"},
    ]);
    for (name, args, stream) in [
        (
            "none streamed",
            r#"{"message":"child","task_name":"child_none","fork_turns":"none"}"#,
            true,
        ),
        (
            "last n nonstreamed",
            r#"{"message":"child","task_name":"child_three","fork_turns":"3"}"#,
            false,
        ),
        (
            "all streamed",
            r#"{"message":"child","task_name":"child_all","fork_turns":"all"}"#,
            true,
        ),
        (
            "omitted nonstreamed",
            r#"{"message":"child","task_name":"child_default"}"#,
            false,
        ),
    ] {
        let upstream = FakeServer::start(move |_| {
            let item = serde_json::json!({"type": "function_call", "call_id": "call_spawn", "name": "spawn_agent", "arguments": args});
            if stream {
                sse_response(&[
                    sse("response.output_item.added", &serde_json::json!({"type": "response.output_item.added", "item": item}).to_string()),
                    sse(
                        "response.completed",
                        &serde_json::json!({"type": "response.completed", "response": {"id": "resp_spawn", "output": [item]}}).to_string(),
                    ),
                ])
            } else {
                FakeResponse::json(200, &serde_json::json!({"id": "resp_spawn", "object": "response", "output": [item]}).to_string())
            }
        })
        .await;
        let (app, _dir) = arc_app(vec![provider(
            "provider",
            100,
            &format!("{}/v1", upstream.url),
            "provider-key",
        )]);
        let body = serde_json::json!({"model": "gpt-test", "input": child_history, "tools": [spawn_tool], "stream": stream, "prompt_cache_key": "native-thread-key"});
        let result = serve(&app, post("/v1/responses", &body.to_string())).await;
        assert_eq!(result.code(), 200, "{name}: {}", result.body_string());
        let sent = upstream.requests().pop().unwrap_or_default().json();
        assert_eq!(
            sent["tools"],
            serde_json::json!([spawn_tool]),
            "{name}: spawn_agent schema changed"
        );
        assert_eq!(
            sent["input"], child_history,
            "{name}: child history changed"
        );
        let got = if stream {
            result
                .body_string()
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
                .find_map(|event| event["item"]["arguments"].as_str().map(str::to_string))
                .unwrap_or_default()
        } else {
            serde_json::from_slice::<serde_json::Value>(&result.body).unwrap_or_default()["output"]
                [0]["arguments"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(got, args, "{name}: fork_turns arguments changed");
    }
}

// ---- the account's credential ----

fn refresh_app(accounts: Vec<Account>, refresh_url: &str) -> (Arc<App>, TempDir) {
    let refresh_url = refresh_url.to_string();
    let (app, dir) =
        super::testing::test_app_with(accounts, |settings| settings.refresh_url = refresh_url);
    (Arc::new(app), dir)
}

// Go TestResponsesProxyRefreshesExpiringCodexToken.
#[tokio::test]
async fn responses_proxy_refreshes_expiring_codex_token() {
    let new_access = super::testing::fake_jwt(Timestamp::now().add(hours(1)));
    let reply =
        format!(r#"{{"access_token":"{new_access}","refresh_token":"<new-refresh-token>"}}"#);
    let refresh = FakeServer::start(move |_| FakeResponse::json(200, &reply)).await;
    let upstream = FakeServer::start(ok_named("resp_test")).await;
    let (app, _dir) = refresh_app(
        vec![device_at(
            "codex-refresh",
            "",
            100,
            &format!("{}/backend-api", upstream.url),
        )],
        &refresh.url,
    );
    let expired = super::testing::fake_jwt(Timestamp::now().add(-minutes(1)));
    write_auth(
        &app,
        "codex-refresh",
        &format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{expired}","refresh_token":"<old-refresh-token>","account_id":"acct-chatgpt"}}}}"#
        ),
    );
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-test","input":"hello"}"#),
    )
    .await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    let asked = refresh.requests().pop().unwrap_or_default().json();
    assert!(
        asked["client_id"].as_str().is_some_and(|id| !id.is_empty())
            && asked["grant_type"] == "refresh_token"
            && asked["refresh_token"] == "<old-refresh-token>",
        "unexpected refresh request: {asked}"
    );
    assert_eq!(
        upstream
            .requests()
            .pop()
            .unwrap_or_default()
            .header("authorization"),
        format!("Bearer {new_access}")
    );
    let persisted = std::fs::read_to_string(
        app.read()
            .account_codex_home("codex-refresh")
            .join("auth.json"),
    )
    .unwrap_or_default();
    let persisted: serde_json::Value = serde_json::from_str(&persisted).unwrap_or_default();
    assert_eq!(
        persisted["tokens"]["refresh_token"], "<new-refresh-token>",
        "refreshed auth was not persisted"
    );
}

// Go TestConcurrentCodexAuthRefreshUsesOneTokenRequest.
#[tokio::test]
async fn concurrent_codex_auth_refresh_uses_one_token_request() {
    let new_access = super::testing::fake_jwt(Timestamp::now().add(hours(1)));
    let reply =
        format!(r#"{{"access_token":"{new_access}","refresh_token":"<new-refresh-token>"}}"#);
    let refresh = FakeServer::start(move |_| FakeResponse {
        chunks: vec![(Duration::from_millis(50), bytes::Bytes::from(reply.clone()))],
        ..FakeResponse::json(200, "")
    })
    .await;
    let (app, _dir) = refresh_app(vec![device_account("codex-lock", "", 0)], &refresh.url);
    let expired = super::testing::fake_jwt(Timestamp::now().add(-minutes(1)));
    write_auth(
        &app,
        "codex-lock",
        &format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{expired}","refresh_token":"<old-refresh-token>"}}}}"#
        ),
    );
    let item = account(&app, 0);
    let (first, second) = tokio::join!(
        app.refreshed_codex_auth(&item),
        app.refreshed_codex_auth(&item)
    );
    for auth in [first, second] {
        let auth = auth.unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(auth.access_token, new_access);
    }
    assert_eq!(refresh.count(), 1, "refresh endpoint calls");
}

// Go TestCodexTokenRefreshBoundaries.
#[tokio::test]
async fn codex_token_refresh_boundaries() {
    let failing = FakeServer::start(|_| FakeResponse::status(500)).await;
    let (app, _dir) = refresh_app(vec![device_account("codex-valid", "", 0)], &failing.url);
    let valid = super::testing::fake_jwt(Timestamp::now().add(hours(1)));
    write_auth(
        &app,
        "codex-valid",
        &format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{valid}","refresh_token":"<refresh-token>"}}}}"#
        ),
    );
    assert!(app.refreshed_codex_auth(&account(&app, 0)).await.is_ok());
    assert_eq!(failing.count(), 0, "valid access token triggered refresh");

    let unauthorized = FakeServer::start(|_| FakeResponse::status(401)).await;
    let (expired_app, _expired_dir) = refresh_app(
        vec![device_account("codex-expired", "", 0)],
        &unauthorized.url,
    );
    let expired = super::testing::fake_jwt(Timestamp::now().add(-minutes(1)));
    write_auth(
        &expired_app,
        "codex-expired",
        &format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{expired}","refresh_token":"<refresh-token>"}}}}"#
        ),
    );
    assert!(
        expired_app
            .refreshed_codex_auth(&account(&expired_app, 0))
            .await
            .is_err(),
        "expired access token with failed refresh returned no error"
    );
}

// Go TestCodexAuthRetriesDuringAuthFileRewrite: Codex CLI rewrites auth.json
// outside the pool's locks; a request landing on the partial write waits
// briefly and uses the completed file.
#[test]
fn codex_auth_retries_during_auth_file_rewrite() {
    let (app, _dir) = test_app(vec![device_account("acct-auth", "", 0)]);
    let path = app.read().account_codex_home("acct-auth").join("auth.json");
    write_auth(&app, "acct-auth", r#"{"auth_mode":"chatgpt","tokens":"#);
    let writer = {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            let _ = std::fs::write(
                path,
                r#"{"auth_mode":"chatgpt","tokens":{"access_token":"<access-token>","refresh_token":"<refresh-token>","account_id":"acct-chatgpt"}}"#,
            );
        })
    };
    let auth = app.read().codex_auth(&account(&app, 0));
    let _ = writer.join();
    let auth = auth.unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        (auth.access_token.as_str(), auth.account_id.as_str()),
        ("<access-token>", "acct-chatgpt")
    );
}

// Go TestCodexAuthUsesAccountNameInsteadOfProfileName.
#[test]
fn codex_auth_uses_account_name_instead_of_profile_name() {
    let (app, _dir) = test_app(vec![device_account("acct-auth", "", 0)]);
    let id_token = super::testing::fake_jwt_claims(&serde_json::json!({
        "email": "user@example.test",
        "name": "Example Person",
        "https://api.openai.com/profile": {"email": "user@example.test", "display_name": "Example Person"},
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-chatgpt", "chatgpt_account_name": "Example Workspace", "chatgpt_plan_type": "team"},
    }));
    write_auth(
        &app,
        "acct-auth",
        &serde_json::json!({"auth_mode": "chatgpt", "tokens": {"id_token": id_token, "access_token": "<access-token>", "refresh_token": "<refresh-token>"}}).to_string(),
    );
    let auth = app
        .read()
        .codex_auth(&account(&app, 0))
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(auth.organization_name, "Example Workspace");
}

// ---- routing gates on the account's allowance ----

// Go TestModelMeterRestrictsRoutingToEntitledAccounts.
#[test]
fn model_meter_restricts_routing_to_entitled_accounts() {
    let (app, _dir) = test_app(vec![
        api_slot("plain", "", 100, "https://plain.example.test"),
        api_slot("entitled", "", 100, "https://entitled.example.test"),
    ]);
    let quota = AccountQuota {
        additional_limits: vec![super::state::QuotaLimit {
            limit_id: "codex_bengalfox".into(),
            limit_name: "GPT-5.3-Codex-Spark".into(),
            ..super::state::QuotaLimit::default()
        }],
        ..AccountQuota::default()
    };
    set_quota(&app, "entitled", "", quota);
    let inner = app.read();
    let (plain, entitled) = (
        inner.account("plain").cloned().unwrap_or_default(),
        inner.account("entitled").cloned().unwrap_or_default(),
    );
    assert!(
        !inner.model_meter_allows(&plain, "gpt-5.3-codex-spark"),
        "an account without the meter must not receive the metered model"
    );
    assert!(
        inner.model_meter_allows(&entitled, "gpt-5.3-codex-spark"),
        "the account holding the meter must receive the metered model"
    );
    // An ordinary subscription model is metered by nobody and stays
    // unrestricted.
    for item in [&plain, &entitled] {
        assert!(
            inner.model_meter_allows(item, "gpt-6-astra"),
            "{} was excluded from an unmetered model",
            item.id
        );
    }
}

// Go TestModelMeterWithoutEvidenceDoesNotRestrict.
#[test]
fn model_meter_without_evidence_does_not_restrict() {
    let (app, _dir) = test_app(vec![
        api_slot("one", "", 100, "https://one.example.test"),
        api_slot("two", "", 100, "https://two.example.test"),
    ]);
    let inner = app.read();
    for id in ["one", "two"] {
        let item = inner.account(id).cloned().unwrap_or_default();
        assert!(
            inner.model_meter_allows(&item, "gpt-5.3-codex-spark"),
            "{id} was excluded with no meter evidence anywhere"
        );
    }
}

fn week_window(remaining: f64, now: Timestamp) -> QuotaWindow {
    QuotaWindow {
        role: "primary".into(),
        label: "Week".into(),
        present: true,
        remaining_percent: Some(remaining),
        window_minutes: Some(super::quota::WEEKLY_WINDOW_MINUTES),
        reset_at: Some(now.add(hours(48)).unix()),
        ..QuotaWindow::default()
    }
}

// Go TestQuotaFloorStopsRoutingBeforeCreditsAreCharged (routing and status;
// the capacity bar joins with the dashboard in 4.5).
#[test]
fn quota_floor_stops_routing_before_credits_are_charged() {
    let now = Timestamp::now();
    let (app, _dir) = test_app(vec![device_account("acct", "", 100)]);
    for (remaining, blocked) in [
        (0.0, true),
        (0.5, true),
        (1.0, true),
        (1.5, false),
        (40.0, false),
    ] {
        let quota = AccountQuota {
            windows: vec![week_window(remaining, now)],
            ..AccountQuota::default()
        };
        assert_eq!(
            super::quota::quota_explicitly_blocks_routing(
                &quota,
                "",
                now,
                app.tuning.quota_floor_percent
            ),
            blocked,
            "remaining {remaining}%"
        );
    }
    // The account must also read as exhausted rather than merely low, or an
    // operator would go looking for why a "low" account takes no traffic.
    super::testing::write_test_codex_auth_tokens(&app, "acct", "<access-token>", "<refresh-token>");
    set_quota(
        &app,
        "acct",
        "",
        AccountQuota {
            windows: vec![week_window(1.0, now)],
            ..AccountQuota::default()
        },
    );
    let item = account(&app, 0);
    let (status, reason) = app.read().account_status(&item, now);
    assert_eq!(status, "exhausted", "status at the floor ({reason})");
}

// Go TestQuotaProtectionRoutingAndConfigurationArePerSlot (routing; the
// admin action joins with the admin API in 4.5).
#[test]
fn quota_protection_routing_is_per_slot() {
    let now = Timestamp::now();
    let protected = Account {
        quota_protection_enabled: true,
        quota_protection_threshold: 20,
        ..device_account("protected", "upstream-protected", 100)
    };
    let (app, _dir) = test_app(vec![
        protected,
        api_slot("backup", "", 10, "https://backup.example.test"),
    ]);
    write_codex_device_auth(
        &app,
        "protected",
        "upstream-protected",
        "protected@example.test",
    );
    app.write().state.quotas_mut().insert(
        "protected".into(),
        QuotaSnapshot {
            last_successful_refresh_at: now,
            quota: Some(AccountQuota {
                windows: vec![QuotaWindow {
                    present: true,
                    observed: true,
                    percentage: 20,
                    remaining_percent: Some(20.0),
                    window_minutes: Some(300),
                    reset_at: Some(now.add(hours(1)).unix()),
                    ..QuotaWindow::default()
                }],
                ..AccountQuota::default()
            }),
            ..QuotaSnapshot::default()
        },
    );
    let selected = app.select_account(
        "gpt-test:protected-route",
        "gpt-test",
        &std::collections::HashMap::new(),
    );
    assert_eq!(
        selected.map(|item| item.id).unwrap_or_default(),
        "backup",
        "threshold-blocked slot selected"
    );
}

// Go TestProTiersFollowCodexPlanNames.
#[test]
fn pro_tiers_follow_codex_plan_names() {
    use super::app::PRO_TIER_LITE;
    use super::app::PRO_TIER_PRO;
    use super::plan::PRO_DEFAULT_WEIGHT;
    use super::plan::PRO_LITE_WEIGHT;
    use super::plan::PRO_MAX_WEIGHT;
    let (app, _dir) = test_app(Vec::new());
    for (raw, family, display, weight, tier) in [
        (
            "chatgptprolite",
            "pro_lite",
            "Pro 100",
            PRO_LITE_WEIGHT,
            PRO_TIER_LITE,
        ),
        (
            "chatgptpro",
            "pro",
            "Pro 200",
            PRO_DEFAULT_WEIGHT,
            PRO_TIER_PRO,
        ),
        ("promax", "pro_max", "Pro 500", PRO_MAX_WEIGHT, PRO_TIER_PRO),
    ] {
        assert_eq!(super::plan::plan_family_from_raw(raw), family, "{raw}");
        let item = Account {
            id: "slot".into(),
            raw_plan_type: raw.into(),
            plan_family: family.into(),
            plan_type: family.into(),
            ..Account::default()
        };
        assert_eq!(
            super::plan::account_plan_display_name(&item, false),
            display,
            "{raw}"
        );
        assert_eq!(
            super::plan::account_weekly_weight(family, "", "", app.tuning.premium_seat_weight),
            (weight, true),
            "{raw}"
        );
        assert_eq!(app.read().pro_tier(&item), tier, "{raw}: Use Pro last tier");
    }
    assert_eq!(
        super::plan::account_weekly_weight("pro", "20x", "", app.tuning.premium_seat_weight).0,
        20.0,
        "a reported multiplier must still win over the assumed tier"
    );
}

// Go TestRoutingIdentityAndCapacityAllowanceAgree (the routing identity half;
// the capacity allowance key joins with the dashboard in 4.5).
#[test]
fn routing_identity_keeps_logins_and_members_apart() {
    let member = |id: &str, workspace: &str, email: &str| Account {
        email: email.into(),
        ..device_account(id, workspace, 0)
    };
    let (app, _dir) = test_app(vec![
        member("member-a", "workspace-1", "a@example.test"),
        member("member-b", "workspace-1", "b@example.test"),
        member("copy-of-a", "workspace-1", "A@example.test"),
        member("personal", "personal-1", "p@example.test"),
        member("other-workspace", "workspace-2", "a@example.test"),
    ]);
    for id in [
        "member-a",
        "member-b",
        "copy-of-a",
        "personal",
        "other-workspace",
    ] {
        super::testing::write_test_codex_auth_tokens(&app, id, "<access-token>", "<refresh-token>");
    }
    let inner = app.read();
    let accounts = inner.config.accounts().to_vec();
    let key = |index: usize| inner.upstream_identity_key(&accounts[index]);
    assert_eq!(
        key(0),
        key(2),
        "two slots from one login must stay one identity"
    );
    assert_ne!(
        key(0),
        key(1),
        "two members of one workspace must be separate identities"
    );
    assert_ne!(
        key(0),
        key(4),
        "one person in two workspaces is two identities"
    );
}
