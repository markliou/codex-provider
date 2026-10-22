//! The Go control-page asset tests (main_test.go TestAdminDashboardAssets and
//! TestAdminAssets*), through the Rust listener. The page, script, style
//! sheet, logo and manifest are the Go pool's own files compiled in
//! unchanged; these checks keep what the page must and must not say.

use super::proxy_tests::arc_app;
use super::proxy_tests::serve;
use super::routes::Request;
use http::Method;

async fn asset(path: &str) -> super::http_io::Recorded {
    let (app, _dir) = arc_app(Vec::new());
    let result = serve(&app, Request::test(Method::GET, path, "")).await;
    assert_eq!(result.code(), 200, "GET {path}");
    result
}

fn has_all(path: &str, body: &str, expected: &[&str]) {
    for want in expected {
        assert!(body.contains(want), "GET {path} did not include {want:?}");
    }
}

fn has_none(path: &str, body: &str, forbidden: &[&str]) {
    for word in forbidden {
        assert!(!body.contains(word), "GET {path} still includes {word:?}");
    }
}

// Go TestAdminDashboardAssets (the build version: Go overrode its ldflags
// variables; the Rust version comes from build-time env, so its formatting
// rule is checked directly and the header against what this build reports).
#[tokio::test]
async fn admin_dashboard_assets() {
    assert_eq!(
        super::admin::format_display_version("vtest", "abcdef123456"),
        "vtest+abcdef12"
    );
    for (path, expected) in [
        ("/admin", "Pool status"),
        ("/admin/assets/app.css", ".badge.error"),
        ("/admin/assets/app.js", "Low quota"),
        (
            "/admin/manifest.webmanifest",
            r##""background_color": "#fff6ee""##,
        ),
    ] {
        assert!(
            asset(path).await.body_string().contains(expected),
            "GET {path} did not include {expected:?}"
        );
    }
    let page = asset("/admin").await;
    assert_eq!(
        page.header("x-codex-pool-version"),
        super::admin::admin_display_version()
    );
    assert_eq!(page.header("cache-control"), "no-store");
    let html = page.body_string();
    assert!(
        !html.contains(super::admin::VERSION_PLACEHOLDER)
            && html.contains(&super::admin::admin_display_version()),
        "admin page did not inject the build version"
    );
    has_all(
        "/admin",
        &html,
        &[
            r##"<meta name="color-scheme" content="light">"##,
            r##"<meta name="theme-color" content="#ff7a59">"##,
        ],
    );
    has_none(
        "/admin",
        &html,
        &[
            "Admin sign in",
            "Username",
            "Account ID",
            "Label",
            "Models",
            "Subscription tier",
            "Email hint",
            "Quota hint",
            "account-form",
            "toast",
        ],
    );
    has_all(
        "/admin",
        &html,
        &[
            "ACCESS",
            "Continue",
            "Password",
            "Add account",
            "Use Pro last",
            "Balanced sticky",
            "routing-strategy-pill",
            "SERVICE STATUS",
            "Active routes",
            "device-auth-url",
            "device-auth-code",
            "device-auth-countdown",
            "Copy verification link",
            "Copy verification code",
            r#"data-theme="sunrise""#,
            r#"id="theme-select""#,
            "Choose dashboard theme",
            ">Sunrise<",
            ">Lagoon<",
            ">Meadow<",
            ">Orchid<",
            ">Citrus<",
            r#"class="rail""#,
            r#"class="rail-nav""#,
            r#"id="accounts-section""#,
            r#"id="active-routes""#,
        ],
    );
    has_none(
        "/admin",
        &html,
        &[
            "ADMIN",
            "Sign in",
            "Console",
            "Preserve Pro quota",
            "PUBLIC STATUS",
            "DEVICE AUTH",
            "Passphrase",
            "Sticky sessions",
        ],
    );
    has_all(
        "/admin",
        &html,
        &[
            "cache-window-main",
            "cache-window-subagent",
            "cache-window-lineage-failover",
            "cache-window-request-hit",
            "cache-window-cold-count",
            "metric-source-chip observed",
            "metric-source-chip calculated",
            "POOL",
            "CALC",
            "routing-cache-events",
            "routing-cache-body",
            "throughput-panel",
            "throughput-active",
            "throughput-current",
            "throughput-charts",
            "Up to 48 hours in memory",
            "chart zooms to available history",
            "LIVE FLOW",
            "Throughput",
        ],
    );
    has_none(
        "/admin",
        &html,
        &[
            r#"class="throughput-panel management-only""#,
            r#"id="cache-window-read""#,
            r#"id="cache-window-write""#,
            "cache-window-write-rate",
            r#"id="cache-window-affinity""#,
            ">Cache write<",
            ">Parent affinity<",
            "metric-source-chip upstream",
            "metric-source-group upstream",
            "OPENAI",
        ],
    );
    // The owner's state is never cached either.
    let (app, _dir) = arc_app(Vec::new());
    let session = super::admin_tests::admin_session(&app);
    let state = serve(
        &app,
        super::admin_tests::admin_request(&session, Method::GET, "/admin/api/state", ""),
    )
    .await;
    assert!(
        state.code() == 200 && state.header("cache-control") == "no-store",
        "admin state cache behavior"
    );

    let js = asset("/admin/assets/app.js").await.body_string();
    let path = "/admin/assets/app.js";
    has_none(
        path,
        &js,
        &[
            "Open the verification URL and enter the code",
            "account-form",
        ],
    );
    has_all(path, &js, &["maskRouteKey", "Session "]);
    has_none(
        path,
        &js,
        &[
            "Device login completed",
            "Device login failed",
            "Sticky session cleared",
            "Refresh failed:",
            "No quota window",
            "Detected after login",
            "Need login",
        ],
    );
    has_all(
        path,
        &js,
        &[
            r#"actionButton("login""#,
            "Repair sign-in",
            "history preserved",
            "Sign-in busy",
        ],
    );
    has_all(
        path,
        &js,
        &[
            "loginJob",
            "activeLoginJob",
            "state.currentLoginJobId = activeLoginJob.jobId",
        ],
    );
    has_all(
        path,
        &js,
        &[
            "currentPublicRepairRef",
            "startPublicDeviceAuth",
            "watchPublicLoginJob",
            r#"poolAction === "repair""#,
            "account-owner-note",
            "Who owns this account?",
            "data-owner-note-ref",
            "data-owner-note-account-id",
            "codeExpiresAt",
        ],
    );
    has_all(
        path,
        &js,
        &[
            "dashboardRefreshIntervalMs = 30 * 1000",
            "scheduleDashboardRefresh",
            r#"cache: "no-store""#,
        ],
    );
    has_none(
        path,
        &js,
        &[
            "state.refreshTimer = window.setInterval",
            "foregroundRefreshIntervalMs",
            "backgroundRefreshIntervalMs",
            r#"document.addEventListener("visibilitychange""#,
        ],
    );
    has_all(
        path,
        &js,
        &[
            "preserveProQuota",
            "codexPoolAdminTheme",
            "themeMetaColors",
            "themeNames.has",
            "document.documentElement.dataset.theme",
            "window.localStorage.getItem(themeStorageKey)",
            "window.localStorage.setItem(themeStorageKey, theme)",
            r##"$("#theme-select").addEventListener("change""##,
            "sticky_balanced",
            "activeRouteCount",
        ],
    );
    has_all(
        path,
        &js,
        &[
            "parentAffinityHitCount",
            "parentAffinityFallbackCount",
            "lineageFailoverCount",
            "usageObservedRequestCount",
            "win.subagent",
            "renderRoutingCacheEvents",
            r#"cacheColumnHeader("Main cache")"#,
            r#"cacheColumnHeader("Subagent cache")"#,
            r#"poolColumnHeader("Affinity/Fallback")"#,
            "cache-token-row",
            "cache-rate",
            "Rate-limit failover",
            "Stream capacity failover",
        ],
    );
    has_all(
        path,
        &js,
        &[
            "renderThroughput",
            "throughputChartMarkup",
            "chartVisibleWindow",
            "throughputChartWindows",
            "chart-series-point",
            "chart-range-chip",
            "renderThroughput(body.dashboard.throughput)",
            "accountThroughputMarkup",
            "requestsPerMinute",
            "outputTokensPerSecond",
            "cacheHitRate",
            "p95LatencyMs",
            "Output vs KV cache",
            "Throughput (5m)",
        ],
    );
    has_all(
        path,
        &js,
        &[
            r#"["Cooling down", summary.cooldown"#,
            r#"["Out of pool", summary.standby"#,
            r#"["Duplicate", summary.duplicate"#,
            r#"["Unavailable", summary.unavailable"#,
        ],
    );
    has_none(
        path,
        &js,
        &[
            "p50TTFBMs",
            "p95TTFBMs",
            "p50TTFTMs",
            "p95TTFTMs",
            ">TTFB ",
            ">TTFT ",
            "Token flow",
            "Requests & latency",
            "inputTokensPerMinute",
            "cachedTokensPerMinute",
            "outputTokensPerMinute",
            "p50LatencyMs",
            "Requests/min",
            "over the past 48 hours",
        ],
    );
    has_none(
        path,
        &js,
        &[
            "Main cache (reqs)",
            "Subagent cache (reqs)",
            "<small>(${reqs})</small>",
            "cache-detail",
            "metric-origin-mini",
            "cache-derived-rate",
            "column-origin upstream",
            "column-origin calculated",
            ">OPENAI<",
            ">CALC<",
            r##"$("#cache-window-read")"##,
            r##"$("#cache-window-write")"##,
            r##"$("#cache-window-affinity")"##,
            "cache-window-write-rate",
            "cacheWriteObservedRequestCount",
            "cacheWriteInputTokens",
            "cacheWriteRate",
            "cacheWindow.routingFailoverCount",
            r#"poolColumnHeader("Failovers")"#,
            ">Write</span>",
            "Token usage: read",
            "write ratio",
        ],
    );
    has_all(
        path,
        &js,
        &[
            "displayResetCountdown",
            "displayUnixDateTime",
            r#"hour: "2-digit", minute: "2-digit""#,
            "resetCreditExpiresSoon",
            "resetCreditExpiryWarningWindowMs = 7 * 24 * 60 * 60 * 1000",
            "expiresAt - now < resetCreditExpiryWarningWindowMs",
            "resetCreditsMarkup",
            r#"reset-credit-expiry${expiresSoon ? " expiring-soon" : ""}"#,
            "Expires ${escapeHTML(expires)}",
            "quotaTone",
            "quotaTrackMarkup",
            r#""critical""#,
            r#""watch""#,
            "Resets in",
            "% left",
            "<progress",
            r#"value="${remaining}""#,
        ],
    );
    has_all(
        path,
        &js,
        &[
            "poolMembershipAttribute",
            "account.inPool === false",
            "account.outOfPool === true",
            r#"data-pool-membership="out""#,
        ],
    );
    has_all(
        path,
        &js,
        &[
            "Seat type: Not reported",
            "Usage: ${metadata.planLimit",
            "5-hour policy:",
            "Flexible credits:",
            "Spend control:",
            "Telemetry:",
            "API-metered · ChatGPT quota not applicable",
            "Protected: threshold reached",
            "quota-protection/set",
            "Duplicate slots share upstream capacity",
            "quota.windows",
            "quota.additionalLimits",
        ],
    );
    has_none(path, &js, &["quota-blocked", "Blocked: ${", "toast"]);

    let css = asset("/admin/assets/app.css").await.body_string();
    let path = "/admin/assets/app.css";
    has_all(
        path,
        &css,
        &[
            ".reset-credit-expiry.expiring-soon",
            "color: var(--red)",
            "font-weight: 800",
        ],
    );
    has_all(
        path,
        &css,
        &[
            "::-webkit-progress-value",
            "background: #e1e6dc",
            "border: 1px solid #a8b9b0",
        ],
    );
    has_all(
        path,
        &css,
        &[
            ".quota-track.watch",
            ".quota-track.low",
            ".quota-track.critical",
            ".quota-track.empty",
            "#f2c14e",
            "#f08a3e",
            "#df314d",
        ],
    );
    has_all(
        path,
        &css,
        &[
            r#"tr[data-pool-membership="out"] .quota-track"#,
            "border-style: dashed",
            "#c4819d",
            "color-mix(in srgb, var(--quota-start) 46%",
            "color-mix(in srgb, var(--quota-end) 46%",
        ],
    );
    has_none(
        path,
        &css,
        &["tr:has(.badge.standby) .quota-track", ".quota-blocked"],
    );
    has_all(
        path,
        &css,
        &["#fff6ee", "#ff7a59", "#6c5ce7", "#0d8f78", "#f2c14e"],
    );
    has_all(
        path,
        &css,
        &[
            r#":root[data-theme="lagoon"]"#,
            r#":root[data-theme="meadow"]"#,
            r#":root[data-theme="orchid"]"#,
            r#":root[data-theme="citrus"]"#,
            ".theme-picker",
            "--body-background",
            "--rail-bg",
            "--hero-bg",
            "--summary-featured-bg",
            "--throughput-bg",
            "--table-head-bg",
            "prefers-reduced-motion",
            ".app-shell:has(#login-view:not([hidden])) .rail-nav",
            "grid-template-columns: repeat(7",
        ],
    );
    has_none(
        path,
        &css,
        &[
            "#120a2e", "#46e6ff", "#ff6f91", "#c095ff", "#0f0b16", "#302440", "#1b1526",
        ],
    );
    has_all(
        path,
        &css,
        &[
            ".cache-column",
            ".routing-count-column",
            ".cache-window-groups",
            ".metric-source-chip.observed",
            ".metric-source-chip.calculated",
            ".cache-token-row",
            ".cache-rate { color: var(--green)",
            ".cache-token-value { color: var(--green)",
            ".routing-cache-table",
            ".event-cache.hit",
            ".event-cache.cold",
            ".throughput-panel",
            ".throughput-current-grid",
            ".throughput-current-stat",
            ".throughput-chart-grid",
            ".throughput-chart-card",
            ".throughput-chart-plot",
            ".throughput-chart-title",
            ".chart-range-chip",
            ".chart-series-line",
            ".chart-series-point",
            ".account-throughput",
            ".throughput-column",
        ],
    );
    has_none(
        path,
        &css,
        &[
            ".metric-source-chip.upstream",
            ".metric-source-group.upstream",
            ".column-origin.upstream",
            ".column-origin.calculated",
            ".cache-rate.good",
            ".cache-rate.fair",
            ".cache-rate.poor",
            ".cache-derived-rate",
        ],
    );
    let logo = asset("/admin/assets/logo.svg").await.body_string();
    has_all(
        "/admin/assets/logo.svg",
        &logo,
        &["Balanced live routes", "#49b6a4", "#f06449", "#f2c14e"],
    );
}

