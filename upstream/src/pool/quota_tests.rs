//! The Go tests of the pool's own upstream work (main_test.go quota refresh,
//! reset_credit_test.go, refused_credential_test.go, quota_schedule_test.go
//! and the priming tests), one by one. Each keeps its Go name in a comment;
//! the assertions are Go's. Parts that read the dashboard join with the
//! dashboard port (step 4.5) and are marked where they are left out.

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

fn by_path(request: &RecordedRequest, path: &str) -> bool {
    request.path == path
}

// Go TestCodexQuotaRefreshUpdatesDashboardState (the dashboard reads join
// with the dashboard in 4.5).
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

// Go TestCodexQuotaRefreshDoesNotInferProPlanLimit (the dashboard read joins
// with the dashboard in 4.5).
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
}

// Go TestCodexQuotaErrorDoesNotPersistUpstreamBody (the dashboard read joins
// with the dashboard in 4.5).
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

/// Go pressResetCredit (the admin session check joins with the admin API in
/// 4.5).
async fn press_reset_credit(app: &App) -> (u16, serde_json::Value) {
    let mut recorder = super::http_io::Recorder::new();
    super::reset_credit::handle_reset_credit(app, "acct", &mut recorder).await;
    let result = recorder.result();
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

// Go TestAdminResetCreditConsumesAndRefreshes (the admin session check joins
// with the admin API in 4.5).
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
