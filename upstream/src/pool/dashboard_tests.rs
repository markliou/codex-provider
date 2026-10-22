//! The Go dashboard projection tests (main_test.go): pool capacity bars,
//! status cards and summaries, throughput and prompt-cache windows, one by
//! one. Each keeps its Go name in a comment; the assertions are Go's.

use super::app::App;
use super::dashboard::PoolCapacityBar;
use super::proxy_tests::api_slot;
use super::quota::WEEKLY_WINDOW_MINUTES;
use super::state::Account;
use super::state::AccountQuota;
use super::state::QuotaSnapshot;
use super::state::QuotaWindow;
use super::testing::device_account;
use super::testing::test_app;
use super::time::Timestamp;

const FIVE_HOURS: i64 = 300;

/// Go capacityWindow.
fn window(minutes: i64, remaining: f64) -> QuotaWindow {
    QuotaWindow {
        label: super::quota::quota_window_label(Some(minutes)),
        window_minutes: Some(minutes),
        remaining_percent: Some(remaining),
        present: true,
        observed: true,
        ..QuotaWindow::default()
    }
}

fn quota(app: &App, id: &str, family: &str, limit: &str, seat: &str, windows: Vec<QuotaWindow>) {
    app.write().state.quotas_mut().insert(
        id.into(),
        QuotaSnapshot {
            account_id: id.into(),
            plan_family: family.into(),
            plan_limit: limit.into(),
            seat_type: seat.into(),
            quota: Some(AccountQuota {
                windows,
                ..AccountQuota::default()
            }),
            ..QuotaSnapshot::default()
        },
    );
}

/// Go capacityBar.
fn bar(app: &App, minutes: i64) -> PoolCapacityBar {
    let bars = app.read().pool_capacity(Timestamp::now());
    bars.iter()
        .find(|bar| bar.window_minutes == minutes)
        .cloned()
        .unwrap_or_else(|| panic!("no bar for a {minutes}-minute window: {bars:?}"))
}

fn provider_slot(id: &str, in_pool: bool, enabled: bool) -> Account {
    Account {
        enabled,
        in_pool,
        priority: 100,
        ..api_slot(id, "", 100, &format!("https://{id}.example.test"))
    }
}

fn member(id: &str, workspace: &str, email: &str, in_pool: bool) -> Account {
    Account {
        email: email.into(),
        in_pool,
        priority: 100,
        ..device_account(id, workspace, 100)
    }
}

fn close(value: f64, want: f64) -> bool {
    (value - want).abs() < 0.01
}

// Go TestPoolCapacityWeightsTheWeeklyWindow.
#[test]
fn pool_capacity_weights_the_weekly_window() {
    let (app, _dir) = test_app(vec![
        provider_slot("pro", true, true),
        provider_slot("plus", true, true),
    ]);
    quota(
        &app,
        "pro",
        "pro",
        "20x",
        "",
        vec![window(WEEKLY_WINDOW_MINUTES, 50.0)],
    );
    quota(
        &app,
        "plus",
        "plus",
        "",
        "",
        vec![window(WEEKLY_WINDOW_MINUTES, 50.0)],
    );
    // (10 + 0.5) / 21 = 50%: both at 50%, so the weighted reading agrees,
    // which proves the denominator scaled with the numerator.
    assert_eq!(bar(&app, WEEKLY_WINDOW_MINUTES).spendable_percent, 50.0);
    // Spend the 20x account down: 20 of the 21 units go, where an unweighted
    // average would say 25%.
    quota(
        &app,
        "pro",
        "pro",
        "20x",
        "",
        vec![window(WEEKLY_WINDOW_MINUTES, 0.0)],
    );
    assert!(
        bar(&app, WEEKLY_WINDOW_MINUTES).spendable_percent <= 3.0,
        "exhausting the 20x account barely moved the reading"
    );
}

// Go TestPoolCapacitySplitsOneWholeBetweenSpendableAndRecoverable.
#[test]
fn pool_capacity_splits_one_whole_between_spendable_and_recoverable() {
    let (app, _dir) = test_app(vec![
        provider_slot("inpool", true, true),
        provider_slot("parked", false, true),
        provider_slot("off", false, false),
    ]);
    quota(
        &app,
        "inpool",
        "plus",
        "",
        "",
        vec![window(WEEKLY_WINDOW_MINUTES, 80.0)],
    );
    quota(
        &app,
        "parked",
        "plus",
        "",
        "",
        vec![window(WEEKLY_WINDOW_MINUTES, 50.0)],
    );
    // A disabled slot is not capacity at all and must not appear on either
    // side.
    quota(
        &app,
        "off",
        "plus",
        "",
        "",
        vec![window(WEEKLY_WINDOW_MINUTES, 100.0)],
    );
    let reading = bar(&app, WEEKLY_WINDOW_MINUTES);
    assert!(
        reading.spendable_percent == 40.0 && reading.recoverable_percent == 25.0,
        "{reading:?}"
    );
    assert!(
        reading.spendable_accounts == 1 && reading.recoverable_accounts == 1,
        "a disabled slot must not count: {reading:?}"
    );
}