// Go TestExpiringResetCreditOutlinesTheStatusBadge.
#[tokio::test]
async fn expiring_reset_credit_outlines_the_status_badge() {
    let js = asset("/admin/assets/app.js").await.body_string();
    has_all(
        "/admin/assets/app.js",
        &js,
        &[
            "function resetCreditExpiryFlag(quota)",
            "function statusBadgeMarkup(tone, label, quota)",
            "expiring-reset",
            // It reuses the one seven-day rule rather than restating the window.
            "resetCreditExpiresSoon(credits.expiresAt)",
            // Nothing to lose when no credit is available, so nothing is marked.
            "Number(credits.availableCount) > 0",
            // Both tables carry it: the same fact is public.
            "statusBadgeMarkup(health.status, statusLabel(health.status), health.quota)",
            "statusBadgeMarkup(tone, label, account.quota)",
        ],
    );
    // The outline must not replace the tone: a Ready account holding an
    // expiring credit is still Ready, so the rule may only take the border.
    let style = asset("/admin/assets/app.css").await.body_string();
    let index = style.find(".badge.expiring-reset").unwrap_or_else(|| {
        panic!("app.css does not outline a badge with an expiring reset credit")
    });
    let mut rule = &style[index..];
    if let Some(end) = rule.find('}') {
        rule = &rule[..end];
    }
    assert!(
        rule.contains("var(--red)"),
        "the outline is not alert red: {rule}"
    );
    if let Some(open) = rule.find('{') {
        for declaration in rule[open + 1..].split(';') {
            let property = declaration.split(':').next().unwrap_or_default().trim();
            assert!(
                property.is_empty() || property == "border-color" || property == "box-shadow",
                "the outline set {property:?}: {rule}"
            );
        }
    }
    // Declared after the tones, or the tone borders would win.
    assert!(
        index > style.find(".badge.ready").unwrap_or(usize::MAX),
        "the outline is declared before the tone borders it must override"
    );
}

