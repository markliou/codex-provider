//! Keeping an account's quota current when other clients spend it too
//! (quota_observed.rs, SPEC.md 9.9): the figures each response carries, and
//! the usage read before sending to a stale account.

use super::app::App;
use super::proxy_tests::codex_app;
use super::proxy_tests::completed;
use super::proxy_tests::created;
use super::proxy_tests::post;
use super::proxy_tests::serve;
use super::proxy_tests::sse_response;
use super::quota;
use super::state::QuotaSnapshot;
use super::testing::FakeResponse;
use super::testing::FakeServer;
use super::testing::TempDir;
use super::testing::device_account;
use super::testing::write_codex_device_auth;
use super::time::Timestamp;
use super::time::hours;
use super::time::minutes;
use serde_json::Value;
use std::sync::Arc;

const REQUEST: &str = r#"{"model":"gpt-test","stream":true,"input":[{"type":"message","role":"user","content":"hi"}],"store":false,"client_metadata":{"session_id":"SESSION"}}"#;

/// A Codex request of its own session, so each one is placed anew.
fn codex_request(session: &str) -> super::routes::Request {
    post("/v1/responses", &REQUEST.replace("SESSION", session))
        .with_header("user-agent", super::ws_tests::CODEX_EXEC_AGENT)
        .with_header("originator", "codex_exec")
        .with_header("session-id", session)
        .with_header("thread-id", session)
}

/// One of the rate-limit shapes recorded from Codex's parser.
fn shape(name: &str) -> Value {
    let raw = include_str!("../../testdata/codex-upstream-failures.json");
    let fixture: Value = serde_json::from_str(raw).unwrap_or_default();
    fixture["rateLimits"]
        .as_array()
        .and_then(|shapes| shapes.iter().find(|shape| shape["name"] == name))
        .cloned()
        .unwrap_or_else(|| panic!("no rate-limit shape {name}"))
}

fn shape_headers(name: &str) -> Vec<(String, String)> {
    shape(name)["headers"]
        .as_object()
        .map(|headers| {
            headers
                .iter()
                .map(|(name, value)| (name.clone(), value.as_str().unwrap_or_default().into()))
                .collect()
        })
        .unwrap_or_default()
}

fn header_map(headers: &[(String, String)]) -> http::HeaderMap {
    let mut map = http::HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            map.insert(name, value);
        }
    }
    map
}

/// A usage answer with the five-hour window `used` percent spent.
fn usage(used: u32) -> String {
    format!(
        r#"{{"plan_type":"team","rate_limit":{{"allowed":true,"limit_reached":false,"primary_window":{{"used_percent":{used},"limit_window_seconds":18000,"reset_at":4102444800}},"secondary_window":{{"used_percent":5,"limit_window_seconds":604800,"reset_at":4103049600}}}},"rate_limit_reset_credits":{{"available_count":1}}}}"#
    )
}

/// The snapshot a usage read `age` ago left behind.
fn read_long_ago(app: &App, id: &str, used: u32, at: Timestamp) {
    let answer: quota::UsageResponse =
        serde_json::from_str(&usage(used)).unwrap_or_else(|error| panic!("{error}"));
    let current = quota::quota_from_usage(&answer, at);
    app.write().state.quotas_mut().insert(
        id.to_string(),
        QuotaSnapshot {
            account_id: id.to_string(),
            plan_family: "team".into(),
            quota: Some(current),
            observed_at: at,
            last_successful_refresh_at: at,
            usage_updated_at: at,
            provenance: "chatgpt_wham_usage".into(),
            ..QuotaSnapshot::default()
        },
    );
}

fn snapshot(app: &App, id: &str) -> QuotaSnapshot {
    app.read().quota_snapshot(id)
}

fn remaining(snapshot: &QuotaSnapshot, minutes: i64) -> Option<f64> {
    let quota = snapshot.quota.as_ref()?;
    quota::quota_window_by_duration(quota, minutes).remaining_percent
}

