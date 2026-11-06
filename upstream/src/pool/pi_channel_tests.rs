//! The Pi channel through the pool's listener (pi_channel.rs, SPEC.md
//! 2.3.7). Upstream is a FakeServer reached through the real transport; its
//! answers are the shapes OpenAI's "Errors and recovery" table names for the
//! route and the Responses events a Pi reads, and the requests it records are
//! held to the Pi request measured in testdata/pi-reference.json.
use super::app::App;
use super::pi_channel::Credential;
use super::pi_channel::Endpoints;
use super::pi_channel::Unavailable;
use super::proxy_tests::completed;
use super::proxy_tests::created;
use super::proxy_tests::delta;
use super::proxy_tests::failed;
use super::proxy_tests::post;
use super::proxy_tests::serve;
use super::proxy_tests::sse_response;
use super::routes::Request;
use super::testing::FakeResponse;
use super::testing::FakeServer;
use super::testing::TempDir;
use super::testing::device_account;
use super::testing::test_app;
use super::testing::write_codex_device_auth;
use super::time::Timestamp;
use serde_json::Value;
use std::sync::Arc;

/// What a Pi pointed at the pool sends (pi-reference.json `viaPool`), cut
/// to one turn: its system prompt as a developer message, its tools, and the
/// max_output_tokens Pi drops only when it talks to OpenAI itself.
const PI_BODY: &str = r#"{"model":"gpt-5.5","input":[{"role":"developer","content":"You are a coding assistant running inside pi"},{"role":"user","content":[{"type":"input_text","text":"say hi"}]}],"stream":true,"prompt_cache_key":"01a1af7f-239f-759c-bf08-8b616bc172d2","store":false,"max_output_tokens":128000,"tools":[{"type":"function","name":"read","description":"Read a file","parameters":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}},"strict":false}],"reasoning":{"effort":"medium","summary":"auto"},"include":["reasoning.encrypted_content"]}"#;

fn pi_request(body: &str) -> Request {
    post("/v1/responses", body)
        .with_header(
            "user-agent",
            "pi-colleague/1.5.0 (linux; node/v22.23.2; x64)",
        )
        .with_header("session_id", "01a1af7f-239f-759c-bf08-8b616bc172d2")
        .with_header(
            "x-client-request-id",
            "01a1af7f-239f-759c-bf08-8b616bc172d2",
        )
        .with_header("x-stainless-lang", "js")
}

fn ok_stream() -> FakeResponse {
    sse_response(&[created("resp_pi"), delta("hi"), completed("resp_pi", "")])
}

fn write_pi_credential(app: &App, id: &str, access: &str, expires: i64) {
    let home = app.read().account_codex_home(id);
    std::fs::create_dir_all(&home).unwrap();
    let credential = Credential {
        client_id: format!("client-{id}"),
        access: access.to_string(),
        refresh: format!("<refresh-{id}>"),
        expires,
        scopes: vec!["chatgpt.tokens.use.direct".to_string()],
    };
    std::fs::write(
        home.join(super::pi_channel::CREDENTIAL_FILE),
        serde_json::to_string(&credential).unwrap(),
    )
    .unwrap();
}

fn read_pi_credential(app: &App, id: &str) -> Credential {
    let home = app.read().account_codex_home(id);
    serde_json::from_str(
        &std::fs::read_to_string(home.join(super::pi_channel::CREDENTIAL_FILE)).unwrap(),
    )
    .unwrap()
}

fn far_future() -> i64 {
    chrono::Utc::now().timestamp_millis() + 3_600_000
}

/// An app with the Pi channel on, its ChatGPT accounts signed in for it, and
/// the channel's endpoints on the fake upstream.
fn pi_app(upstream: &FakeServer, ids: &[&str]) -> (Arc<App>, TempDir) {
    let accounts = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            device_account(
                id,
                &format!("chatgpt-{id}"),
                100 - i64::try_from(index).unwrap(),
            )
        })
        .collect();
    let (app, dir) = test_app(accounts);
    for id in ids {
        write_codex_device_auth(&app, id, &format!("chatgpt-{id}"), "owner@example.test");
        write_pi_credential(&app, id, &format!("<pi-access-{id}>"), far_future());
    }
    app.write().pi_channel = true;
    app.pi_channel.set_endpoints(Endpoints {
        authorize: format!("{}/api/accounts/authorize", upstream.url),
        token: format!("{}/api/accounts/oauth/token", upstream.url),
        responses: format!("{}/v1/responses", upstream.url),
    });
    (Arc::new(app), dir)
}