// Go TestDashboardReadsTheQuotaFloorFromTheServer.
#[tokio::test]
async fn dashboard_reads_the_quota_floor_from_the_server() {
    let js = asset("/admin/assets/app.js").await.body_string();
    has_all(
        "/admin/assets/app.js",
        &js,
        &[
            "state.quotaFloorPercent = finiteMetric(serviceState.quotaFloorPercent)",
            "state.quotaFloorPercent = finiteMetric(body.dashboard.quotaFloorPercent)",
            "remaining > state.quotaFloorPercent",
        ],
    );
    has_none(
        "/admin/assets/app.js",
        &js,
        &["windowRemaining(entry) !== 0"],
    );
}

// Go TestAdminAssetsGateTheResetCreditButton.
#[tokio::test]
async fn admin_assets_gate_the_reset_credit_button() {
    let js = asset("/admin/assets/app.js").await.body_string();
    has_all(
        "/admin/assets/app.js",
        &js,
        &[
            "quota/reset-credit",
            "Use reset credit",
            "managementAccountId && available",
        ],
    );
    // The public renderer must pass no account id, so the control cannot
    // appear.
    has_none(
        "/admin/assets/app.js",
        &js,
        &[
            "quotaMarkup(account.remainingQuota, account.quota, null, null, account.quotaFreshness, account.lastSuccessfulRefreshAt, account.quotaMetering, account.id)",
        ],
    );
}

