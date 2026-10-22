//! The Go tests of the pool's own upstream work (main_test.go quota refresh,
//! reset_credit_test.go, refused_credential_test.go, quota_schedule_test.go
//! and the priming tests), one by one. Each keeps its Go name in a comment;
//! the assertions are Go's.

use super::app::App;
use super::proxy_tests::account;
use super::proxy_tests::write_auth;
use super::quota_refresh::RefreshError;
use super::state::Account;
use super::state::AccountQuota;
use super::state::QuotaResetCredits;
use super::testing::FakeResponse;
use super::testing::FakeServer;
use super::testing::RecordedRequest;
use super::testing::TempDir;
use super::testing::device_account;
use super::testing::fake_jwt;
use super::time::Timestamp;
use super::time::hours;
use std::sync::Arc;

/// Go testApp with `a.codexBaseURL = <server>/backend-api` and a valid
/// access token on each slot.
fn backend_app(server: &FakeServer, accounts: Vec<Account>) -> (Arc<App>, TempDir) {
    let base = format!("{}/backend-api", server.url);
    let ids: Vec<String> = accounts.iter().map(|item| item.id.clone()).collect();
    let (app, dir) =
        super::testing::test_app_with(accounts, |settings| settings.codex_base_url = base);
    let token = fake_jwt(Timestamp::now().add(hours(1)));
    for id in ids {
        write_auth(
            &app,
            &id,
            &format!(
                r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{token}","refresh_token":"<refresh-token>"}}}}"#
            ),
        );
    }
    (Arc::new(app), dir)
}

fn json(body: String) -> FakeResponse {
    FakeResponse::json(200, &body)
}

/// Go's GET /admin/api/public-dashboard, through the router.
async fn public_dashboard(app: &Arc<App>) -> String {
    let result = super::proxy_tests::serve(
        app,
        super::routes::Request::test(http::Method::GET, "/admin/api/public-dashboard", ""),
    )
    .await;
    assert_eq!(result.code(), 200, "public dashboard");
    result.body_string()
}

fn by_path(request: &RecordedRequest, path: &str) -> bool {
    request.path == path
}