// Go TestPoolCapacityDenominatorSpansTheWholePool.
#[test]
fn pool_capacity_denominator_spans_the_whole_pool() {
    let (app, _dir) = test_app(vec![
        provider_slot("a", true, true),
        provider_slot("b", true, true),
    ]);
    for id in ["a", "b"] {
        quota(
            &app,
            id,
            "plus",
            "",
            "",
            vec![window(WEEKLY_WINDOW_MINUTES, 100.0)],
        );
    }
    assert_eq!(bar(&app, WEEKLY_WINDOW_MINUTES).spendable_percent, 100.0);
    app.write().config.accounts_mut()[1].in_pool = false;
    let reading = bar(&app, WEEKLY_WINDOW_MINUTES);
    assert!(
        reading.spendable_percent == 50.0 && reading.recoverable_percent == 50.0,
        "parking half the pool: {reading:?}"
    );
}

// Go TestPoolCapacityZeroesTheFiveHourReadingWhenWeeklyIsSpent.
#[test]
fn pool_capacity_zeroes_the_five_hour_reading_when_weekly_is_spent() {
    let (app, _dir) = test_app(vec![
        provider_slot("spent", true, true),
        provider_slot("healthy", true, true),
    ]);
    // Upstream still reports a full five-hour window on the exhausted account.
    quota(
        &app,
        "spent",
        "plus",
        "",
        "",
        vec![
            window(FIVE_HOURS, 100.0),
            window(WEEKLY_WINDOW_MINUTES, 0.0),
        ],
    );
    quota(
        &app,
        "healthy",
        "plus",
        "",
        "",
        vec![
            window(FIVE_HOURS, 60.0),
            window(WEEKLY_WINDOW_MINUTES, 90.0),
        ],
    );
    assert_eq!(
        bar(&app, FIVE_HOURS).spendable_percent,
        30.0,
        "a spent weekly budget must zero its five-hour share"
    );
    assert_eq!(bar(&app, WEEKLY_WINDOW_MINUTES).spendable_percent, 45.0);
}

// Go TestPoolCapacityTreatsAnUncappedAccountAsFullOnFiveHours.
#[test]
fn pool_capacity_treats_an_uncapped_account_as_full_on_five_hours() {
    let (app, _dir) = test_app(vec![
        provider_slot("capped", true, true),
        provider_slot("uncapped", true, true),
    ]);
    quota(
        &app,
        "capped",
        "plus",
        "",
        "",
        vec![window(FIVE_HOURS, 0.0), window(WEEKLY_WINDOW_MINUTES, 50.0)],
    );
    // A Business Premium seat: weekly only, no five-hour limit at all.
    quota(
        &app,
        "uncapped",
        "business",
        "",
        "premium",
        vec![window(WEEKLY_WINDOW_MINUTES, 50.0)],
    );
    let reading = bar(&app, FIVE_HOURS);
    assert_eq!(
        reading.spendable_accounts, 2,
        "the uncapped account was dropped from the five-hour reading"
    );
    // The uncapped seat weighs 5 and counts full; the capped one weighs 1 at
    // zero: 5 of 6 units.
    assert!(
        close(reading.spendable_percent, 5.0 / 6.0 * 100.0),
        "{reading:?}"
    );
    assert!(
        reading.assumed,
        "a reading leaning on the assumed premium multiplier was not flagged"
    );
}

// Go TestPoolCapacityIgnoresAnAccountThatReportedNoWindow.
#[test]
fn pool_capacity_ignores_an_account_that_reported_no_window() {
    let (app, _dir) = test_app(vec![
        provider_slot("reporting", true, true),
        provider_slot("silent", true, true),
    ]);
    quota(
        &app,
        "reporting",
        "plus",
        "",
        "",
        vec![
            window(FIVE_HOURS, 40.0),
            window(WEEKLY_WINDOW_MINUTES, 40.0),
        ],
    );
    quota(&app, "silent", "plus", "", "", Vec::new());
    for minutes in [FIVE_HOURS, WEEKLY_WINDOW_MINUTES] {
        let reading = bar(&app, minutes);
        assert!(
            reading.spendable_accounts == 1 && reading.spendable_percent == 40.0,
            "{minutes}-minute bar = {reading:?}"
        );
    }
}

// Go TestPoolCapacityCountsWorkspaceMembersAsSpendable.
#[test]
fn pool_capacity_counts_workspace_members_as_spendable() {
    let (app, _dir) = test_app(vec![
        member("member-a", "workspace-1", "a@example.test", true),
        member("member-b", "workspace-1", "b@example.test", true),
    ]);
    for id in ["member-a", "member-b"] {
        super::testing::write_test_codex_auth_tokens(&app, id, "<access-token>", "<refresh-token>");
        quota(
            &app,
            id,
            "business",
            "",
            "standard",
            vec![window(WEEKLY_WINDOW_MINUTES, 50.0)],
        );
    }
    let reading = bar(&app, WEEKLY_WINDOW_MINUTES);
    assert!(
        reading.spendable_accounts == 2
            && reading.recoverable_accounts == 0
            && reading.blocked_accounts == 0,
        "{reading:?}"
    );
    assert!(
        reading.spendable_percent == 50.0 && reading.recoverable_percent == 0.0,
        "{reading:?}"
    );
}