// Go TestAdminAssetsLabelInferredSeat.
#[tokio::test]
async fn admin_assets_label_inferred_seat() {
    let js = asset("/admin/assets/app.js").await.body_string();
    has_all(
        "/admin/assets/app.js",
        &js,
        &[
            "seatTypeInferred",
            "(inferred)",
            "publicSeatMarkup",
            "account-seat",
        ],
    );
}

// Go TestAdminAssetsMarkTheRoutingGateOnQuotaWindows.
#[tokio::test]
async fn admin_assets_mark_the_routing_gate_on_quota_windows() {
    let js = asset("/admin/assets/app.js").await.body_string();
    has_all(
        "/admin/assets/app.js",
        &js,
        &[
            "quota-window-exhausted",
            r"\u00b7 Exhausted",
            "quota-window-constraint",
            "unavailable (",
            "windowBlocksRouting",
            "Date.now() / 1000 < resetAt",
            "exhausted: blocking",
            "blockedBy: blocking ? [] : gatingLabels",
        ],
    );
    let css = asset("/admin/assets/app.css").await.body_string();
    has_all(
        "/admin/assets/app.css",
        &css,
        &[
            ".quota-window-held .quota-track",
            ".quota-window-exhausted { color: var(--red); }",
            ".quota-window-constraint { color: var(--red);",
        ],
    );
    has_none(
        "/admin/assets/app.js",
        &js,
        &[
            "Rate gate",
            "Budget",
            "blocking this account",
            "quota-window-gate",
            "quota-window-role",
        ],
    );
}

// Go TestAdminAssetsReportTheResetOutcomeBesideTheControl.
#[tokio::test]
async fn admin_assets_report_the_reset_outcome_beside_the_control() {
    let js = asset("/admin/assets/app.js").await.body_string();
    has_all(
        "/admin/assets/app.js",
        &js,
        &[
            "resetCreditNotice",
            "reset-credit-notice",
            // The failure path must record a notice, not only call notify().
            "state.resetCreditNotice = { accountId: id, message: error.message, ok: false }",
            // And the control itself must show the request is in flight.
            r#"button.textContent = "Spending…""#,
            // The notice lives inside the quota disclosure the refresh rebuilds;
            // without a preserved expanded state a failed spend reads as a
            // button that did nothing.
            "state.openQuotaDetails",
            "data-details-key",
        ],
    );
    let css = asset("/admin/assets/app.css").await.body_string();
    has_all(
        "/admin/assets/app.css",
        &css,
        &[".reset-credit-notice.failed"],
    );
}
