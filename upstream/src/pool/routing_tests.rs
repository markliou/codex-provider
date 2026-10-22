//! The Go routing tests that drive selection directly (main_test.go), one by
//! one. Each keeps its Go name in a comment; the assertions are Go's.

use super::app::App;
use super::response_info::PromptCacheUsage;
use super::response_info::ProxyResponseInfo;
use super::routing::ROUTING_CACHE_EVENT_LIMIT;
use super::routing::ROUTING_CACHE_EVENT_TTL_HOURS;
use super::routing::RequestFacts;
use super::routing::RequestIdentity;
use super::routing::RoutingDecision;
use super::routing::SelectError;
use super::routing::reset_credit_expiry_boost;
use super::settings::ROUTING_STRATEGY_BALANCED;
use super::state::Account;
use super::state::AccountQuota;
use super::state::Cooldown;
use super::state::LineageBinding;
use super::state::QuotaErrorInfo;
use super::state::QuotaResetCredits;
use super::state::QuotaSnapshot;
use super::state::QuotaWindow;
use super::state::RoutingCacheEvent;
use super::state::StickySession;
use super::testing::TempDir;
use super::testing::provider_account;
use super::testing::test_app;
use super::testing::test_app_with;
use super::testing::write_codex_device_auth;
use super::time::Timestamp;
use super::time::hours;
use super::time::millis;
use super::time::minutes;
use chrono::TimeDelta;
use http::HeaderMap;
use std::collections::HashMap;

const STRICT_MAIN_METADATA: &str = r#"{"thread_id":"main"}"#;
const STRICT_CHILD_METADATA: &str =
    r#"{"thread_id":"child","parent_thread_id":"main","subagent_kind":"thread_spawn"}"#;

fn none() -> HashMap<String, bool> {
    HashMap::new()
}

fn excluding(ids: &[&str]) -> HashMap<String, bool> {
    ids.iter().map(|id| ((*id).to_string(), true)).collect()
}

fn balanced(accounts: Vec<Account>) -> (App, TempDir) {
    test_app_with(accounts, |settings| {
        settings.routing_strategy = ROUTING_STRATEGY_BALANCED.to_string();
    })
}

/// Go strictLineageTestApp's request: a routing decision for a Codex turn
/// carrying this turn metadata.
fn request(app: &App, model: &str, metadata: &str) -> RoutingDecision {
    let headers = HeaderMap::new();
    let facts = RequestFacts {
        headers: &headers,
        native: false,
        websocket: false,
    };
    let payload = serde_json::json!({"client_metadata": {"x-codex-turn-metadata": metadata}});
    app.routing_decision(
        &facts,
        payload.as_object().unwrap_or(&serde_json::Map::new()),
        model,
        "client-key",
    )
}

fn sticky(key: &str, account_id: &str, now: Timestamp) -> StickySession {
    StickySession {
        key: key.to_string(),
        model_id: "gpt-test".to_string(),
        account_id: account_id.to_string(),
        created_at: now,
        last_success_at: now,
        expires_at: now.add(hours(1)),
        ..StickySession::default()
    }
}

fn plan(mut item: Account, plan_type: &str) -> Account {
    item.plan_type = plan_type.to_string();
    item
}

fn week_snapshot(id: &str, remaining: f64, credit_expires_in: Option<TimeDelta>) -> QuotaSnapshot {
    let mut quota = AccountQuota {
        windows: vec![QuotaWindow {
            label: "Week".into(),
            window_minutes: Some(10080),
            remaining_percent: Some(remaining),
            present: true,
            observed: true,
            ..QuotaWindow::default()
        }],
        ..AccountQuota::default()
    };
    if let Some(expires_in) = credit_expires_in {
        quota.reset_credits = Some(QuotaResetCredits {
            available_count: Some(1),
            expires_at: Some(Timestamp::now().add(expires_in).unix()),
            ..QuotaResetCredits::default()
        });
    }
    QuotaSnapshot {
        account_id: id.to_string(),
        quota: Some(quota),
        ..QuotaSnapshot::default()
    }
}

fn set_quota(app: &App, id: &str, snapshot: QuotaSnapshot) {
    app.write()
        .state
        .quotas_mut()
        .insert(id.to_string(), snapshot);
}

fn selected_id(result: Result<Account, SelectError>) -> String {
    result
        .unwrap_or_else(|error| panic!("selection failed: {error}"))
        .id
}

// Go TestStickySessionWaitsForAuthRepairInsteadOfFailingOver.
#[test]
fn sticky_session_waits_for_auth_repair_instead_of_failing_over() {
    let now = Timestamp::now();
    let mut one = super::testing::device_account("slot-one", "upstream-one", 0);
    one.pending_auth_verification = true;
    one.last_login_at = now.add(-hours(1));
    let mut two = super::testing::device_account("slot-two", "upstream-two", 0);
    two.last_login_at = now.add(-hours(1));
    let (app, _dir) = test_app(vec![one, two]);
    write_codex_device_auth(&app, "slot-one", "upstream-one", "one@example.test");
    write_codex_device_auth(&app, "slot-two", "upstream-two", "two@example.test");
    let key = "gpt-test:thread:root";
    let original = StickySession {
        key: key.into(),
        model_id: "gpt-test".into(),
        account_id: "slot-one".into(),
        created_at: now.add(-hours(1)),
        last_success_at: now.add(-minutes(1)),
        ..StickySession::default()
    };
    app.write()
        .state
        .sticky_sessions_mut()
        .insert(key.into(), original.clone());
    let mut route = RoutingDecision {
        sticky_key: key.into(),
        ..RoutingDecision::default()
    };
    let result = app.select_account_for_route(&mut route, "gpt-test", &none());
    assert!(
        matches!(result, Err(SelectError::AuthRepairPending)),
        "{result:?}"
    );
    let got = app
        .read()
        .state
        .sticky_sessions()
        .get(key)
        .cloned()
        .unwrap_or_default();
    assert_eq!(got.account_id, original.account_id);
    assert_eq!(got.last_success_at, original.last_success_at);
}

// Go TestStrictLineageStaleInFlightSuccessDoesNotMoveLineageBack.
#[test]
fn strict_lineage_stale_in_flight_success_does_not_move_lineage_back() {
    let (app, _dir) = test_app(vec![
        provider_account("account-a", 100),
        provider_account("account-b", 100),
    ]);
    app.mark_success(
        &request(&app, "gpt-test", STRICT_MAIN_METADATA),
        "gpt-test",
        "account-a",
        &ProxyResponseInfo::default(),
    );
    // The main agent's next request is routed (and in flight) on account-a...
    let mut in_flight_main = request(&app, "gpt-test", STRICT_MAIN_METADATA);
    assert!(
        app.select_account_for_route(&mut in_flight_main, "gpt-test", &none())
            .is_ok()
    );
    // ...while the child deliberately moves the lineage to account-b.
    let mut child = request(&app, "gpt-test", STRICT_CHILD_METADATA);
    child.lineage_account_id = "account-a".into();
    app.mark_success(
        &child,
        "gpt-test",
        "account-b",
        &ProxyResponseInfo::default(),
    );
    // The in-flight main request then completes on account-a. That success is
    // stale and must not drag the lineage back.
    app.mark_success(
        &in_flight_main,
        "gpt-test",
        "account-a",
        &ProxyResponseInfo::default(),
    );
    let binding = app
        .read()
        .state
        .lineage_bindings()
        .get("main")
        .cloned()
        .unwrap_or_default();
    assert_eq!(binding.account_id, "account-b");
    let mut next = request(&app, "gpt-test", STRICT_MAIN_METADATA);
    assert_eq!(
        selected_id(app.select_account_for_route(&mut next, "gpt-test", &none())),
        "account-b"
    );
}