// Go TestPoolCapacityCountsOneAllowanceOnce.
#[test]
fn pool_capacity_counts_one_allowance_once() {
    let (app, _dir) = test_app(vec![
        member("copy-a", "workspace-1", "same@example.test", true),
        member("copy-b", "workspace-1", "same@example.test", false),
    ]);
    for id in ["copy-a", "copy-b"] {
        super::testing::write_test_codex_auth_tokens(&app, id, "<access-token>", "<refresh-token>");
        quota(
            &app,
            id,
            "plus",
            "",
            "",
            vec![window(WEEKLY_WINDOW_MINUTES, 80.0)],
        );
    }
    let reading = bar(&app, WEEKLY_WINDOW_MINUTES);
    assert!(
        reading.spendable_accounts == 1 && reading.recoverable_accounts == 0,
        "one credential counted twice: {reading:?}"
    );
    assert_eq!(
        reading.spendable_percent, 80.0,
        "shared allowance double counted"
    );
}

// Go TestPoolCapacityCountsEachWorkspaceMemberSeparately.
#[test]
fn pool_capacity_counts_each_workspace_member_separately() {
    let (app, _dir) = test_app(vec![
        member("member-a", "workspace-1", "a@example.test", true),
        member("member-b", "workspace-1", "b@example.test", true),
        member("member-c", "workspace-1", "c@example.test", false),
    ]);
    for (id, remaining) in [("member-a", 40.0), ("member-b", 60.0), ("member-c", 100.0)] {
        super::testing::write_test_codex_auth_tokens(&app, id, "<access-token>", "<refresh-token>");
        quota(
            &app,
            id,
            "business",
            "",
            "standard",
            vec![window(WEEKLY_WINDOW_MINUTES, remaining)],
        );
    }
    let reading = bar(&app, WEEKLY_WINDOW_MINUTES);
    assert_eq!(
        reading.spendable_accounts + reading.recoverable_accounts,
        3,
        "workspace members collapsed"
    );
    assert!(
        close(
            reading.spendable_percent + reading.recoverable_percent,
            200.0 / 3.0
        ),
        "a member allowance was lost: {reading:?}"
    );
}

// Go TestPoolCapacityFlagsAndRetunesTheAssumedPremiumWeight.
#[test]
fn pool_capacity_flags_and_retunes_the_assumed_premium_weight() {
    let (app, _dir) = test_app(vec![
        provider_slot("standard", true, true),
        provider_slot("premium", true, true),
    ]);
    quota(
        &app,
        "standard",
        "business",
        "",
        "standard",
        vec![window(WEEKLY_WINDOW_MINUTES, 0.0)],
    );
    quota(
        &app,
        "premium",
        "business",
        "",
        "premium",
        vec![window(WEEKLY_WINDOW_MINUTES, 100.0)],
    );
    // The premium seat weighs 5 of the 6 units, so a full one reads 83%.
    let reading = bar(&app, WEEKLY_WINDOW_MINUTES);
    assert!(
        reading.assumed,
        "a reading using the assumed ratio was not flagged"
    );
    assert!(
        close(reading.spendable_percent, 5.0 / 6.0 * 100.0),
        "{reading:?}"
    );
    let tuned = super::tuning::Tuning::from_env(|name| {
        (name == "CODEX_POOL_PREMIUM_SEAT_MULTIPLIER").then(|| "3".to_string())
    })
    .unwrap_or_else(|error| panic!("{error}"));
    app.write().tuning.premium_seat_weight = tuned.premium_seat_weight;
    // Now 3 of 4 units.
    assert_eq!(
        bar(&app, WEEKLY_WINDOW_MINUTES).spendable_percent,
        75.0,
        "retuned multiplier was ignored"
    );
    // A wrong multiplier silently distorts every reading it touches, so an
    // invalid setting must fail rather than fall back.
    for raw in ["0", "-2", "abc"] {
        assert!(
            super::tuning::Tuning::from_env(
                |name| (name == "CODEX_POOL_PREMIUM_SEAT_MULTIPLIER").then(|| raw.to_string())
            )
            .is_err(),
            "{raw:?} was accepted as a multiplier"
        );
    }
}

// Go TestPoolCapacityOmitsAWindowWithNoDenominator.
#[test]
fn pool_capacity_omits_a_window_with_no_denominator() {
    let (app, _dir) = test_app(vec![provider_slot("a", true, true)]);
    assert!(
        app.read().pool_capacity(Timestamp::now()).is_empty(),
        "capacity reported without any quota snapshot"
    );
}

