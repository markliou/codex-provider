//! What the control page shows: account rows, summary cards, the pool
//! capacity bars, throughput and prompt-cache windows.
//!
//! A port of the dashboard projections in cmd/codex-pool/main.go
//! (poolCapacityLocked, dashboardSummaryLocked, throughputSnapshotLocked,
//! promptCacheWindowFilteredLocked, accountHealthItemLocked,
//! publicDashboardAccountLocked and their helpers). The public projections are
//! the redaction boundary of an unauthenticated page: they carry masked
//! display names, plan metadata and opaque references, never local or
//! upstream account ids, credentials or request-level attribution.

use super::app::Inner;
use super::outcome::THROUGHPUT_BUCKET_INTERVAL_MINUTES;
use super::outcome::THROUGHPUT_BUCKET_TTL_HOURS;
use super::outcome::THROUGHPUT_LATENCY_BOUNDS_MILLIS;
use super::outcome::sub_sat;
use super::plan;
use super::quota;
use super::state::Account;
use super::state::AccountHealth;
use super::state::AccountQuota;
use super::state::QuotaWindow;
use super::state::ThroughputBucket;
use super::status::active_cooldowns;
use super::status::to_value;
use super::time::Timestamp;
use super::time::hours;
use super::time::minutes;
use chrono::TimeDelta;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::HashMap;

/// A successful request within this long marks an account as currently
/// being consumed.
const ACCOUNT_ACTIVE_WINDOW_SECONDS: i64 = 60;
const THROUGHPUT_SERIES_INTERVAL_MINUTES: i64 = 10;
const FIVE_HOUR_WINDOW_MINUTES: i64 = 300;

pub fn account_active(health: &AccountHealth, now: Timestamp) -> bool {
    !health.last_success_at.is_zero()
        && now.sub(&health.last_success_at) < TimeDelta::seconds(ACCOUNT_ACTIVE_WINDOW_SECONDS)
}

/// One window's pool-wide reading, shaped for the bar the dashboard draws.
/// The two percentages share one denominator, the pool's whole weighted
/// allowance, so they sit on a fixed 0-100 scale and never sum past it.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolCapacityBar {
    pub label: String,
    pub window_minutes: i64,
    /// What routing can spend right now, as a share of the whole pool.
    pub spendable_percent: f64,
    /// How much further the reading would rise if every enabled account
    /// were routable.
    pub recoverable_percent: f64,
    pub spendable_accounts: i64,
    pub recoverable_accounts: i64,
    /// Recoverable allowances routing holds back rather than an operator
    /// having parked them.
    pub blocked_accounts: i64,
    pub weighted: bool,
    /// An assumed multiplier contributed to the reading.
    pub assumed: bool,
}

/// One quota allowance and the local slot that represents it. Several slots
/// can hold copies of one credential and share its allowance.
struct CapacityIdentity {
    quota: AccountQuota,
    weight: f64,
    assumed: bool,
    /// Some slot for this allowance is one routing will select.
    spendable: bool,
    /// The allowance has an in-pool slot routing refuses (the duplicate
    /// guard): real capacity, not idle by choice.
    blocked: bool,
}

/// The window of exactly this duration upstream reported. One it did not
/// report is absent evidence, not a zero.
pub fn reported_window_of_duration(
    quota: &AccountQuota,
    window_minutes: i64,
) -> Option<QuotaWindow> {
    quota::quota_reported_windows(quota)
        .into_iter()
        .find(|window| window.present && window.window_minutes == Some(window_minutes))
}

impl CapacityIdentity {
    /// How much of this allowance the window leaves, as a fraction, if the
    /// window can be read for it at all. An account with no five-hour cap is
    /// fully available on that axis, and one whose weekly budget is spent can
    /// serve nothing whatever its five-hour window says.
    fn window_share(&self, window_minutes: i64, floor: f64) -> Option<f64> {
        let spent = |window: &QuotaWindow| quota::quota_window_spent(window, floor);
        let weekly = reported_window_of_duration(&self.quota, quota::WEEKLY_WINDOW_MINUTES);
        if window_minutes == quota::WEEKLY_WINDOW_MINUTES {
            let weekly = weekly?;
            // Held back by the floor is not spendable.
            return Some(if spent(&weekly) {
                0.0
            } else {
                quota::quota_window_remaining(&weekly) / 100.0
            });
        }
        if weekly.as_ref().is_some_and(spent) {
            return Some(0.0);
        }
        if let Some(window) = reported_window_of_duration(&self.quota, window_minutes) {
            return Some(if spent(&window) {
                0.0
            } else {
                quota::quota_window_remaining(&window) / 100.0
            });
        }
        // No window of its own: uncapped only when something else about the
        // account was reported, so a failed refresh is not read as full.
        weekly.map(|_| 1.0)
    }
}