// Go TestStrictLineageNeverFallsBackToThreadOwnSticky.
#[test]
fn strict_lineage_never_falls_back_to_thread_own_sticky() {
    let (app, _dir) = test_app(vec![
        provider_account("lineage", 100),
        provider_account("replacement", 50),
        provider_account("child-old", 10),
    ]);
    let now = Timestamp::now();
    {
        let mut inner = app.write();
        inner.state.lineage_bindings_mut().insert(
            "main".into(),
            LineageBinding {
                lineage_root_id: "main".into(),
                account_id: "lineage".into(),
                created_at: now,
                moved_at: now,
                last_success_at: now,
                expires_at: now.add(hours(1)),
                ..LineageBinding::default()
            },
        );
        // The child's own sticky route still points at a healthy but
        // different account.
        inner.state.sticky_sessions_mut().insert(
            "gpt-test:thread:child".into(),
            sticky("gpt-test:thread:child", "child-old", now),
        );
        inner.state.cooldowns_mut().insert(
            "lineage".into(),
            vec![Cooldown {
                model_id: "gpt-test".into(),
                next_retry_at: now.add(hours(1)),
                reason: "rate_limited".into(),
                ..Cooldown::default()
            }],
        );
    }
    let mut route = request(&app, "gpt-test", STRICT_CHILD_METADATA);
    let selected = selected_id(app.select_account_for_route(&mut route, "gpt-test", &none()));
    assert_eq!(selected, "replacement");
    app.mark_success(&route, "gpt-test", &selected, &ProxyResponseInfo::default());
    let binding = app
        .read()
        .state
        .lineage_bindings()
        .get("main")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (binding.account_id.as_str(), binding.failover_from.as_str()),
        ("replacement", "lineage")
    );
}

// Go TestStrictLineageRetryJoinsSiblingMove.
#[test]
fn strict_lineage_retry_joins_sibling_move() {
    let (app, _dir) = test_app(vec![
        provider_account("account-a", 100),
        provider_account("account-c", 90),
        provider_account("account-b", 10),
    ]);
    app.mark_success(
        &request(&app, "gpt-test", STRICT_MAIN_METADATA),
        "gpt-test",
        "account-a",
        &ProxyResponseInfo::default(),
    );
    let mut main_route = request(&app, "gpt-test", STRICT_MAIN_METADATA);
    assert_eq!(
        selected_id(app.select_account_for_route(&mut main_route, "gpt-test", &none())),
        "account-a"
    );
    // While the main agent's first attempt fails on account-a, a sibling
    // moves the lineage to account-b.
    let mut child = request(&app, "gpt-test", STRICT_CHILD_METADATA);
    child.lineage_account_id = "account-a".into();
    app.mark_success(
        &child,
        "gpt-test",
        "account-b",
        &ProxyResponseInfo::default(),
    );
    assert_eq!(
        selected_id(app.select_account_for_route(
            &mut main_route,
            "gpt-test",
            &excluding(&["account-a"])
        )),
        "account-b"
    );
}

// Go TestStrictLineageReplacementServesEveryLineageModel.
#[test]
fn strict_lineage_replacement_serves_every_lineage_model() {
    let mut main_only = provider_account("main-only", 100);
    main_only.allowed_models = vec!["gpt-main".into()];
    let mut mini_only = provider_account("mini-only", 90);
    mini_only.allowed_models = vec!["gpt-mini".into()];
    let (app, _dir) = test_app(vec![main_only, mini_only, provider_account("both", 10)]);
    app.mark_success(
        &request(&app, "gpt-main", STRICT_MAIN_METADATA),
        "gpt-main",
        "main-only",
        &ProxyResponseInfo::default(),
    );
    let mut child = request(&app, "gpt-mini", STRICT_CHILD_METADATA);
    let selected = selected_id(app.select_account_for_route(&mut child, "gpt-mini", &none()));
    assert_eq!(selected, "both");
    app.mark_success(&child, "gpt-mini", &selected, &ProxyResponseInfo::default());
    let mut main_route = request(&app, "gpt-main", STRICT_MAIN_METADATA);
    assert_eq!(
        selected_id(app.select_account_for_route(&mut main_route, "gpt-main", &none())),
        "both"
    );
}

// Go TestBoundSessionsMoveToRedOutlinedAccounts.
#[test]
fn bound_sessions_move_to_red_outlined_accounts() {
    let (app, _dir) = balanced(vec![
        plan(provider_account("plain", 100), "business"),
        plan(provider_account("red", 100), "business"),
        plan(provider_account("red-pro", 100), "pro"),
    ]);
    app.write().preserve_pro_quota = true;
    set_quota(&app, "plain", week_snapshot("plain", 50.0, None));
    set_quota(&app, "red", week_snapshot("red", 50.0, None));
    // A transient failure once left the lineage on an ordinary account
    // without a red outline.
    set_quota(&app, "red-pro", week_snapshot("red-pro", 50.0, None));
    app.mark_success(
        &request(&app, "gpt-test", STRICT_MAIN_METADATA),
        "gpt-test",
        "plain",
        &ProxyResponseInfo::default(),
    );
    app.mark_success(
        &request(&app, "gpt-test", STRICT_CHILD_METADATA),
        "gpt-test",
        "plain",
        &ProxyResponseInfo::default(),
    );
    let mut main_route = request(&app, "gpt-test", STRICT_MAIN_METADATA);
    assert_eq!(
        selected_id(app.select_account_for_route(&mut main_route, "gpt-test", &none())),
        "plain"
    );

    // A red-outlined Pro account takes it despite Use Pro last.
    set_quota(
        &app,
        "red-pro",
        week_snapshot("red-pro", 50.0, Some(hours(24))),
    );
    let mut main_route = request(&app, "gpt-test", STRICT_MAIN_METADATA);
    let selected = selected_id(app.select_account_for_route(&mut main_route, "gpt-test", &none()));
    assert_eq!(selected, "red-pro");
    app.mark_success(
        &main_route,
        "gpt-test",
        &selected,
        &ProxyResponseInfo::default(),
    );

    // Once an ordinary account is red-outlined too, Use Pro last moves the
    // lineage down to it, from one red outline to another.
    set_quota(&app, "red", week_snapshot("red", 50.0, Some(hours(24))));
    let mut main_route = request(&app, "gpt-test", STRICT_MAIN_METADATA);
    let selected = selected_id(app.select_account_for_route(&mut main_route, "gpt-test", &none()));
    assert_eq!(selected, "red");
    app.mark_success(
        &main_route,
        "gpt-test",
        &selected,
        &ProxyResponseInfo::default(),
    );
    let binding = app
        .read()
        .state
        .lineage_bindings()
        .get("main")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (binding.account_id.as_str(), binding.failover_from.as_str()),
        ("red", "red-pro")
    );
    let mut child = request(&app, "gpt-test", STRICT_CHILD_METADATA);
    assert_eq!(
        selected_id(app.select_account_for_route(&mut child, "gpt-test", &none())),
        "red"
    );

    // It never hops to another red outline in the same plan tier, and Use Pro
    // last never pulls it off a red outline.
    set_quota(&app, "plain", week_snapshot("plain", 50.0, Some(hours(1))));
    let mut main_route = request(&app, "gpt-test", STRICT_MAIN_METADATA);
    assert_eq!(
        selected_id(app.select_account_for_route(&mut main_route, "gpt-test", &none())),
        "red"
    );

    // The same exception holds for a sticky session outside lineage affinity.
    set_quota(&app, "plain", week_snapshot("plain", 50.0, None));
    let now = Timestamp::now();
    app.write().state.sticky_sessions_mut().insert(
        "gpt-test:other".into(),
        sticky("gpt-test:other", "plain", now),
    );
    assert_eq!(
        selected_id(app.select_account("gpt-test:other", "gpt-test", &none())),
        "red"
    );
}