/// Device slots on a fake Codex backend, with the usage read before use on
/// (the test app turns it off) and the global reserve on, as it is live.
fn checking_app(server_url: &str, slots: &[(&str, &str, i64)]) -> (Arc<App>, TempDir) {
    let accounts = slots
        .iter()
        .map(|(id, upstream, priority)| device_account(id, upstream, *priority))
        .collect();
    let base = format!("{server_url}/backend-api");
    let (mut app, dir) =
        super::testing::test_app_with(accounts, |settings| settings.codex_base_url = base);
    for (id, upstream, _) in slots {
        write_codex_device_auth(&app, id, upstream, &format!("{id}@example.test"));
    }
    app.tuning.placement_check_wait = super::quota_observed::PLACEMENT_CHECK_WAIT;
    app.write().quota_reserve.enabled = true;
    (Arc::new(app), dir)
}

fn ok_stream() -> FakeResponse {
    sse_response(&[created("resp_ok"), completed("resp_ok", "")])
}

// A response's x-codex-* headers update the windows another client spent:
// the figures move, freshness returns, and what only the usage endpoint
// reports (reset credits, the read's own instant) is kept.
#[tokio::test]
async fn response_headers_update_the_serving_account_s_windows() {
    let headers = shape_headers("rate limit headers");
    let upstream = FakeServer::start(move |request| {
        if request.path != "/backend-api/codex/responses" {
            return FakeResponse::json(404, "{}");
        }
        let mut response = ok_stream();
        response.headers.extend(headers.iter().cloned());
        response
    })
    .await;
    let (app, _dir) = codex_app(&upstream.url, &[("device", "acct-up", 100)]);
    let long_ago = Timestamp::now().add(-hours(3));
    read_long_ago(&app, "device", 10, long_ago);
    assert_eq!(
        quota::quota_telemetry_state(&snapshot(&app, "device"), Timestamp::now()),
        "stale"
    );

    let result = serve(&app, codex_request("session-headers")).await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());

    let after = snapshot(&app, "device");
    assert_eq!(
        remaining(&after, quota::FIVE_HOUR_WINDOW_MINUTES),
        Some(60.0)
    );
    assert_eq!(remaining(&after, quota::WEEKLY_WINDOW_MINUTES), Some(30.0));
    assert_eq!(
        quota::quota_telemetry_state(&after, Timestamp::now()),
        "fresh"
    );
    assert_eq!(after.last_successful_refresh_at, long_ago);
    let quota = after.quota.as_ref().unwrap_or_else(|| panic!("no quota"));
    assert_eq!(
        quota
            .reset_credits
            .as_ref()
            .and_then(|credits| credits.available_count),
        Some(1),
        "a response says nothing of reset credits; the last reading stays"
    );
    assert_eq!(app.read().config.accounts()[0].remaining_quota, Some(30));
    // Only the usage endpoint was not asked: the figures came for free.
    assert!(
        upstream
            .requests()
            .iter()
            .all(|request| request.path == "/backend-api/codex/responses")
    );
    // Persisted by the response's own success write, not a write of its own
    // (quota_observed.rs), so a restart does not lose what moved.
    let saved = std::fs::read_to_string(app.settings.data_dir.join("state").join("runtime.json"))
        .unwrap_or_default();
    let saved: Value = serde_json::from_str(&saved).unwrap_or_default();
    assert_eq!(
        saved["quotas"]["device"]["quota"]["windows"][0]["remainingPercent"], 60.0,
        "{saved}"
    );
}