// Go TestDashboardStatusSeparatesQuotaAndErrors.
#[test]
fn dashboard_status_separates_quota_and_errors() {
    let plain = |id: &str, enabled: bool, in_pool: bool| Account {
        id: id.into(),
        enabled,
        in_pool,
        ..Account::default()
    };
    let now = Timestamp::now();
    let (app, _dir) = test_app(vec![
        plain("ready", true, true),
        Account {
            remaining_quota: Some(20),
            ..plain("low", true, true)
        },
        plain("quota-error", true, true),
        plain("stale-failure", true, true),
        plain("cooldown", true, true),
        plain("disabled", false, true),
        Account {
            pending_pool_activation: true,
            enabled: false,
            in_pool: false,
            ..device_account("staged", "", 0)
        },
        Account {
            in_pool: false,
            ..device_account("standby", "", 0)
        },
        device_account("missing", "", 0),
    ]);
    {
        let mut inner = app.write();
        inner.state.health_mut().insert(
            "stale-failure".into(),
            super::state::AccountHealth {
                consecutive_failure: 2,
                last_failure_reason: "upstream_transport_error".into(),
                last_failure_at: now.add(-super::time::hours(1)),
                ..super::state::AccountHealth::default()
            },
        );
        inner.state.quotas_mut().insert(
            "quota-error".into(),
            QuotaSnapshot {
                account_id: "quota-error".into(),
                quota_error: Some(super::state::QuotaErrorInfo {
                    code: "invalid_token".into(),
                    message: "credential unavailable".into(),
                    timestamp: now.add(-super::time::minutes(1)),
                    ..super::state::QuotaErrorInfo::default()
                }),
                ..QuotaSnapshot::default()
            },
        );
        inner.state.cooldowns_mut().insert(
            "cooldown".into(),
            vec![super::state::Cooldown {
                model_id: "gpt-test".into(),
                next_retry_at: now.add(super::time::minutes(1)),
                reason: "rate_limited".into(),
                ..super::state::Cooldown::default()
            }],
        );
    }
    let inner = app.read();
    for (id, want) in [
        ("ready", "ready"),
        ("low", "low"),
        ("quota-error", "error"),
        ("stale-failure", "ready"),
        ("cooldown", "cooldown"),
        ("disabled", "disabled"),
        ("staged", "disabled"),
        ("standby", "standby"),
        ("missing", "missing_auth"),
    ] {
        let item = inner.account(id).cloned().unwrap_or_default();
        assert_eq!(inner.account_status(&item, now).0, want, "account {id}");
    }
    let (status, reason) = inner.account_status(&inner.config.accounts()[2], now);
    assert!(
        status == "error" && reason.contains("invalid_token"),
        "quota error status did not retain the sanitized code: {status}/{reason}"
    );
    let (status, reason) = inner.account_status(&inner.config.accounts()[3], now);
    assert!(
        status == "ready" && reason == "Ready",
        "stale failure status = {status}/{reason}"
    );
}

// Go TestDashboardSummariesPartitionVisibleAccountStates.
#[test]
fn dashboard_summaries_partition_visible_account_states() {
    let now = Timestamp::now();
    let provider = |id: &str, enabled: bool, in_pool: bool| Account {
        enabled,
        in_pool,
        ..api_slot(id, "", 0, &format!("https://{id}.example.test/v1"))
    };
    let shared = |id: &str, priority: i64| device_account(id, "shared-upstream", priority);
    let (app, _dir) = test_app(vec![
        provider("ready", true, true),
        Account {
            remaining_quota: Some(20),
            ..provider("low", true, true)
        },
        provider("cooldown", true, true),
        provider("standby", true, false),
        provider("disabled", false, true),
        device_account("missing", "", 0),
        shared("primary", 100),
        shared("healthy-duplicate", 90),
        shared("invalid-duplicate", 80),
    ]);
    for id in ["primary", "healthy-duplicate", "invalid-duplicate"] {
        // Copies of one login: one account id and one email.
        super::testing::write_codex_device_auth(&app, id, "shared-upstream", "shared@example.test");
    }
    {
        let mut inner = app.write();
        inner.state.cooldowns_mut().insert(
            "cooldown".into(),
            vec![super::state::Cooldown {
                model_id: "gpt-test".into(),
                next_retry_at: now.add(super::time::minutes(1)),
                reason: "rate_limited".into(),
                ..super::state::Cooldown::default()
            }],
        );
        inner.state.quotas_mut().insert(
            "invalid-duplicate".into(),
            QuotaSnapshot {
                account_id: "invalid-duplicate".into(),
                quota_error: Some(super::state::QuotaErrorInfo {
                    code: "token_invalidated".into(),
                    message: "credential unavailable".into(),
                    timestamp: now,
                    ..super::state::QuotaErrorInfo::default()
                }),
                ..QuotaSnapshot::default()
            },
        );
    }
    let inner = app.read();
    let accounts = inner.config.accounts().to_vec();
    assert_eq!(
        inner.account_status(&accounts[7], now).0,
        "duplicate",
        "healthy duplicate status"
    );
    assert_eq!(
        inner.account_status(&accounts[8], now).0,
        "error",
        "credential error must take precedence over duplicate"
    );
    let detailed = inner.dashboard_summary(now);
    for (key, want) in [
        ("total", 9),
        ("ready", 2),
        ("low", 1),
        ("cooldown", 1),
        ("standby", 1),
        ("duplicate", 1),
        ("error", 1),
        ("missing_auth", 1),
        ("disabled", 1),
        ("authenticating", 0),
        ("unavailable", 3),
    ] {
        assert_eq!(
            detailed.get(key).copied().unwrap_or(-1),
            want,
            "management summary[{key}]"
        );
    }
    let public = inner.public_dashboard_summary(now);
    for (key, want) in [
        ("total", 9),
        ("ready", 2),
        ("low", 1),
        ("cooldown", 1),
        ("standby", 1),
        ("duplicate", 1),
        ("unavailable", 3),
    ] {
        assert_eq!(
            public.get(key).copied().unwrap_or(-1),
            want,
            "public summary[{key}]"
        );
    }
    let classified: i64 = [
        "ready",
        "low",
        "cooldown",
        "standby",
        "duplicate",
        "unavailable",
    ]
    .iter()
    .map(|key| public[key])
    .sum();
    assert_eq!(
        classified, public["total"],
        "public summary categories must add up to the total"
    );
    for private in ["error", "missing_auth", "authenticating", "disabled"] {
        assert!(
            !public.contains_key(private),
            "public summary exposed auth-specific bucket {private}"
        );
    }
    let duplicate = inner.public_dashboard_account(&accounts[7], 7, now);
    assert!(
        duplicate["statusTone"] == "standby" && duplicate["statusLabel"] == "Duplicate",
        "{duplicate}"
    );
    assert_eq!(
        duplicate["outOfPool"], false,
        "duplicate public account was marked out of pool"
    );
    assert_eq!(
        inner.public_dashboard_account(&accounts[3], 3, now)["outOfPool"],
        true,
        "real out-of-pool account was not marked"
    );
}

