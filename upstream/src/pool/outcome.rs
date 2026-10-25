//! What the pool records once an attempt has an outcome: account health and
//! cooldowns, sticky, thread, lineage and response bindings, prompt-cache
//! statistics, routing events, throughput, and the log line for a failure
//! with Codex's own verdict beside the pool's.
//!
//! A port of markFailure, markSuccess*, markTerminalResponseFailure,
//! recordLineageSuccessLocked, appendRoutingCacheEventLocked and the
//! throughput recording in the Go pool's main.go, and of codex_verdict.go.
//! The verdict is Codex's code compiled into this process
//! (crate::classify), so it is asked directly rather than over loopback.

use super::app::App;
use super::app::Inner;
use super::app::manual_quota_available;
use super::app::quota_error_blocks_routing;
use super::hash::operational_identifier_hash;
use super::plan::is_codex_device_auth;
use super::quota;
use super::quota::sanitized_error_code;
use super::response_info::PROMPT_CACHE_MIN_TOKENS;
use super::response_info::PromptCacheUsage;
use super::response_info::ProxyResponseInfo;
use super::response_info::terminal_error_code;
use super::routing::ROUTING_CACHE_EVENT_LIMIT;
use super::routing::RequestIdentity;
use super::routing::RoutingDecision;
use super::routing::identity_parent_id;
use super::routing::thread_binding_state_key;
use super::state::AccountHealth;
use super::state::Cooldown;
use super::state::LineageBinding;
use super::state::QuotaErrorInfo;
use super::state::ResponseBinding;
use super::state::RoutingCacheEvent;
use super::state::StickySession;
use super::state::ThreadBinding;
use super::state::ThroughputBucket;
use super::time::Timestamp;
use super::time::minutes;
use super::time::seconds;
use chrono::TimeDelta;
use std::collections::HashMap;

pub const UPSTREAM_5XX_COOLDOWN_SECONDS: i64 = 10;
pub const ROUTING_CACHE_EVENT_VIEW_LIMIT: usize = 50;
pub const UPSTREAM_5XX_FAILOVER_AFTER: i64 = 3;
pub const UPSTREAM_5XX_FAILURE_WINDOW_MINUTES: i64 = 2;
/// Throughput buckets are in-memory operational aggregates, not a request
/// log. One-minute account buckets keep enough resolution for five-minute
/// management rows; 48 hours are kept, with a hard cap so telemetry cannot
/// consume memory without limit. They are never written to runtime.json.
pub const THROUGHPUT_BUCKET_INTERVAL_MINUTES: i64 = 1;
pub const THROUGHPUT_BUCKET_TTL_HOURS: i64 = 48;
pub const THROUGHPUT_BUCKET_LIMIT: usize = 100_000;
pub const THROUGHPUT_LATENCY_BOUNDS_MILLIS: &[u64] = &[
    100, 250, 500, 1000, 2000, 5000, 10000, 30000, 60000, 120000, 300000, 600000, 900000,
];

#[derive(Clone, Debug, Default)]
pub struct ThroughputMeasurement {
    pub started_at: Timestamp,
    pub completed_at: Timestamp,
    pub account_id: String,
    pub streaming: bool,
    pub success: bool,
    pub cancelled: bool,
    pub finished: bool,
    pub usage: PromptCacheUsage,
    /// The model and why a cancelled request ended, for its log line.
    pub model: String,
    pub cancel_reason: &'static str,
}

/// Prices the cooldown for a retryable terminal failure. Overload,
/// throttling and a server fault all describe a server that needs a moment
/// rather than an allowance that is spent, so they share the longer 5xx
/// cooldown; anything else gets the shorter default.
pub fn retryable_terminal_cooldown(reason: &str) -> TimeDelta {
    match reason {
        "server_is_overloaded" | "slow_down" | "server_error" | "internal_server_error" => {
            seconds(UPSTREAM_5XX_COOLDOWN_SECONDS)
        }
        // A spent allowance does not come back in seconds. Hold the account
        // out until the next quota refresh has seen the spent window, which
        // then keeps it out (or lets it back the moment its owner resets it); a
        // short cooldown let new sessions land on it and bounce off in the gap.
        // The longest poll interval, since the next poll may come that late.
        "usage_limit_reached"
        | "usage_not_included"
        | "insufficient_quota"
        | "credit_balance_exhausted"
        | "organization_spend_limit_exceeded"
        | "project_spend_limit_exceeded"
        | "organization_usage_limit_exceeded" => {
            TimeDelta::from_std(super::quota_schedule::MAX_INTERVAL).unwrap_or_else(|_| minutes(6))
        }
        _ => seconds(30),
    }
}

/// The cooldowns a retry round may clear: a server-side condition recorded
/// moments earlier, which is exactly what the round is waiting out. A drained
/// allowance (rate_limited) is not one of them, so its cooldown stands.
pub fn retryable_cooldown_reason(reason: &str) -> bool {
    matches!(
        reason,
        "server_is_overloaded"
            | "slow_down"
            | "upstream_5xx"
            | "server_error"
            | "internal_server_error"
    )
}

pub fn normalized_routing_outcome(value: &str) -> &str {
    match value {
        "sticky_reuse"
        | "new_route_assignment"
        | "parent_affinity"
        | "parent_affinity_fallback"
        | "lineage_follow"
        | "quota_failover"
        | "rate_limit_failover"
        | "stream_capacity_failover"
        | "stream_transient_failover"
        | "auth_failover"
        | "transport_failover"
        | "repeated_5xx_failover" => value,
        _ => "",
    }
}

/// Internal fallback routing historically calls its prompt-prefix hash
/// source "prompt". Diagnostics expose the product-level "fallback" name
/// instead; the internal source is not renamed because cache-key
/// compatibility logic also consumes it.
pub fn routing_event_source(source: &str) -> &str {
    match source {
        "previous_response_id"
        | "thread_id"
        | "session"
        | "project"
        | "prompt_cache_key"
        | "conversation"
        | "session_id"
        | "conversation_id" => source,
        _ => "fallback",
    }
}

