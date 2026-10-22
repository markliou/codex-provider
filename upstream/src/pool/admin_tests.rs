//! The Go admin and dashboard API tests (main_test.go), one by one. Each
//! keeps its Go name in a comment; the assertions are Go's.
//!
//! Go's adminSession logged in with the password; here a session is signed
//! directly (the token and CSRF cookie a login issues), because one PBKDF2
//! verification at 100,000 rounds takes about a second in a debug build. The
//! login tests themselves still log in.

use super::app::App;
use super::http_io::Recorded;
use super::proxy_tests::arc_app;
use super::proxy_tests::serve;
use super::routes::Request;
use super::state::Account;
use super::testing::device_account;
use super::time::Timestamp;
use super::time::hours;
use http::Method;
use std::sync::Arc;

/// An owner session: the Cookie header and the CSRF token.
pub(super) struct Session {
    cookie: String,
    csrf: String,
}

pub(super) fn admin_session(app: &App) -> Session {
    let token = app.sign_session(&app.settings.admin_user, Timestamp::now().add(hours(12)));
    let csrf = super::crypto::random_id();
    Session {
        cookie: format!("codex_pool_session={token}; codex_pool_csrf={csrf}"),
        csrf,
    }
}

pub(super) fn admin_request(
    session: &Session,
    method: Method,
    target: &str,
    body: &str,
) -> Request {
    Request::test(method, target, body)
        .with_header("cookie", &session.cookie)
        .with_header("x-csrf-token", &session.csrf)
}

pub(super) fn body_json(result: &Recorded) -> serde_json::Value {
    serde_json::from_slice(&result.body).unwrap_or_default()
}

fn set_cookies(result: &Recorded) -> Vec<String> {
    result
        .headers
        .get_all(http::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_string))
        .collect()
}

fn cookie_header(result: &Recorded) -> String {
    set_cookies(result)
        .iter()
        .filter_map(|cookie| cookie.split(';').next().map(str::to_string))
        .collect::<Vec<_>>()
        .join("; ")
}

async fn login(app: &Arc<App>, body: &str, remote: &str) -> Recorded {
    let mut request = Request::test(Method::POST, "/admin/api/login", body);
    request.remote_addr = remote.to_string();
    serve(app, request).await
}