// Go TestStickySessionTTLExpiresAndReselects.
#[test]
fn sticky_session_ttl_expires_and_reselects() {
    let (app, _dir) = test_app_with(
        vec![provider_account("old", 10), provider_account("new", 100)],
        |settings| {
            settings.session_affinity_ttl = hours(1);
        },
    );
    let now = Timestamp::now();
    app.write().state.sticky_sessions_mut().insert(
        "gpt-test:session".into(),
        StickySession {
            key: "gpt-test:session".into(),
            model_id: "gpt-test".into(),
            account_id: "old".into(),
            created_at: now.add(-hours(2)),
            last_success_at: now.add(-hours(2)),
            expires_at: now.add(-minutes(1)),
            ..StickySession::default()
        },
    );
    assert_eq!(
        selected_id(app.select_account("gpt-test:session", "gpt-test", &none())),
        "new"
    );
    assert!(
        !app.read()
            .state
            .sticky_sessions()
            .contains_key("gpt-test:session")
    );
}

// Go TestStickySessionTTLRefreshesOnSuccess.
#[test]
fn sticky_session_ttl_refreshes_on_success() {
    let (app, _dir) = test_app_with(vec![provider_account("one", 100)], |settings| {
        settings.session_affinity_ttl = hours(1);
    });
    let now = Timestamp::now();
    let previous_expiry = now.add(minutes(30));
    app.write().state.sticky_sessions_mut().insert(
        "gpt-test:session".into(),
        StickySession {
            key: "gpt-test:session".into(),
            model_id: "gpt-test".into(),
            account_id: "one".into(),
            created_at: now.add(-minutes(30)),
            last_success_at: now.add(-minutes(30)),
            expires_at: previous_expiry,
            ..StickySession::default()
        },
    );
    assert_eq!(
        selected_id(app.select_account("gpt-test:session", "gpt-test", &none())),
        "one"
    );
    let route = RoutingDecision {
        sticky_key: "gpt-test:session".into(),
        ..RoutingDecision::default()
    };
    app.mark_success(&route, "gpt-test", "one", &ProxyResponseInfo::default());
    let refreshed = app
        .read()
        .state
        .sticky_sessions()
        .get("gpt-test:session")
        .cloned()
        .unwrap_or_default();
    assert!(refreshed.expires_at.after(&previous_expiry));
}

// Go TestStickyBalancedDistributesNewSessionsDeterministically.
#[test]
fn sticky_balanced_distributes_new_sessions_deterministically() {
    let (app, _dir) = balanced(vec![
        provider_account("account-a", 100),
        provider_account("account-b", 100),
        provider_account("account-c", 100),
    ]);
    let mut counts: HashMap<String, usize> = HashMap::new();
    const SESSIONS: usize = 3000;
    for index in 0..SESSIONS {
        let key = format!("gpt-test:thread:balanced-{index}");
        let first = selected_id(app.select_account(&key, "gpt-test", &none()));
        // The first request has not succeeded yet, so no sticky binding exists.
        // A concurrent first turn for the same session must still choose the
        // same account instead of racing a mutable cursor.
        let second = selected_id(app.select_account(&key, "gpt-test", &none()));
        assert_eq!(second, first, "unbound session {key} changed account");
        *counts.entry(first).or_default() += 1;
    }
    for id in ["account-a", "account-b", "account-c"] {
        let got = counts.get(id).copied().unwrap_or(0);
        assert!(
            (850..=1150).contains(&got),
            "balanced assignments = {counts:?}; {id} received {got}"
        );
    }
}

// Go TestStickyBalancedKeepsExistingRoutesAndUsesPriorityTiers.
#[test]
fn sticky_balanced_keeps_existing_routes_and_uses_priority_tiers() {
    let (app, _dir) = balanced(vec![
        provider_account("sticky-low", 10),
        provider_account("primary-a", 100),
        provider_account("primary-b", 100),
    ]);
    let now = Timestamp::now();
    app.write().state.sticky_sessions_mut().insert(
        "gpt-test:existing".into(),
        sticky("gpt-test:existing", "sticky-low", now),
    );
    assert_eq!(
        selected_id(app.select_account("gpt-test:existing", "gpt-test", &none())),
        "sticky-low"
    );
    for index in 0..100 {
        let selected =
            selected_id(app.select_account(&format!("gpt-test:new-{index}"), "gpt-test", &none()));
        assert_ne!(
            selected, "sticky-low",
            "balanced strategy bypassed the highest priority tier"
        );
    }
}

// Go TestStickyBalancedReranksAfterFailoverExclusion.
#[test]
fn sticky_balanced_reranks_after_failover_exclusion() {
    let (app, _dir) = balanced(vec![
        provider_account("account-a", 100),
        provider_account("account-b", 100),
        provider_account("account-c", 100),
    ]);
    let key = "gpt-test:thread:failover";
    let first = selected_id(app.select_account(key, "gpt-test", &none()));
    let second = selected_id(app.select_account(key, "gpt-test", &excluding(&[&first])));
    assert_ne!(second, first);
    let again = selected_id(app.select_account(key, "gpt-test", &excluding(&[&first])));
    assert_eq!(again, second);
}

// Go TestStickyBalancedCountsDuplicateUpstreamIdentityOnce.
#[test]
fn sticky_balanced_counts_duplicate_upstream_identity_once() {
    let (app, _dir) = balanced(vec![
        super::testing::device_account("shared-primary", "upstream-shared", 100),
        super::testing::device_account("shared-duplicate", "upstream-shared", 100),
        super::testing::device_account("independent", "upstream-independent", 100),
    ]);
    write_codex_device_auth(
        &app,
        "shared-primary",
        "upstream-shared",
        "shared@example.test",
    );
    write_codex_device_auth(
        &app,
        "shared-duplicate",
        "upstream-shared",
        "shared@example.test",
    );
    write_codex_device_auth(
        &app,
        "independent",
        "upstream-independent",
        "independent@example.test",
    );
    let mut counts: HashMap<String, usize> = HashMap::new();
    for index in 0..2000 {
        let selected = selected_id(app.select_account(
            &format!("gpt-test:duplicate-balance-{index}"),
            "gpt-test",
            &none(),
        ));
        *counts.entry(selected).or_default() += 1;
    }
    let count = |id: &str| counts.get(id).copied().unwrap_or(0);
    assert!(
        count("shared-primary") == 0 || count("shared-duplicate") == 0,
        "both local copies of one upstream identity received balanced routes: {counts:?}"
    );
    let shared = count("shared-primary") + count("shared-duplicate");
    assert!(
        (850..=1150).contains(&shared) && (850..=1150).contains(&count("independent")),
        "duplicate identity created artificial capacity: {counts:?}"
    );
}

// Go TestStickyBalancedPreserveProQuotaFormsPreferredTierBeforePriority.
#[test]
fn sticky_balanced_preserve_pro_quota_forms_preferred_tier_before_priority() {
    let (app, _dir) = balanced(vec![
        plan(provider_account("plus-a", 10), "plus"),
        plan(provider_account("plus-b", 10), "plus"),
        plan(provider_account("pro-high-priority", 100), "pro"),
    ]);
    app.write().preserve_pro_quota = true;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for index in 0..1000 {
        let selected = selected_id(app.select_account(
            &format!("gpt-test:preserve-pro-{index}"),
            "gpt-test",
            &none(),
        ));
        *counts.entry(selected).or_default() += 1;
    }
    assert_eq!(counts.get("pro-high-priority").copied().unwrap_or(0), 0);
    for id in ["plus-a", "plus-b"] {
        let got = counts.get(id).copied().unwrap_or(0);
        assert!(
            (400..=600).contains(&got),
            "non-Pro tier not balanced: {counts:?}"
        );
    }
}

// Go TestPreserveProQuotaModeMovesStickySessionBackToNonPro.
#[test]
fn preserve_pro_quota_mode_moves_sticky_session_back_to_non_pro() {
    let mut pro = plan(provider_account("pro", 100), "pro");
    pro.plan_limit = "20x".into();
    let (app, _dir) = test_app(vec![plan(provider_account("plus", 10), "plus"), pro]);
    let now = Timestamp::now();
    app.write().state.sticky_sessions_mut().insert(
        "gpt-test:session".into(),
        sticky("gpt-test:session", "pro", now),
    );
    assert_eq!(
        selected_id(app.select_account("gpt-test:session", "gpt-test", &none())),
        "pro"
    );
    app.write().preserve_pro_quota = true;
    let selected = selected_id(app.select_account("gpt-test:session", "gpt-test", &none()));
    assert_eq!(selected, "plus");
    let route = RoutingDecision {
        sticky_key: "gpt-test:session".into(),
        ..RoutingDecision::default()
    };
    app.mark_success(&route, "gpt-test", &selected, &ProxyResponseInfo::default());
    let session = app
        .read()
        .state
        .sticky_sessions()
        .get("gpt-test:session")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (session.account_id.as_str(), session.failover_from.as_str()),
        ("plus", "pro")
    );
}