pub fn token_balance(cached: u64, written: u64) -> i64 {
    if cached >= written {
        i64::try_from(cached - written).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(written - cached).unwrap_or(i64::MAX)
    }
}

pub fn sub_sat(value: u64, base: u64) -> u64 {
    value.saturating_sub(base)
}

fn agent_kind(identity: &RequestIdentity) -> &'static str {
    if identity.is_subagent {
        "subagent"
    } else {
        "main"
    }
}

fn elapsed_millis(start: Timestamp, end: Timestamp) -> u64 {
    if start.is_zero() || end.is_zero() || end.before(&start) {
        return 0;
    }
    u64::try_from(end.sub(&start).num_milliseconds()).unwrap_or(0)
}

pub fn add_histogram_observation(mut histogram: Vec<u64>, value: u64) -> Vec<u64> {
    histogram.resize(THROUGHPUT_LATENCY_BOUNDS_MILLIS.len() + 1, 0);
    let index = THROUGHPUT_LATENCY_BOUNDS_MILLIS
        .iter()
        .position(|bound| value <= *bound)
        .unwrap_or(THROUGHPUT_LATENCY_BOUNDS_MILLIS.len());
    histogram[index] += 1;
    histogram
}

// ---- Codex's verdict, for the log ----

/// Renders a verdict for a log line: "codex=<kind>/<api> retry=yes", with
/// "codex_skips" when Codex itself does not act on the message.
pub fn verdict_log_field(verdict: &crate::classify::Verdict) -> String {
    let mut retry = if verdict.retryable {
        "yes".to_string()
    } else {
        "no".to_string()
    };
    if verdict.retryable
        && let Some(after) = verdict.retry_after_ms
    {
        retry.push_str(&format!("({after}ms)"));
    }
    let or_dash = |value: &str| {
        if value.is_empty() {
            "-".to_string()
        } else {
            value.to_string()
        }
    };
    let mut field = format!(
        "codex={}/{} retry={retry}",
        or_dash(verdict.kind),
        or_dash(&verdict.api)
    );
    if !verdict.acted {
        field.push_str(" codex_skips");
    }
    field
}

/// Codex's verdict on a terminal stream failure, the event as upstream sent
/// it on SSE or the Responses WebSocket.
pub fn verdict_for_terminal(info: &ProxyResponseInfo) -> Option<crate::classify::Verdict> {
    if info.terminal_data.is_empty() {
        return None;
    }
    Some(if info.terminal_source == "ws" {
        crate::classify::ws_message(&info.terminal_data)
    } else {
        crate::classify::sse_event(&info.terminal_data)
    })
}