fn choose(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

#[derive(Default)]
struct ThroughputAggregate {
    requests: u64,
    successes: u64,
    failures: u64,
    cancelled: u64,
    streaming: u64,
    usage_observed: u64,
    output_observed: u64,
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    total_duration_millis: u64,
    duration_histogram: Vec<u64>,
}

impl ThroughputAggregate {
    fn add(&mut self, bucket: &ThroughputBucket) {
        self.requests += bucket.request_count;
        self.successes += bucket.success_count;
        self.failures += bucket.failure_count;
        self.cancelled += bucket.cancelled_count;
        self.streaming += bucket.streaming_request_count;
        self.usage_observed += bucket.usage_observed_request_count;
        self.output_observed += bucket.output_observed_request_count;
        self.input_tokens += bucket.input_tokens;
        self.cached_tokens += bucket.cached_tokens;
        self.output_tokens += bucket.output_tokens;
        self.total_duration_millis += bucket.total_duration_millis;
        if self.duration_histogram.len() < bucket.duration_histogram.len() {
            self.duration_histogram
                .resize(bucket.duration_histogram.len(), 0);
        }
        for (total, value) in self
            .duration_histogram
            .iter_mut()
            .zip(&bucket.duration_histogram)
        {
            *total += value;
        }
    }
}

fn histogram_percentile_millis(histogram: &[u64], percentile: u64) -> Value {
    let total: u64 = histogram.iter().sum();
    if total == 0 {
        return Value::Null;
    }
    let target = (total * percentile).div_ceil(100);
    let mut seen = 0;
    for (index, count) in histogram.iter().enumerate() {
        seen += count;
        if seen < target {
            continue;
        }
        let bound = THROUGHPUT_LATENCY_BOUNDS_MILLIS
            .get(index)
            .or(THROUGHPUT_LATENCY_BOUNDS_MILLIS.last())
            .copied()
            .unwrap_or_default();
        return json!(bound);
    }
    Value::Null
}

fn window_seconds(window: TimeDelta) -> i64 {
    window.num_seconds()
}

/// Output throughput is all observed output tokens over the wall-clock
/// window, never summed per-request generation time, which under-reports
/// parallel work.
fn add_output_and_cache(
    result: &mut serde_json::Map<String, Value>,
    aggregate: &ThroughputAggregate,
    window: TimeDelta,
) {
    let seconds = window.num_milliseconds() as f64 / 1000.0;
    result.insert(
        "outputTokensPerSecond".into(),
        if aggregate.output_observed > 0 {
            json!(aggregate.output_tokens as f64 / seconds)
        } else {
            Value::Null
        },
    );
    result.insert(
        "cacheHitRate".into(),
        if aggregate.input_tokens > 0 {
            json!(
                aggregate.cached_tokens.min(aggregate.input_tokens) as f64
                    / aggregate.input_tokens as f64
            )
        } else {
            Value::Null
        },
    );
}

fn aggregate_projection(aggregate: &ThroughputAggregate, window: TimeDelta) -> Value {
    let mut window_minutes = window.num_milliseconds() as f64 / 60_000.0;
    if window_minutes <= 0.0 {
        window_minutes = THROUGHPUT_BUCKET_INTERVAL_MINUTES as f64;
    }
    let per_minute = |value: u64| value as f64 / window_minutes;
    let mut result = serde_json::Map::new();
    for (name, value) in [
        ("windowSeconds", json!(window_seconds(window))),
        ("requestCount", json!(aggregate.requests)),
        ("successCount", json!(aggregate.successes)),
        ("failureCount", json!(aggregate.failures)),
        ("cancelledCount", json!(aggregate.cancelled)),
        ("streamingRequestCount", json!(aggregate.streaming)),
        ("usageObservedRequestCount", json!(aggregate.usage_observed)),
        (
            "outputObservedRequestCount",
            json!(aggregate.output_observed),
        ),
        ("inputTokens", json!(aggregate.input_tokens)),
        ("cachedTokens", json!(aggregate.cached_tokens)),
        ("outputTokens", json!(aggregate.output_tokens)),
        ("requestsPerMinute", json!(per_minute(aggregate.requests))),
        (
            "inputTokensPerMinute",
            json!(per_minute(aggregate.input_tokens)),
        ),
        (
            "cachedTokensPerMinute",
            json!(per_minute(aggregate.cached_tokens)),
        ),
        (
            "outputTokensPerMinute",
            json!(per_minute(aggregate.output_tokens)),
        ),
        (
            "averageLatencyMs",
            if aggregate.requests == 0 {
                Value::Null
            } else {
                json!(aggregate.total_duration_millis as f64 / aggregate.requests as f64)
            },
        ),
        (
            "p50LatencyMs",
            histogram_percentile_millis(&aggregate.duration_histogram, 50),
        ),
        (
            "p95LatencyMs",
            histogram_percentile_millis(&aggregate.duration_histogram, 95),
        ),
    ] {
        result.insert(name.into(), value);
    }
    add_output_and_cache(&mut result, aggregate, window);
    result.insert(
        "successRate".into(),
        if aggregate.requests > 0 {
            json!(aggregate.successes as f64 / aggregate.requests as f64)
        } else {
            Value::Null
        },
    );
    Value::Object(result)
}

/// Public status buckets for the unauthenticated page.
pub fn public_dashboard_status(status: &str) -> (&'static str, &'static str) {
    match status {
        "ready" => ("ready", "Ready"),
        "low" => ("low", "Limited"),
        "protected" => ("low", "Protected"),
        "exhausted" => ("low", "Exhausted"),
        "cooldown" => ("cooldown", "Cooling down"),
        "standby" => ("standby", "Out of pool"),
        // Grouped with standby visually, but labelled explicitly so the slot
        // is not read as extra capacity.
        "duplicate" => ("standby", "Duplicate"),
        _ => ("error", "Unavailable"),
    }
}

pub fn public_pool_label(item: &Account) -> &'static str {
    if item.pending_auth_verification {
        "Verification pending"
    } else if !item.enabled {
        "Unavailable"
    } else if !item.in_pool {
        "Out of pool"
    } else {
        "In pool"
    }
}