// ---- throughput ----

use super::outcome::ThroughputMeasurement;
use super::response_info::PromptCacheUsage;
use super::time::hours;
use super::time::minutes;
use super::time::seconds;
use chrono::TimeDelta;

fn plain(id: &str) -> Account {
    Account {
        id: id.into(),
        ..Account::default()
    }
}

fn get_u64(value: &serde_json::Value, key: &str) -> u64 {
    value[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} is not a count: {value}"))
}

fn get_f64(value: &serde_json::Value, key: &str) -> f64 {
    value[key]
        .as_f64()
        .unwrap_or_else(|| panic!("{key} is not a number: {value}"))
}

// Go TestThroughputRollingWindowsAndAccountFiltering.
#[test]
fn throughput_rolling_windows_and_account_filtering() {
    let (app, _dir) = test_app(vec![plain("a"), plain("b")]);
    let now = Timestamp::now().truncate(minutes(1)).add(seconds(10));
    #[allow(clippy::too_many_arguments)]
    let record = |account: &str,
                  offset: TimeDelta,
                  duration: TimeDelta,
                  success: bool,
                  cancelled: bool,
                  streaming: bool,
                  input: u64,
                  cached: u64,
                  output: u64,
                  output_present: bool| {
        let completed_at = now.add(offset);
        app.write()
            .record_throughput_measurement(ThroughputMeasurement {
                started_at: completed_at.add(-duration),
                completed_at,
                account_id: account.into(),
                streaming,
                success,
                cancelled,
                usage: PromptCacheUsage {
                    input_tokens: input,
                    cached_tokens: cached,
                    output_tokens: output,
                    output_present,
                    present: input > 0 || output_present,
                    ..PromptCacheUsage::default()
                },
                ..ThroughputMeasurement::default()
            });
    };
    record(
        "a",
        -seconds(10),
        seconds(2),
        true,
        false,
        true,
        1200,
        600,
        120,
        true,
    );
    record(
        "a",
        -seconds(20),
        TimeDelta::milliseconds(250),
        false,
        false,
        false,
        0,
        0,
        0,
        false,
    );
    record(
        "b",
        -seconds(30),
        seconds(1),
        false,
        true,
        false,
        0,
        0,
        0,
        false,
    );
    record(
        "a",
        -minutes(3),
        seconds(1),
        true,
        false,
        false,
        300,
        0,
        60,
        true,
    );
    let inner = app.read();
    let window = inner.throughput_window("a", now, minutes(1));
    assert_eq!(
        (
            get_u64(&window, "requestCount"),
            get_u64(&window, "successCount"),
            get_u64(&window, "failureCount"),
            get_u64(&window, "cancelledCount")
        ),
        (2, 1, 1, 0),
        "account rolling request counts"
    );
    assert_eq!(get_f64(&window, "requestsPerMinute"), 2.0);
    assert_eq!(get_f64(&window, "outputTokensPerSecond"), 2.0);
    assert_eq!(
        (
            get_u64(&window, "p50LatencyMs"),
            get_u64(&window, "p95LatencyMs")
        ),
        (250, 2000),
        "latency percentiles"
    );
    assert_eq!(get_f64(&window, "cacheHitRate"), 0.5);
    let all = inner.throughput_window("", now, minutes(1));
    assert_eq!(
        (
            get_u64(&all, "requestCount"),
            get_u64(&all, "cancelledCount")
        ),
        (3, 1),
        "pool rolling counts"
    );
    let five = inner.throughput_window("a", now, minutes(5));
    assert_eq!(
        (
            get_u64(&five, "requestCount"),
            get_u64(&five, "outputTokens")
        ),
        (3, 180),
        "five-minute account window"
    );
}