// A WebSocket's codex.rate_limits event is read like the headers, credits
// included; a metered model's header family updates that meter alone; any
// other event leaves the snapshot as it was.
#[test]
fn rate_limit_event_and_metered_headers_update_the_snapshot() {
    let (app, _dir) =
        super::testing::test_app_with(vec![device_account("device", "acct-up", 100)], |_| {});
    let event = shape("rate limits event")["event"].to_string();
    let state_file = app.settings.data_dir.join("state").join("runtime.json");
    let written = || std::fs::read(&state_file).ok();
    let before_event = written();
    app.note_rate_limit_event("device", event.as_bytes());
    assert_eq!(
        written(),
        before_event,
        "a reading from a response must not write the state file of its own"
    );
    let after = snapshot(&app, "device");
    assert_eq!(
        remaining(&after, quota::FIVE_HOUR_WINDOW_MINUTES),
        Some(58.0)
    );
    assert_eq!(remaining(&after, quota::WEEKLY_WINDOW_MINUTES), Some(29.0));
    let quota = after.quota.clone().unwrap_or_default();
    assert_eq!(
        quota
            .credits
            .as_ref()
            .and_then(|credits| credits.balance.as_deref()),
        Some("12")
    );
    assert_eq!(
        quota::quota_telemetry_state(&after, Timestamp::now()),
        "fresh"
    );

    app.note_rate_limit_headers(
        "device",
        &header_map(&shape_headers("rate limit headers of a metered model")),
    );
    let metered = snapshot(&app, "device").quota.unwrap_or_default();
    assert_eq!(metered.windows, quota.windows, "the Codex windows stay");
    let meter = metered
        .additional_limits
        .iter()
        .find(|limit| limit.limit_id == "codex_bengalfox")
        .unwrap_or_else(|| panic!("no meter: {:?}", metered.additional_limits));
    assert_eq!(meter.limit_name, "GPT-5.3-Codex-Spark-Next");
    assert_eq!(
        meter
            .windows
            .first()
            .and_then(|window| window.remaining_percent),
        Some(75.0)
    );

    let before = snapshot(&app, "device");
    app.note_rate_limit_event("device", br#"{"type":"codex.response.metadata"}"#);
    app.note_rate_limit_headers("device", &http::HeaderMap::new());
    assert_eq!(snapshot(&app, "device"), before);
}

// An account another pool drained looks roomy here on a reading from hours
// ago. Before a request is sent to it, its usage is read, and the request
// goes where there is room; a second request reads nothing again.
#[tokio::test]
async fn a_stale_account_is_read_before_it_is_sent_to() {
    let upstream = FakeServer::start(|request| match request.path.as_str() {
        "/backend-api/wham/usage" => {
            let used = if request.header("chatgpt-account-id") == "acct-a" {
                100
            } else {
                20
            };
            FakeResponse::json(200, &usage(used))
        }
        "/backend-api/codex/responses" => ok_stream(),
        _ => FakeResponse::json(404, "{}"),
    })
    .await;
    let (app, _dir) = checking_app(&upstream.url, &[("a", "acct-a", 100), ("b", "acct-b", 10)]);
    read_long_ago(&app, "a", 10, Timestamp::now().add(-hours(3)));

    let result = serve(&app, codex_request("session-one")).await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    let result = serve(&app, codex_request("session-two")).await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());

    let requests = upstream.requests();
    let reads: Vec<&str> = requests
        .iter()
        .filter(|request| request.path == "/backend-api/wham/usage")
        .map(|request| request.header("chatgpt-account-id"))
        .collect();
    assert_eq!(reads, ["acct-a", "acct-b"], "each stale account read once");
    let served: Vec<&str> = requests
        .iter()
        .filter(|request| request.path == "/backend-api/codex/responses")
        .map(|request| request.header("chatgpt-account-id"))
        .collect();
    assert_eq!(
        served,
        ["acct-b", "acct-b"],
        "the drained account is passed over"
    );
    assert_eq!(
        remaining(&snapshot(&app, "a"), quota::FIVE_HOUR_WINDOW_MINUTES),
        Some(0.0)
    );
    // The background poll counts both reads as its own.
    let mut elsewhere: Vec<String> = app
        .take_quota_reads_elsewhere()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    elsewhere.sort();
    assert_eq!(elsewhere, ["a", "b"]);
}

// Two pools on one account: a reading ten minutes old still counts as fresh
// for the dashboard, but the other pool may have spent the account into its
// reserve since, so it is read before a request goes to it. A reading a
// minute old is not read again.
#[tokio::test]
async fn a_reading_minutes_old_is_read_again_before_use() {
    let upstream = FakeServer::start(|request| match request.path.as_str() {
        "/backend-api/wham/usage" => {
            let used = if request.header("chatgpt-account-id") == "acct-a" {
                95
            } else {
                20
            };
            FakeResponse::json(200, &usage(used))
        }
        "/backend-api/codex/responses" => ok_stream(),
        _ => FakeResponse::json(404, "{}"),
    })
    .await;
    let (app, _dir) = checking_app(&upstream.url, &[("a", "acct-a", 100), ("b", "acct-b", 10)]);
    let now = Timestamp::now();
    read_long_ago(&app, "a", 10, now.add(-minutes(10)));
    read_long_ago(&app, "b", 20, now.add(-minutes(1)));
    assert_eq!(
        quota::quota_telemetry_state(&snapshot(&app, "a"), now),
        "fresh"
    );

    let result = serve(&app, codex_request("session-one")).await;
    assert_eq!(result.code(), 200, "proxy: {}", result.body_string());

    let requests = upstream.requests();
    let reads: Vec<&str> = requests
        .iter()
        .filter(|request| request.path == "/backend-api/wham/usage")
        .map(|request| request.header("chatgpt-account-id"))
        .collect();
    assert_eq!(reads, ["acct-a"], "only the ten-minute reading is read");
    let served: Vec<&str> = requests
        .iter()
        .filter(|request| request.path == "/backend-api/codex/responses")
        .map(|request| request.header("chatgpt-account-id"))
        .collect();
    assert_eq!(
        served,
        ["acct-b"],
        "the account in its reserve is passed over"
    );
}