pub fn public_pool_action(item: &Account, repair_available: bool) -> &'static str {
    if repair_available {
        "repair"
    } else if item.pending_auth_verification {
        ""
    } else if item.in_pool {
        "pool-remove"
    } else {
        "pool-add"
    }
}

pub fn public_pool_action_label(
    item: &Account,
    repair_available: bool,
    repair_active: bool,
) -> &'static str {
    if repair_available {
        if repair_active {
            "Continue repair"
        } else {
            "Repair"
        }
    } else if item.pending_auth_verification {
        ""
    } else if item.in_pool {
        "Leave pool"
    } else {
        "Join pool"
    }
}

fn public_dashboard_account_detail(item: &Account) -> String {
    if plan::normalize_plan_type(&item.plan_type) == "unknown" {
        return String::new();
    }
    plan::account_plan_display_name(item, false)
}

/// The management projection of one slot (Go publicAccount; the index is
/// not part of it).
pub fn public_account(item: &Account) -> Value {
    let display_name = plan::management_credential_display_name(item);
    let metadata = plan::credential_metadata(item);
    let mut result = json!({
        "id": item.id,
        "label": display_name,
        "displayName": display_name,
        "ownerNote": item.owner_note,
        "credentialMetadata": metadata,
        "authType": item.auth_type,
        "enabled": item.enabled,
        "inPool": item.in_pool,
        "priority": item.priority,
        "remainingQuota": item.remaining_quota,
        "quotaProtectionEnabled": item.quota_protection_enabled,
        "quotaProtectionThreshold": item.quota_protection_threshold,
        "quotaMetering": plan::quota_metering_kind(item),
        "allowedModels": nil_if_empty(&item.allowed_models),
        "excludedModels": nil_if_empty(&item.excluded_models),
        "wireApi": item.wire_api,
        "hasUpstreamApiKey": !item.upstream_api_key.is_empty(),
        "lastLoginAt": item.last_login_at,
    });
    for name in [
        "email",
        "organizationName",
        "rawPlanType",
        "planFamily",
        "planType",
        "planLimit",
        "seatType",
        "seatTypeRaw",
        "seatTypeDisplay",
        "seatTypeInferred",
        "quotaPolicy",
        "planDisplayName",
        "planRank",
    ] {
        result[name] = metadata[name].clone();
    }
    result
}

/// Go's nil slice marshals as null; an empty Rust Vec would read as [].
fn nil_if_empty(values: &[String]) -> Value {
    if values.is_empty() {
        Value::Null
    } else {
        json!(values)
    }
}

pub fn public_accounts(values: &[Account]) -> Vec<Value> {
    values.iter().map(public_account).collect()
}

impl Inner {
    pub fn prompt_cache_stats_for_account(&self, account_id: &str) -> (u64, u64, u64) {
        self.state
            .prompt_cache()
            .values()
            .filter(|stat| stat.account_id == account_id)
            .fold((0, 0, 0), |(input, cached, requests), stat| {
                (
                    input + stat.input_tokens,
                    cached + stat.cached_tokens,
                    requests + stat.request_count,
                )
            })
    }

    /// The pool-wide prompt-cache totals since the last reset (PromptCache
    /// minus PromptCacheBaseline), so the dashboard reports a hit rate over
    /// fresh traffic. With no reset yet the window equals the lifetime totals.
    pub fn prompt_cache_window(&self) -> Value {
        self.prompt_cache_window_filtered("", self.state.prompt_cache_reset_at)
    }