// Go TestThroughputLifecyclePruningAndIdentityCleanup.
#[test]
fn throughput_lifecycle_pruning_and_identity_cleanup() {
    let (app, _dir) = test_app(vec![plain("a")]);
    let now = Timestamp::now();
    let mut measurement = app.begin_throughput_measurement(false);
    assert_eq!(app.read().active_proxy_requests, 1);
    measurement.account_id = "a".into();
    measurement.success = true;
    measurement.completed_at = now;
    app.finish_throughput_measurement(&mut measurement);
    assert_eq!(
        (
            app.read().active_proxy_requests,
            app.read().throughput_buckets.len()
        ),
        (0, 1),
        "finished throughput state"
    );
    let mut inner = app.write();
    inner
        .throughput_buckets
        .push(super::state::ThroughputBucket {
            bucket_at: now.add(-hours(48)).add(-minutes(1)),
            account_id: "a".into(),
            request_count: 1,
            ..super::state::ThroughputBucket::default()
        });
    assert!(
        inner.prune_throughput_buckets(now) && inner.throughput_buckets.len() == 1,
        "throughput TTL prune"
    );
    inner.clear_sticky_for_account("a");
    assert_eq!(
        inner.throughput_buckets.len(),
        1,
        "pool route cleanup erased rolling throughput history"
    );
    inner.clear_account_identity_scoped_state("a");
    assert!(
        inner.throughput_buckets.is_empty(),
        "identity cleanup retained attributed throughput history"
    );
    assert!(
        inner.throughput_series("", now).is_empty(),
        "empty in-memory history produced chart points"
    );
    let encoded = serde_json::to_string(&inner.state).unwrap_or_default();
    assert!(
        !encoded.contains("throughputBuckets"),
        "throughput history was persisted instead of remaining in memory"
    );
}

// Go TestThroughputSeriesUsesPartialBoundaryDurations.
#[test]
fn throughput_series_uses_partial_boundary_durations() {
    let (app, _dir) = test_app(vec![plain("acct")]);
    let now = Timestamp::parse("2026-07-28T10:05:30Z").unwrap_or_else(|error| panic!("{error}"));
    let cutoff = now.add(-hours(48));
    let mut inner = app.write();
    for at in [cutoff.truncate(minutes(1)), now.truncate(minutes(1))] {
        inner
            .throughput_buckets
            .push(super::state::ThroughputBucket {
                bucket_at: at,
                account_id: "acct".into(),
                request_count: 1,
                ..super::state::ThroughputBucket::default()
            });
    }
    let series = inner.throughput_series("", now);
    assert_eq!(series.len(), 48 * 6 + 1, "series point count");
    assert_eq!(series[0]["windowSeconds"], 270, "oldest partial window");
    assert_eq!(
        series[series.len() - 1]["windowSeconds"],
        330,
        "newest partial window"
    );
}