// Go TestPreserveProQuotaSpendsProLiteBetweenOrdinaryAndPro.
#[test]
fn preserve_pro_quota_spends_pro_lite_between_ordinary_and_pro() {
    let (app, _dir) = balanced(vec![
        plan(provider_account("business", 10), "business"),
        plan(provider_account("pro-lite", 10), "pro_lite"),
        plan(provider_account("pro", 10), "pro"),
    ]);
    app.write().preserve_pro_quota = true;
    for index in 0..200 {
        let selected =
            selected_id(app.select_account(&format!("gpt-test:tier-{index}"), "gpt-test", &none()));
        assert_eq!(selected, "business", "session {index}");
    }
    assert_eq!(
        selected_id(app.select_account(
            "gpt-test:tier-lite",
            "gpt-test",
            &excluding(&["business"])
        )),
        "pro-lite"
    );
    assert_eq!(
        selected_id(app.select_account(
            "gpt-test:tier-pro",
            "gpt-test",
            &excluding(&["business", "pro-lite"])
        )),
        "pro"
    );
    let now = Timestamp::now();
    app.write().state.sticky_sessions_mut().insert(
        "gpt-test:on-lite".into(),
        sticky("gpt-test:on-lite", "pro-lite", now),
    );
    assert_eq!(
        selected_id(app.select_account("gpt-test:on-lite", "gpt-test", &none())),
        "business"
    );
}

// Go TestPreserveProQuotaModeUsesProWhenNonProCoolingDown.
#[test]
fn preserve_pro_quota_mode_uses_pro_when_non_pro_cooling_down() {
    let mut pro = plan(provider_account("pro", 100), "pro");
    pro.plan_limit = "20x".into();
    let (app, _dir) = test_app(vec![plan(provider_account("plus", 10), "plus"), pro]);
    {
        let mut inner = app.write();
        inner.preserve_pro_quota = true;
        inner.state.cooldowns_mut().insert(
            "plus".into(),
            vec![Cooldown {
                model_id: "gpt-test".into(),
                next_retry_at: Timestamp::now().add(minutes(1)),
                reason: "rate_limited".into(),
                ..Cooldown::default()
            }],
        );
    }
    assert_eq!(
        selected_id(app.select_account("gpt-test:new", "gpt-test", &none())),
        "pro"
    );
}

// Go TestExplicitQuotaAuthErrorStillBlocksRouting.
#[test]
fn explicit_quota_auth_error_still_blocks_routing() {
    let (app, _dir) = test_app(vec![plan(provider_account("pro", 100), "pro")]);
    set_quota(
        &app,
        "pro",
        QuotaSnapshot {
            account_id: "pro".into(),
            plan_type: "pro".into(),
            quota: Some(AccountQuota {
                hourly: QuotaWindow {
                    percentage: 98,
                    present: true,
                    ..QuotaWindow::default()
                },
                ..AccountQuota::default()
            }),
            quota_error: Some(QuotaErrorInfo {
                code: "invalid_token".into(),
                message: "credential unavailable".into(),
                timestamp: Timestamp::now(),
                ..QuotaErrorInfo::default()
            }),
            ..QuotaSnapshot::default()
        },
    );
    assert!(
        app.select_account("gpt-test:auth-error", "gpt-test", &none())
            .is_err()
    );
}

fn bare_account(id: &str) -> Account {
    Account {
        id: id.into(),
        enabled: true,
        in_pool: true,
        priority: 100,
        ..Account::default()
    }
}

fn pick(app: &App, key: &str, ids: &[&str]) -> String {
    let inner = app.read();
    let eligible: Vec<Account> = inner
        .config
        .accounts()
        .iter()
        .filter(|item| ids.contains(&item.id.as_str()))
        .cloned()
        .collect();
    inner
        .pick_balanced_account(key, eligible)
        .map(|item| item.id)
        .unwrap_or_default()
}

// Go TestSessionPlacementPrefersIdleAccountsAndAvoidsAHerd.
#[test]
fn session_placement_prefers_idle_accounts_and_avoids_a_herd() {
    let (app, _dir) = balanced(vec![
        bare_account("a"),
        bare_account("b"),
        bare_account("c"),
    ]);
    // Among idle accounts, remaining allowance decides.
    set_quota(&app, "a", week_snapshot("a", 90.0, None));
    set_quota(&app, "b", week_snapshot("b", 10.0, None));
    let to_a = (0..400)
        .filter(|i| pick(&app, &format!("lineage:idle-{i}"), &["a", "b"]) == "a")
        .count();
    #[allow(clippy::cast_precision_loss)]
    let share = to_a as f64 / 400.0;
    assert!(
        (0.8..=0.97).contains(&share),
        "a took {share:.2} of new sessions, want about 0.9"
    );

    // An account carrying a session keeps its allowance for it while another
    // account is idle, however much more it has left.
    app.note_session_placement("lineage:running", "a");
    for i in 0..50 {
        assert_eq!(pick(&app, &format!("lineage:new-{i}"), &["a", "b"]), "b");
    }
    // The running session itself is not counted against its own account.
    assert_eq!(
        app.read()
            .active_session_counts(Timestamp::now(), "lineage:running")
            .get("a"),
        None
    );

    // A just-reset account takes one new session, not the whole queue.
    app.write().session_placements.clear();
    set_quota(&app, "a", week_snapshot("a", 40.0, None));
    set_quota(&app, "b", week_snapshot("b", 40.0, None));
    set_quota(&app, "c", week_snapshot("c", 100.0, None));
    let mut placed = std::collections::HashSet::new();
    for i in 0..3 {
        let key = format!("lineage:burst-{i}");
        let got = pick(&app, &key, &["a", "b", "c"]);
        assert!(placed.insert(got.clone()), "burst session {i} joined {got}");
        app.note_session_placement(&key, &got);
    }
    assert_eq!(
        pick(&app, "lineage:steady", &["a", "b", "c"]),
        pick(&app, "lineage:steady", &["a", "b", "c"])
    );
}