    /// The per-account window, with the account's own reset time when set.
    pub fn prompt_cache_window_for_account(&self, account_id: &str) -> Value {
        let reset_at = self
            .state
            .prompt_cache_reset_at_by_account()
            .get(account_id)
            .copied()
            .unwrap_or(self.state.prompt_cache_reset_at);
        self.prompt_cache_window_filtered(account_id, reset_at)
    }

    fn prompt_cache_window_filtered(&self, account_id: &str, reset_at: Timestamp) -> Value {
        const AGENT_FIELDS: [&str; 14] = [
            "inputTokens",
            "cachedTokens",
            "requestCount",
            "usageObservedRequestCount",
            "cacheWriteTokens",
            "cacheWriteInputTokens",
            "cacheWriteObservedRequestCount",
            "cacheHitRequestCount",
            "cacheEligibleRequestCount",
            "coldRequestCount",
            "parentAffinityHitCount",
            "parentAffinityFallbackCount",
            "lineageFailoverCount",
            "routingFailoverCount",
        ];
        let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
        let mut agents: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
        for kind in ["main", "subagent"] {
            agents.insert(kind, AGENT_FIELDS.iter().map(|field| (*field, 0)).collect());
        }
        let baseline = self.state.prompt_cache_baseline();
        for (key, stat) in self.state.prompt_cache() {
            if !account_id.is_empty() && stat.account_id != account_id {
                continue;
            }
            let base = baseline.get(key).cloned().unwrap_or_default();
            let deltas = [
                sub_sat(stat.input_tokens, base.input_tokens),
                sub_sat(stat.cached_tokens, base.cached_tokens),
                sub_sat(stat.request_count, base.request_count),
                sub_sat(
                    stat.usage_observed_request_count,
                    base.usage_observed_request_count,
                ),
                sub_sat(stat.cache_write_tokens, base.cache_write_tokens),
                sub_sat(stat.cache_write_input_tokens, base.cache_write_input_tokens),
                sub_sat(
                    stat.cache_write_observed_request_count,
                    base.cache_write_observed_request_count,
                ),
                sub_sat(stat.cache_hit_request_count, base.cache_hit_request_count),
                sub_sat(
                    stat.cache_eligible_request_count,
                    base.cache_eligible_request_count,
                ),
                sub_sat(stat.cold_request_count, base.cold_request_count),
                sub_sat(
                    stat.parent_affinity_hit_count,
                    base.parent_affinity_hit_count,
                ),
                sub_sat(
                    stat.parent_affinity_fallback_count,
                    base.parent_affinity_fallback_count,
                ),
                sub_sat(stat.lineage_failover_count, base.lineage_failover_count),
                sub_sat(stat.routing_failover_count, base.routing_failover_count),
            ];
            let kind = if stat.agent_kind == "subagent" {
                "subagent"
            } else {
                "main"
            };
            for (field, delta) in AGENT_FIELDS.iter().zip(deltas) {
                *totals.entry(field).or_default() += delta;
                if let Some(agent) = agents.get_mut(kind) {
                    *agent.entry(field).or_default() += delta;
                }
            }
            *totals.entry("unrecognizedFailureCount").or_default() += sub_sat(
                stat.unrecognized_failure_count,
                base.unrecognized_failure_count,
            );
        }
        let mut result = serde_json::Map::new();
        for field in AGENT_FIELDS
            .iter()
            .chain(std::iter::once(&"unrecognizedFailureCount"))
        {
            result.insert(
                (*field).to_string(),
                json!(totals.get(field).copied().unwrap_or_default()),
            );
        }
        result.insert("main".into(), json!(agents.get("main")));
        result.insert("subagent".into(), json!(agents.get("subagent")));
        result.insert("resetAt".into(), to_value(&reset_at));
        Value::Object(result)
    }

    /// Snapshots the current totals as the new baseline, starting a fresh
    /// pool-wide window without discarding lifetime totals, and clears
    /// per-account overrides so every account shares this reset time.
    pub fn reset_prompt_cache_window(&mut self, now: Timestamp) {
        let baseline = self.state.prompt_cache().clone();
        *self.state.prompt_cache_baseline_mut() = baseline;
        self.state.prompt_cache_reset_at = now;
        self.state.prompt_cache_reset_at_by_account = None;
    }

    /// Rebaselines one account's keys and records a per-account reset time.
    pub fn reset_prompt_cache_window_for_account(&mut self, account_id: &str, now: Timestamp) {
        let own: Vec<(String, super::state::PromptCacheStat)> = self
            .state
            .prompt_cache()
            .iter()
            .filter(|(_, stat)| stat.account_id == account_id)
            .map(|(key, stat)| (key.clone(), stat.clone()))
            .collect();
        self.state.prompt_cache_baseline_mut().extend(own);
        self.state
            .prompt_cache_reset_at_by_account_mut()
            .insert(account_id.to_string(), now);
    }