// A read that fails does not hold the request, and the account is not read
// again on the next request: one failing endpoint must not double the
// account's traffic.
#[tokio::test]
async fn a_failed_read_is_not_repeated_on_every_request() {
    let upstream = FakeServer::start(|request| match request.path.as_str() {
        "/backend-api/wham/usage" => FakeResponse::json(500, r#"{"detail":"busy"}"#),
        "/backend-api/codex/responses" => ok_stream(),
        _ => FakeResponse::json(404, "{}"),
    })
    .await;
    let (app, _dir) = checking_app(&upstream.url, &[("a", "acct-a", 100)]);
    for session in ["session-one", "session-two", "session-three"] {
        let result = serve(&app, codex_request(session)).await;
        assert_eq!(result.code(), 200, "proxy: {}", result.body_string());
    }
    let reads = upstream
        .requests()
        .iter()
        .filter(|request| request.path == "/backend-api/wham/usage")
        .count();
    assert_eq!(reads, 1);
}

// An older snapshot holds only the role fields; a response's windows are
// matched by length, so a five-hour reading never lands on the week.
#[test]
fn observed_windows_match_by_length() {
    let usage: quota::UsageResponse =
        serde_json::from_str(&usage(10)).unwrap_or_else(|error| panic!("{error}"));
    let mut prior = quota::quota_from_usage(&usage, Timestamp::now());
    prior.windows.clear();
    let headers = header_map(&[
        ("x-codex-primary-used-percent".into(), "90".into()),
        ("x-codex-primary-window-minutes".into(), "10080".into()),
    ]);
    let snapshots = crate::rate_limits::parse_all_rate_limits(&headers);
    let merged = quota::apply_rate_limit_snapshots(Some(&prior), &snapshots, Timestamp::now())
        .unwrap_or_else(|| panic!("nothing applied"));
    assert_eq!(
        quota::quota_window_by_duration(&merged, quota::WEEKLY_WINDOW_MINUTES).remaining_percent,
        Some(10.0)
    );
    assert_eq!(
        quota::quota_window_by_duration(&merged, quota::FIVE_HOUR_WINDOW_MINUTES).remaining_percent,
        Some(90.0),
        "the five-hour window keeps its own reading"
    );
}

// The background poll reads each account on a native Codex's cadence for its
// most used window, except that an account the reserve already holds out of
// routing is read once a minute, not every 15 or 5 seconds until its reset.
#[test]
fn the_poll_interval_follows_codex_except_for_a_held_account() {
    use super::quota_schedule::INTERVAL;
    use std::time::Duration;
    let (app, _dir) = checking_app("http://127.0.0.1:9", &[("a", "acct-a", 100)]);
    assert_eq!(app.quota_poll_interval("a"), INTERVAL, "never read");
    assert_eq!(app.quota_poll_interval("gone"), INTERVAL, "no such account");
    let now = Timestamp::now();
    read_long_ago(&app, "a", 50, now);
    assert_eq!(app.quota_poll_interval("a"), Duration::from_secs(60));
    read_long_ago(&app, "a", 80, now);
    assert_eq!(app.quota_poll_interval("a"), Duration::from_secs(30));
    read_long_ago(&app, "a", 89, now);
    assert_eq!(app.quota_poll_interval("a"), Duration::from_secs(30));
    // 5% of the five-hour window left: the 10% reserve holds it.
    read_long_ago(&app, "a", 95, now);
    assert_eq!(
        app.quota_poll_interval("a"),
        INTERVAL,
        "held by the reserve"
    );
    app.write().quota_reserve.enabled = false;
    assert_eq!(app.quota_poll_interval("a"), Duration::from_secs(15));
    read_long_ago(&app, "a", 99, now);
    assert_eq!(app.quota_poll_interval("a"), Duration::from_secs(5));
}