// Go TestExpiringResetCreditAccountsAreTheirOwnTier.
#[test]
fn expiring_reset_credit_accounts_are_their_own_tier() {
    let shares = |app: &App| {
        let inner = app.read();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for i in 0..400 {
            let picked = inner
                .pick_balanced_account(&format!("lineage:s-{i}"), inner.config.accounts().to_vec())
                .map(|item| item.id)
                .unwrap_or_default();
            *counts.entry(picked).or_default() += 1;
        }
        counts
    };
    let count = |counts: &HashMap<String, usize>, id: &str| counts.get(id).copied().unwrap_or(0);
    let accounts = vec![bare_account("a"), bare_account("b"), bare_account("c")];
    let (app, _dir) = balanced(accounts.clone());
    set_quota(&app, "a", week_snapshot("a", 50.0, Some(hours(24))));
    set_quota(&app, "b", week_snapshot("b", 50.0, None));
    set_quota(&app, "c", week_snapshot("c", 50.0, None));
    assert_eq!(count(&shares(&app), "a"), 400);
    app.note_session_placement("lineage:running", "a");
    let picked = {
        let inner = app.read();
        inner
            .pick_balanced_account("lineage:new", inner.config.accounts().to_vec())
            .map(|item| item.id)
    };
    assert_eq!(
        picked.as_deref(),
        Some("a"),
        "a busy red-outlined account still comes before idle ordinary ones"
    );

    let (app, _dir2) = balanced(accounts);
    set_quota(&app, "a", week_snapshot("a", 50.0, Some(hours(12))));
    set_quota(&app, "b", week_snapshot("b", 50.0, Some(hours(6 * 24))));
    set_quota(&app, "c", week_snapshot("c", 50.0, None));
    let got = shares(&app);
    assert!(
        count(&got, "c") == 0 && count(&got, "a") > count(&got, "b"),
        "{got:?}"
    );

    set_quota(&app, "a", week_snapshot("a", 50.0, Some(hours(10 * 24))));
    set_quota(&app, "b", week_snapshot("b", 50.0, None));
    let got = shares(&app);
    assert!(
        ["a", "b", "c"].iter().all(|id| count(&got, id) >= 90),
        "{got:?}"
    );
    let boost = reset_credit_expiry_boost(
        week_snapshot("x", 50.0, Some(minutes(1))).quota.as_ref(),
        Timestamp::now(),
    );
    assert!((6.9..=7.0).contains(&boost), "boost at expiry = {boost:.2}");

    // The red outline outranks Use Pro last.
    let (app, _dir3) = balanced(vec![
        plan(bare_account("pro"), "pro"),
        plan(bare_account("plain"), "business"),
    ]);
    app.write().preserve_pro_quota = true;
    set_quota(&app, "pro", week_snapshot("pro", 50.0, Some(hours(24))));
    set_quota(&app, "plain", week_snapshot("plain", 50.0, None));
    let picked = |app: &App| {
        let inner = app.read();
        inner
            .pick_balanced_account("lineage:pro-check", inner.config.accounts().to_vec())
            .map(|item| item.id)
    };
    assert_eq!(picked(&app).as_deref(), Some("pro"));
    // Among red-outlined accounts Use Pro last still orders the plan tiers.
    set_quota(&app, "plain", week_snapshot("plain", 50.0, Some(hours(24))));
    assert_eq!(picked(&app).as_deref(), Some("plain"));
}

// Go TestRoutingCacheEventRedactionAndPruning.
#[test]
fn routing_cache_event_redaction_and_pruning() {
    let mut item = bare_account("private-account-id");
    item.label = "Primary".into();
    let (app, _dir) = test_app(vec![item]);
    let route = RoutingDecision {
        sticky_key: "gpt-test:thread:raw-thread-id".into(),
        upstream_prompt_cache_key: "raw-prompt-cache-key".into(),
        source: "thread_id".into(),
        identity: RequestIdentity {
            thread_id: "raw-thread-id".into(),
            lineage_root_id: "raw-lineage-id".into(),
            ..RequestIdentity::default()
        },
        ..RoutingDecision::default()
    };
    app.mark_success(
        &route,
        "gpt-test",
        "private-account-id",
        &ProxyResponseInfo {
            response_id: "raw-response-id".into(),
            request_id: "raw-request-id".into(),
            precommit_committed: true,
            precommit_close_reason: "response.output_text.delta".into(),
            usage: PromptCacheUsage {
                input_tokens: 2048,
                cached_tokens: 1536,
                cache_write_tokens: Some(256),
                present: true,
                ..PromptCacheUsage::default()
            },
            ..ProxyResponseInfo::default()
        },
    );
    let event = {
        let inner = app.read();
        assert_eq!(inner.state.routing_cache_events().len(), 1);
        inner.state.routing_cache_events()[0].clone()
    };
    assert_eq!(event.routing_outcome, "new_route_assignment");
    assert_eq!(event.cache_read_rate, Some(0.75));
    assert_eq!(event.cache_write_rate, Some(0.125));
    assert_eq!(event.cache_reuse_balance, Some(1280));
    let encoded = serde_json::to_string(&app.read().routing_cache_event_views(Timestamp::now()))
        .unwrap_or_default();
    assert!(
        encoded.contains(r#""precommitCommitted":true"#),
        "{encoded}"
    );
    assert!(
        encoded.contains(r#""precommitCloseReason":"response.output_text.delta""#),
        "{encoded}"
    );
    for raw in [
        "raw-thread-id",
        "raw-lineage-id",
        "raw-prompt-cache-key",
        "raw-response-id",
        "raw-request-id",
        "private-account-id",
    ] {
        assert!(
            !encoded.contains(raw),
            "browser event leaked {raw}: {encoded}"
        );
    }
    for hash in [
        &event.thread_id_hash,
        &event.lineage_root_id_hash,
        &event.prompt_cache_key_hash,
        &event.response_id_hash,
        &event.request_id_hash,
    ] {
        assert!(!hash.is_empty());
    }

    let now = Timestamp::now();
    {
        let mut inner = app.write();
        let events = inner.state.routing_cache_events_mut();
        events.clear();
        events.push(RoutingCacheEvent {
            timestamp: now.add(-hours(ROUTING_CACHE_EVENT_TTL_HOURS) - minutes(1)),
            ..RoutingCacheEvent::default()
        });
        for index in 0..ROUTING_CACHE_EVENT_LIMIT + 8 {
            events.push(RoutingCacheEvent {
                timestamp: now.add(millis(i64::try_from(index).unwrap_or(0))),
                request_id_hash: index.to_string(),
                ..RoutingCacheEvent::default()
            });
        }
        assert!(inner.prune_routing_cache_events(now));
        assert_eq!(
            inner.state.routing_cache_events().len(),
            ROUTING_CACHE_EVENT_LIMIT
        );
        assert_eq!(inner.state.routing_cache_events()[0].request_id_hash, "8");
    }
    let views = app.read().routing_cache_event_views(now.add(minutes(1)));
    assert_eq!(views.len(), super::outcome::ROUTING_CACHE_EVENT_VIEW_LIMIT);
    assert_eq!(
        views[0]["requestIdHash"],
        (ROUTING_CACHE_EVENT_LIMIT + 7).to_string()
    );
}

// Go TestRecordingACooldownPrunesExpiredOnes.
#[test]
fn recording_a_cooldown_prunes_expired_ones() {
    let mut item = bare_account("acct");
    item.auth_type = "provider_api_key".into();
    let (app, _dir) = test_app(vec![item]);
    let now = Timestamp::now();
    let cooldown = |model: &str, at: Timestamp, reason: &str| Cooldown {
        model_id: model.into(),
        next_retry_at: at,
        reason: reason.into(),
        ..Cooldown::default()
    };
    app.write().state.cooldowns_mut().insert(
        "acct".into(),
        vec![
            cooldown("gpt-old", now.add(-hours(48)), "rate_limited"),
            cooldown("gpt-old", now.add(-minutes(1)), "server_is_overloaded"),
            cooldown("gpt-live", now.add(minutes(10)), "rate_limited"),
        ],
    );
    app.mark_failure("acct", "gpt-test", "rate_limited", minutes(1));
    let entries = app
        .read()
        .state
        .cooldowns()
        .get("acct")
        .cloned()
        .unwrap_or_default();
    let models: Vec<&str> = entries
        .iter()
        .map(|entry| entry.model_id.as_str())
        .collect();
    // The active one must survive: it is evidence still in force.
    assert_eq!(models, ["gpt-live", "gpt-test"]);
    // An account whose every entry has expired keeps only the new one.
    app.write().state.cooldowns_mut().insert(
        "acct".into(),
        vec![cooldown("gpt-old", now.add(-hours(1)), "rate_limited")],
    );
    app.mark_cooldown("acct", "gpt-test", "server_is_overloaded", minutes(1));
    let entries = app
        .read()
        .state
        .cooldowns()
        .get("acct")
        .cloned()
        .unwrap_or_default();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].model_id, "gpt-test");
}

fn decide(app: &App, payload: serde_json::Value, model: &str) -> RoutingDecision {
    let headers = HeaderMap::new();
    let facts = RequestFacts {
        headers: &headers,
        native: false,
        websocket: false,
    };
    app.routing_decision(
        &facts,
        payload.as_object().unwrap_or(&serde_json::Map::new()),
        model,
        "client-key",
    )
}