    pub fn throughput_snapshot(&self, account_id: &str, now: Timestamp) -> Value {
        let mut result = json!({
            "bucketIntervalSeconds": THROUGHPUT_BUCKET_INTERVAL_MINUTES * 60,
            "retentionHours": THROUGHPUT_BUCKET_TTL_HOURS,
        });
        if !account_id.is_empty() {
            // Account rows stay a compact recent view; the 48-hour series is
            // pool-wide so the public payload cannot reveal traffic
            // attribution for an individual credential.
            result["windows"] = json!({"5m": self.throughput_window(account_id, now, minutes(5))});
            return result;
        }
        result["activeRequests"] = json!(self.active_proxy_requests);
        result["seriesIntervalSeconds"] = json!(THROUGHPUT_SERIES_INTERVAL_MINUTES * 60);
        result["current"] =
            self.throughput_window("", now, minutes(THROUGHPUT_SERIES_INTERVAL_MINUTES));
        result["series"] = Value::Array(self.throughput_series("", now));
        result
    }

    fn bucket_visible(
        &self,
        bucket: &ThroughputBucket,
        account_id: &str,
        now: Timestamp,
        cutoff: Timestamp,
    ) -> bool {
        (account_id.is_empty() || bucket.account_id == account_id)
            && !bucket.bucket_at.after(&now)
            && bucket
                .bucket_at
                .add(minutes(THROUGHPUT_BUCKET_INTERVAL_MINUTES))
                .after(&cutoff)
    }

    pub fn throughput_window(&self, account_id: &str, now: Timestamp, window: TimeDelta) -> Value {
        let cutoff = now.add(-window);
        let mut aggregate = ThroughputAggregate::default();
        for bucket in self
            .throughput_buckets
            .iter()
            .filter(|bucket| self.bucket_visible(bucket, account_id, now, cutoff))
        {
            aggregate.add(bucket);
        }
        aggregate_projection(&aggregate, window)
    }

    pub fn throughput_series(&self, account_id: &str, now: Timestamp) -> Vec<Value> {
        let interval = minutes(THROUGHPUT_SERIES_INTERVAL_MINUTES);
        let end = now.truncate(interval);
        let start = end.add(-hours(THROUGHPUT_BUCKET_TTL_HOURS));
        let cutoff = now.add(-hours(THROUGHPUT_BUCKET_TTL_HOURS));
        let mut aggregates: HashMap<i64, ThroughputAggregate> = HashMap::new();
        for bucket in self
            .throughput_buckets
            .iter()
            .filter(|bucket| self.bucket_visible(bucket, account_id, now, cutoff))
        {
            let at = bucket.bucket_at.truncate(interval);
            if at.before(&start) || at.after(&end) {
                continue;
            }
            aggregates.entry(at.unix()).or_default().add(bucket);
        }
        if aggregates.is_empty() {
            // An empty series lets the UI tell "no history yet" from a real
            // idle 48 hours; a chart of zeroes is not manufactured.
            return Vec::new();
        }
        let mut points = Vec::new();
        let mut at = start;
        while !at.after(&end) {
            let window_start = if cutoff.after(&at) { cutoff } else { at };
            let window_end = if now.before(&at.add(interval)) {
                now
            } else {
                at.add(interval)
            };
            // The first and newest points can be partial buckets: use their
            // visible wall time, floored at one minute and capped at the
            // interval, so boundary rates are neither diluted nor amplified.
            let window = window_end
                .sub(&window_start)
                .max(minutes(THROUGHPUT_BUCKET_INTERVAL_MINUTES))
                .min(interval);
            let empty = ThroughputAggregate::default();
            let aggregate = aggregates.get(&at.unix()).unwrap_or(&empty);
            // The 48-hour history carries only the two metrics the chart shows.
            let mut point = serde_json::Map::new();
            point.insert("windowSeconds".into(), json!(window_seconds(window)));
            add_output_and_cache(&mut point, aggregate, window);
            point.insert("at".into(), to_value(&at));
            points.push(Value::Object(point));
            at = at.add(interval);
        }
        points
    }