// Go TestCodexQuotaRefreshUpdatesDashboardState.
#[tokio::test]
async fn codex_quota_refresh_updates_dashboard_state() {
    let reset_at = Timestamp::now().add(hours(7)).unix();
    let credit_expires = Timestamp::now()
        .add(hours(14 * 24))
        .truncate(super::time::seconds(1));
    let (used, available) = (credit_expires.add(-hours(1)), credit_expires);
    let server = FakeServer::start(move |request| match request.path.as_str() {
        "/backend-api/wham/usage" => json(format!(
            r#"{{"plan_type":"team","workspace_name":"Acme Workspace","rate_limit":{{"allowed":true,"limit_reached":false,"primary_window":{{"used_percent":30,"limit_window_seconds":18000,"reset_after_seconds":60}},"secondary_window":{{"used_percent":80,"limit_window_seconds":604800,"reset_at":{reset_at}}}}},"rate_limit_reset_credits":{{"available_count":1}}}}"#
        )),
        "/backend-api/wham/rate-limit-reset-credits" => json(format!(
            r#"{{"available_count":1,"credits":[{{"status":"used","expires_at":"{used}"}},{{"status":"available","expires_at":"{available}"}}]}}"#
        )),
        "/backend-api/wham/accounts/check" => json(
            r#"{"accounts":[{"id":"acct-chatgpt","name":"Acme Workspace","plan_type":"team","structure":"workspace"}],"account_ordering":["acct-chatgpt"]}"#.to_string(),
        ),
        other => FakeResponse::json(404, &format!(r#"{{"unexpected":"{other}"}}"#)),
    })
    .await;
    let (app, _dir) = backend_app(
        &server,
        vec![device_account("codex-quota", "acct-chatgpt", 100)],
    );
    let snapshot = app
        .refresh_account_quota("codex-quota")
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let requests = server.requests();
    assert!(
        requests
            .iter()
            .any(|request| by_path(request, "/backend-api/wham/usage")
                && !request.header("authorization").is_empty()
                && request.header("chatgpt-account-id") == "acct-chatgpt"),
        "quota refresh did not send ChatGPT-Account-Id"
    );
    assert!(
        requests
            .iter()
            .filter(|request| by_path(request, "/backend-api/wham/rate-limit-reset-credits"))
            .all(|request| !request.header("authorization").is_empty()
                && request.header("chatgpt-account-id") == "acct-chatgpt"),
        "reset-credit details request omitted account authorization context"
    );
    // Codex's account check names the plan "team"; the web app's entitlement
    // said "chatgptteamplan". Both are the Business family.
    assert_eq!(
        (
            snapshot.raw_plan_type.as_str(),
            snapshot.plan_family.as_str(),
            snapshot.plan_type.as_str(),
            snapshot.organization_name.as_str()
        ),
        ("team", "business", "business", "Acme Workspace")
    );
    let item = account(&app, 0);
    assert_eq!(
        (item.organization_name.as_str(), item.label.as_str()),
        ("Acme Workspace", "codex-quota")
    );
    let body = public_dashboard(&app).await;
    assert!(
        body.contains("Credential 1") && body.contains("Business · Acme Workspace"),
        "public dashboard omitted team organization label: {body}"
    );
    let quota = snapshot.quota.clone().unwrap_or_default();
    assert!(
        quota.hourly.percentage == 70 && quota.hourly.window_minutes == Some(300),
        "hourly: {:?}",
        quota.hourly
    );
    assert!(
        quota.weekly.percentage == 20 && quota.weekly.reset_at == Some(reset_at),
        "weekly: {:?}",
        quota.weekly
    );
    assert_eq!(
        quota
            .reset_credits
            .as_ref()
            .and_then(|credits| credits.expires_at),
        Some(credit_expires.unix()),
        "reset-credit expiry was not normalized"
    );
    let detail_hits = || {
        server
            .requests()
            .iter()
            .filter(|request| by_path(request, "/backend-api/wham/rate-limit-reset-credits"))
            .count()
    };
    assert_eq!(detail_hits(), 1);
    let cached = app
        .refresh_account_quota("codex-quota")
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        cached
            .quota
            .and_then(|quota| quota.reset_credits)
            .and_then(|credits| credits.expires_at),
        Some(credit_expires.unix()),
        "cached reset-credit expiry was not retained"
    );
    assert_eq!(
        detail_hits(),
        1,
        "an unchanged reset-credit count triggered another details request"
    );
    assert_eq!(
        account(&app, 0).remaining_quota,
        Some(20),
        "remaining quota hint not updated"
    );
    let (status, reason) = app
        .read()
        .account_status(&account(&app, 0), Timestamp::now());
    assert!(
        status == "low" && reason.contains("Quota window"),
        "quota status = {status}/{reason}"
    );
    let body = public_dashboard(&app).await;
    assert!(
        body.contains(r#""hourly""#)
            && body.contains(r#""weekly""#)
            && body.contains(&format!(r#""expiresAt":{}"#, credit_expires.unix())),
        "public dashboard did not include quota windows: {body}"
    );
    assert!(
        !body.contains("acct-chatgpt") && !body.contains("<refresh-token>"),
        "public dashboard exposed credential/account internals: {body}"
    );
}

// Go TestCodexQuotaRefreshClearsStoredPersonalOrganizationName.
#[tokio::test]
async fn codex_quota_refresh_clears_stored_personal_organization_name() {
    let reset_at = Timestamp::now().add(hours(1)).unix();
    let server = FakeServer::start(move |request| match request.path.as_str() {
        "/backend-api/wham/usage" => json(format!(
            r#"{{"plan_type":"team","rate_limit":{{"allowed":true,"limit_reached":false,"primary_window":{{"used_percent":30,"limit_window_seconds":18000,"reset_after_seconds":60}},"secondary_window":{{"used_percent":40,"limit_window_seconds":604800,"reset_at":{reset_at}}}}}}}"#
        )),
        "/backend-api/wham/accounts/check" => json(
            r#"{"accounts":[{"id":"acct-chatgpt","name":"Personal Name","plan_type":"team","structure":"personal"}],"account_ordering":["acct-chatgpt"]}"#.to_string(),
        ),
        _ => FakeResponse::status(404),
    })
    .await;
    let item = Account {
        organization_name: "Personal Name".into(),
        plan_type: "team".into(),
        ..device_account("codex-team", "acct-chatgpt", 0)
    };
    let (app, _dir) = backend_app(&server, vec![item]);
    let snapshot = app
        .refresh_account_quota("codex-team")
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        snapshot.plan_type == "business" && snapshot.organization_name.is_empty(),
        "personal account name was retained in quota snapshot"
    );
    assert!(
        account(&app, 0).organization_name.is_empty(),
        "personal account name was retained in account config"
    );
}

// Go TestCodexQuotaRefreshDoesNotInferProPlanLimit.
#[tokio::test]
async fn codex_quota_refresh_does_not_infer_pro_plan_limit() {
    let server = FakeServer::start(|request| match request.path.as_str() {
        "/backend-api/wham/usage" => json(
            r#"{"plan_type":"pro","rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":10,"limit_window_seconds":18000},"secondary_window":{"used_percent":20,"limit_window_seconds":604800}}}"#.to_string(),
        ),
        "/backend-api/wham/accounts/check" => json(r#"{"accounts":[{"id":"acct-pro","name":"Personal Pro","plan_type":"pro","structure":"personal"}]}"#.to_string()),
        _ => FakeResponse::status(404),
    })
    .await;
    let (app, _dir) = backend_app(&server, vec![device_account("codex-pro", "acct-pro", 100)]);
    let snapshot = app
        .refresh_account_quota("codex-pro")
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    // Codex's own account check, as a native Codex sends it: no query, no
    // browser headers, a Codex User-Agent and the account's credential.
    let check = server
        .requests()
        .into_iter()
        .find(|request| {
            by_path(request, "/backend-api/wham/accounts/check") && request.query.is_empty()
        })
        .unwrap_or_else(|| panic!("quota refresh did not read Codex's account check"));
    assert!(
        check.header("user-agent").starts_with("codex")
            && check.header("chatgpt-account-id") == "acct-pro"
            && check.header("authorization").starts_with("Bearer "),
        "account check headers are not Codex's"
    );
    for browser in [
        "origin",
        "referer",
        "sec-fetch-mode",
        "x-openai-target-path",
        "oai-language",
    ] {
        assert_eq!(
            check.header(browser),
            "",
            "account check carried the browser header {browser}"
        );
    }
    assert!(
        snapshot.plan_type == "pro"
            && snapshot.plan_limit.is_empty()
            && snapshot.organization_name.is_empty(),
        "{snapshot:?}"
    );
    let item = account(&app, 0);
    assert!(
        item.plan_limit.is_empty() && item.label == "codex-pro",
        "account inferred a generic Pro plan limit"
    );
    let body = public_dashboard(&app).await;
    assert!(
        body.contains("Credential 1")
            && body.contains("Pro")
            && !body.contains("Pro 20x")
            && !body.contains(r#""planLimit":"20x""#),
        "public dashboard inferred Pro plan limit: {body}"
    );
}

// Go TestCodexQuotaErrorDoesNotPersistUpstreamBody.
#[tokio::test]
async fn codex_quota_error_does_not_persist_upstream_body() {
    let server = FakeServer::start(|_| {
        FakeResponse::json(
            401,
            r#"{"error":{"code":"invalid_token","message":"secret-body-token"}}"#,
        )
    })
    .await;
    let (app, _dir) = backend_app(&server, vec![device_account("codex-quota-error", "", 100)]);
    let token = fake_jwt(Timestamp::now().add(hours(1)));
    write_auth(
        &app,
        "codex-quota-error",
        &format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{token}","refresh_token":"<refresh-token>","account_id":"acct-chatgpt"}}}}"#
        ),
    );
    let error = app
        .refresh_account_quota("codex-quota-error")
        .await
        .err()
        .unwrap_or_else(|| panic!("quota refresh with upstream 401 returned no error"));
    assert!(
        !error.to_string().contains("secret-body-token"),
        "quota error exposed upstream body"
    );
    let snapshot = app
        .read()
        .state
        .quotas()
        .get("codex-quota-error")
        .cloned()
        .unwrap_or_default();
    let stored = snapshot.quota_error.unwrap_or_default();
    assert_eq!(
        stored.code, "invalid_token",
        "quota error was not persisted with code"
    );
    assert!(
        !stored.message.contains("secret-body-token"),
        "persisted quota error exposed upstream body"
    );
    let (status, reason) = app
        .read()
        .account_status(&account(&app, 0), Timestamp::now());
    assert!(
        status == "error" && reason.contains("invalid_token"),
        "quota refresh failure status = {status}/{reason}"
    );
    let body = public_dashboard(&app).await;
    for internal in ["invalid_token", "secret-body-token", "acct-chatgpt"] {
        assert!(
            !body.contains(internal),
            "public dashboard exposed quota failure internals: {body}"
        );
    }
}

// Go TestQuotaRefreshDoesNotPerpetuateStoredPlanLimit.
#[tokio::test]
async fn quota_refresh_does_not_perpetuate_stored_plan_limit() {
    for (name, check, want) in [
        (
            "metadata reports the new multiplier",
            r#"{"accounts":[{"id":"acct-pro","plan_type":"pro","plan_limit":"10x","structure":"personal"}]}"#,
            "10x",
        ),
        (
            "metadata reports no multiplier",
            r#"{"accounts":[{"id":"acct-pro","plan_type":"pro","structure":"personal"}]}"#,
            "",
        ),
    ] {
        let server = FakeServer::start(move |request| match request.path.as_str() {
            "/backend-api/wham/usage" => json(
                r#"{"plan_type":"pro","rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":10,"limit_window_seconds":604800}}}"#.to_string(),
            ),
            "/backend-api/wham/accounts/check" => json(check.to_string()),
            _ => FakeResponse::status(404),
        })
        .await;
        let item = Account {
            plan_type: "pro".into(),
            plan_family: "pro".into(),
            raw_plan_type: "chatgptpro".into(),
            plan_limit: "20x".into(),
            ..device_account("codex-pro", "acct-pro", 100)
        };
        let (app, _dir) = backend_app(&server, vec![item]);
        let snapshot = app
            .refresh_account_quota("codex-pro")
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            (
                snapshot.plan_limit.as_str(),
                account(&app, 0).plan_limit.as_str()
            ),
            (want, want),
            "{name}"
        );
    }
}

// Go TestResetCreditDetailsFailureRetainsKnownDateWithoutBlockingQuota.
#[tokio::test]
async fn reset_credit_details_failure_retains_known_date_without_blocking_quota() {
    let server = FakeServer::start(|_| FakeResponse::status(503)).await;
    let base = server.url.clone();
    let (app, _dir) =
        super::testing::test_app_with(Vec::new(), |settings| settings.codex_base_url = base);
    let now = Timestamp::parse("2026-09-04T12:00:00Z").unwrap_or_else(|error| panic!("{error}"));
    let expires_at = now.add(hours(24)).unix();
    let mut current = AccountQuota {
        reset_credits: Some(QuotaResetCredits {
            available_count: Some(1),
            ..QuotaResetCredits::default()
        }),
        ..AccountQuota::default()
    };
    let prior = QuotaResetCredits {
        available_count: Some(1),
        expires_at: Some(expires_at),
        details_observed_at: now.add(-hours(6)),
        ..QuotaResetCredits::default()
    };
    let auth = super::metadata::CodexAuthInfo {
        access_token: "<access-token>".into(),
        ..super::metadata::CodexAuthInfo::default()
    };
    app.enrich_reset_credit_expiration("", &auth, &mut current, Some(&prior), now)
        .await;
    assert_eq!(server.count(), 1, "reset-credit details hits");
    assert_eq!(
        current.reset_credits.and_then(|credits| credits.expires_at),
        Some(expires_at),
        "optional details failure discarded known expiry"
    );
    assert!(
        app.read().state.quotas().is_empty(),
        "optional details failure changed quota state"
    );
}

// The repair check of refreshAccountQuotaForRepair: a sign-in that produced
// another upstream account is refused before anything is written.
#[tokio::test]
async fn repair_refresh_refuses_another_upstream_account() {
    let server = FakeServer::start(|request| match request.path.as_str() {
        "/backend-api/wham/usage" => json(
            r#"{"plan_type":"plus","rate_limit":{"allowed":true,"limit_reached":false}}"#
                .to_string(),
        ),
        _ => FakeResponse::status(404),
    })
    .await;
    let (app, _dir) = backend_app(&server, vec![device_account("slot", "", 0)]);
    let token = fake_jwt(Timestamp::now().add(hours(1)));
    write_auth(
        &app,
        "slot",
        &format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"{token}","refresh_token":"<refresh-token>","account_id":"acct-new"}}}}"#
        ),
    );
    let result = app
        .refresh_account_quota_for_repair("slot", "acct-old")
        .await;
    assert_eq!(result.err(), Some(RefreshError::RepairIdentityMismatch));
    assert!(
        app.read()
            .state
            .quotas()
            .get("slot")
            .is_none_or(|snapshot| snapshot.quota.is_none()),
        "a refused repair wrote quota state"
    );
}