fn or_dash(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

impl App {
    /// Records an upstream HTTP refusal (401/403, 429, 5xx) with the pool's
    /// reason and Codex's verdict on the same answer.
    pub fn log_codex_verdict_for_http(
        &self,
        account_id: &str,
        model: &str,
        status: http::StatusCode,
        headers: &http::HeaderMap,
        body: &[u8],
        pool_reason: &str,
    ) {
        let text = super::response_info::bounded_verdict_data(&String::from_utf8_lossy(body));
        let verdict = crate::classify::http_response(status, headers.clone(), Some(text));
        let code = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(body)
            .ok()
            .and_then(|payload| {
                payload
                    .get("error")
                    .and_then(serde_json::Value::as_object)
                    .map(terminal_error_code)
            })
            .unwrap_or_default();
        self.logger.printf(&format!(
            "upstream http failure: account={account_id} model={model} status={} pool={} code={} {}",
            status.as_u16(),
            or_dash(pool_reason),
            or_dash(&code),
            verdict_log_field(&verdict)
        ));
    }

    /// Which identity produced an upstream terminal failure. The caller is
    /// shown upstream's message and nothing else, so an operator holding the
    /// request id from that message has no way to tell which account or model
    /// produced it, nor one account faulting from a whole pool faulting,
    /// unless this line exists. Every retried attempt logs too.
    pub fn log_terminal_response_failure(
        &self,
        account_id: &str,
        model: &str,
        info: &ProxyResponseInfo,
        retried: bool,
    ) {
        let mut line = format!(
            "upstream terminal failure: account={account_id} model={model} event={} class={} code={} disposition={}",
            or_dash(&info.terminal_event),
            or_dash(&info.terminal_failure_class),
            or_dash(&info.terminal_error_code),
            if retried { "retried" } else { "final" }
        );
        if info.terminal_event == "response.incomplete" {
            // Codex names a missing reason "unknown" (sse/responses.rs).
            let reason = info.terminal_incomplete_reason.as_str();
            line.push_str(&format!(
                " reason={}",
                if reason.is_empty() { "unknown" } else { reason }
            ));
        }
        if !info.request_id.is_empty() {
            line.push_str(&format!(" request={}", info.request_id));
        }
        if !info.terminal_error_message.is_empty() {
            line.push_str(&format!(" message={:?}", info.terminal_error_message));
        }
        if let Some(verdict) = verdict_for_terminal(info) {
            line.push(' ');
            line.push_str(&verdict_log_field(&verdict));
        }
        self.logger.printf(&line);
    }

    pub fn begin_throughput_measurement(&self, streaming: bool) -> ThroughputMeasurement {
        self.write().active_proxy_requests += 1;
        ThroughputMeasurement {
            started_at: Timestamp::now(),
            streaming,
            ..ThroughputMeasurement::default()
        }
    }

    pub fn finish_throughput_measurement(&self, measurement: &mut ThroughputMeasurement) {
        if measurement.finished {
            return;
        }
        if measurement.cancelled {
            self.log_interrupted_request(measurement);
        }
        self.write()
            .finish_throughput_measurement(Some(measurement), Timestamp::now());
    }

    /// One line for a request that ended without an answer: the client left,
    /// the upstream socket closed, or a failure was handed to Codex's
    /// reconnect. Before this line such a request only raised the dashboard's
    /// cancelled count, so a turn that broke off left nothing in the log to
    /// say why.
    fn log_interrupted_request(&self, measurement: &ThroughputMeasurement) {
        let elapsed = Timestamp::now().sub(&measurement.started_at);
        self.logger.printf(&format!(
            "request interrupted: account={} model={} after={:.1}s reason={}",
            or_dash(&measurement.account_id),
            or_dash(&measurement.model),
            elapsed.num_milliseconds() as f64 / 1000.0,
            or_dash(measurement.cancel_reason),
        ));
    }

    /// Returns the account's consecutive failures, counting this one.
    pub fn mark_failure(
        &self,
        account_id: &str,
        model: &str,
        reason: &str,
        duration: TimeDelta,
    ) -> i64 {
        let mut inner = self.write();
        let now = Timestamp::now();
        let mut health = inner
            .state
            .health()
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        // Repeated 5xx failover is a recent-health signal, not a permanent
        // strike counter. If an account had an old blip and then serves a hot
        // sticky route later, one fresh 5xx must not immediately move the
        // route and lose KV cache locality.
        if reason == "upstream_5xx"
            && (health.last_failure_reason != reason
                || now.sub(&health.last_failure_at) > minutes(UPSTREAM_5XX_FAILURE_WINDOW_MINUTES))
        {
            health.consecutive_failure = 0;
        }
        health.last_failure_at = now;
        health.last_failure_reason = reason.to_string();
        health.consecutive_failure += 1;
        let consecutive = health.consecutive_failure;
        inner
            .state
            .health_mut()
            .insert(account_id.to_string(), health);
        inner.state.failure_count += 1;
        if duration > TimeDelta::zero() {
            inner.append_cooldown(account_id, model, reason, now.add(duration), now);
        }
        let _ = inner.save();
        consecutive
    }

    pub fn mark_cooldown(&self, account_id: &str, model: &str, reason: &str, duration: TimeDelta) {
        if duration <= TimeDelta::zero() {
            return;
        }
        let mut inner = self.write();
        let now = Timestamp::now();
        inner.append_cooldown(account_id, model, reason, now.add(duration), now);
        let _ = inner.save();
    }

    pub fn mark_account_auth_failure(&self, account_id: &str, model: &str, reason: &str) {
        let mut reason = sanitized_error_code(reason);
        if reason.is_empty() {
            reason = "account_auth_failed".to_string();
        }
        let mut inner = self.write();
        let now = Timestamp::now();
        inner.note_failure_health(account_id, &reason, now);
        inner.state.failure_count += 1;
        inner.quarantine_credential(account_id, model, &reason, now);
        let _ = inner.save();
    }

    /// Drops the cooldowns a capacity refusal placed on these accounts for
    /// one model. A capacity round only reaches this point once every identity
    /// has refused, and it has since waited; the cooldowns were recorded to
    /// steer traffic away during the blip, so continuing to honour them would
    /// keep the pool pinned to whichever account happened not to be cooled and
    /// starve the identities that may already have recovered. Only capacity
    /// reasons are cleared, so an auth or quota cooldown still stands.
    pub fn clear_capacity_cooldowns(&self, account_ids: &HashMap<String, bool>, model: &str) {
        if account_ids.is_empty() {
            return;
        }
        let mut inner = self.write();
        let mut changed = false;
        for account_id in account_ids.keys() {
            let entries = inner
                .state
                .cooldowns()
                .get(account_id)
                .cloned()
                .unwrap_or_default();
            let kept: Vec<Cooldown> = entries
                .into_iter()
                .filter(|entry| {
                    let clear = entry.model_id == model && retryable_cooldown_reason(&entry.reason);
                    changed |= clear;
                    !clear
                })
                .collect();
            if kept.is_empty() {
                inner.state.cooldowns_mut().remove(account_id);
            } else {
                inner.state.cooldowns_mut().insert(account_id.clone(), kept);
            }
        }
        if changed {
            let _ = inner.save();
        }
    }

    /// coolDown must be false when the caller intends to retry this same
    /// account inside the current request: a cooldown would remove it from
    /// selection and the retry could never be routed back to it.
    pub fn mark_retryable_terminal_response_failure(
        &self,
        account_id: &str,
        model: &str,
        info: &ProxyResponseInfo,
        cool_down: bool,
    ) {
        self.log_terminal_response_failure(account_id, model, info, true);
        let mut inner = self.write();
        let now = Timestamp::now();
        let mut reason = sanitized_error_code(&info.terminal_error_code);
        if reason.is_empty() {
            reason = "upstream_response_failed".to_string();
        }
        inner.note_failure_health(account_id, &reason, now);
        if cool_down {
            inner.append_cooldown(
                account_id,
                model,
                &reason,
                now.add(retryable_terminal_cooldown(&reason)),
                now,
            );
        }
        // An upstream failed attempt recovered inside one client request.
        // Count the upstream failure and protect the account, but do not
        // finish the request-level throughput measurement, increment
        // RequestCount, or create a failed routing event; the eventual final
        // response owns those records.
        inner.state.failure_count += 1;
        inner.state.upstream_response_failed_count += 1;
        let _ = inner.save();
    }

    /// Records a failed response. cool_down false keeps a capacity or
    /// transient failure from cooling the account, for an overload the
    /// session is still allowed to retry on the same account.
    pub fn mark_terminal_response_failure(
        &self,
        route: &RoutingDecision,
        model: &str,
        account_id: &str,
        info: &ProxyResponseInfo,
        measurement: Option<&mut ThroughputMeasurement>,
        cool_down: bool,
    ) {
        self.log_terminal_response_failure(account_id, model, info, false);
        let mut inner = self.write();
        let now = Timestamp::now();
        inner.finish_throughput_measurement(measurement, now);
        let class = if info.terminal_failure_class.is_empty() {
            "unknown"
        } else {
            info.terminal_failure_class.as_str()
        };
        let mut reason = sanitized_error_code(&info.terminal_error_code);
        if reason.is_empty() {
            reason = "upstream_response_failed".to_string();
        }
        if info.terminal_event == "response.incomplete" {
            reason = "stream_incomplete".to_string();
        }
        match class {
            "capacity" | "transient" => {
                inner.note_failure_health(account_id, &reason, now);
                if cool_down {
                    inner.append_cooldown(
                        account_id,
                        model,
                        &reason,
                        now.add(retryable_terminal_cooldown(&reason)),
                        now,
                    );
                }
            }
            "authentication" => {
                inner.note_failure_health(account_id, &reason, now);
                inner.quarantine_credential(account_id, model, &reason, now);
            }
            // Request-specific and unknown terminal failures intentionally do
            // not touch account health. Context length, invalid prompt, policy
            // rejection, or a new unknown code does not prove another
            // credential could serve the request.
            _ => {}
        }
        inner.record_prompt_cache_result(
            account_id,
            model,
            &route.identity,
            &info.usage,
            false,
            false,
            false,
            "upstream_response_failed",
            now,
        );
        if class == "unknown" {
            inner.note_unrecognized_failure(account_id, model, &route.identity);
        }
        inner.append_routing_cache_event(
            route,
            model,
            account_id,
            &info.failover_from_account_id,
            "upstream_response_failed",
            info,
            now,
        );
        inner.state.request_count += 1;
        inner.state.failure_count += 1;
        match info.terminal_event.as_str() {
            "response.failed" => inner.state.upstream_response_failed_count += 1,
            "response.incomplete" => inner.state.stream_incomplete_count += 1,
            _ => {}
        }
        let _ = inner.save();
    }

    pub fn mark_success(
        &self,
        route: &RoutingDecision,
        model: &str,
        account_id: &str,
        info: &ProxyResponseInfo,
    ) {
        self.mark_success_with_measurement(route, model, account_id, info, None);
    }

    pub fn mark_success_with_measurement(
        &self,
        route: &RoutingDecision,
        model: &str,
        account_id: &str,
        info: &ProxyResponseInfo,
        measurement: Option<&mut ThroughputMeasurement>,
    ) {
        let mut inner = self.write();
        let now = Timestamp::now();
        inner.finish_throughput_measurement(measurement, now);
        let key = route.sticky_key.clone();
        let ttl = inner.sticky_ttl();
        let mut health = inner
            .state
            .health()
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        health.last_success_at = now;
        health.consecutive_failure = 0;
        inner
            .state
            .health_mut()
            .insert(account_id.to_string(), health);
        let prior = inner
            .state
            .sticky_sessions()
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let mut failover_from = prior.failover_from.clone();
        if !prior.account_id.is_empty() && prior.account_id != account_id {
            failover_from.clone_from(&prior.account_id);
        }
        inner.state.sticky_sessions_mut().insert(
            key.clone(),
            StickySession {
                key: key.clone(),
                model_id: model.to_string(),
                account_id: account_id.to_string(),
                created_at: if prior.created_at.is_zero() {
                    now
                } else {
                    prior.created_at
                },
                last_success_at: now,
                expires_at: now.add(ttl),
                failover_from,
                ..StickySession::default()
            },
        );
        let mut lineage_failover = route.identity.is_subagent
            && !prior.account_id.is_empty()
            && prior.account_id != account_id;
        if !route.identity.thread_id.is_empty() {
            let binding_key = thread_binding_state_key(model, &route.identity.thread_id);
            let prior_binding = inner
                .state
                .thread_bindings()
                .get(&binding_key)
                .cloned()
                .unwrap_or_default();
            if route.identity.is_subagent
                && !prior_binding.account_id.is_empty()
                && prior_binding.account_id != account_id
            {
                lineage_failover = true;
            }
            let lineage_root_id = if route.identity.lineage_root_id.is_empty() {
                route.identity.thread_id.clone()
            } else {
                route.identity.lineage_root_id.clone()
            };
            inner.state.thread_bindings_mut().insert(
                binding_key,
                ThreadBinding {
                    thread_id: route.identity.thread_id.clone(),
                    session_id: route.identity.session_id.clone(),
                    parent_thread_id: identity_parent_id(&route.identity).to_string(),
                    lineage_root_id,
                    subagent_kind: route.identity.subagent_kind.clone(),
                    model_id: model.to_string(),
                    account_id: account_id.to_string(),
                    sticky_key: key.clone(),
                    prompt_cache_key: route.upstream_prompt_cache_key.clone(),
                    created_at: if prior_binding.created_at.is_zero() {
                        now
                    } else {
                        prior_binding.created_at
                    },
                    last_success_at: now,
                    expires_at: now.add(ttl),
                    ..ThreadBinding::default()
                },
            );
        }
        if route.lineage_affinity && !route.identity.lineage_root_id.is_empty() {
            inner.record_lineage_success(route, account_id, now);
        }
        if !info.response_id.is_empty() {
            inner.state.response_bindings_mut().insert(
                info.response_id.clone(),
                ResponseBinding {
                    response_id: info.response_id.clone(),
                    sticky_key: key,
                    model_id: model.to_string(),
                    account_id: account_id.to_string(),
                    created_at: now,
                    expires_at: now.add(ttl),
                    ..ResponseBinding::default()
                },
            );
        }
        let parent_affinity_hit = route.parent_affinity_attempted
            && !route.lineage_account_id.is_empty()
            && route.lineage_account_id == account_id;
        let parent_affinity_fallback = route.parent_affinity_attempted && !parent_affinity_hit;
        if route.parent_affinity_attempted
            && !route.lineage_account_id.is_empty()
            && route.lineage_account_id != account_id
        {
            lineage_failover = true;
        }
        let (outcome, failover_from_account_id) = inner.routing_outcome(
            route,
            model,
            account_id,
            &prior,
            info,
            parent_affinity_hit,
            parent_affinity_fallback,
            now,
        );
        inner.record_prompt_cache_result(
            account_id,
            model,
            &route.identity,
            &info.usage,
            parent_affinity_hit,
            parent_affinity_fallback,
            lineage_failover,
            &outcome,
            now,
        );
        inner.append_routing_cache_event(
            route,
            model,
            account_id,
            &failover_from_account_id,
            &outcome,
            info,
            now,
        );
        inner.state.request_count += 1;
        inner.state.success_count += 1;
        let _ = inner.save();
    }
}

impl Inner {
    /// Records a cooldown and drops the account's expired ones in the same
    /// pass. Selection already ignores an expired entry, so keeping it changed
    /// no behavior, but nothing ever removed it either: the list only grew,
    /// for the life of the volume. Pruning on write keeps it proportional to
    /// what is actually in force without adding a sweeper. Only entries whose
    /// retry time has passed are dropped; an active cooldown is evidence still
    /// in force.
    pub fn append_cooldown(
        &mut self,
        account_id: &str,
        model: &str,
        reason: &str,
        retry_at: Timestamp,
        now: Timestamp,
    ) {
        let mut kept: Vec<Cooldown> = self
            .state
            .cooldowns()
            .get(account_id)
            .into_iter()
            .flatten()
            .filter(|entry| entry.next_retry_at.after(&now))
            .cloned()
            .collect();
        kept.push(Cooldown {
            model_id: model.to_string(),
            next_retry_at: retry_at,
            reason: reason.to_string(),
            ..Cooldown::default()
        });
        self.state
            .cooldowns_mut()
            .insert(account_id.to_string(), kept);
    }

    fn note_failure_health(&mut self, account_id: &str, reason: &str, now: Timestamp) {
        let mut health: AccountHealth = self
            .state
            .health()
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        health.last_failure_at = now;
        health.last_failure_reason = reason.to_string();
        health.consecutive_failure += 1;
        self.state
            .health_mut()
            .insert(account_id.to_string(), health);
    }

    /// A refused credential: a device-auth account records a quota error that
    /// gates routing until it signs in again; an API-key provider cools down.
    fn quarantine_credential(
        &mut self,
        account_id: &str,
        model: &str,
        reason: &str,
        now: Timestamp,
    ) {
        if self.account(account_id).is_some_and(is_codex_device_auth) {
            let mut prior = self.quota_snapshot(account_id);
            prior.account_id = account_id.to_string();
            prior.quota_error = Some(QuotaErrorInfo {
                code: reason.to_string(),
                message: "account credential is unavailable; sign in again".to_string(),
                timestamp: now,
                ..QuotaErrorInfo::default()
            });
            self.state
                .quotas_mut()
                .insert(account_id.to_string(), prior);
        } else {
            self.append_cooldown(account_id, model, reason, now.add(minutes(15)), now);
        }
    }

    pub fn finish_throughput_measurement(
        &mut self,
        measurement: Option<&mut ThroughputMeasurement>,
        now: Timestamp,
    ) {
        let Some(measurement) = measurement else {
            return;
        };
        if measurement.finished {
            return;
        }
        measurement.finished = true;
        if measurement.completed_at.is_zero() {
            measurement.completed_at = now;
        }
        self.active_proxy_requests = self.active_proxy_requests.saturating_sub(1);
        self.record_throughput_measurement(measurement.clone());
    }

    pub fn record_throughput_measurement(&mut self, mut measurement: ThroughputMeasurement) {
        let completed_at = if measurement.completed_at.is_zero() {
            Timestamp::now()
        } else {
            measurement.completed_at
        };
        if measurement.started_at.is_zero() || completed_at.before(&measurement.started_at) {
            measurement.started_at = completed_at;
        }
        self.prune_throughput_buckets(completed_at);
        let bucket_at = completed_at.truncate(minutes(THROUGHPUT_BUCKET_INTERVAL_MINUTES));
        let index = match self.throughput_buckets.iter().rposition(|bucket| {
            bucket.bucket_at == bucket_at && bucket.account_id == measurement.account_id
        }) {
            Some(index) => index,
            None => {
                self.throughput_buckets.push(ThroughputBucket {
                    bucket_at,
                    account_id: measurement.account_id.clone(),
                    ..ThroughputBucket::default()
                });
                self.throughput_buckets.len() - 1
            }
        };
        let bucket = &mut self.throughput_buckets[index];
        bucket.request_count += 1;
        if measurement.cancelled {
            bucket.cancelled_count += 1;
        } else if measurement.success {
            bucket.success_count += 1;
        } else {
            bucket.failure_count += 1;
        }
        if measurement.streaming {
            bucket.streaming_request_count += 1;
        }
        if measurement.usage.present {
            bucket.usage_observed_request_count += 1;
            bucket.input_tokens += measurement.usage.input_tokens;
            bucket.cached_tokens += measurement.usage.cached_tokens;
        }
        if measurement.usage.output_present {
            bucket.output_observed_request_count += 1;
            bucket.output_tokens += measurement.usage.output_tokens;
        }
        let duration = elapsed_millis(measurement.started_at, completed_at);
        bucket.total_duration_millis += duration;
        bucket.duration_histogram =
            add_histogram_observation(std::mem::take(&mut bucket.duration_histogram), duration);
        if self.throughput_buckets.len() > THROUGHPUT_BUCKET_LIMIT {
            let extra = self.throughput_buckets.len() - THROUGHPUT_BUCKET_LIMIT;
            self.throughput_buckets
                .sort_by_key(|bucket| bucket.bucket_at);
            self.throughput_buckets.drain(..extra);
        }
    }

    pub fn prune_throughput_buckets(&mut self, now: Timestamp) -> bool {
        if self.throughput_buckets.is_empty() {
            return false;
        }
        let cutoff = now.add(-super::time::hours(THROUGHPUT_BUCKET_TTL_HOURS));
        let before = self.throughput_buckets.len();
        let interval = minutes(THROUGHPUT_BUCKET_INTERVAL_MINUTES);
        self.throughput_buckets
            .retain(|bucket| bucket.bucket_at.add(interval).after(&cutoff));
        let mut changed = self.throughput_buckets.len() != before;
        if self.throughput_buckets.len() > THROUGHPUT_BUCKET_LIMIT {
            self.throughput_buckets
                .sort_by_key(|bucket| bucket.bucket_at);
            let extra = self.throughput_buckets.len() - THROUGHPUT_BUCKET_LIMIT;
            self.throughput_buckets.drain(..extra);
            changed = true;
        }
        changed
    }

    /// Moves the lineage binding only for a deliberate move: the request
    /// observed the current binding (route.lineage_account_id, set on its last
    /// selection attempt) and still succeeded elsewhere because that account
    /// was unusable for it. A success on another account by a request that
    /// observed an older binding is stale (it was in flight while a sibling
    /// moved the lineage) and must not move the lineage back.
    pub fn record_lineage_success(
        &mut self,
        route: &RoutingDecision,
        account_id: &str,
        now: Timestamp,
    ) {
        let ttl = self.sticky_ttl();
        let root = route.identity.lineage_root_id.clone();
        let existing = self
            .state
            .lineage_bindings()
            .get(&root)
            .filter(|current| !current.account_id.is_empty() && current.expires_at.after(&now))
            .cloned();
        let mut current = match existing {
            None => {
                let mut fresh = LineageBinding {
                    lineage_root_id: root.clone(),
                    account_id: account_id.to_string(),
                    created_at: now,
                    moved_at: now,
                    ..LineageBinding::default()
                };
                if !route.lineage_account_id.is_empty() && route.lineage_account_id != account_id {
                    fresh.failover_from.clone_from(&route.lineage_account_id);
                }
                fresh
            }
            Some(current) if current.account_id == account_id => current,
            Some(mut current) if route.lineage_account_id == current.account_id => {
                current.failover_from =
                    std::mem::replace(&mut current.account_id, account_id.to_string());
                current.moved_at = now;
                current
            }
            Some(mut current) => {
                // Stale success: keep the lineage where it is, but the lineage
                // is clearly still active, so keep it alive.
                current.expires_at = now.add(ttl);
                self.state.lineage_bindings_mut().insert(root, current);
                return;
            }
        };
        current.last_success_at = now;
        current.expires_at = now.add(ttl);
        self.state.lineage_bindings_mut().insert(root, current);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_prompt_cache_result(
        &mut self,
        account_id: &str,
        model: &str,
        identity: &RequestIdentity,
        usage: &PromptCacheUsage,
        parent_affinity_hit: bool,
        parent_affinity_fallback: bool,
        lineage_failover: bool,
        routing_outcome: &str,
        now: Timestamp,
    ) {
        let kind = agent_kind(identity);
        let key = format!("{account_id}:{model}:{kind}");
        let stat = self.state.prompt_cache_mut().entry(key).or_default();
        stat.account_id = account_id.to_string();
        stat.model_id = model.to_string();
        stat.agent_kind = kind.to_string();
        stat.request_count += 1;
        if usage.present {
            stat.usage_observed_request_count += 1;
            stat.input_tokens += usage.input_tokens;
            stat.cached_tokens += usage.cached_tokens;
            if usage.cached_tokens > 0 {
                stat.cache_hit_request_count += 1;
            }
            if usage.input_tokens >= PROMPT_CACHE_MIN_TOKENS {
                stat.cache_eligible_request_count += 1;
            }
            if let Some(written) = usage.cache_write_tokens {
                stat.cache_write_tokens += written;
                stat.cache_write_input_tokens += usage.input_tokens;
                stat.cache_write_observed_request_count += 1;
            }
            if usage.input_tokens >= PROMPT_CACHE_MIN_TOKENS && usage.cached_tokens == 0 {
                stat.cold_request_count += 1;
            }
        }
        if parent_affinity_hit {
            stat.parent_affinity_hit_count += 1;
        }
        if parent_affinity_fallback {
            stat.parent_affinity_fallback_count += 1;
        }
        if lineage_failover {
            stat.lineage_failover_count += 1;
        }
        if routing_outcome.ends_with("_failover") {
            stat.routing_failover_count += 1;
        }
        stat.updated_at = now;
    }

    /// Counts an unclassified terminal failure on the same prompt-cache stat
    /// the request was just recorded on, so the dashboard's resettable window
    /// carries it with the other observed counters.
    pub fn note_unrecognized_failure(
        &mut self,
        account_id: &str,
        model: &str,
        identity: &RequestIdentity,
    ) {
        let key = format!("{account_id}:{model}:{}", agent_kind(identity));
        self.state
            .prompt_cache_mut()
            .entry(key)
            .or_default()
            .unrecognized_failure_count += 1;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn routing_outcome(
        &self,
        route: &RoutingDecision,
        model: &str,
        account_id: &str,
        prior: &StickySession,
        info: &ProxyResponseInfo,
        parent_affinity_hit: bool,
        parent_affinity_fallback: bool,
        now: Timestamp,
    ) -> (String, String) {
        let normalized = normalized_routing_outcome(&info.failover_outcome);
        if !normalized.is_empty() {
            return (
                normalized.to_string(),
                info.failover_from_account_id.clone(),
            );
        }
        if !prior.account_id.is_empty() && prior.account_id != account_id {
            let outcome = self.persisted_failover_outcome(&prior.account_id, model, now);
            if !outcome.is_empty() {
                return (outcome.to_string(), prior.account_id.clone());
            }
        }
        if parent_affinity_hit {
            return ("parent_affinity".to_string(), String::new());
        }
        if parent_affinity_fallback {
            return ("parent_affinity_fallback".to_string(), String::new());
        }
        if !prior.account_id.is_empty() && prior.account_id == account_id {
            return ("sticky_reuse".to_string(), String::new());
        }
        if route.lineage_affinity
            && !prior.account_id.is_empty()
            && route.lineage_account_id == account_id
        {
            // A bound thread moved without an upstream retry because its
            // lineage (main agent or a sibling subagent) now lives on another
            // account.
            return ("lineage_follow".to_string(), prior.account_id.clone());
        }
        ("new_route_assignment".to_string(), String::new())
    }

    pub fn persisted_failover_outcome(
        &self,
        account_id: &str,
        model: &str,
        now: Timestamp,
    ) -> &'static str {
        let Some(item) = self.account(account_id) else {
            return "auth_failover";
        };
        let snapshot = self.quota_snapshot(account_id);
        if quota_error_blocks_routing(snapshot.quota_error.as_ref()) {
            return "auth_failover";
        }
        if let Some(quota) = &snapshot.quota
            && (quota::quota_explicitly_blocks_routing(
                quota,
                model,
                now,
                self.tuning.quota_floor_percent,
            ) || self.quota_protection_status(item, &snapshot, now).blocked)
        {
            return "quota_failover";
        }
        if manual_quota_available(item) == Some(false) {
            return "quota_failover";
        }
        for cooldown in self.state.cooldowns().get(account_id).into_iter().flatten() {
            if cooldown.model_id != model || !cooldown.next_retry_at.after(&now) {
                continue;
            }
            match sanitized_error_code(&cooldown.reason).as_str() {
                "rate_limited" => return "rate_limit_failover",
                "account_auth_failed"
                | "invalid_token"
                | "token_invalidated"
                | "token_revoked"
                | "unauthorized"
                | "forbidden" => return "auth_failover",
                "upstream_transport_error" => return "transport_failover",
                "upstream_5xx" => return "repeated_5xx_failover",
                _ => {}
            }
        }
        ""
    }

    /// Only bounded operational metadata is recorded. Raw thread, sticky,
    /// prompt-cache, response and request identifiers are hashed before the
    /// event is persisted; prompt bodies, tool arguments, credentials, emails
    /// and upstream account identities must never be added here.
    #[allow(clippy::too_many_arguments)]
    pub fn append_routing_cache_event(
        &mut self,
        route: &RoutingDecision,
        model: &str,
        account_id: &str,
        failover_from_account_id: &str,
        routing_outcome: &str,
        info: &ProxyResponseInfo,
        now: Timestamp,
    ) {
        let usage = &info.usage;
        let mut parent_affinity = "not_attempted";
        if route.parent_affinity_attempted {
            parent_affinity =
                if !route.lineage_account_id.is_empty() && route.lineage_account_id == account_id {
                    "hit"
                } else {
                    "fallback"
                };
        }
        #[allow(clippy::cast_precision_loss)]
        let read_rate = (usage.present && usage.input_tokens > 0)
            .then(|| usage.cached_tokens as f64 / usage.input_tokens as f64);
        #[allow(clippy::cast_precision_loss)]
        let write_rate = usage
            .cache_write_tokens
            .filter(|_| usage.input_tokens > 0)
            .map(|written| written as f64 / usage.input_tokens as f64);
        let reuse_balance = usage
            .cache_write_tokens
            .map(|written| token_balance(usage.cached_tokens, written));
        let event = RoutingCacheEvent {
            timestamp: now,
            request_id_hash: operational_identifier_hash("request", &info.request_id),
            response_id_hash: operational_identifier_hash("response", &info.response_id),
            model_id: model.to_string(),
            account_id: account_id.to_string(),
            agent_kind: agent_kind(&route.identity).to_string(),
            thread_id_hash: operational_identifier_hash("thread", &route.identity.thread_id),
            lineage_root_id_hash: operational_identifier_hash(
                "lineage",
                &route.identity.lineage_root_id,
            ),
            sticky_key_hash: operational_identifier_hash("sticky", &route.sticky_key),
            prompt_cache_key_hash: operational_identifier_hash(
                "prompt-cache",
                &route.upstream_prompt_cache_key,
            ),
            routing_outcome: routing_outcome.to_string(),
            routing_source: routing_event_source(&route.source).to_string(),
            terminal_event: info.terminal_event.clone(),
            terminal_failure_class: info.terminal_failure_class.clone(),
            terminal_error_code: sanitized_error_code(&info.terminal_error_code),
            precommit_committed: info.precommit_committed,
            precommit_close_reason: info.precommit_close_reason.clone(),
            precommit_blocks: info.precommit_blocks,
            precommit_bytes: info.precommit_bytes,
            precommit_hold_ms: info.precommit_hold_ms,
            parent_affinity: parent_affinity.to_string(),
            failover_from_account_id: failover_from_account_id.to_string(),
            usage_observed: usage.present,
            input_tokens: usage.input_tokens,
            cached_tokens: usage.cached_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            uncached_input_tokens: sub_sat(usage.input_tokens, usage.cached_tokens),
            cache_read_rate: read_rate,
            cache_write_rate: write_rate,
            cache_reuse_balance: reuse_balance,
            cache_hit: usage.present && usage.cached_tokens > 0,
            cold_cache_eligible: usage.present
                && usage.input_tokens >= PROMPT_CACHE_MIN_TOKENS
                && usage.cached_tokens == 0,
            client_originator: route.client.originator.clone(),
            client_user_agent: route.client.user_agent.clone(),
            client_request_kind: route.client.request_kind.clone(),
            client_turn_state: route.client.turn_state.clone(),
            client_entry: route.client.entry.clone(),
            ..RoutingCacheEvent::default()
        };
        self.prune_routing_cache_events(now);
        let events = self.state.routing_cache_events_mut();
        events.push(event);
        if events.len() > ROUTING_CACHE_EVENT_LIMIT {
            let extra = events.len() - ROUTING_CACHE_EVENT_LIMIT;
            events.drain(..extra);
        }
    }

    /// The dashboard's view of recent routing events, newest first. Browser
    /// APIs need a compact diagnostic view, not the full persisted rolling
    /// buffer: the server-side limit stays even though the UI also slices
    /// defensively, otherwise a client could pull all 500 request
    /// correlations on every refresh. Account ids leave only as labels and
    /// hashed refs.
    pub fn routing_cache_event_views(&self, now: Timestamp) -> Vec<serde_json::Value> {
        let cutoff = now.add(-super::time::hours(
            super::routing::ROUTING_CACHE_EVENT_TTL_HOURS,
        ));
        let label = |account_id: &str, fallback: &str| {
            self.account_index(account_id)
                .map(|index| {
                    super::plan::public_credential_display_name(
                        &self.config.accounts()[index],
                        Some(index),
                    )
                })
                .unwrap_or_else(|| fallback.to_string())
        };
        let mut result = Vec::new();
        for event in self.state.routing_cache_events().iter().rev() {
            if event.timestamp.before(&cutoff) {
                continue;
            }
            result.push(serde_json::json!({
                "timestamp": event.timestamp, "requestIdHash": event.request_id_hash, "responseIdHash": event.response_id_hash,
                "modelId": event.model_id, "accountLabel": label(&event.account_id, "Credential"),
                "accountRef": operational_identifier_hash("account", &event.account_id),
                "agentKind": event.agent_kind, "threadIdHash": event.thread_id_hash, "lineageRootIdHash": event.lineage_root_id_hash,
                "stickyKeyHash": event.sticky_key_hash, "promptCacheKeyHash": event.prompt_cache_key_hash,
                "routingOutcome": event.routing_outcome, "routingSource": event.routing_source, "parentAffinity": event.parent_affinity,
                "terminalEvent": event.terminal_event, "terminalFailureClass": event.terminal_failure_class, "terminalErrorCode": event.terminal_error_code,
                "precommitCommitted": event.precommit_committed, "precommitCloseReason": event.precommit_close_reason,
                "precommitBlocks": event.precommit_blocks, "precommitBytes": event.precommit_bytes,
                "failoverFromAccountLabel": label(&event.failover_from_account_id, ""),
                "failoverFromAccountRef": operational_identifier_hash("account", &event.failover_from_account_id),
                "usageObserved": event.usage_observed, "inputTokens": event.input_tokens, "cachedTokens": event.cached_tokens,
                "cacheWriteTokens": event.cache_write_tokens, "uncachedInputTokens": event.uncached_input_tokens,
                "cacheReadRate": event.cache_read_rate, "cacheWriteRate": event.cache_write_rate, "cacheReuseBalance": event.cache_reuse_balance,
                "cacheHit": event.cache_hit, "coldCacheEligible": event.cold_cache_eligible,
                "clientOriginator": event.client_originator, "clientUserAgent": event.client_user_agent,
                "clientRequestKind": event.client_request_kind, "clientTurnState": event.client_turn_state, "clientEntry": event.client_entry,
            }));
            if result.len() == ROUTING_CACHE_EVENT_VIEW_LIMIT {
                break;
            }
        }
        result
    }

    /// Clears everything recorded about an account's upstream identity: on
    /// deletion, or a verified identity change.
    pub fn clear_account_identity_scoped_state(&mut self, account_id: &str) {
        self.state.health_mut().remove(account_id);
        self.state.cooldowns_mut().remove(account_id);
        self.state.quotas_mut().remove(account_id);
        self.state
            .prompt_cache_mut()
            .retain(|_, stat| stat.account_id != account_id);
        if self.state.prompt_cache_baseline.is_some() {
            self.state
                .prompt_cache_baseline_mut()
                .retain(|_, stat| stat.account_id != account_id);
        }
        if self.state.prompt_cache_reset_at_by_account.is_some() {
            self.state
                .prompt_cache_reset_at_by_account_mut()
                .remove(account_id);
        }
        self.clear_sticky_for_account(account_id);
        // Throughput is identity-scoped history. Pool removal/disable only
        // clears routes and must not erase the operator's rolling traffic
        // view, while account deletion or a verified identity change must
        // remove attribution.
        self.throughput_buckets
            .retain(|bucket| bucket.account_id != account_id);
    }

    pub fn clear_account_runtime_state(&mut self, account_id: &str) {
        if let Some(health) = self
            .state
            .health
            .as_mut()
            .and_then(|health| health.get_mut(account_id))
        {
            health.consecutive_failure = 0;
            health.last_failure_reason = String::new();
        } else if self.state.health.is_some() {
            self.state
                .health_mut()
                .insert(account_id.to_string(), AccountHealth::default());
        }
        if self.state.cooldowns.is_some() {
            self.state.cooldowns_mut().remove(account_id);
        }
        if self.state.quotas.is_some() {
            let mut snapshot = self.quota_snapshot(account_id);
            if !snapshot.account_id.is_empty()
                || snapshot.quota.is_some()
                || snapshot.quota_error.is_some()
                || !snapshot.usage_updated_at.is_zero()
            {
                snapshot.account_id = account_id.to_string();
                snapshot.quota_error = None;
                self.state
                    .quotas_mut()
                    .insert(account_id.to_string(), snapshot);
            }
        }
    }
}

/// Waits out one capacity round. Restores every identity that refused for
/// capacity, not only the one tried last: after a wait the best identity is
/// often one that refused earlier, and leaving the others excluded would pin
/// the whole remaining budget on a single account. Returns how many it
/// restored, so the caller can give back the attempts that round consumed,
/// and whether the wait finished (false when the caller went away first).
///
/// Retrying the same account is not another account attempt, so no caller
/// may charge this to the attempt budget: a single-account pool has an attempt
/// limit of one and could never retry at all.
pub async fn spend_capacity_retry_round(
    app: &App,
    model: &str,
    delay: std::time::Duration,
    excluded: &mut HashMap<String, bool>,
    capacity_excluded: &mut HashMap<String, bool>,
    cancelled: impl std::future::Future<Output = ()>,
) -> (usize, bool) {
    let mut restored = 0;
    for id in capacity_excluded.keys() {
        if excluded.get(id).copied().unwrap_or(false) {
            excluded.remove(id);
            restored += 1;
        }
    }
    app.clear_capacity_cooldowns(capacity_excluded, model);
    capacity_excluded.clear();
    let waited = tokio::select! {
        () = tokio::time::sleep(delay) => true,
        () = cancelled => false,
    };
    (restored, waited)
}