    /// The raw status counts and the unavailable roll-up behind the cards
    /// above the account table. Standby and duplicate stay separate: both can
    /// be unroutable, but only standby is out of the pool. The visible
    /// mutually-exclusive cards always reconcile to the total.
    pub fn dashboard_summary(&self, now: Timestamp) -> BTreeMap<&'static str, i64> {
        let mut summary: BTreeMap<&'static str, i64> = [
            "ready",
            "low",
            "protected",
            "exhausted",
            "cooldown",
            "standby",
            "duplicate",
            "error",
            "missing_auth",
            "authenticating",
            "disabled",
            "unavailable",
        ]
        .into_iter()
        .map(|name| (name, 0))
        .collect();
        summary.insert(
            "total",
            i64::try_from(self.config.accounts().len()).unwrap_or_default(),
        );
        for item in self.config.accounts() {
            let (status, _) = self.account_status(item, now);
            match summary.get_mut(status) {
                Some(count) => *count += 1,
                None => {
                    *summary.entry("unavailable").or_default() += 1;
                    continue;
                }
            }
            if matches!(
                status,
                "protected"
                    | "exhausted"
                    | "error"
                    | "missing_auth"
                    | "authenticating"
                    | "disabled"
            ) {
                *summary.entry("unavailable").or_default() += 1;
            }
        }
        summary
    }

    /// Only operational buckets, not the auth-specific breakdown; mutually
    /// exclusive and adding up to the total.
    pub fn public_dashboard_summary(&self, now: Timestamp) -> BTreeMap<&'static str, i64> {
        let detailed = self.dashboard_summary(now);
        let get = |name: &str| detailed.get(name).copied().unwrap_or_default();
        [
            ("total", get("total")),
            ("ready", get("ready")),
            ("low", get("low") + get("protected") + get("exhausted")),
            ("cooldown", get("cooldown")),
            ("standby", get("standby")),
            ("duplicate", get("duplicate")),
            (
                "unavailable",
                get("unavailable") - get("protected") - get("exhausted"),
            ),
        ]
        .into_iter()
        .collect()
    }

    /// What actually owns one quota allowance: the workspace and the member's
    /// email. Members of one Business workspace share its account id while
    /// each holds their own allowance; two slots from one login share one. A
    /// slot with no email of its own counts alone.
    pub fn capacity_allowance_key(&self, item: &Account) -> String {
        let workspace = self.upstream_workspace_key(item);
        let mut email = plan::normalize_email(&item.email);
        if email.is_empty()
            && let Ok(auth) = self.codex_auth(item)
        {
            email = plan::normalize_email(&auth.email);
        }
        if workspace.is_empty() && email.is_empty() {
            return format!("slot:{}", item.id);
        }
        format!("allowance:{workspace}|{email}")
    }

    fn capacity_identities(&self, now: Timestamp) -> Vec<CapacityIdentity> {
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, CapacityIdentity> = HashMap::new();
        for item in self.config.accounts().iter().filter(|item| item.enabled) {
            // Routing's own verdict, not a reimplementation of it.
            let blocked = item.in_pool
                && !self
                    .duplicate_upstream_account_primary(item, now)
                    .is_empty();
            let spendable = item.in_pool && !blocked;
            let Some(snapshot) = self.state.quotas().get(&item.id) else {
                continue;
            };
            let Some(quota) = snapshot.quota.clone() else {
                continue;
            };
            let key = self.capacity_allowance_key(item);
            let (weight, assumed) = plan::account_weekly_weight(
                &choose(&snapshot.plan_family, &item.plan_family),
                &choose(&snapshot.plan_limit, &item.plan_limit),
                &choose(&snapshot.seat_type, &item.seat_type),
                self.tuning.premium_seat_weight,
            );
            match groups.get_mut(&key) {
                None => {
                    order.push(key.clone());
                    groups.insert(
                        key,
                        CapacityIdentity {
                            quota,
                            weight,
                            assumed,
                            spendable,
                            blocked,
                        },
                    );
                }
                Some(entry) => {
                    entry.blocked = entry.blocked || blocked;
                    // Spendability belongs to the allowance: if any copy can
                    // route, prefer that copy's snapshot and multiplier.
                    if spendable {
                        entry.spendable = true;
                        entry.quota = quota;
                        entry.weight = weight;
                        entry.assumed = assumed;
                    }
                }
            }
        }
        order
            .into_iter()
            .filter_map(|key| groups.remove(&key))
            .collect()
    }

    /// Both windows across the pool, each a share of the whole weighted
    /// allowance: the solid fill is what routing can spend now, the dashed run
    /// what the pool would hold if every enabled account were reachable.
    pub fn pool_capacity(&self, now: Timestamp) -> Vec<PoolCapacityBar> {
        let identities = self.capacity_identities(now);
        let mut bars = Vec::new();
        for window_minutes in [FIVE_HOUR_WINDOW_MINUTES, quota::WEEKLY_WINDOW_MINUTES] {
            let mut bar = PoolCapacityBar {
                label: quota::quota_window_label(Some(window_minutes)),
                window_minutes,
                weighted: true,
                ..PoolCapacityBar::default()
            };
            let (mut total, mut spendable, mut recoverable) = (0.0, 0.0, 0.0);
            for identity in &identities {
                let Some(share) =
                    identity.window_share(window_minutes, self.tuning.quota_floor_percent)
                else {
                    continue;
                };
                total += identity.weight;
                if identity.spendable {
                    spendable += identity.weight * share;
                    bar.spendable_accounts += 1;
                } else {
                    recoverable += identity.weight * share;
                    bar.recoverable_accounts += 1;
                    if identity.blocked {
                        bar.blocked_accounts += 1;
                    }
                }
                bar.assumed |= identity.assumed;
            }
            // Nothing reported this window: no denominator, and a bar drawn
            // against nothing would read as exhaustion.
            if total <= 0.0 {
                continue;
            }
            bar.spendable_percent = spendable / total * 100.0;
            bar.recoverable_percent = recoverable / total * 100.0;
            bars.push(bar);
        }
        bars
    }

    pub fn active_route_count(&self, account_id: &str, now: Timestamp) -> i64 {
        let count = self
            .state
            .sticky_sessions()
            .values()
            .filter(|route| {
                route.account_id == account_id && !self.sticky_session_expired(route, now)
            })
            .count();
        i64::try_from(count).unwrap_or_default()
    }

    /// The management health row of one slot.
    pub fn account_health_item(&self, item: &Account, now: Timestamp) -> Value {
        let cooldowns = active_cooldowns(
            self.state
                .cooldowns()
                .get(&item.id)
                .map_or(&[][..], Vec::as_slice),
            now,
        );
        let (status, reason) = self.account_status(item, now);
        let health = self
            .state
            .health()
            .get(&item.id)
            .cloned()
            .unwrap_or_default();
        let snapshot = quota::quota_snapshot_for_display(&self.quota_snapshot(&item.id), now);
        let (cache_input, cache_cached, cache_requests) =
            self.prompt_cache_stats_for_account(&item.id);
        let mut result = json!({
            "accountId": item.id,
            "available": status == "ready" || status == "low",
            "status": status,
            "statusReason": reason,
            "cooldowns": cooldowns,
            "lastSuccessAt": health.last_success_at,
            "lastFailureAt": health.last_failure_at,
            "lastFailureReason": health.last_failure_reason,
            "consecutiveFailure": health.consecutive_failure,
            "active": account_active(&health, now),
            "activeRouteCount": self.active_route_count(&item.id, now),
            "cacheInputTokens": cache_input,
            "cacheCachedTokens": cache_cached,
            "cacheRequestCount": cache_requests,
            "cacheWindow": self.prompt_cache_window_for_account(&item.id),
            "throughput": self.throughput_snapshot(&item.id, now),
            "remainingQuota": item.remaining_quota,
            "quota": to_value(&snapshot.quota),
            "usageUpdatedAt": snapshot.usage_updated_at,
            "lastSuccessfulRefreshAt": snapshot.last_successful_refresh_at,
            "quotaFreshness": snapshot.freshness,
            "quotaProvenance": snapshot.provenance,
            "quotaError": to_value(&snapshot.quota_error),
            "quotaProtection": to_value(&self.quota_protection_status(item, &snapshot, now)),
            "quotaMetering": plan::quota_metering_kind(item),
        });
        if let Some(job) = self.active_login_job_for_account(&item.id) {
            // Management-only: a page reload recovers the device URL and code
            // instead of orphaning a live login.
            result["loginJob"] = to_value(job);
        }
        result
    }

    /// An opaque per-process reference for public actions: never the local
    /// or upstream account id.
    pub fn public_account_ref(&self, account_id: &str) -> String {
        use base64::Engine;
        let key: &[u8] = if self.settings.session_key.is_empty() {
            b"codex-pool-public-account-ref"
        } else {
            &self.settings.session_key
        };
        let sum =
            super::crypto::hmac_sha256(key, format!("public-account:{account_id}").as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&sum[..16])
    }

    pub fn public_account_ref_matches(&self, account_id: &str, reference: &str) -> bool {
        super::crypto::constant_time_eq(
            self.public_account_ref(account_id).as_bytes(),
            reference.as_bytes(),
        )
    }

    /// The public dashboard row of one slot.
    pub fn public_dashboard_account(&self, item: &Account, index: usize, now: Timestamp) -> Value {
        let (status, _) = self.account_status(item, now);
        let snapshot = quota::quota_snapshot_for_display(&self.quota_snapshot(&item.id), now);
        let mut display = item.clone();
        if !snapshot.organization_name.is_empty() {
            display
                .organization_name
                .clone_from(&snapshot.organization_name);
        }
        if !snapshot.plan_type.is_empty() {
            display.plan_type.clone_from(&snapshot.plan_type);
            display.plan_family.clone_from(&snapshot.plan_family);
            display.raw_plan_type.clone_from(&snapshot.raw_plan_type);
            display.plan_rank = plan::plan_rank(&snapshot.plan_type);
        }
        if !snapshot.plan_limit.is_empty() {
            display.plan_limit.clone_from(&snapshot.plan_limit);
        }
        // The snapshot is authoritative once it carries a seat, but before the
        // first successful refresh it carries none; overwriting then would
        // blank the seat already persisted on the slot.
        if !snapshot.seat_type.is_empty() {
            display.seat_type.clone_from(&snapshot.seat_type);
            display.seat_type_inferred = snapshot.seat_type_inferred;
        }
        if !snapshot.seat_type_raw.is_empty() {
            display.seat_type_raw.clone_from(&snapshot.seat_type_raw);
        }
        display.quota_policy.clone_from(&snapshot.quota_policy);
        let (tone, label) = public_dashboard_status(status);
        let remaining = display
            .remaining_quota
            .or_else(|| snapshot.quota.as_ref().map(quota::remaining_quota_hint));
        let (cache_input, cache_cached, cache_requests) =
            self.prompt_cache_stats_for_account(&item.id);
        let repair_available = self.public_repair_available(item);
        let seat = plan::clean_metadata_token(&display.seat_type);
        json!({
            "displayName": plan::public_credential_display_name(&display, Some(index)),
            "detail": public_dashboard_account_detail(&display),
            // The seat tier travels beside the plan detail: an inferred one
            // must never read as part of the plan name upstream reported.
            "seatType": seat,
            "seatTypeInferred": display.seat_type_inferred && !seat.is_empty(),
            "ownerNote": item.owner_note,
            "statusTone": tone,
            "statusLabel": label,
            // Membership is separate from the tone: duplicates use the neutral
            // standby presentation while their slot stays in the pool.
            "outOfPool": !item.in_pool,
            "poolLabel": public_pool_label(item),
            "poolRef": self.public_account_ref(&item.id),
            "poolAction": public_pool_action(item, repair_available),
            "poolActionLabel": public_pool_action_label(item, repair_available, self.public_repair_job_active(&item.id)),
            "remainingQuota": remaining,
            "quota": to_value(&snapshot.quota),
            "quotaUnavailable": snapshot.quota_error.is_some(),
            "quotaFreshness": snapshot.freshness,
            "lastSuccessfulRefreshAt": snapshot.last_successful_refresh_at,
            "quotaMetering": plan::quota_metering_kind(item),
            "active": account_active(&self.state.health().get(&item.id).cloned().unwrap_or_default(), now),
            "cacheInputTokens": cache_input,
            "cacheCachedTokens": cache_cached,
            "cacheRequestCount": cache_requests,
            "cacheWindow": self.prompt_cache_window_for_account(&item.id),
        })
    }

    /// The owner's whole operational state.
    pub fn admin_state(&self, now: Timestamp) -> Value {
        let or_default = |value: &str, fallback: &str| {
            if value.is_empty() {
                fallback.to_string()
            } else {
                value.to_string()
            }
        };
        json!({
            "running": true,
            "routingStrategy": self.effective_routing_strategy(),
            "defaultModel": self.config.default_model,
            "preserveProQuota": self.preserve_pro_quota,
            "lineageAffinity": self.lineage_affinity,
            "quotaPrime": self.quota_prime,
            "promptCacheKeyMode": or_default(&self.settings.prompt_cache_key_mode, "auto"),
            "promptCacheKeyScope": or_default(&self.settings.prompt_cache_key_scope, "auto"),
            "promptCacheKeyPolicy": or_default(&self.settings.prompt_cache_key_policy, "preserve"),
            "promptCacheBuckets": self.settings.prompt_cache_buckets,
            "promptCacheRetention": self.settings.prompt_cache_retention,
            "promptCache": to_value(self.state.prompt_cache()),
            "promptCacheWindow": self.prompt_cache_window(),
            "throughput": self.throughput_snapshot("", now),
            "routingCacheEvents": self.routing_cache_event_views(now),
            "threadBindings": to_value(self.state.thread_bindings()),
            "accounts": public_accounts(self.config.accounts()),
            "requestCount": self.state.request_count,
            "successCount": self.state.success_count,
            "failureCount": self.state.failure_count,
            "upstreamResponseFailedCount": self.state.upstream_response_failed_count,
            "streamIncompleteCount": self.state.stream_incomplete_count,
            "summary": self.dashboard_summary(now),
            "poolCapacity": self.pool_capacity(now),
            "quotaFloorPercent": self.tuning.quota_floor_percent,
        })
    }

    /// The public page's payload: pool-wide aggregates only. Per-account
    /// throughput, raw buckets, model attribution and request-level routing
    /// events stay management-only.
    pub fn public_dashboard(&self, now: Timestamp) -> Value {
        let accounts: Vec<Value> = self
            .config
            .accounts()
            .iter()
            .enumerate()
            .map(|(index, item)| self.public_dashboard_account(item, index, now))
            .collect();
        json!({
            "updatedAt": self.state.updated_at,
            "summary": self.public_dashboard_summary(now),
            "poolCapacity": self.pool_capacity(now),
            "quotaFloorPercent": self.tuning.quota_floor_percent,
            "accounts": accounts,
            "promptCacheWindow": self.prompt_cache_window(),
            "throughput": self.throughput_snapshot("", now),
        })
    }
}