// ---- reset credits (reset_credit_test.go) ----

// Go TestNativeResetCreditChoiceIsThePickersFirst: available only, soonest
// expiry first, one without an expiry last, ties in upstream's order (tui
// reset_credits.rs reset_credit_options).
#[test]
fn native_reset_credit_choice_is_the_pickers_first() {
    use super::reset_credit::ResetCreditListing;
    use super::reset_credit::native_reset_credit_choice;
    for (name, raw, want) in [
        (
            "soonest expiry first",
            r#"{"available_count":3,"credits":[{"id":"late","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-11-30T00:00:00Z"},{"id":"never","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":null},{"id":"soon","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-10-22T00:00:00Z"}]}"#,
            "soon",
        ),
        (
            "only available credits",
            r#"{"available_count":1,"credits":[{"id":"spent","status":"redeemed","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-10-21T00:00:00Z"},{"id":"busy","status":"redeeming","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-10-21T00:00:00Z"},{"id":"left","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-12-01T00:00:00Z"}]}"#,
            "left",
        ),
        (
            "ties keep upstream order",
            r#"{"available_count":2,"credits":[{"id":"first","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-11-01T00:00:00Z"},{"id":"second","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-11-01T00:00:00Z"}]}"#,
            "first",
        ),
        // app-server rejects a listing with a timestamp it cannot parse, and
        // the picker falls back to an option without an id.
        (
            "unparsable expiry names none",
            r#"{"available_count":1,"credits":[{"id":"odd","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"tomorrow"}]}"#,
            "",
        ),
        (
            "unparsable grant names none",
            r#"{"available_count":1,"credits":[{"id":"odd","status":"available","granted_at":"","expires_at":"2026-11-01T00:00:00Z"}]}"#,
            "",
        ),
        (
            "no available count names none",
            r#"{"available_count":0,"credits":[{"id":"x","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":null}]}"#,
            "",
        ),
    ] {
        let listing: ResetCreditListing =
            serde_json::from_str(raw).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(native_reset_credit_choice(&listing), want, "{name}");
    }
}