// Go TestManagementThroughputProjection.
#[test]
fn management_throughput_projection() {
    let (app, _dir) = test_app(vec![Account {
        enabled: true,
        in_pool: true,
        ..plain("acct")
    }]);
    let now = Timestamp::now();
    app.write()
        .throughput_buckets
        .push(super::state::ThroughputBucket {
            bucket_at: now.truncate(minutes(1)),
            account_id: "acct".into(),
            request_count: 2,
            success_count: 2,
            input_tokens: 200,
            cached_tokens: 150,
            output_tokens: 120,
            usage_observed_request_count: 2,
            output_observed_request_count: 2,
            ..super::state::ThroughputBucket::default()
        });
    let inner = app.read();
    let state = inner.admin_state(now);
    let series = state["throughput"]["series"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(series.len(), 48 * 6 + 1, "management pool series");
    let last = series.last().cloned().unwrap_or_default();
    assert!(
        get_f64(&last, "cacheHitRate") == 0.75 && get_f64(&last, "outputTokensPerSecond") > 0.0,
        "management latest series point = {last}"
    );
    for forbidden in [
        "requestCount",
        "inputTokensPerMinute",
        "requestsPerMinute",
        "p95LatencyMs",
    ] {
        assert!(
            last.get(forbidden).is_none(),
            "management historical series retained {forbidden}"
        );
    }
    let health = inner.account_health_item(&inner.config.accounts()[0], now);
    assert_eq!(
        health["throughput"]["windows"]["5m"]["requestCount"], 2,
        "management account throughput"
    );
}

// Go TestPublicDashboardAccountIncludesCacheStats.
#[test]
fn public_dashboard_account_includes_cache_stats() {
    let (app, _dir) = test_app(vec![Account {
        enabled: true,
        in_pool: true,
        ..plain("acct")
    }]);
    app.write().state.prompt_cache_mut().insert(
        "acct:gpt-test".into(),
        super::state::PromptCacheStat {
            account_id: "acct".into(),
            model_id: "gpt-test".into(),
            request_count: 4,
            input_tokens: 1000,
            cached_tokens: 750,
            ..super::state::PromptCacheStat::default()
        },
    );
    let inner = app.read();
    let item = inner.public_dashboard_account(&inner.config.accounts()[0], 0, Timestamp::now());
    assert_eq!(
        (
            get_u64(&item, "cacheInputTokens"),
            get_u64(&item, "cacheCachedTokens"),
            get_u64(&item, "cacheRequestCount")
        ),
        (1000, 750, 4)
    );
}

// ---- prompt-cache windows ----

fn usage(input: u64, cached: u64, write: Option<u64>) -> PromptCacheUsage {
    PromptCacheUsage {
        input_tokens: input,
        cached_tokens: cached,
        cache_write_tokens: write,
        present: true,
        ..PromptCacheUsage::default()
    }
}

/// Go recordPromptCacheUsageLocked: a main-agent request with no affinity.
fn record_usage(app: &App, account: &str, usage: PromptCacheUsage, now: Timestamp) {
    app.write().record_prompt_cache_result(
        account,
        "gpt-test",
        &super::routing::RequestIdentity::default(),
        &usage,
        false,
        false,
        false,
        "",
        now,
    );
}

// Go TestAccountActiveLocked.
#[test]
fn account_active_window() {
    use super::dashboard::account_active;
    let now = Timestamp::now();
    for (name, last, want) in [
        ("never used", Timestamp::zero(), false),
        ("just now", now, true),
        ("within window", now.add(-seconds(30)), true),
        ("on boundary", now.add(-seconds(60)), false),
        ("past window", now.add(-minutes(2)), false),
    ] {
        let health = super::state::AccountHealth {
            last_success_at: last,
            ..super::state::AccountHealth::default()
        };
        assert_eq!(account_active(&health, now), want, "{name}");
    }
}

// Go TestPromptCacheStatsForAccountLocked.
#[test]
fn prompt_cache_stats_for_account() {
    let (app, _dir) = test_app(vec![Account {
        enabled: true,
        in_pool: true,
        ..plain("acct")
    }]);
    {
        let mut inner = app.write();
        for (key, account, model, requests, input, cached) in [
            ("acct:gpt-a", "acct", "gpt-a", 3, 1000, 700),
            ("acct:gpt-b", "acct", "gpt-b", 2, 500, 100),
            ("other:gpt-a", "other", "gpt-a", 9, 9000, 9000),
        ] {
            inner.state.prompt_cache_mut().insert(
                key.into(),
                super::state::PromptCacheStat {
                    account_id: account.into(),
                    model_id: model.into(),
                    request_count: requests,
                    input_tokens: input,
                    cached_tokens: cached,
                    ..super::state::PromptCacheStat::default()
                },
            );
        }
    }
    assert_eq!(
        app.read().prompt_cache_stats_for_account("acct"),
        (1500, 800, 5)
    );
    assert_eq!(
        app.read().prompt_cache_stats_for_account("missing"),
        (0, 0, 0)
    );
}

// Go TestPromptCacheColdStartAndResetWindow.
#[test]
fn prompt_cache_cold_start_and_reset_window() {
    let (app, _dir) = test_app(vec![plain("acct")]);
    let now = Timestamp::now();
    record_usage(&app, "acct", usage(2000, 1900, None), now); // warm
    record_usage(&app, "acct", usage(1500, 0, None), now); // cold start (eligible, no cache)
    record_usage(&app, "acct", usage(500, 0, None), now); // sub-1024: not cache-eligible, not cold
    assert_eq!(
        app.read()
            .state
            .prompt_cache()
            .get("acct:gpt-test:main")
            .map(|stat| stat.cold_request_count),
        Some(1),
        "cold request count"
    );
    let window = app.read().prompt_cache_window();
    assert_eq!(
        (
            get_u64(&window, "inputTokens"),
            get_u64(&window, "cachedTokens"),
            get_u64(&window, "coldRequestCount"),
            get_u64(&window, "requestCount")
        ),
        (4000, 1900, 1, 3),
        "pre-reset window"
    );
    app.write().reset_prompt_cache_window(now);
    let window = app.read().prompt_cache_window();
    assert_eq!(
        (
            get_u64(&window, "inputTokens"),
            get_u64(&window, "cachedTokens"),
            get_u64(&window, "coldRequestCount")
        ),
        (0, 0, 0),
        "window right after reset"
    );
    record_usage(&app, "acct", usage(3000, 2700, None), now);
    record_usage(&app, "acct", usage(1200, 0, None), now);
    let window = app.read().prompt_cache_window();
    assert_eq!(
        (
            get_u64(&window, "inputTokens"),
            get_u64(&window, "cachedTokens"),
            get_u64(&window, "coldRequestCount"),
            get_u64(&window, "requestCount")
        ),
        (4200, 2700, 1, 2),
        "post-reset window"
    );
    let lifetime = app
        .read()
        .state
        .prompt_cache()
        .get("acct:gpt-test:main")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (lifetime.request_count, lifetime.cold_request_count),
        (5, 2),
        "lifetime totals not preserved"
    );
}

// Go TestPromptCacheWriteAggregationAndResetWindow.
#[test]
fn prompt_cache_write_aggregation_and_reset_window() {
    let (app, _dir) = test_app(vec![plain("acct")]);
    let now = Timestamp::now();
    record_usage(&app, "acct", usage(2000, 1500, Some(400)), now);
    record_usage(&app, "acct", usage(1500, 0, None), now);
    record_usage(&app, "acct", usage(1000, 250, Some(0)), now);
    let stat = app
        .read()
        .state
        .prompt_cache()
        .get("acct:gpt-test:main")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (
            stat.request_count,
            stat.usage_observed_request_count,
            stat.cache_hit_request_count,
            stat.cache_eligible_request_count,
            stat.cold_request_count
        ),
        (3, 3, 2, 2, 1),
        "cache request counters"
    );
    assert_eq!(
        (
            stat.cache_write_tokens,
            stat.cache_write_input_tokens,
            stat.cache_write_observed_request_count
        ),
        (400, 3000, 2),
        "cache write counters"
    );
    let window = app.read().prompt_cache_window();
    let main = &window["main"];
    assert_eq!(
        (
            get_u64(main, "cacheWriteTokens"),
            get_u64(main, "cacheWriteInputTokens"),
            get_u64(main, "cacheWriteObservedRequestCount"),
            get_u64(main, "cacheHitRequestCount")
        ),
        (400, 3000, 2, 2),
        "cache write window"
    );
    app.write().reset_prompt_cache_window(now);
    record_usage(
        &app,
        "acct",
        usage(3000, 2400, Some(200)),
        now.add(seconds(1)),
    );
    let window = app.read().prompt_cache_window();
    assert_eq!(
        (
            get_u64(&window, "cacheWriteTokens"),
            get_u64(&window, "cacheWriteObservedRequestCount"),
            get_u64(&window, "cacheHitRequestCount")
        ),
        (200, 1, 1),
        "post-reset write window"
    );
}