// Go TestAdminLoginAcceptsPasswordOnly.
#[tokio::test]
async fn admin_login_accepts_password_only() {
    let (app, _dir) = arc_app(Vec::new());
    let result = login(&app, r#"{"password":"admin-password"}"#, "192.0.2.1:1234").await;
    assert_eq!(
        result.code(),
        200,
        "password-only login: {}",
        result.body_string()
    );
    assert!(
        result.body_string().contains("csrfToken"),
        "password-only login did not return a csrf token"
    );
}

// Go TestAdminLoginRateLimit.
#[tokio::test]
async fn admin_login_rate_limit() {
    let (app, _dir) = arc_app(Vec::new());
    for attempt in 1..=5 {
        let result = login(
            &app,
            r#"{"username":"admin","password":"wrong"}"#,
            "198.51.100.10:1234",
        )
        .await;
        assert_eq!(result.code(), 401, "failed login {attempt}");
    }
    let result = login(
        &app,
        r#"{"username":"admin","password":"admin-password"}"#,
        "198.51.100.10:1234",
    )
    .await;
    assert_eq!(result.code(), 429, "rate-limited login");
}

// Go TestAdminCookieSecureBehindForwardedHTTPS.
#[tokio::test]
async fn admin_cookie_secure_behind_forwarded_https() {
    let (app, _dir) = arc_app(Vec::new());
    let request = Request::test(
        Method::POST,
        "/admin/api/login",
        r#"{"username":"admin","password":"admin-password"}"#,
    )
    .with_header("x-forwarded-proto", "https");
    let result = serve(&app, request).await;
    assert_eq!(result.code(), 200);
    let cookies = set_cookies(&result);
    assert_eq!(cookies.len(), 2);
    for cookie in cookies {
        assert!(
            cookie.starts_with("codex_pool_") && cookie.contains("; Secure"),
            "cookie was not Secure behind forwarded HTTPS: {cookie}"
        );
    }
}

// Go TestAdminLoginAndCSRFMiddleware.
#[tokio::test]
async fn admin_login_and_csrf_middleware() {
    let (app, _dir) = arc_app(Vec::new());
    let result = login(
        &app,
        r#"{"username":"admin","password":"admin-password"}"#,
        "192.0.2.1:1234",
    )
    .await;
    assert_eq!(result.code(), 200);
    let csrf = body_json(&result)["csrfToken"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let cookies = set_cookies(&result);
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.starts_with("codex_pool_session=")
                && cookie.contains("; HttpOnly")
                && cookie.contains("; SameSite=Strict"))
            && cookies.iter().any(
                |cookie| cookie.starts_with("codex_pool_csrf=") && !cookie.contains("HttpOnly")
            )
            && !csrf.is_empty(),
        "login did not establish expected cookies: {cookies:?}"
    );
    let session = Session {
        cookie: cookie_header(&result),
        csrf,
    };
    let create = |body: &'static str| {
        let app = app.clone();
        let request = admin_request(&session, Method::POST, "/admin/api/accounts", body);
        async move { serve(&app, request).await }
    };
    let result = create(r#"{"email":"User@Example.Test","planType":"plus","allowedModels":["gpt-5.5"],"codexHome":"/tmp/evil/.codex"}"#).await;
    assert_eq!(
        result.code(),
        201,
        "admin account create: {}",
        result.body_string()
    );
    let body = result.body_string();
    assert!(
        body.contains(r#""authType":"codex_device_auth""#),
        "account create did not default to Codex device auth: {body}"
    );
    for expected in [
        r#""id":"acct-credential""#,
        r#""email":"""#,
        r#""displayName":"acct-credential""#,
    ] {
        assert!(
            body.contains(expected),
            "account create response missing {expected}: {body}"
        );
    }
    for forbidden in [
        "user@example.test",
        r#""planType":"plus""#,
        "/tmp/evil",
        r#""allowedModels":[""#,
    ] {
        assert!(
            !body.contains(forbidden),
            "account create accepted caller-supplied {forbidden}: {body}"
        );
    }
    let second = create(r#"{"email":"User@Example.Test","planType":"pro"}"#).await;
    assert_eq!(second.code(), 201);
    let second = second.body_string();
    assert!(
        second.contains(r#""id":"acct-credential-2""#)
            && second.contains(r#""displayName":"acct-credential-2""#),
        "{second}"
    );
    for forbidden in [
        "user@example.test",
        "us***@example.test",
        r#""planType":"pro""#,
    ] {
        assert!(
            !second.contains(forbidden),
            "second device-auth account used caller-supplied identity metadata: {second}"
        );
    }
    let provider = create(
        r#"{"authType":"provider_api_key","email":"Provider@Example.Test","planType":"team","upstreamBaseUrl":"https://upstream.example.test","upstreamApiKey":"provider-secret"}"#,
    )
    .await;
    assert_eq!(
        provider.code(),
        201,
        "provider account create: {}",
        provider.body_string()
    );
    let provider = provider.body_string();
    for expected in [
        r#""id":"acct-provider""#,
        r#""displayName":"pr***er@example.test""#,
        r#""email":"pr***er@example.test""#,
    ] {
        assert!(
            provider.contains(expected),
            "provider account create response missing {expected}: {provider}"
        );
    }
    for forbidden in ["provider@example.test", "provider-secret", "provider-team"] {
        assert!(
            !provider.contains(forbidden),
            "provider account create response used sensitive metadata: {provider}"
        );
    }
}

// Go TestManagementAPIsRequireAdminAndCSRF.
#[tokio::test]
async fn management_apis_require_admin_and_csrf() {
    let (app, _dir) = arc_app(vec![device_account("acct", "", 0)]);
    for (method, path, body) in [
        (Method::GET, "/admin/api/accounts", ""),
        (Method::POST, "/admin/api/accounts", r#"{"id":"new"}"#),
        (Method::GET, "/admin/api/accounts/health", ""),
        (Method::POST, "/admin/api/accounts/acct/enable", ""),
        (Method::POST, "/admin/api/accounts/acct/login", ""),
        (Method::POST, "/admin/api/accounts/quota/refresh-all", ""),
        (Method::POST, "/admin/api/jobs/job-id/cancel", ""),
        (Method::DELETE, "/admin/api/accounts/acct", ""),
        (Method::DELETE, "/admin/api/sticky-sessions/key", ""),
    ] {
        let result = serve(&app, Request::test(method.clone(), path, body)).await;
        assert_eq!(result.code(), 401, "{method} {path} without admin");
    }
    let session = admin_session(&app);
    let request = Request::test(Method::POST, "/admin/api/accounts/acct/enable", "")
        .with_header("cookie", &session.cookie);
    assert_eq!(
        serve(&app, request).await.code(),
        403,
        "mutating management API without CSRF"
    );
}

// Go TestAdminLoginKeyTrustsOnlyTheConfiguredProxy.
#[test]
fn admin_login_key_trusts_only_the_configured_proxy() {
    let networks =
        super::settings::trusted_proxies("172.18.0.0/16").unwrap_or_else(|error| panic!("{error}"));
    let (app, _dir) =
        super::testing::test_app_with(Vec::new(), |settings| settings.trusted_proxies = networks);
    let key = |remote: &str, forwarded: &str| {
        let mut request = Request::test(Method::POST, "/admin/api/login", "");
        request.remote_addr = remote.to_string();
        if !forwarded.is_empty() {
            request = request.with_header("x-forwarded-for", forwarded);
        }
        super::admin_api::admin_login_key(&app, &request)
    };
    assert_eq!(
        key("172.18.0.5:41000", "203.0.113.9"),
        "203.0.113.9",
        "behind the proxy the key is the client"
    );
    assert_eq!(
        key("172.18.0.5:41000", "198.51.100.1, 203.0.113.9"),
        "203.0.113.9",
        "a client-supplied hop must not win over the proxy's"
    );
    assert_eq!(
        key("203.0.113.50:5000", "10.0.0.1"),
        "203.0.113.50",
        "a direct client's own forwarded header must be ignored"
    );
    assert!(
        super::settings::trusted_proxies("not-a-network").is_err(),
        "an invalid trusted proxy must stop startup"
    );
}

// Go TestResetCreditRequiresAdminSession.
#[tokio::test]
async fn reset_credit_requires_admin_session() {
    let (app, _dir) = arc_app(vec![device_account("acct", "", 100)]);
    let code = serve(
        &app,
        Request::test(
            Method::POST,
            "/admin/api/accounts/acct/quota/reset-credit",
            "",
        ),
    )
    .await
    .code();
    assert!(
        code == 401 || code == 403,
        "unauthenticated reset returned {code}"
    );
}

async fn post_settings(app: &Arc<App>, session: &Session, body: &str) -> Recorded {
    serve(
        app,
        admin_request(session, Method::POST, "/admin/api/settings", body),
    )
    .await
}

fn saved_config(app: &App) -> serde_json::Value {
    let path = app.settings.data_dir.join("config.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_default()).unwrap_or_default()
}

// Go TestAdminSettingsTogglePreserveProQuota.
#[tokio::test]
async fn admin_settings_toggle_preserve_pro_quota() {
    let (app, _dir) = arc_app(Vec::new());
    let session = admin_session(&app);
    let result = post_settings(&app, &session, r#"{"preserveProQuota":true}"#).await;
    assert_eq!(
        result.code(),
        200,
        "settings update: {}",
        result.body_string()
    );
    assert!(
        app.read().preserve_pro_quota,
        "settings update did not enable preserveProQuota"
    );
    assert!(result.body_string().contains(r#""preserveProQuota":true"#));
    assert_eq!(
        saved_config(&app)["preserveProQuota"],
        true,
        "settings update did not persist preserveProQuota"
    );
    let result = post_settings(&app, &session, r#"{"preserveProQuota":false}"#).await;
    assert_eq!(result.code(), 200);
    assert!(
        !app.read().preserve_pro_quota,
        "settings update did not disable preserveProQuota"
    );
    assert_eq!(
        saved_config(&app)["preserveProQuota"],
        false,
        "settings update did not persist disabled preserveProQuota"
    );
}

// Go TestAdminSettingsToggleLineageAffinityKeepsOtherSettings.
#[tokio::test]
async fn admin_settings_toggle_lineage_affinity_keeps_other_settings() {
    let (app, _dir) = arc_app(Vec::new());
    app.write().preserve_pro_quota = true;
    let session = admin_session(&app);
    let result = post_settings(&app, &session, r#"{"lineageAffinity":false}"#).await;
    assert_eq!(
        result.code(),
        200,
        "lineage affinity disable: {}",
        result.body_string()
    );
    assert!(
        !app.read().lineage_affinity,
        "settings update did not disable lineageAffinity"
    );
    assert!(
        app.read().preserve_pro_quota,
        "lineage-only settings update reset preserveProQuota"
    );
    assert!(result.body_string().contains(r#""lineageAffinity":false"#));
    assert_eq!(saved_config(&app)["lineageAffinity"], false);
    let result = post_settings(&app, &session, r#"{"lineageAffinity":true}"#).await;
    assert!(
        result.code() == 200 && app.read().lineage_affinity,
        "lineage affinity enable"
    );
    assert_eq!(
        post_settings(&app, &session, "{}").await.code(),
        400,
        "empty settings update"
    );
}

// Go TestAdminSettingsToggleQuotaPrimeKeepsOtherSettings.
#[tokio::test]
async fn admin_settings_toggle_quota_prime_keeps_other_settings() {
    let (app, _dir) = arc_app(Vec::new());
    let session = admin_session(&app);
    let result = post_settings(&app, &session, r#"{"quotaPrime":true}"#).await;
    assert!(
        result.code() == 200 && app.read().quota_prime && app.read().lineage_affinity,
        "quota prime enable: {}",
        result.body_string()
    );
    assert!(result.body_string().contains(r#""quotaPrime":true"#));
    assert_eq!(
        saved_config(&app)["quotaPrime"],
        true,
        "settings update did not persist quotaPrime"
    );
}

// ---- the public dashboard and its actions ----

use super::login_tests::PROMPT;
use super::login_tests::fake_codex;
use super::login_tests::history_sizes;
use super::login_tests::login_app;
use super::login_tests::seed_device_auth_identity_history;
use super::login_tests::wait_for_job;
use super::login_tests::writes_auth;
use super::testing::TempDir;

async fn get(app: &Arc<App>, target: &str) -> Recorded {
    serve(app, Request::test(Method::GET, target, "")).await
}

async fn post(app: &Arc<App>, target: &str, body: &str) -> Recorded {
    serve(app, Request::test(Method::POST, target, body)).await
}

fn public_ref(app: &App, id: &str) -> String {
    app.read().public_account_ref(id)
}

// Go TestPublicDashboardEnabledByDefaultFromEnv and
// TestPublicDashboardCanBeDisabledFromEnv.
#[tokio::test]
async fn public_dashboard_follows_its_env_switch() {
    for (value, enabled, code) in [("", true, 200), ("false", false, 404)] {
        let mut env = std::collections::HashMap::new();
        env.insert("CODEX_POOL_API_KEY", "client-key".to_string());
        env.insert(
            "CODEX_POOL_ADMIN_PASSWORD_HASH",
            super::settings::Settings::for_test(std::path::PathBuf::new()).admin_hash,
        );
        env.insert("CODEX_POOL_ADDR", "127.0.0.1:8317".to_string());
        env.insert("CODEX_POOL_PUBLIC_DASHBOARD", value.to_string());
        let settings = super::settings::Settings::from_env(|name| env.get(name).cloned())
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            settings.public_dashboard, enabled,
            "CODEX_POOL_PUBLIC_DASHBOARD={value:?}"
        );
        let (app, _dir) = super::testing::test_app_with(Vec::new(), |test| {
            test.public_dashboard = settings.public_dashboard
        });
        let app = Arc::new(app);
        assert_eq!(get(&app, "/admin/api/public-dashboard").await.code(), code);
    }
}

// Go TestPublicDashboardRedactsAccountSecrets.
#[tokio::test]
async fn public_dashboard_redacts_account_secrets() {
    let account = Account {
        label: "private@example.test · Plus".into(),
        email: "private@example.test".into(),
        account_id: "chatgpt-private-id".into(),
        organization_name: "Private private@example.test".into(),
        plan_type: "plus".into(),
        codex_home: "/data/accounts/private-account-id/.codex".into(),
        remaining_quota: Some(12),
        allowed_models: vec!["gpt-test".into()],
        ..super::proxy_tests::provider(
            "private-account-id",
            0,
            "https://upstream.example.test/v1",
            "upstream-secret-value",
        )
    };
    let (app, _dir) = arc_app(vec![account]);
    {
        let mut inner = app.write();
        inner
            .state
            .routing_cache_events_mut()
            .push(super::state::RoutingCacheEvent {
                timestamp: Timestamp::now(),
                account_id: "private-account-id".into(),
                routing_outcome: "sticky_reuse".into(),
                input_tokens: 2048,
                ..super::state::RoutingCacheEvent::default()
            });
        inner
            .throughput_buckets
            .push(super::state::ThroughputBucket {
                bucket_at: Timestamp::now().truncate(super::time::minutes(1)),
                account_id: "private-account-id".into(),
                request_count: 1,
                ..super::state::ThroughputBucket::default()
            });
    }
    let result = get(&app, "/admin/api/public-dashboard").await;
    assert_eq!(result.code(), 200);
    assert_eq!(result.header("cache-control"), "no-store");
    let body = result.body_string();
    for forbidden in [
        "private-account-id",
        "private@example.test",
        "chatgpt-private-id",
        "Private private@example.test",
        "upstream.example.test",
        "upstream-secret-value",
        "gpt-test",
        "credentialMetadata",
        "statusReason",
        "allowedModels",
        "planType",
        "planLimit",
        "email",
        "routingCacheEvents",
        "sticky_reuse",
        "throughputBuckets",
    ] {
        assert!(
            !body.contains(forbidden),
            "public dashboard exposed {forbidden:?}"
        );
    }
    assert!(
        body.contains("pr***te@example.test")
            && body.contains("Plus")
            && body.contains(r#""statusTone":"low""#)
            && body.contains(r#""statusLabel":"Limited""#),
        "public dashboard omitted expected status data: {body}"
    );
    let throughput = body_json(&result)["dashboard"]["throughput"].clone();
    assert_eq!(
        throughput["current"]["requestCount"], 1,
        "public current throughput"
    );
    let series = throughput["series"].as_array().cloned().unwrap_or_default();
    assert_eq!(series.len(), 48 * 6 + 1, "public 48-hour throughput series");
    let latest = series.last().cloned().unwrap_or_default();
    for forbidden in [
        "requestCount",
        "inputTokensPerMinute",
        "requestsPerMinute",
        "p95LatencyMs",
    ] {
        assert!(
            latest.get(forbidden).is_none(),
            "public historical series retained {forbidden}"
        );
    }
    assert_eq!(
        body.matches(r#""throughput""#).count(),
        1,
        "public dashboard must expose only pool-wide throughput"
    );
    assert_eq!(
        get(&app, "/admin/api/accounts").await.code(),
        401,
        "unauthenticated management API"
    );
    let session = admin_session(&app);
    let management = serve(
        &app,
        admin_request(&session, Method::GET, "/admin/api/accounts", ""),
    )
    .await;
    assert_eq!(management.code(), 200);
    let management = management.body_string();
    for forbidden in [
        "private@example.test",
        "chatgpt-private-id",
        "/data/accounts/private-account-id/.codex",
        "upstream.example.test",
        "upstream-secret-value",
    ] {
        assert!(
            !management.contains(forbidden),
            "management account list exposed {forbidden:?}"
        );
    }
    assert!(
        management.contains("pr***te@example.test"),
        "management account list omitted masked email"
    );
    assert!(
        !management.contains("Private private@example.test")
            && management.contains("Private pr***te@example.test"),
        "management account list did not mask email in organization name"
    );
}

// Go TestPublicPoolToggleDoesNotExposeAccountID.
#[tokio::test]
async fn public_pool_toggle_does_not_expose_account_id() {
    let account = Account {
        label: "private@example.test · Plus".into(),
        email: "private@example.test".into(),
        plan_type: "plus".into(),
        ..super::proxy_tests::provider(
            "private-account-id",
            0,
            "https://upstream.example.test/v1",
            "upstream-secret-value",
        )
    };
    let (app, _dir) = arc_app(vec![account]);
    super::proxy_tests::set_sticky(
        &app,
        "gpt-test:session",
        "private-account-id",
        Timestamp::now(),
    );
    let result = get(&app, "/admin/api/public-dashboard").await;
    assert_eq!(result.code(), 200);
    let body = result.body_string();
    for forbidden in [
        "private-account-id",
        "private@example.test",
        "upstream.example.test",
        "upstream-secret-value",
        "credentialMetadata",
        "statusReason",
        "inPool",
    ] {
        assert!(
            !body.contains(forbidden),
            "public dashboard exposed {forbidden:?}"
        );
    }
    let row = body_json(&result)["dashboard"]["accounts"][0].clone();
    assert!(
        row["displayName"] == "pr***te@example.test"
            && row["detail"] == "Plus"
            && row["poolLabel"] == "In pool"
            && row["poolAction"] == "pool-remove",
        "public dashboard did not return expected display state: {row}"
    );
    let reference = row["poolRef"].as_str().unwrap_or_default().to_string();
    assert!(!reference.is_empty());
    let removed = post(
        &app,
        &format!("/admin/api/public-dashboard/accounts/{reference}/pool-remove"),
        "",
    )
    .await;
    assert_eq!(
        removed.code(),
        200,
        "public pool-remove: {}",
        removed.body_string()
    );
    assert!(
        !super::proxy_tests::account(&app, 0).in_pool,
        "public pool-remove did not remove account from pool"
    );
    assert!(
        !app.read()
            .state
            .sticky_sessions()
            .contains_key("gpt-test:session"),
        "public pool-remove did not clear sticky sessions"
    );
    let added = post(
        &app,
        &format!("/admin/api/public-dashboard/accounts/{reference}/pool-add"),
        "",
    )
    .await;
    assert_eq!(
        added.code(),
        200,
        "public pool-add: {}",
        added.body_string()
    );
    let item = super::proxy_tests::account(&app, 0);
    assert!(
        item.enabled && item.in_pool,
        "public pool-add did not enable pool participation"
    );
}

// Go TestPublicRepairActionReplacesLeavePoolOnlyForInvalidCredential.
#[tokio::test]
async fn public_repair_action_replaces_leave_pool_only_for_invalid_credential() {
    let now = Timestamp::now();
    let codex = Account {
        last_login_at: now.add(-hours(1)),
        ..device_account("slot-codex", "upstream-one", 0)
    };
    let provider = Account {
        account_id: "provider-one".into(),
        auth_type: "provider_api_key".into(),
        upstream_base_url: "https://provider.example.test/v1".into(),
        enabled: true,
        in_pool: true,
        ..Account::default()
    };
    let provider = Account {
        id: "slot-provider".into(),
        ..provider
    };
    let (app, _dir) = arc_app(vec![codex, provider]);
    super::testing::write_codex_device_auth(
        &app,
        "slot-codex",
        "upstream-one",
        "same@example.test",
    );
    let row = |index: usize| {
        let inner = app.read();
        let item = inner.config.accounts()[index].clone();
        inner.public_dashboard_account(&item, index, now)
    };
    let healthy = row(0);
    assert!(
        healthy["poolAction"] == "pool-remove" && healthy["poolActionLabel"] == "Leave pool",
        "healthy public action = {healthy}"
    );
    app.write().state.quotas_mut().insert(
        "slot-codex".into(),
        super::state::QuotaSnapshot {
            account_id: "slot-codex".into(),
            quota_error: Some(super::state::QuotaErrorInfo {
                code: "token_invalidated".into(),
                message: "redacted".into(),
                timestamp: now,
                ..super::state::QuotaErrorInfo::default()
            }),
            ..super::state::QuotaSnapshot::default()
        },
    );
    let invalid = row(0);
    assert!(
        invalid["poolAction"] == "repair" && invalid["poolActionLabel"] == "Repair",
        "invalid credential action = {invalid}"
    );
    assert_eq!(
        row(1)["poolAction"],
        "pool-remove",
        "provider API-key account exposed repair"
    );
    let reference = public_ref(&app, "slot-provider");
    let result = post(
        &app,
        &format!("/admin/api/public-dashboard/accounts/{reference}/repair"),
        "",
    )
    .await;
    assert_eq!(
        result.code(),
        409,
        "provider public repair: {}",
        result.body_string()
    );
}

fn pending_repair_slot(id: &str) -> Account {
    Account {
        pending_auth_verification: true,
        pending_auth_expected_account_id: "upstream-one".into(),
        last_login_at: Timestamp::now().add(-hours(1)),
        ..device_account(id, "upstream-one", 0)
    }
}

// Go TestPublicRepairJobUsesOpaqueRefAndRedactedPolling.
#[tokio::test]
async fn public_repair_job_uses_opaque_ref_and_redacted_polling() {
    let bin = TempDir::new();
    let cli = fake_codex(&bin, &format!("{PROMPT}while true; do sleep 1; done\n"));
    let (app, _dir) = login_app(vec![pending_repair_slot("slot-public-repair")], &cli, None);
    let reference = public_ref(&app, "slot-public-repair");
    let started = post(
        &app,
        &format!("/admin/api/public-dashboard/accounts/{reference}/repair"),
        "",
    )
    .await;
    assert_eq!(
        started.code(),
        202,
        "public repair start: {}",
        started.body_string()
    );
    let forbidden = [
        "slot-public-repair",
        "upstream-one",
        "job-login-",
        r#""jobId""#,
        r#""accountId""#,
        r#""message""#,
        r#""error""#,
    ];
    for word in forbidden {
        assert!(
            !started.body_string().contains(word),
            "public repair start exposed {word:?}"
        );
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut poll = String::new();
    while std::time::Instant::now() < deadline {
        let result = get(
            &app,
            &format!("/admin/api/public-dashboard/accounts/{reference}/repair"),
        )
        .await;
        assert_eq!(
            result.code(),
            200,
            "public repair poll: {}",
            result.body_string()
        );
        poll = result.body_string();
        if poll.contains("ABCD-EFGH") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        poll.contains("ABCD-EFGH") && poll.contains("https://auth.openai.com/activate"),
        "public repair poll did not expose device prompt: {poll}"
    );
    for word in forbidden {
        assert!(!poll.contains(word), "public repair poll exposed {word:?}");
    }
    let job = app
        .read()
        .latest_public_repair_job_for_account("slot-public-repair")
        .cloned()
        .unwrap_or_default();
    assert!(job.public_repair, "public repair job policy missing");
    app.cancel_login_job(&job.id)
        .unwrap_or_else(|error| panic!("{error}"));
}

// Go TestPublicRepairCannotAdoptOwnerLoginOrReplaceIdentity.
#[tokio::test]
async fn public_repair_cannot_adopt_owner_login_or_replace_identity() {
    let bin = TempDir::new();
    let id_token = super::testing::fake_jwt_claims(
        &serde_json::json!({"email": "visitor@example.test", "https://api.openai.com/auth": {"chatgpt_account_id": "upstream-visitor"}}),
    );
    let cli = fake_codex(&bin, &writes_auth(&id_token));
    let slot = Account {
        email: "old@example.test".into(),
        ..pending_repair_slot("slot-public-repair")
    };
    let (app, _dir) = login_app(vec![slot], &cli, None);
    seed_device_auth_identity_history(&app, "slot-public-repair", Timestamp::now());
    app.write().jobs.insert(
        "job-owner".into(),
        super::app::LoginJob {
            id: "job-owner".into(),
            account_id: "slot-public-repair".into(),
            status: "waiting_for_user".into(),
            ..super::app::LoginJob::default()
        },
    );
    assert_eq!(
        app.start_public_repair_job("slot-public-repair").err(),
        Some(super::login::LoginStartError::AnotherJobInProgress),
        "public repair adopted owner job"
    );
    app.write().jobs.remove("job-owner");
    let job = app
        .start_public_repair_job("slot-public-repair")
        .unwrap_or_else(|error| panic!("{error}"));
    let current = wait_for_job(&app, &job.id, super::login_tests::terminal).await;
    assert!(
        current.status == "failed" && current.public_repair && !current.history_reset,
        "public mismatch job = {current:?}"
    );
    let item = super::proxy_tests::account(&app, 0);
    assert!(
        item.account_id == "upstream-one"
            && item.email == "old@example.test"
            && item.pending_auth_verification
            && item.pending_auth_expected_account_id == "upstream-one",
        "public mismatch replaced account metadata or released gate: {item:?}"
    );
    assert_eq!(
        history_sizes(&app),
        (1, 1, 1, 1, 1),
        "public mismatch discarded identity history"
    );
}

// Go TestPublicAndManagementAccountOwnerNotePersists.
#[tokio::test]
async fn public_and_management_account_owner_note_persists() {
    let (app, _dir) = arc_app(vec![device_account("slot-note", "upstream-one", 0)]);
    let reference = public_ref(&app, "slot-note");
    let result = post(
        &app,
        &format!("/admin/api/public-dashboard/accounts/{reference}/note"),
        r#"{"ownerNote":"  Alice   workstation  "}"#,
    )
    .await;
    assert_eq!(result.code(), 200, "public note: {}", result.body_string());
    assert!(
        super::proxy_tests::account(&app, 0).owner_note == "Alice workstation"
            && result
                .body_string()
                .contains(r#""ownerNote":"Alice workstation""#),
        "public note was not normalized"
    );
    let persisted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(app.settings.data_dir.join("config.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    assert_eq!(
        persisted["accounts"][0]["ownerNote"], "Alice workstation",
        "public note was not persisted"
    );
    let too_long = "界".repeat(super::plan::MAX_OWNER_NOTE_RUNES + 1);
    let result = post(
        &app,
        &format!("/admin/api/public-dashboard/accounts/{reference}/note"),
        &format!(r#"{{"ownerNote":"{too_long}"}}"#),
    )
    .await;
    assert_eq!(result.code(), 400, "overlong public note");
    let session = admin_session(&app);
    let result = serve(
        &app,
        admin_request(
            &session,
            Method::POST,
            "/admin/api/accounts/slot-note/note",
            r#"{"ownerNote":"Bob"}"#,
        ),
    )
    .await;
    assert_eq!(
        result.code(),
        200,
        "management note: {}",
        result.body_string()
    );
    assert!(
        super::proxy_tests::account(&app, 0).owner_note == "Bob"
            && result.body_string().contains(r#""ownerNote":"Bob""#),
        "management note was not saved"
    );
}

// Go TestAccountDeletePurgesCodexCredentials.
#[tokio::test]
async fn account_delete_purges_codex_credentials() {
    let (app, _dir) = arc_app(vec![device_account("acct-delete", "", 0)]);
    {
        let mut inner = app.write();
        inner.state.health_mut().insert(
            "acct-delete".into(),
            super::state::AccountHealth {
                last_failure_reason: "old failure".into(),
                ..super::state::AccountHealth::default()
            },
        );
        inner.state.quotas_mut().insert(
            "acct-delete".into(),
            super::state::QuotaSnapshot {
                account_id: "acct-delete".into(),
                quota: Some(super::proxy_tests::windows(Some(50), None)),
                ..super::state::QuotaSnapshot::default()
            },
        );
    }
    super::proxy_tests::write_auth(&app, "acct-delete", r#"{"auth_mode":"chatgpt"}"#);
    let session = admin_session(&app);
    let result = serve(
        &app,
        admin_request(
            &session,
            Method::DELETE,
            "/admin/api/accounts/acct-delete",
            "",
        ),
    )
    .await;
    assert_eq!(result.code(), 200, "delete: {}", result.body_string());
    assert!(
        !app.read().account_root("acct-delete").exists(),
        "credential directory still exists"
    );
    let inner = app.read();
    assert!(
        !inner.state.health().contains_key("acct-delete"),
        "deleted account health was retained"
    );
    assert!(
        !inner.state.quotas().contains_key("acct-delete"),
        "deleted account quota was retained"
    );
}

// Go TestCreateCodexDeviceAuthAccountStagesUntilLogin.
#[tokio::test]
async fn create_codex_device_auth_account_stages_until_login() {
    let (app, _dir) = arc_app(Vec::new());
    let session = admin_session(&app);
    let body = r#"{"authType":"codex_device_auth","enabled":true,"inPool":true,"priority":100,"pendingAuthVerification":true,"pendingAuthExpectedAccountId":"untrusted-upstream"}"#;
    let result = serve(
        &app,
        admin_request(&session, Method::POST, "/admin/api/accounts", body),
    )
    .await;
    assert_eq!(
        result.code(),
        201,
        "account create: {}",
        result.body_string()
    );
    let account = body_json(&result)["account"].clone();
    assert!(
        account["enabled"] == false && account["inPool"] == false,
        "new device-auth account response was routable before login"
    );
    let accounts = app.read().config.accounts().to_vec();
    assert_eq!(accounts.len(), 1);
    let staged = &accounts[0];
    assert!(
        !staged.enabled
            && !staged.in_pool
            && staged.pending_pool_activation
            && !staged.pending_auth_verification
            && staged.pending_auth_expected_account_id.is_empty(),
        "new device-auth account was not staged: {staged:?}"
    );
}

// Go TestPoolParticipationCannotChangeDuringAuthRepair.
#[tokio::test]
async fn pool_participation_cannot_change_during_auth_repair() {
    let slot = Account {
        pending_auth_verification: true,
        last_login_at: Timestamp::now().add(-hours(1)),
        ..device_account("slot-one", "upstream-one", 0)
    };
    let (app, _dir) = arc_app(vec![slot]);
    super::proxy_tests::set_sticky(&app, "gpt-test:thread:root", "slot-one", Timestamp::now());
    let reference = public_ref(&app, "slot-one");
    let result = post(
        &app,
        &format!("/admin/api/public-dashboard/accounts/{reference}/pool-remove"),
        "",
    )
    .await;
    assert_eq!(
        result.code(),
        409,
        "public pool-remove during repair: {}",
        result.body_string()
    );
    let session = admin_session(&app);
    let result = serve(
        &app,
        admin_request(
            &session,
            Method::POST,
            "/admin/api/accounts/slot-one/pool-remove",
            "",
        ),
    )
    .await;
    assert_eq!(
        result.code(),
        409,
        "management pool-remove during repair: {}",
        result.body_string()
    );
    assert!(
        super::proxy_tests::account(&app, 0).in_pool,
        "repair conflict changed pool participation"
    );
    assert_eq!(
        app.read()
            .state
            .sticky_sessions()
            .get("gpt-test:thread:root")
            .map(|session| session.account_id.clone()),
        Some("slot-one".to_string()),
        "repair conflict cleared or rewrote sticky route"
    );
}