/// Go resetCreditUpstream: the listing, the consume answers in order (the
/// last repeats), and a usage read for the refresh after a verdict.
fn reset_credit_upstream(
    answers: Vec<FakeResponse>,
) -> impl Fn(&RecordedRequest) -> FakeResponse + Send + Sync + 'static {
    let answers = std::sync::Mutex::new(answers);
    move |request| {
        match request.path.as_str() {
        "/backend-api/wham/rate-limit-reset-credits" => json(
            r#"{"available_count":2,"credits":[{"id":"credit-late","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-12-01T00:00:00Z"},{"id":"credit-soon","status":"available","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-10-22T00:00:00Z"}]}"#.to_string(),
        ),
        "/backend-api/wham/rate-limit-reset-credits/consume" => {
            let mut answers = answers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let answer = &answers[0];
            let copy = FakeResponse {
                status: answer.status,
                headers: answer.headers.clone(),
                chunks: answer.chunks.clone(),
            };
            if answers.len() > 1 {
                answers.remove(0);
            }
            copy
        }
        _ => json(
            r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_after_seconds":18000}},"rate_limit_reset_credits":{"available_count":1}}"#.to_string(),
        ),
    }
    }
}

async fn reset_credit_app(server: &FakeServer) -> (Arc<App>, TempDir) {
    let (app, dir) = backend_app(server, vec![device_account("acct", "", 100)]);
    super::testing::write_test_codex_auth_tokens(&app, "acct", "<access-token>", "<refresh-token>");
    app.write().state.quotas_mut().insert(
        "acct".into(),
        super::state::QuotaSnapshot {
            account_id: "acct".into(),
            quota: Some(AccountQuota {
                reset_credits: Some(QuotaResetCredits {
                    available_count: Some(2),
                    ..QuotaResetCredits::default()
                }),
                ..AccountQuota::default()
            }),
            ..super::state::QuotaSnapshot::default()
        },
    );
    (app, dir)
}

/// Go pressResetCredit: the owner's button, through the admin route with a
/// signed-in session.
async fn press_reset_credit(app: &Arc<App>) -> (u16, serde_json::Value) {
    let session = super::admin_tests::admin_session(app);
    let result = super::proxy_tests::serve(
        app,
        super::admin_tests::admin_request(
            &session,
            http::Method::POST,
            "/admin/api/accounts/acct/quota/reset-credit",
            "",
        ),
    )
    .await;
    (
        result.code(),
        serde_json::from_slice(&result.body).unwrap_or_default(),
    )
}

/// The consume bodies, and how many listings came before each: a resend must
/// not list again, while a new spend does.
fn consumes(server: &FakeServer) -> (Vec<String>, Vec<usize>) {
    let mut listings = 0;
    let mut bodies = Vec::new();
    let mut listings_at = Vec::new();
    for request in server.requests() {
        match request.path.as_str() {
            "/backend-api/wham/rate-limit-reset-credits" => listings += 1,
            "/backend-api/wham/rate-limit-reset-credits/consume" => {
                bodies.push(request.body_string());
                listings_at.push(listings);
            }
            _ => {}
        }
    }
    (bodies, listings_at)
}