fn metadata_payload(metadata: &str) -> serde_json::Value {
    serde_json::json!({"client_metadata": {"x-codex-turn-metadata": metadata}})
}

fn thread_binding(
    thread: &str,
    root: &str,
    model: &str,
    account: &str,
    now: Timestamp,
) -> super::state::ThreadBinding {
    super::state::ThreadBinding {
        thread_id: thread.to_string(),
        lineage_root_id: root.to_string(),
        model_id: model.to_string(),
        account_id: account.to_string(),
        sticky_key: format!("{model}:thread:{thread}"),
        created_at: now,
        last_success_at: now,
        expires_at: now.add(hours(1)),
        ..super::state::ThreadBinding::default()
    }
}

// Go TestPreviousResponseIDRoutesToOriginalAccount.
#[test]
fn previous_response_id_routes_to_original_account() {
    let (app, _dir) = test_app(vec![
        provider_account("low", 10),
        provider_account("high", 100),
    ]);
    let now = Timestamp::now();
    {
        let mut inner = app.write();
        inner.state.sticky_sessions_mut().insert(
            "gpt-test:conversation-a".into(),
            sticky("gpt-test:conversation-a", "low", now),
        );
        inner.state.response_bindings_mut().insert(
            "resp_previous".into(),
            super::state::ResponseBinding {
                response_id: "resp_previous".into(),
                sticky_key: "gpt-test:conversation-a".into(),
                model_id: "gpt-test".into(),
                account_id: "low".into(),
                created_at: now,
                expires_at: now.add(hours(1)),
                ..super::state::ResponseBinding::default()
            },
        );
    }
    let route = decide(
        &app,
        serde_json::json!({"previous_response_id": "resp_previous", "input": "next"}),
        "gpt-test",
    );
    assert_eq!(
        selected_id(app.select_account(&route.sticky_key, "gpt-test", &none())),
        "low"
    );
}

// Go TestRequestIdentityParsesCodexMetadataPrecedenceAndMalformedFallback.
#[test]
fn request_identity_parses_codex_metadata_precedence_and_malformed_fallback() {
    use super::routing::request_identity_from;
    let headers = |pairs: &[(&'static str, &'static str)]| {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, http::HeaderValue::from_static(value));
        }
        map
    };
    let object = |value: serde_json::Value| value.as_object().cloned().unwrap_or_default();
    let identity = request_identity_from(
        &headers(&[
            (
                "x-codex-turn-metadata",
                r#"{"thread_id":"header-thread","parent_thread_id":"header-parent"}"#,
            ),
            ("x-codex-parent-thread-id", "direct-parent"),
            ("x-openai-subagent", "collab_spawn"),
        ]),
        &object(serde_json::json!({
            "thread_id": "top-thread",
            "parent_thread_id": "top-parent",
            "client_metadata": {
                "thread_id": "flat-thread",
                "session_id": "flat-session",
                "x-codex-turn-metadata": r#"{"thread_id":"canonical-thread","parent_thread_id":"canonical-parent","forked_from_thread_id":"fork-source","subagent_kind":"thread_spawn"}"#,
            },
        })),
    );
    assert_eq!(
        (
            identity.thread_id.as_str(),
            identity.parent_thread_id.as_str(),
            identity.session_id.as_str()
        ),
        ("canonical-thread", "canonical-parent", "flat-session"),
        "metadata precedence"
    );
    assert!(
        identity.forked_from_id == "fork-source"
            && identity.subagent_kind == "thread_spawn"
            && identity.is_subagent
    );

    let identity = request_identity_from(
        &headers(&[("x-codex-turn-metadata", "{not-json")]),
        &object(serde_json::json!({"thread_id": "fallback-thread", "input": "unchanged"})),
    );
    assert!(
        identity.thread_id == "fallback-thread" && !identity.is_subagent,
        "malformed metadata did not fall back safely: {identity:?}"
    );

    let identity = request_identity_from(
        &headers(&[
            ("x-codex-thread-id", "header-child"),
            ("x-codex-parent-thread-id", "header-parent"),
            ("x-openai-subagent", "collab_spawn"),
        ]),
        &object(
            serde_json::json!({"client_metadata": {"x-codex-turn-metadata": "{malformed", "session_id": "flat-session"}}),
        ),
    );
    assert!(
        identity.thread_id == "header-child"
            && identity.parent_thread_id == "header-parent"
            && identity.session_id == "flat-session"
            && identity.is_subagent,
        "compatibility headers or malformed nested fallback produced {identity:?}"
    );
}