/// The account's Codex side as the Pi channel must leave it: no cooldown,
/// and no quarantine (a quarantine records a quota error asking for a new
/// sign-in, outcome.rs quarantine_credential).
fn codex_credential_untouched(app: &App, id: &str) -> bool {
    let inner = app.read();
    !inner.state.cooldowns().contains_key(id)
        && inner
            .state
            .quotas()
            .get(id)
            .is_none_or(|snapshot| snapshot.quota_error.is_none())
}

#[tokio::test]
async fn a_pi_request_goes_upstream_as_pi_sends_it() {
    let upstream = FakeServer::start(|_| ok_stream()).await;
    let (app, _dir) = pi_app(&upstream, &["acct-a"]);
    let result = serve(&app, pi_request(PI_BODY)).await;
    assert_eq!(result.code(), 200, "{}", result.body_string());
    assert!(result.body_string().contains("response.completed"));
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.path, "/v1/responses");
    // Pi's identity and the account's plan-usage token; nothing of the
    // Codex channel (no ChatGPT-Account-ID, no installation id), and the
    // colleague's own name stays in the pool's log.
    assert_eq!(
        sent.header("user-agent"),
        "pi/1.5.0 (linux; node/v22.23.2; x64)"
    );
    assert_eq!(sent.header("authorization"), "Bearer <pi-access-acct-a>");
    assert_eq!(sent.header("x-stainless-package-version"), "7.19.0");
    assert_eq!(
        sent.header("session_id"),
        "01a1af7f-239f-759c-bf08-8b616bc172d2"
    );
    assert!(sent.headers.get("chatgpt-account-id").is_none());
    assert!(sent.headers.get("originator").is_none());
    let body: Value = sent.json();
    assert!(body.get("max_output_tokens").is_none());
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    // Field order and nested bytes are Pi's.
    let text = sent.body_string();
    assert!(text.starts_with(r#"{"model":"gpt-5.5","input":[{"role":"developer""#));
    assert!(text.contains(r#""parameters":{"type":"object","required":["path"],"properties""#));
}

#[tokio::test]
async fn a_codex_request_still_goes_on_the_codex_channel() {
    let upstream = FakeServer::start(|_| ok_stream()).await;
    let (app, _dir) = pi_app(&upstream, &["acct-a"]);
    // A Codex custom-provider request: Codex's identity format, session and
    // thread headers, client_metadata.
    let request = post(
        "/v1/responses",
        r#"{"model":"gpt-5.5","input":[],"stream":true,"store":false,"tool_choice":"auto","parallel_tool_calls":true,"reasoning":null,"include":[],"client_metadata":{}}"#,
    )
    .with_header("user-agent", "codex_exec/0.166.0 (Linux 6.1; x86_64) unknown")
    .with_header("originator", "codex_exec")
    .with_header("session-id", "s")
    .with_header("thread-id", "t");
    let _ = serve(&app, request).await;
    assert!(
        upstream
            .requests()
            .iter()
            .all(|request| request.path != "/v1/responses"),
        "a Codex request must not go on the Pi channel"
    );
}

#[tokio::test]
async fn no_account_signed_in_for_pi_is_said_plainly() {
    let upstream = FakeServer::start(|_| ok_stream()).await;
    let (app, _dir) = pi_app(&upstream, &["acct-a"]);
    let home = app.read().account_codex_home("acct-a");
    std::fs::remove_file(home.join(super::pi_channel::CREDENTIAL_FILE)).unwrap();
    let result = serve(&app, pi_request(PI_BODY)).await;
    assert_eq!(result.code(), 503);
    assert!(result.body_string().contains("pi_channel_unavailable"));
    assert_eq!(upstream.count(), 0);
}

#[tokio::test]
async fn a_usage_limit_rests_the_account_on_this_channel_only() {
    // "subscription_sharing_usage_limit_exceeded | 429 | Pause new
    // plan-based requests" (Errors and recovery).
    let upstream = FakeServer::start(|request| {
        if request.header("authorization") == "Bearer <pi-access-acct-a>" {
            FakeResponse::json(
                429,
                r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"Usage limit reached"}}"#,
            )
        } else {
            ok_stream()
        }
    })
    .await;
    let (app, _dir) = pi_app(&upstream, &["acct-a", "acct-b"]);
    let result = serve(&app, pi_request(PI_BODY)).await;
    assert_eq!(result.code(), 200, "{}", result.body_string());
    assert_eq!(upstream.count(), 2);
    assert!(matches!(
        app.pi_unavailable("acct-a", Timestamp::now()),
        Some(Unavailable::Limited { .. })
    ));
    assert!(codex_credential_untouched(&app, "acct-a"));
}

#[tokio::test]
async fn an_unauthorized_token_asks_for_a_new_sign_in_without_touching_codex() {
    // "401 | Signed identity or direct permission not accepted | Re-sign-in"
    // ({"detail": "..."}, a pre-stream admission error).
    let upstream = FakeServer::start(|request| {
        if request.header("authorization") == "Bearer <pi-access-acct-a>" {
            FakeResponse::json(401, r#"{"detail":"not accepted"}"#)
        } else {
            ok_stream()
        }
    })
    .await;
    let (app, _dir) = pi_app(&upstream, &["acct-a", "acct-b"]);
    let result = serve(&app, pi_request(PI_BODY)).await;
    assert_eq!(result.code(), 200, "{}", result.body_string());
    assert!(matches!(
        app.pi_unavailable("acct-a", Timestamp::now()),
        Some(Unavailable::SignIn(_))
    ));
    assert!(codex_credential_untouched(&app, "acct-a"));
}

#[tokio::test]
async fn an_unsupported_capability_is_the_callers_to_fix() {
    // "subscription_sharing_unsupported_capability | 400 | Stop. Check
    // error.param ... Don't resend the same body."
    let upstream = FakeServer::start(|_| {
        FakeResponse::json(
            400,
            r#"{"error":{"code":"subscription_sharing_unsupported_capability","param":"tools","message":"unsupported"}}"#,
        )
    })
    .await;
    let (app, _dir) = pi_app(&upstream, &["acct-a", "acct-b"]);
    let result = serve(&app, pi_request(PI_BODY)).await;
    assert_eq!(result.code(), 400);
    assert!(
        result
            .body_string()
            .contains("subscription_sharing_unsupported_capability")
    );
    assert_eq!(
        upstream.count(),
        1,
        "the same body must not go to another account"
    );
    assert!(app.pi_unavailable("acct-a", Timestamp::now()).is_none());
}

#[tokio::test]
async fn a_resting_account_stays_resting_after_a_restart() {
    // A restart must not spend a request on an account upstream just
    // refused: the mark is kept in the account's directory.
    let upstream = FakeServer::start(|_| ok_stream()).await;
    let (app, dir) = pi_app(&upstream, &["acct-a"]);
    app.pi_mark(
        "acct-a",
        Unavailable::Limited {
            code: "subscription_sharing_usage_limit_exceeded".to_string(),
            until: Timestamp::now().add(super::time::minutes(30)),
        },
    );
    let accounts = app.read().config.accounts().to_vec();
    let restarted = App::for_test(dir.path(), accounts);
    assert!(matches!(
        restarted.pi_unavailable("acct-a", Timestamp::now()),
        Some(Unavailable::Limited { .. })
    ));
    // An ended rest is forgotten, file included.
    restarted.pi_mark(
        "acct-a",
        Unavailable::Limited {
            code: "subscription_sharing_usage_limit_exceeded".to_string(),
            until: Timestamp::now().add(-super::time::minutes(1)),
        },
    );
    assert!(
        restarted
            .pi_unavailable("acct-a", Timestamp::now())
            .is_none()
    );
    let home = restarted.read().account_codex_home("acct-a");
    assert!(!home.join(super::pi_channel::STATE_FILE).exists());
}

#[tokio::test]
async fn a_usage_limit_mid_stream_rests_the_account() {
    // "Usage-limit failures can arrive mid-stream as response.failed".
    let upstream = FakeServer::start(|_| {
        sse_response(&[
            created("resp_pi"),
            delta("partial"),
            failed(
                "resp_pi",
                r#"{"code":"subscription_sharing_usage_limit_exceeded","message":"Usage limit reached"}"#,
                "",
            ),
        ])
    })
    .await;
    let (app, _dir) = pi_app(&upstream, &["acct-a"]);
    let result = serve(&app, pi_request(PI_BODY)).await;
    assert!(result.body_string().contains("response.failed"));
    assert!(matches!(
        app.pi_unavailable("acct-a", Timestamp::now()),
        Some(Unavailable::Limited { .. })
    ));
    assert!(codex_credential_untouched(&app, "acct-a"));
}

#[tokio::test]
async fn an_expired_token_is_refreshed_as_pi_refreshes_it() {
    let upstream = FakeServer::start(|request| {
        if request.path == "/api/accounts/oauth/token" {
            FakeResponse::json(
                200,
                r#"{"access_token":"<pi-access-new>","refresh_token":"<refresh-new>","id_token":"<id>","token_type":"Bearer","expires_in":3600,"scope":"openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"}"#,
            )
        } else {
            ok_stream()
        }
    })
    .await;
    let (app, _dir) = pi_app(&upstream, &["acct-a"]);
    write_pi_credential(&app, "acct-a", "<pi-access-old>", 0);
    let result = serve(&app, pi_request(PI_BODY)).await;
    assert_eq!(result.code(), 200, "{}", result.body_string());
    let requests = upstream.requests();
    let token = &requests[0];
    assert_eq!(token.path, "/api/accounts/oauth/token");
    // Pi's refresh (openai-chatgpt.js refreshAccessToken), its form order.
    assert_eq!(
        token.body_string(),
        "grant_type=refresh_token&client_id=client-acct-a&refresh_token=%3Crefresh-acct-a%3E&resource=https%3A%2F%2Fapi.openai.com%2Fv1"
    );
    assert_eq!(
        token.header("content-type"),
        "application/x-www-form-urlencoded"
    );
    assert_eq!(
        requests[1].header("authorization"),
        "Bearer <pi-access-new>"
    );
    let stored = read_pi_credential(&app, "acct-a");
    assert_eq!(stored.access, "<pi-access-new>");
    assert_eq!(stored.refresh, "<refresh-new>");
    assert_eq!(stored.client_id, "client-acct-a");
}

#[tokio::test]
async fn a_spent_refresh_token_asks_for_a_new_sign_in() {
    // "refresh_token_reused | Re-sign-in".
    let upstream = FakeServer::start(|request| {
        if request.path == "/api/accounts/oauth/token" {
            FakeResponse::json(400, r#"{"error":"refresh_token_reused"}"#)
        } else {
            ok_stream()
        }
    })
    .await;
    let (app, _dir) = pi_app(&upstream, &["acct-a"]);
    write_pi_credential(&app, "acct-a", "<pi-access-old>", 0);
    let result = serve(&app, pi_request(PI_BODY)).await;
    assert_ne!(result.code(), 200);
    assert!(matches!(
        app.pi_unavailable("acct-a", Timestamp::now()),
        Some(Unavailable::SignIn(_))
    ));
    assert!(codex_credential_untouched(&app, "acct-a"));
}

#[tokio::test]
async fn an_app_that_asked_for_no_stream_gets_one_answer() {
    let upstream = FakeServer::start(|_| {
        sse_response(&[
            created("resp_pi"),
            super::proxy_tests::sse(
                "response.output_item.done",
                r#"{"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#,
            ),
            completed("resp_pi", ""),
        ])
    })
    .await;
    let (app, _dir) = pi_app(&upstream, &["acct-a"]);
    let result = serve(
        &app,
        post("/v1/responses", r#"{"model":"gpt-5.5","input":"hello"}"#),
    )
    .await;
    assert_eq!(result.code(), 200, "{}", result.body_string());
    let answer: Value = serde_json::from_str(&result.body_string()).unwrap();
    assert_eq!(answer["id"], "resp_pi");
    let sent = upstream.requests()[0].body_string();
    assert_eq!(
        sent,
        r#"{"model":"gpt-5.5","input":[{"role":"user","content":[{"type":"input_text","text":"hello"}]}],"stream":true,"store":false}"#
    );
}

#[tokio::test]
async fn the_owner_signs_an_account_in_as_pi_signs_in() {
    let upstream = FakeServer::start(|request| {
        assert_eq!(request.path, "/api/accounts/oauth/token");
        FakeResponse::json(
            200,
            r#"{"access_token":"<pi-access-new>","refresh_token":"<refresh-new>","id_token":"<id>","token_type":"Bearer","expires_in":3600,"scope":"openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"}"#,
        )
    })
    .await;
    let (app, _dir) = pi_app(&upstream, &["acct-a"]);
    let authorize = app.pi_login_start("acct-a").unwrap();
    let url = url::Url::parse(&authorize).unwrap();
    let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    // Pi's authorize request (loginOpenAIChatGPT), the account's host id.
    assert_eq!(query["client_id"], "dynamic_agent_client");
    assert_eq!(query["agent_name_hint"], "Pi");
    assert!(query["ext_agent_host_id"].starts_with("urn:uuid:"));
    assert_eq!(query["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
    assert_eq!(query["resource"], "https://api.openai.com/v1");
    assert_eq!(
        query["scope"],
        "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"
    );
    assert_eq!(query["code_challenge_method"], "S256");
    let host = query["ext_agent_host_id"].clone();
    // A wrong state is refused.
    assert!(
        app.pi_login_complete(
            "acct-a",
            "http://127.0.0.1:1455/auth/callback?code=c&state=other&client_id=issued"
        )
        .await
        .is_err()
    );
    let callback = format!(
        "http://127.0.0.1:1455/auth/callback?code=the-code&state={}&client_id=issued-client",
        query["state"]
    );
    app.pi_login_complete("acct-a", &callback).await.unwrap();
    let sent = upstream.requests()[0].body_string();
    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(sent.as_bytes())
            .into_owned()
            .collect();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["client_id"], "issued-client");
    assert_eq!(form["code"], "the-code");
    assert_eq!(form["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
    assert_eq!(form["resource"], "https://api.openai.com/v1");
    assert!(!form["code_verifier"].is_empty());
    let stored = read_pi_credential(&app, "acct-a");
    assert_eq!(stored.client_id, "issued-client");
    assert_eq!(stored.access, "<pi-access-new>");
    // The host id is kept for the next sign-in.
    let again = app.pi_login_start("acct-a").unwrap();
    assert!(
        again.contains(&url::form_urlencoded::byte_serialize(host.as_bytes()).collect::<String>())
    );
}

#[test]
fn pis_headers_are_the_measured_ones() {
    // The constants in pi_channel.rs against the capture they were taken
    // from: a Pi re-measured (scripts/conformance/pi-capture.sh) that sends
    // other headers fails here until the channel is matched to it.
    let reference: Value =
        serde_json::from_str(include_str!("../../testdata/pi-reference.json")).unwrap();
    assert_eq!(reference["piRelease"], super::pi_channel::PI_RELEASE);
    let measured: Vec<(String, String)> = reference["responses"]["headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            (
                pair[0].as_str().unwrap().to_string(),
                pair[1].as_str().unwrap().to_string(),
            )
        })
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "host" | "connection" | "content-type" | "content-length" | "accept-encoding"
            )
        })
        .collect();
    let mut inbound = http::HeaderMap::new();
    inbound.insert("session_id", http::HeaderValue::from_static("<session>"));
    let (headers, order) = super::pi_channel::pi_request_headers("<access>", &inbound);
    let ours: Vec<(String, String)> = order
        .trim_end_matches('|')
        .split(',')
        .map(|name| {
            (
                name.to_string(),
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(ours, measured);
}

#[tokio::test]
async fn the_channel_counts_its_own_usage() {
    // Upstream gives an app no plan-usage reading; the pool sums each
    // answer's usage for the Console's Pi badge.
    let upstream = FakeServer::start(|_| {
        sse_response(&[
            created("resp_pi"),
            completed(
                "resp_pi",
                r#"{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":2,"total_tokens":12}"#,
            ),
        ])
    })
    .await;
    let (app, _dir) = pi_app(&upstream, &["acct-a"]);
    for _ in 0..2 {
        let result = serve(&app, pi_request(PI_BODY)).await;
        assert_eq!(result.code(), 200, "{}", result.body_string());
    }
    let usage = app.pi_usage("acct-a");
    assert_eq!(usage["fiveHours"]["requests"], 2);
    assert_eq!(usage["fiveHours"]["inputTokens"], 20);
    assert_eq!(usage["fiveHours"]["cachedTokens"], 8);
    assert_eq!(usage["week"]["outputTokens"], 4);
}