// Go TestPromptCacheWindowPerAccountReset.
#[test]
fn prompt_cache_window_per_account_reset() {
    let (app, _dir) = test_app(vec![plain("a"), plain("b")]);
    let now = Timestamp::now();
    record_usage(&app, "a", usage(1000, 800, None), now);
    record_usage(&app, "b", usage(1000, 600, None), now);
    // Reset only account a; b's window must be untouched.
    app.write().reset_prompt_cache_window_for_account("a", now);
    assert_eq!(
        get_u64(
            &app.read().prompt_cache_window_for_account("a"),
            "inputTokens"
        ),
        0,
        "account a window not reset"
    );
    let b = app.read().prompt_cache_window_for_account("b");
    assert_eq!(
        (get_u64(&b, "inputTokens"), get_u64(&b, "cachedTokens")),
        (1000, 600),
        "account b window should be untouched by a's reset"
    );
    record_usage(&app, "a", usage(2000, 1900, None), now);
    let a = app.read().prompt_cache_window_for_account("a");
    assert_eq!(
        (get_u64(&a, "inputTokens"), get_u64(&a, "cachedTokens")),
        (2000, 1900),
        "account a post-reset window"
    );
    // A pool-wide reset clears per-account overrides.
    app.write().reset_prompt_cache_window(now);
    assert!(
        app.read().state.prompt_cache_reset_at_by_account.is_none(),
        "pool-wide reset should clear per-account overrides"
    );
    assert_eq!(
        get_u64(
            &app.read().prompt_cache_window_for_account("b"),
            "inputTokens"
        ),
        0,
        "account b window after pool-wide reset"
    );
}

// ---- public account rows ----

// Go TestPublicDashboardExposesSeatTier.
#[test]
fn public_dashboard_exposes_seat_tier() {
    let (app, _dir) = test_app(vec![Account {
        plan_family: "business".into(),
        plan_type: "business".into(),
        organization_name: "acme".into(),
        seat_type: "premium".into(),
        seat_type_inferred: true,
        ..device_account("biz", "", 100)
    }]);
    let inner = app.read();
    let payload = inner.public_dashboard_account(&inner.config.accounts()[0], 0, Timestamp::now());
    assert_eq!(
        payload["seatType"], "premium",
        "public dashboard dropped the seat tier"
    );
    assert_eq!(
        payload["seatTypeInferred"], true,
        "public dashboard dropped the inferred marker"
    );
    let detail = payload["detail"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        !detail.contains("premium"),
        "inferred seat leaked into the plan detail: {detail}"
    );
}

// Go TestPublicDashboardOmitsUnknownSeatTier.
#[test]
fn public_dashboard_omits_unknown_seat_tier() {
    let (app, _dir) = test_app(vec![Account {
        plan_family: "plus".into(),
        plan_type: "plus".into(),
        ..device_account("plus", "", 100)
    }]);
    let inner = app.read();
    let payload = inner.public_dashboard_account(&inner.config.accounts()[0], 0, Timestamp::now());
    assert!(
        payload["seatType"] == "" && payload["seatTypeInferred"] == false,
        "non-business account reported a seat: {payload}"
    );
}

// Go TestQuotaOrganizationControlsTeamDisplayName.
#[test]
fn quota_organization_controls_team_display_name() {
    let (app, _dir) = test_app(vec![Account {
        email: "user@example.test".into(),
        plan_type: "team".into(),
        ..device_account("acct-team", "", 0)
    }]);
    app.write().state.quotas_mut().insert(
        "acct-team".into(),
        QuotaSnapshot {
            account_id: "acct-team".into(),
            organization_name: "Example Workspace".into(),
            plan_type: "team".into(),
            ..QuotaSnapshot::default()
        },
    );
    let inner = app.read();
    let dashboard =
        inner.public_dashboard_account(&inner.config.accounts()[0], 0, Timestamp::now());
    assert_eq!(
        dashboard["detail"], "Team · Example Workspace",
        "quota organization was not used in team display"
    );
}

// Go TestOrganizationSetActionIsNotAvailable.
#[tokio::test]
async fn organization_set_action_is_not_available() {
    use super::admin_tests::admin_request;
    use super::admin_tests::admin_session;
    let (app, _dir) = super::proxy_tests::arc_app(vec![Account {
        email: "user@example.test".into(),
        plan_type: "team".into(),
        ..device_account("acct-team", "", 0)
    }]);
    let session = admin_session(&app);
    let result = super::proxy_tests::serve(
        &app,
        admin_request(
            &session,
            http::Method::POST,
            "/admin/api/accounts/acct-team/organization/set",
            r#"{"organizationName":"Example Workspace"}"#,
        ),
    )
    .await;
    assert_eq!(
        result.code(),
        404,
        "organization override action: {}",
        result.body_string()
    );
}