// Go TestCodexThreadsKeepIndependentStickyAndPreviousResponseChains.
#[test]
fn codex_threads_keep_independent_sticky_and_previous_response_chains() {
    use super::routing::thread_binding_state_key;
    let (app, _dir) = test_app(Vec::new());
    let now = Timestamp::now();
    {
        let mut inner = app.write();
        let mut child = thread_binding("child", "main", "gpt-test", "one", now);
        child.parent_thread_id = "main".into();
        child.subagent_kind = "thread_spawn".into();
        inner
            .state
            .thread_bindings_mut()
            .insert(thread_binding_state_key("gpt-test", "child"), child);
        inner.state.response_bindings_mut().insert(
            "resp-child".into(),
            super::state::ResponseBinding {
                response_id: "resp-child".into(),
                sticky_key: "gpt-test:thread:child".into(),
                model_id: "gpt-test".into(),
                account_id: "one".into(),
                created_at: now,
                expires_at: now.add(hours(1)),
                ..super::state::ResponseBinding::default()
            },
        );
    }
    let request = |metadata: &str, previous: Option<&str>| {
        let mut payload = serde_json::json!({"client_metadata": {"x-codex-turn-metadata": metadata}, "prompt_cache_key": "client-thread-key"});
        if let Some(previous) = previous {
            payload["previous_response_id"] = serde_json::Value::String(previous.to_string());
        }
        decide(&app, payload, "gpt-test")
    };
    let main_route = request(r#"{"thread_id":"main"}"#, None);
    let child_route = request(
        r#"{"thread_id":"child","parent_thread_id":"main","subagent_kind":"thread_spawn"}"#,
        None,
    );
    let sibling_route = request(
        r#"{"thread_id":"sibling","parent_thread_id":"main","subagent_kind":"thread_spawn"}"#,
        None,
    );
    let continued = request(
        r#"{"thread_id":"wrong-version-skew-id"}"#,
        Some("resp-child"),
    );
    assert!(
        main_route.sticky_key != child_route.sticky_key
            && child_route.sticky_key != sibling_route.sticky_key
            && main_route.sticky_key != sibling_route.sticky_key,
        "thread routes collapsed"
    );
    assert!(
        continued.sticky_key == "gpt-test:thread:child" && continued.identity.thread_id == "child",
        "previous_response_id lost child chain"
    );
    app.mark_success(
        &continued,
        "gpt-test",
        "one",
        &ProxyResponseInfo {
            response_id: "resp-child-next".into(),
            ..ProxyResponseInfo::default()
        },
    );
    assert!(
        !app.read()
            .state
            .thread_bindings()
            .contains_key(&thread_binding_state_key(
                "gpt-test",
                "wrong-version-skew-id"
            )),
        "version-skewed metadata created a second thread binding for a bound response chain"
    );
    assert_eq!(main_route.upstream_prompt_cache_key, "client-thread-key");
    assert_eq!(child_route.upstream_prompt_cache_key, "client-thread-key");
}

// Go TestChildSoftPrefersEligibleParentAndNestedLineageRoot.
#[test]
fn child_soft_prefers_eligible_parent_and_nested_lineage_root() {
    use super::routing::thread_binding_state_key;
    let (app, _dir) = test_app(vec![
        provider_account("parent-account", 10),
        provider_account("pool-first", 100),
    ]);
    let now = Timestamp::now();
    app.write().state.thread_bindings_mut().insert(
        thread_binding_state_key("gpt-test", "root-thread"),
        thread_binding(
            "root-thread",
            "root-thread",
            "gpt-test",
            "parent-account",
            now,
        ),
    );
    let child_payload = serde_json::json!({
        "client_metadata": {"x-codex-turn-metadata": r#"{"thread_id":"child-thread","parent_thread_id":"root-thread","subagent_kind":"thread_spawn"}"#},
        "prompt_cache_key": "child-key",
    });
    let mut child_route = decide(&app, child_payload.clone(), "gpt-test");
    let selected = selected_id(app.select_account_for_route(&mut child_route, "gpt-test", &none()));
    assert!(
        selected == "parent-account" && child_route.identity.lineage_root_id == "root-thread",
        "child affinity/lineage"
    );
    app.mark_success(
        &child_route,
        "gpt-test",
        &selected,
        &ProxyResponseInfo::default(),
    );
    let continued = decide(&app, child_payload, "gpt-test");
    assert!(
        !continued.parent_affinity_attempted,
        "parent affinity was counted again after the child acquired its own sticky binding"
    );
    app.mark_success(
        &continued,
        "gpt-test",
        &selected,
        &ProxyResponseInfo::default(),
    );
    {
        let inner = app.read();
        let hits = inner
            .state
            .prompt_cache()
            .get("parent-account:gpt-test:subagent")
            .map_or(0, |stat| stat.parent_affinity_hit_count);
        assert_eq!(hits, 1, "parent affinity hit count");
        let events = inner.state.routing_cache_events();
        assert!(
            events.len() >= 2
                && events[0].routing_outcome == "parent_affinity"
                && events[1].routing_outcome == "sticky_reuse"
        );
    }
    let nested = decide(
        &app,
        metadata_payload(
            r#"{"thread_id":"nested-thread","parent_thread_id":"child-thread","subagent_kind":"thread_spawn"}"#,
        ),
        "gpt-test",
    );
    assert!(
        nested.identity.lineage_root_id == "root-thread"
            && nested.lineage_account_id == "parent-account",
        "nested lineage was not inherited"
    );
    assert_ne!(
        nested.sticky_key, child_route.sticky_key,
        "nested child reused parent sticky key"
    );
}

// Go TestBoundChildFollowsMainAgentAfterMainMovesAccount.
#[test]
fn bound_child_follows_main_agent_after_main_moves_account() {
    let (app, _dir) = test_app(vec![
        provider_account("account-a", 100),
        provider_account("account-b", 100),
    ]);
    let main = r#"{"thread_id":"main"}"#;
    let child = r#"{"thread_id":"child","parent_thread_id":"main","subagent_kind":"thread_spawn"}"#;
    let info = ProxyResponseInfo::default();
    app.mark_success(
        &request(&app, "gpt-test", main),
        "gpt-test",
        "account-a",
        &info,
    );
    app.mark_success(
        &request(&app, "gpt-test", child),
        "gpt-test",
        "account-a",
        &info,
    );
    // The main agent fails over to account-b; the bound child must follow it
    // on its next request instead of staying on its own sticky account-a.
    std::thread::sleep(std::time::Duration::from_millis(1));
    app.mark_success(
        &request(&app, "gpt-test", main),
        "gpt-test",
        "account-b",
        &info,
    );
    let mut child_route = request(&app, "gpt-test", child);
    assert_eq!(child_route.lineage_account_id, "account-b");
    let selected = selected_id(app.select_account_for_route(&mut child_route, "gpt-test", &none()));
    assert_eq!(
        selected, "account-b",
        "bound child must take the main agent's account"
    );
    app.mark_success(&child_route, "gpt-test", &selected, &info);
    let outcome = app
        .read()
        .state
        .routing_cache_events()
        .last()
        .map(|event| event.routing_outcome.clone())
        .unwrap_or_default();
    assert_eq!(outcome, "lineage_follow");
    // The reverse direction: a child that fails over pulls the main agent
    // along.
    std::thread::sleep(std::time::Duration::from_millis(1));
    app.mark_success(
        &request(&app, "gpt-test", child),
        "gpt-test",
        "account-a",
        &info,
    );
    let mut main_route = request(&app, "gpt-test", main);
    assert_eq!(
        selected_id(app.select_account_for_route(&mut main_route, "gpt-test", &none())),
        "account-a"
    );
}

// Go TestChildFindsParentBoundUnderDifferentModel.
#[test]
fn child_finds_parent_bound_under_different_model() {
    use super::routing::thread_binding_state_key;
    let (app, _dir) = test_app(vec![
        provider_account("parent-account", 10),
        provider_account("pool-first", 100),
    ]);
    app.write().state.thread_bindings_mut().insert(
        thread_binding_state_key("gpt-main", "main"),
        thread_binding(
            "main",
            "main",
            "gpt-main",
            "parent-account",
            Timestamp::now(),
        ),
    );
    let mut route = request(
        &app,
        "gpt-test",
        r#"{"thread_id":"child","parent_thread_id":"main","subagent_kind":"thread_spawn"}"#,
    );
    assert_eq!(route.identity.lineage_root_id, "main");
    assert_eq!(
        selected_id(app.select_account_for_route(&mut route, "gpt-test", &none())),
        "parent-account"
    );
}

// Go TestUnboundLineageMembersConvergeOnSameNewAccount.
#[test]
fn unbound_lineage_members_converge_on_same_new_account() {
    // Balanced routing hashes the route key; failover mode would pick the
    // same top-priority account for everyone and prove nothing.
    let (app, _dir) = balanced(
        (0..6)
            .map(|index| provider_account(&format!("account-{index}"), 100))
            .collect(),
    );
    let mut main_route = request(&app, "gpt-test", r#"{"thread_id":"main"}"#);
    let main_selected =
        selected_id(app.select_account_for_route(&mut main_route, "gpt-test", &none()));
    // No binding exists yet (concurrent first turns), so only the
    // lineage-seeded balance key can keep these together.
    for child in ["child-a", "child-b", "child-c", "child-d"] {
        let mut route = request(
            &app,
            "gpt-test",
            &format!(
                r#"{{"thread_id":"{child}","parent_thread_id":"main","subagent_kind":"thread_spawn"}}"#
            ),
        );
        assert_ne!(
            route.sticky_key, main_route.sticky_key,
            "child {child} reused the main sticky key"
        );
        assert_eq!(
            selected_id(app.select_account_for_route(&mut route, "gpt-test", &none())),
            main_selected,
            "unbound child {child}"
        );
    }
}

const PARENT_CHILD_METADATA: &str =
    r#"{"thread_id":"child","parent_thread_id":"parent","subagent_kind":"thread_spawn"}"#;

fn bind_parent(app: &App, account: &str) {
    use super::routing::thread_binding_state_key;
    app.write().state.thread_bindings_mut().insert(
        thread_binding_state_key("gpt-test", "parent"),
        thread_binding("parent", "parent", "gpt-test", account, Timestamp::now()),
    );
}

fn last_outcome(app: &App) -> String {
    app.read()
        .state
        .routing_cache_events()
        .last()
        .map(|event| event.routing_outcome.clone())
        .unwrap_or_default()
}

// Go TestChildTemporaryLineageRootCorrectsWhenParentAppears.
#[test]
fn child_temporary_lineage_root_corrects_when_parent_appears() {
    use super::routing::thread_binding_state_key;
    let (app, _dir) = test_app(vec![provider_account("account", 100)]);
    let initial = request(&app, "gpt-test", PARENT_CHILD_METADATA);
    assert_eq!(
        initial.identity.lineage_root_id, "parent",
        "temporary lineage root must be the immediate parent"
    );
    app.mark_success(
        &initial,
        "gpt-test",
        "account",
        &ProxyResponseInfo::default(),
    );
    let mut parent = thread_binding("parent", "root", "gpt-test", "account", Timestamp::now());
    parent.parent_thread_id = "root".into();
    app.write()
        .state
        .thread_bindings_mut()
        .insert(thread_binding_state_key("gpt-test", "parent"), parent);
    let corrected = request(&app, "gpt-test", PARENT_CHILD_METADATA);
    assert_eq!(corrected.identity.lineage_root_id, "root");
    app.mark_success(
        &corrected,
        "gpt-test",
        "account",
        &ProxyResponseInfo::default(),
    );
    let persisted = app
        .read()
        .state
        .thread_bindings()
        .get(&thread_binding_state_key("gpt-test", "child"))
        .map(|binding| binding.lineage_root_id.clone())
        .unwrap_or_default();
    assert_eq!(persisted, "root", "persisted corrected lineage root");
}

// Go TestIneligibleParentFallsBackWithoutBypassingSafeguards.
#[test]
fn ineligible_parent_falls_back_without_bypassing_safeguards() {
    let cases: Vec<(&str, Box<dyn Fn(&App)>)> = vec![
        (
            "disabled",
            Box::new(|app: &App| app.write().config.accounts_mut()[0].enabled = false),
        ),
        (
            "out of pool",
            Box::new(|app: &App| app.write().config.accounts_mut()[0].in_pool = false),
        ),
        (
            "missing provider auth",
            Box::new(|app: &App| {
                app.write().config.accounts_mut()[0].upstream_base_url = String::new()
            }),
        ),
        (
            "model incompatible",
            Box::new(|app: &App| {
                app.write().config.accounts_mut()[0].allowed_models = vec!["other-model".into()]
            }),
        ),
        (
            "quota exhausted",
            Box::new(|app: &App| app.write().config.accounts_mut()[0].remaining_quota = Some(0)),
        ),
        (
            "cooldown",
            Box::new(|app: &App| {
                app.write().state.cooldowns_mut().insert(
                    "parent-account".into(),
                    vec![Cooldown {
                        model_id: "gpt-test".into(),
                        next_retry_at: Timestamp::now().add(hours(1)),
                        reason: "rate_limited".into(),
                        ..Cooldown::default()
                    }],
                );
            }),
        ),
        (
            "pro preservation",
            Box::new(|app: &App| {
                let mut inner = app.write();
                inner.preserve_pro_quota = true;
                inner.config.accounts_mut()[0].plan_type = "pro".into();
                inner.config.accounts_mut()[1].plan_type = "plus".into();
            }),
        ),
    ];
    for (name, configure) in cases {
        let (app, _dir) = test_app(vec![
            provider_account("parent-account", 100),
            provider_account("fallback", 10),
        ]);
        bind_parent(&app, "parent-account");
        configure(&app);
        let mut route = request(&app, "gpt-test", PARENT_CHILD_METADATA);
        let selected = selected_id(app.select_account_for_route(&mut route, "gpt-test", &none()));
        assert_eq!(selected, "fallback", "{name}: ineligible parent selected");
        app.mark_success(&route, "gpt-test", &selected, &ProxyResponseInfo::default());
        assert_eq!(last_outcome(&app), "parent_affinity_fallback", "{name}");
    }
}

// Go TestChildAffinityDoesNotBypassDuplicateIdentityRepresentative.
#[test]
fn child_affinity_does_not_bypass_duplicate_identity_representative() {
    let (app, _dir) = test_app(vec![
        super::testing::device_account("parent-slot", "shared-upstream", 10),
        super::testing::device_account("representative-slot", "shared-upstream", 100),
    ]);
    write_codex_device_auth(
        &app,
        "parent-slot",
        "shared-upstream",
        "shared@example.test",
    );
    write_codex_device_auth(
        &app,
        "representative-slot",
        "shared-upstream",
        "shared@example.test",
    );
    bind_parent(&app, "parent-slot");
    let mut route = request(&app, "gpt-test", PARENT_CHILD_METADATA);
    assert_eq!(
        selected_id(app.select_account_for_route(&mut route, "gpt-test", &none())),
        "representative-slot",
        "child affinity selected duplicate non-representative"
    );
}

// Go TestLineageAffinityOffRestoresFirstRequestOnlyParentPreference.
#[test]
fn lineage_affinity_off_restores_first_request_only_parent_preference() {
    let (app, _dir) = test_app(vec![
        provider_account("account-a", 100),
        provider_account("account-b", 100),
    ]);
    app.write().lineage_affinity = false;
    let main = r#"{"thread_id":"main"}"#;
    let child = r#"{"thread_id":"child","parent_thread_id":"main","subagent_kind":"thread_spawn"}"#;
    let info = ProxyResponseInfo::default();
    app.mark_success(
        &request(&app, "gpt-test", main),
        "gpt-test",
        "account-b",
        &info,
    );
    // An unbound child still softly prefers its parent's account.
    let mut child_route = request(&app, "gpt-test", child);
    assert_eq!(
        child_route.balance_key, child_route.sticky_key,
        "lineage-off balance key must be per thread"
    );
    let selected = selected_id(app.select_account_for_route(&mut child_route, "gpt-test", &none()));
    assert_eq!(
        selected, "account-b",
        "unbound child must take the parent's account"
    );
    app.mark_success(&child_route, "gpt-test", &selected, &info);
    // Once bound, the child keeps its own route even after the parent moves.
    std::thread::sleep(std::time::Duration::from_millis(1));
    app.mark_success(
        &request(&app, "gpt-test", main),
        "gpt-test",
        "account-a",
        &info,
    );
    let mut bound = request(&app, "gpt-test", child);
    assert_eq!(
        selected_id(app.select_account_for_route(&mut bound, "gpt-test", &none())),
        "account-b"
    );
}

// Go TestLineageBindingsArePrunedAndClearedWithAccount.
#[test]
fn lineage_bindings_are_pruned_and_cleared_with_account() {
    let (app, _dir) = test_app(Vec::new());
    let now = Timestamp::now();
    let binding = |root: &str, account: &str, expires_at: Timestamp| LineageBinding {
        lineage_root_id: root.into(),
        account_id: account.into(),
        expires_at,
        ..LineageBinding::default()
    };
    let mut inner = app.write();
    for (root, account, expires_at) in [
        ("expired", "one", now.add(-minutes(1))),
        ("active", "one", now.add(hours(1))),
        ("other", "two", now.add(hours(1))),
    ] {
        inner
            .state
            .lineage_bindings_mut()
            .insert(root.into(), binding(root, account, expires_at));
    }
    assert!(
        inner.prune_expired_runtime_state(now),
        "expired lineage binding was not pruned"
    );
    assert!(!inner.state.lineage_bindings().contains_key("expired"));
    inner.clear_sticky_for_account("one");
    assert!(
        !inner.state.lineage_bindings().contains_key("active"),
        "lineage binding survived clearing its account"
    );
    assert!(
        inner.state.lineage_bindings().contains_key("other"),
        "clearing one account removed another account's lineage binding"
    );
}

// Go TestThreadBindingsAndResponsesAreTTLPruned.
#[test]
fn thread_bindings_and_responses_are_ttl_pruned() {
    let (app, _dir) = test_app(Vec::new());
    let now = Timestamp::now();
    let mut inner = app.write();
    inner.state.sticky_sessions_mut().insert(
        "expired".into(),
        StickySession {
            key: "expired".into(),
            expires_at: now.add(-minutes(1)),
            ..StickySession::default()
        },
    );
    inner.state.response_bindings_mut().insert(
        "expired".into(),
        super::state::ResponseBinding {
            response_id: "expired".into(),
            expires_at: now.add(-minutes(1)),
            ..super::state::ResponseBinding::default()
        },
    );
    for (id, expires_at) in [
        ("expired", now.add(-minutes(1))),
        ("active", now.add(hours(1))),
    ] {
        inner.state.thread_bindings_mut().insert(
            id.into(),
            super::state::ThreadBinding {
                thread_id: id.into(),
                expires_at,
                ..super::state::ThreadBinding::default()
            },
        );
    }
    assert!(
        inner.prune_expired_runtime_state(now),
        "expired runtime state was not reported as pruned"
    );
    assert!(
        !inner.state.thread_bindings().contains_key("expired"),
        "expired thread binding was retained"
    );
    assert!(
        inner.state.thread_bindings().contains_key("active"),
        "active thread binding was pruned"
    );
}