// Go TestResetCreditSpendsTheSoonestExpiringCredit (with Go
// TestConsumeResetCreditBodyIsCodexs's field order on the wire).
#[tokio::test]
async fn reset_credit_spends_the_soonest_expiring_credit() {
    let server = FakeServer::start(reset_credit_upstream(vec![json(
        r#"{"code":"reset"}"#.to_string(),
    )]))
    .await;
    let (app, _dir) = reset_credit_app(&server).await;
    let (code, body) = press_reset_credit(&app).await;
    assert!(
        code == 200 && body["consumed"] == true,
        "reset returned {code} {body}"
    );
    let (bodies, listings_at) = consumes(&server);
    assert!(
        listings_at[0] > 0,
        "the spend did not list the credits first"
    );
    let sent: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap_or_default();
    assert!(
        sent["credit_id"] == "credit-soon"
            && sent["redeem_request_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
        "consume body {}",
        bodies[0]
    );
    assert!(
        bodies[0].starts_with(r#"{"redeem_request_id":"#),
        "consume body must keep Codex's field order: {}",
        bodies[0]
    );
    // The credit id is upstream's identifier: it goes to the consume request
    // and nowhere else.
    assert!(
        !body.to_string().contains("credit-soon"),
        "the credit id reached the admin response"
    );
}

// Go TestResetCreditRetryResendsTheFailedAttempt: a failed spend is resent
// unchanged by the next press, as Codex's "Try again" resends it.
#[tokio::test]
async fn reset_credit_retry_resends_the_failed_attempt() {
    let server = FakeServer::start(reset_credit_upstream(vec![
        FakeResponse::status(502),
        json(r#"{"code":"already_redeemed"}"#.to_string()),
        json(r#"{"code":"reset"}"#.to_string()),
    ]))
    .await;
    let (app, _dir) = reset_credit_app(&server).await;
    assert_eq!(press_reset_credit(&app).await.0, 502, "failed spend");
    let (code, body) = press_reset_credit(&app).await;
    assert!(
        code == 200 && body["consumed"] == true,
        "resent spend returned {code} {body}"
    );
    let (bodies, listings_at) = consumes(&server);
    assert!(
        bodies.len() == 2 && bodies[0] == bodies[1] && listings_at[1] == listings_at[0],
        "retry was not a resend: {listings_at:?} {bodies:?}"
    );
    // The verdict ended that attempt: the next press is a new spend.
    assert_eq!(press_reset_credit(&app).await.0, 200, "next spend");
    let (bodies, listings_at) = consumes(&server);
    assert!(
        listings_at[2] > listings_at[1] && bodies[2] != bodies[1],
        "a spend after a verdict reused the old attempt: {listings_at:?}"
    );
}

// Go TestResetCreditNamedCreditGone: no_credit for a named credit means that
// credit is gone, not that the account has none.
#[tokio::test]
async fn reset_credit_named_credit_gone() {
    let server = FakeServer::start(reset_credit_upstream(vec![json(
        r#"{"code":"no_credit"}"#.to_string(),
    )]))
    .await;
    let (app, _dir) = reset_credit_app(&server).await;
    let (code, body) = press_reset_credit(&app).await;
    assert!(
        code == 200
            && body["consumed"] == false
            && body["message"]
                .as_str()
                .is_some_and(|message| message.contains("no longer available")),
        "named no_credit returned {code} {body}"
    );
}

async fn reset_app_with_credits(server: &FakeServer, credits: i64) -> (Arc<App>, TempDir) {
    let (app, dir) = reset_credit_app(server).await;
    if let Some(snapshot) = app.write().state.quotas_mut().get_mut("acct")
        && let Some(reset) = snapshot
            .quota
            .as_mut()
            .and_then(|quota| quota.reset_credits.as_mut())
    {
        reset.available_count = Some(credits);
    }
    (app, dir)
}

// Go TestAdminResetCreditConsumesAndRefreshes.
#[tokio::test]
async fn admin_reset_credit_consumes_and_refreshes() {
    let server = FakeServer::start(|request| match request.path.as_str() {
        "/backend-api/wham/rate-limit-reset-credits/consume" => {
            // Upstream reads redeem_request_id and rejects the request when it
            // is absent; only the name upstream actually reads passes here.
            if request.json()["redeem_request_id"].as_str().unwrap_or_default().is_empty() {
                return FakeResponse::json(400, r#"{"detail":"redeem_request_id must not be empty"}"#);
            }
            json(r#"{"code":"reset","windows_reset":1}"#.to_string())
        }
        "/backend-api/wham/usage" => json(
            r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_after_seconds":18000}},"rate_limit_reset_credits":{"available_count":2}}"#.to_string(),
        ),
        _ => json("{}".to_string()),
    })
    .await;
    let (app, _dir) = reset_app_with_credits(&server, 3).await;
    let (code, body) = press_reset_credit(&app).await;
    assert_eq!(code, 200, "reset credit: {body}");
    assert!(
        body["outcome"] == "reset"
            && body["consumed"] == true
            && body["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
        "outcome not reported: {body}"
    );
    let requests = server.requests();
    let consumed: Vec<_> = requests
        .iter()
        .filter(|request| request.path == "/backend-api/wham/rate-limit-reset-credits/consume")
        .collect();
    assert_eq!(
        consumed.len(),
        1,
        "one credit spent means exactly one upstream consume"
    );
    // An id upstream rejects is indistinguishable from one it never saw, so a
    // retry could spend a second credit. Upstream documents a UUID.
    let key = consumed[0].json()["redeem_request_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let parts: Vec<&str> = key.split('-').collect();
    assert!(
        parts.iter().map(|part| part.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12]
            && key.chars().all(|character| character == '-'
                || character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
            && parts[2].starts_with('4')
            && parts[3].starts_with(['8', '9', 'a', 'b']),
        "consume request key {key:?} is not a version 4 UUID"
    );
    // The reset changes the windows the dashboard shows, so it must re-read
    // them.
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/backend-api/wham/usage"),
        "quota was not refreshed after the reset"
    );
}

// Go TestAdminResetCreditReportsARefusalHonestly.
#[tokio::test]
async fn admin_reset_credit_reports_a_refusal_honestly() {
    let server = FakeServer::start(|request| {
        if request.path == "/backend-api/wham/rate-limit-reset-credits/consume" {
            return json(r#"{"code":"nothing_to_reset"}"#.to_string());
        }
        json(r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_after_seconds":18000}}}"#.to_string())
    })
    .await;
    let (app, _dir) = reset_app_with_credits(&server, 1).await;
    let (code, body) = press_reset_credit(&app).await;
    assert_eq!(code, 200, "declined reset: {body}");
    // The pool sanitizes metadata to lowercase; upstream's spelling is
    // snake_case.
    assert!(
        body["outcome"] == "nothing_to_reset" && body["consumed"] == false,
        "a refusal was reported as a spend: {body}"
    );
}

// Go TestAdminResetCreditRefusesWithoutCredits.
#[tokio::test]
async fn admin_reset_credit_refuses_without_credits() {
    let server = FakeServer::start(|_| json(r#"{"code":"reset"}"#.to_string())).await;
    let (app, _dir) = reset_app_with_credits(&server, 0).await;
    let (code, _) = press_reset_credit(&app).await;
    assert_eq!(code, 409, "reset without credits");
    assert_eq!(
        server.count(),
        0,
        "upstream was called with no credit available"
    );
}

// ---- the poll and the quota prime ----

use super::state::QuotaSnapshot;
use super::state::QuotaWindow;

fn week_window(reset_at: Option<i64>, remaining: f64) -> QuotaWindow {
    QuotaWindow {
        label: "Week".into(),
        window_minutes: Some(10080),
        reset_at,
        remaining_percent: Some(remaining),
        present: true,
        observed: true,
        ..QuotaWindow::default()
    }
}

fn windows_quota(windows: Vec<QuotaWindow>) -> AccountQuota {
    AccountQuota {
        windows,
        ..AccountQuota::default()
    }
}

// Go TestUnanchoredBudgetWindowDetection.
#[test]
fn unanchored_budget_window_detection() {
    use super::prime::unanchored_budget_window;
    use super::time::minutes;
    let now = Timestamp::parse("2026-09-08T02:00:00Z").unwrap_or_else(|error| panic!("{error}"));
    let at = |delta: chrono::TimeDelta| Some(now.add(delta).unix());
    for (name, quota, want) in [
        (
            "full window ahead and untouched",
            windows_quota(vec![week_window(at(hours(7 * 24)), 100.0)]),
            true,
        ),
        (
            "snapshot age stays within the slack",
            windows_quota(vec![week_window(at(hours(7 * 24) - minutes(4)), 100.0)]),
            true,
        ),
        (
            "already counting down",
            windows_quota(vec![week_window(at(hours(7 * 24) - minutes(50)), 100.0)]),
            false,
        ),
        (
            "untouched but well into the window",
            windows_quota(vec![week_window(at(hours(3 * 24)), 100.0)]),
            false,
        ),
        (
            "already spent against",
            windows_quota(vec![week_window(at(hours(7 * 24)), 96.0)]),
            false,
        ),
        (
            "no reset reported",
            windows_quota(vec![week_window(None, 100.0)]),
            false,
        ),
        ("nothing reported", AccountQuota::default(), false),
    ] {
        assert_eq!(
            unanchored_budget_window(&quota, now).is_some(),
            want,
            "{name}"
        );
    }
}

// Go TestUnanchoredBudgetWindowPrefersTheLongestWindow.
#[test]
fn unanchored_budget_window_prefers_the_longest_window() {
    let now = Timestamp::parse("2026-09-08T02:00:00Z").unwrap_or_else(|error| panic!("{error}"));
    let five_hours = QuotaWindow {
        label: "5h".into(),
        window_minutes: Some(300),
        ..week_window(Some(now.add(hours(5)).unix()), 100.0)
    };
    let quota = windows_quota(vec![
        five_hours,
        week_window(Some(now.add(hours(7 * 24)).unix()), 100.0),
    ]);
    assert_eq!(
        super::prime::unanchored_budget_window(&quota, now).map(|window| window.label),
        Some("Week".to_string())
    );
}

fn unanchored_snapshot() -> QuotaSnapshot {
    QuotaSnapshot {
        account_id: "only".into(),
        quota: Some(windows_quota(vec![week_window(
            Some(Timestamp::now().add(hours(7 * 24)).unix()),
            100.0,
        )])),
        ..QuotaSnapshot::default()
    }
}

fn anchored_snapshot() -> QuotaSnapshot {
    QuotaSnapshot {
        account_id: "only".into(),
        quota: Some(windows_quota(vec![week_window(
            Some(Timestamp::now().add(hours(7 * 24 - 1)).unix()),
            100.0,
        )])),
        ..QuotaSnapshot::default()
    }
}

/// Go's provider slot that a prime reaches over HTTP: {ID: "only", AuthType:
/// provider_api_key, AllowedModels: [gpt-test]}.
fn prime_slot(server: &FakeServer, enabled: bool, in_pool: bool) -> Account {
    Account {
        enabled,
        in_pool,
        allowed_models: vec!["gpt-test".into()],
        ..super::proxy_tests::api_slot("only", "", 100, &server.url)
    }
}

/// Go testApp with the prime grace fixed at zero, as Go's testApp sets it.
fn prime_app(accounts: Vec<Account>, grace: std::time::Duration) -> (Arc<App>, TempDir) {
    let (app, dir) = super::testing::test_app_with(accounts, |settings| {
        settings.quota_prime_grace = Some(grace)
    });
    (Arc::new(app), dir)
}

// Go TestQuotaWindowPrimeSpendsAndLeavesNoRoutingTrace.
#[tokio::test]
async fn quota_window_prime_spends_and_leaves_no_routing_trace() {
    let server =
        FakeServer::start(|_| FakeResponse::json(200, r#"{"id":"resp_prime","output":[]}"#)).await;
    let (app, _dir) = prime_app(
        vec![prime_slot(&server, true, true)],
        std::time::Duration::ZERO,
    );
    let snapshot = unanchored_snapshot();
    // Nothing is primed while the owner has priming off.
    app.write().quota_prime = false;
    app.prime_unanchored_quota_window("only", &snapshot).await;
    assert!(
        server.count() == 0 && app.read().state.quota_window_primes().is_empty(),
        "a prime was sent with priming off"
    );
    app.write().quota_prime = true;
    app.prime_unanchored_quota_window("only", &snapshot).await;
    assert_eq!(server.count(), 1, "prime request count");
    assert_eq!(
        server.requests()[0].json()["model"],
        "gpt-test",
        "prime must name the slot's allowed model"
    );
    {
        let inner = app.read();
        let state = &inner.state;
        assert!(
            state.sticky_sessions().is_empty()
                && state.response_bindings().is_empty()
                && state.routing_cache_events().is_empty(),
            "prime left routing state behind"
        );
        assert_eq!(
            (
                state.request_count,
                state.success_count,
                state.failure_count
            ),
            (0, 0, 0),
            "prime counted as a client request"
        );
        let health = state.health().get("only").cloned().unwrap_or_default();
        assert!(
            health.last_success_at.is_zero() && health.consecutive_failure == 0,
            "prime touched account health"
        );
    }
    // The interval floor must stop a second prime on the same window.
    app.prime_unanchored_quota_window("only", &snapshot).await;
    assert_eq!(
        server.count(),
        1,
        "prime repeated inside the interval floor"
    );
}

// Go TestQuotaWindowPrimeCoversOutOfPoolSlots.
#[tokio::test]
async fn quota_window_prime_covers_out_of_pool_slots() {
    let server =
        FakeServer::start(|_| FakeResponse::json(200, r#"{"id":"resp_prime","output":[]}"#)).await;
    let (app, _dir) = prime_app(
        vec![prime_slot(&server, true, false)],
        std::time::Duration::ZERO,
    );
    app.write().quota_prime = true;
    app.prime_unanchored_quota_window("only", &unanchored_snapshot())
        .await;
    assert_eq!(server.count(), 1, "out-of-pool slot was not primed");
}

// Go TestQuotaPrimeIdentityOwnerIsStableAcrossPoolMembership.
#[test]
fn quota_prime_identity_owner_is_stable_across_pool_membership() {
    let slot = |id: &str, workspace: &str, email: &str, in_pool: bool| Account {
        email: email.into(),
        in_pool,
        ..device_account(id, workspace, 100)
    };
    let (app, _dir) = super::testing::test_app(vec![
        slot("slot-b", "workspace-1", "a@example.test", true),
        slot("slot-a", "workspace-1", "a@example.test", false),
        slot("slot-c", "workspace-2", "b@example.test", true),
    ]);
    for id in ["slot-a", "slot-b", "slot-c"] {
        super::testing::write_test_codex_auth_tokens(&app, id, "<access-token>", "<refresh-token>");
    }
    let inner = app.read();
    // The out-of-pool copy still competes for ownership, which the routing
    // duplicate guard would not have allowed.
    for id in ["slot-a", "slot-b"] {
        let item = inner.account(id).cloned().unwrap_or_default();
        assert_eq!(
            inner.quota_prime_identity_owner(&item),
            "slot-a",
            "identity owner for {id}"
        );
    }
    let other = inner.account("slot-c").cloned().unwrap_or_default();
    assert_eq!(inner.quota_prime_identity_owner(&other), "slot-c");
}

// Go TestQuotaWindowPrimeSkipsAnchoredAndIneligibleAccounts.
#[tokio::test]
async fn quota_window_prime_skips_anchored_and_ineligible_accounts() {
    let server =
        FakeServer::start(|_| FakeResponse::json(200, r#"{"id":"resp_prime","output":[]}"#)).await;
    let (app, _dir) = prime_app(
        vec![prime_slot(&server, true, true)],
        std::time::Duration::ZERO,
    );
    app.write().quota_prime = true;
    app.prime_unanchored_quota_window("only", &anchored_snapshot())
        .await;
    assert_eq!(server.count(), 0, "anchored window was primed");
    // A disabled slot is not in service at all, so nothing may be spent on it.
    let (off, _off_dir) = prime_app(
        vec![prime_slot(&server, false, true)],
        std::time::Duration::ZERO,
    );
    off.write().quota_prime = true;
    off.prime_unanchored_quota_window("only", &unanchored_snapshot())
        .await;
    assert_eq!(server.count(), 0, "disabled slot was primed");
    // A failed refresh carries no evidence about the window.
    let mut errored = unanchored_snapshot();
    errored.quota_error = Some(super::state::QuotaErrorInfo {
        code: "token_expired".into(),
        ..super::state::QuotaErrorInfo::default()
    });
    let (broken, _broken_dir) = prime_app(
        vec![prime_slot(&server, true, true)],
        std::time::Duration::ZERO,
    );
    broken.write().quota_prime = true;
    broken.prime_unanchored_quota_window("only", &errored).await;
    assert_eq!(server.count(), 0, "errored refresh was primed");
}

// Go TestQuotaPrimeWaitsForTheUserFirst.
#[tokio::test]
async fn quota_prime_waits_for_the_user_first() {
    let server =
        FakeServer::start(|_| FakeResponse::json(200, r#"{"id":"resp_prime","output":[]}"#)).await;
    let (app, _dir) = prime_app(
        vec![prime_slot(&server, true, true)],
        std::time::Duration::from_secs(600),
    );
    app.write().quota_prime = true;
    app.prime_unanchored_quota_window("only", &unanchored_snapshot())
        .await;
    app.prime_unanchored_quota_window("only", &unanchored_snapshot())
        .await;
    assert_eq!(server.count(), 0, "primed inside the grace");
    let due = app
        .prime_grace_due("only")
        .unwrap_or_else(|| panic!("no grace is waiting"));
    let left = due.sub(&Timestamp::now());
    assert!(
        left >= super::time::minutes(9) && left <= super::time::minutes(10),
        "grace due in {left:?}"
    );
    // The user used the account: the window anchored and the wait is dropped.
    app.prime_unanchored_quota_window("only", &anchored_snapshot())
        .await;
    assert!(
        app.prime_grace_due("only").is_none() && server.count() == 0,
        "an anchored window kept its wait or was primed"
    );
    // Untouched again, and this time the grace runs out.
    app.prime_unanchored_quota_window("only", &unanchored_snapshot())
        .await;
    app.prime_grace
        .set("only", Timestamp::now().add(-super::time::seconds(1)));
    app.prime_unanchored_quota_window("only", &unanchored_snapshot())
        .await;
    assert_eq!(
        server.count(),
        1,
        "an untouched window was not primed after the grace"
    );
    assert!(
        app.prime_grace_due("only").is_none(),
        "the wait outlived its prime"
    );
}

// Go TestQuotaPrimeGraceIsRandomUnlessFixedForTests (the random draw; the
// fixed override's parsing is settings.rs quota_prime_grace).
#[test]
fn quota_prime_grace_is_random_unless_fixed_for_tests() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..50 {
        let grace = super::prime::random_quota_prime_grace();
        assert!(
            grace >= std::time::Duration::from_secs(300)
                && grace <= std::time::Duration::from_secs(1800),
            "grace {grace:?} outside [5m, 30m]"
        );
        seen.insert(grace);
    }
    assert!(
        seen.len() >= 40,
        "the grace is not random: {} distinct in 50",
        seen.len()
    );
    assert_eq!(
        super::settings::quota_prime_grace("0s"),
        Ok(Some(std::time::Duration::ZERO)),
        "a fixed grace was not honored"
    );
    assert!(
        super::settings::quota_prime_grace("soon").is_err(),
        "an invalid grace was accepted"
    );
}

/// What a stand-in prime runner saw: its home, the auth.json, config.toml
/// and installation id written there, and the model.
type SeenPrime = (std::path::PathBuf, String, String, String, String);

// Go TestQuotaPrimeRunsCodexWithoutTheRefreshToken.
#[tokio::test]
async fn quota_prime_runs_codex_without_the_refresh_token() {
    let (app, _dir) = super::testing::test_app(vec![device_account("device", "acct-device", 0)]);
    super::testing::write_codex_device_auth(&app, "device", "acct-device", "device@example.test");
    let auth_path = app.read().account_codex_home("device").join("auth.json");
    let before = std::fs::read(&auth_path).unwrap_or_default();
    let item = account(&app, 0);
    let current = app
        .read()
        .codex_auth(&item)
        .unwrap_or_else(|error| panic!("{error}"));
    let installation = app.presented_installation_id(&item).unwrap_or_default();
    let seen: Arc<std::sync::Mutex<Option<SeenPrime>>> = Arc::default();
    let record = seen.clone();
    app.set_prime_runner(Arc::new(
        move |home: &std::path::Path, workdir: &std::path::Path, model: &str| {
            let auth = std::fs::read_to_string(home.join("auth.json")).unwrap_or_default();
            let config = std::fs::read_to_string(home.join("config.toml")).unwrap_or_default();
            let id = std::fs::read_to_string(home.join("installation_id")).unwrap_or_default();
            let workdir_ok = if workdir.as_os_str().is_empty() {
                String::new()
            } else {
                model.to_string()
            };
            if let Ok(mut seen) = record.lock() {
                *seen = Some((home.to_path_buf(), auth, config, id, workdir_ok));
            }
            Box::pin(async { Ok(()) })
        },
    ));
    app.send_quota_prime_request(&item, "gpt-test")
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let (home, auth, config, id, model) = seen
        .lock()
        .ok()
        .and_then(|seen| seen.clone())
        .unwrap_or_else(|| panic!("a Codex account was not primed by Codex"));
    let file: serde_json::Value = serde_json::from_str(&auth).unwrap_or_default();
    assert_eq!(
        file["tokens"]["refresh_token"], "",
        "the prime's Codex must have no refresh token: refreshing would revoke the pool's copy"
    );
    assert!(
        file["auth_mode"] == "chatgpt"
            && file["tokens"]["access_token"] == current.access_token.as_str()
            && file["tokens"]["account_id"] == "acct-device"
            && file["last_refresh"]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
        "prime credential = {auth}"
    );
    assert!(
        config.contains(r#"model = "gpt-test""#)
            && config.contains(r#"cli_auth_credentials_store = "file""#),
        "prime config = {config}"
    );
    assert_eq!(model, "gpt-test", "prime model or workdir");
    // The prime's Codex presents the account's installation id, not one made
    // up for the throwaway home.
    assert!(
        !installation.is_empty() && id == installation,
        "prime installation id"
    );
    assert!(
        !home.exists(),
        "the prime's home must be removed afterwards"
    );
    assert_eq!(
        std::fs::read(&auth_path).unwrap_or_default(),
        before,
        "the prime changed the account's own credential"
    );
}

// Go TestCodexChildEnvironmentCarriesNoPoolSecrets.
#[test]
fn codex_child_environment_carries_no_pool_secrets() {
    let environment = |name: &str| match name {
        "CODEX_POOL_API_KEY" => Some("<pool-key>".to_string()),
        "CODEX_POOL_ADMIN_PASSWORD_HASH" => Some("<admin-hash>".to_string()),
        "SSL_CERT_FILE" => Some("/certs/ca.pem".to_string()),
        _ => None,
    };
    let env =
        super::prime::codex_child_environment(std::path::Path::new("/tmp/prime/home"), environment);
    let joined: Vec<String> = env
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    assert!(
        !joined.iter().any(|line| line.contains("CODEX_POOL_")),
        "Codex child saw pool secrets: {joined:?}"
    );
    assert!(
        joined.contains(&"CODEX_HOME=/tmp/prime/home".to_string())
            && joined.contains(&"SSL_CERT_FILE=/certs/ca.pem".to_string()),
        "{joined:?}"
    );
}

// Go TestRevokedCredentialStopsQuotaPollingUntilLoginChangesIt.
#[tokio::test]
async fn revoked_credential_stops_quota_polling_until_login_changes_it() {
    let revoked = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = revoked.clone();
    let server = FakeServer::start(move |_| {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            // The code upstream answered for a deactivated account (production
            // log, 2026-10-21); Codex has no fixture for it.
            return FakeResponse::json(401, r#"{"error":{"code":"token_revoked"}}"#);
        }
        json(r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_after_seconds":18000}}}"#.to_string())
    })
    .await;
    let (app, _dir) = backend_app(&server, vec![device_account("gone", "", 100)]);
    super::testing::write_test_codex_auth_tokens(&app, "gone", "<access-token>", "<refresh-token>");
    app.refresh_all_codex_quotas().await;
    let first = server.count();
    assert!(first > 0, "the first poll did not ask upstream");
    assert!(
        app.credential_refused(&account(&app, 0)),
        "a 401 token_revoked was not recorded as a refused credential"
    );
    for _ in 0..3 {
        app.refresh_all_codex_quotas().await;
    }
    assert_eq!(
        server.count(),
        first,
        "polls after the refusal asked upstream again"
    );
    // Someone logs in again: new tokens, and the next poll asks.
    revoked.store(false, std::sync::atomic::Ordering::SeqCst);
    super::testing::write_test_codex_auth_tokens(
        &app,
        "gone",
        "<new-access-token>",
        "<new-refresh-token>",
    );
    app.refresh_all_codex_quotas().await;
    assert!(server.count() > first, "a new login was not polled");
    assert!(
        !app.credential_refused(&account(&app, 0)),
        "the refusal outlived the login that replaced its tokens"
    );
}

// Go TestTransientQuotaFailureKeepsPolling.
#[tokio::test]
async fn transient_quota_failure_keeps_polling() {
    let server = FakeServer::start(|_| FakeResponse::status(502)).await;
    let (app, _dir) = backend_app(&server, vec![device_account("busy", "", 100)]);
    super::testing::write_test_codex_auth_tokens(&app, "busy", "<access-token>", "<refresh-token>");
    app.refresh_all_codex_quotas().await;
    app.refresh_all_codex_quotas().await;
    assert!(server.count() >= 2, "a 502 stopped polling");
}

// Go TestAccountActivitySetsTheCadence.
#[test]
fn account_activity_sets_the_cadence() {
    let (app, _dir) = super::testing::test_app(Vec::new());
    assert!(
        !app.account_in_use("acct", Timestamp::now()),
        "an account nothing went to is in use"
    );
    app.note_account_activity("acct");
    assert!(
        app.account_in_use("acct", Timestamp::now()),
        "a request did not put the account in use"
    );
    let later = Timestamp::now()
        .add(chrono::TimeDelta::from_std(super::quota_schedule::ACTIVE_WINDOW).unwrap_or_default())
        .add(super::time::seconds(1));
    assert!(
        !app.account_in_use("acct", later),
        "the account stayed in use past the active window"
    );
}
