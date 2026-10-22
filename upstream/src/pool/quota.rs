//! Reading Codex's usage answer into the pool's quota snapshot, and the quota
//! rules routing and the dashboard share.
//!
//! A port of quotaFromUsage and its neighbours in cmd/codex-pool/main.go. The
//! wire types decode the way Go's do: a wrong type in a typed field fails the
//! whole read (the caller records decode_failed), a null leaves the field
//! empty, and the few fields Go reads leniently (`used_percent`,
//! `rate_limit_reached_type`) are read leniently here.

use super::plan::clean_metadata_text;
use super::plan::clean_metadata_token;
use super::state::AccountQuota;
use super::state::QuotaCredits;
use super::state::QuotaLimit;
use super::state::QuotaResetCredits;
use super::state::QuotaSnapshot;
use super::state::QuotaSpendControl;
use super::state::QuotaWindow;
use super::time::Timestamp;
use super::time::minutes;
use super::time::seconds;
use serde::Deserialize;
use serde::Deserializer;
use serde_json::Value;

/// Models upstream no longer serves. They must never be advertised, and a
/// per-model meter naming one is dropped (Go retiredCodexModelSlugs).
pub const RETIRED_CODEX_MODEL_SLUGS: &[&str] = &[
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex-spark",
    "gpt-5.2-codex",
];

/// The reported duration of the Codex five-hour window.
pub const FIVE_HOUR_WINDOW_MINUTES: i64 = 300;
/// The reported duration of the Codex weekly window, the budget the tier
/// multipliers describe.
pub const WEEKLY_WINDOW_MINUTES: i64 = 10080;

/// Quota telemetry is refreshed every four to six minutes for an account in
/// use and every 20 to 30 for an idle one (quota_schedule.rs), so fresh covers
/// the idle cadence plus a grace interval; read sooner it would mark every
/// idle account stale. Freshness itself is never a routing gate: SPEC section
/// 6.4 deliberately keeps telemetry failures fail-open.
pub const QUOTA_TELEMETRY_FRESHNESS_MINUTES: i64 = 35;

/// Reset-credit expiry is descriptive metadata from a separate endpoint, not
/// routing evidence. Refresh it far less often than quota windows so the
/// dashboard can show the date without doubling the quota polls.
pub const RESET_CREDIT_DETAILS_REFRESH_HOURS: i64 = 6;

// ---- Codex's usage answer (GET /wham/usage) ----

#[derive(Debug, Default, Deserialize)]
pub struct UsageResponse {
    #[serde(default)]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub subscription_plan: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitInfo>,
    #[serde(default)]
    pub code_review_rate_limit: Option<RateLimitInfo>,
    #[serde(default)]
    pub credits: Option<CreditInfo>,
    #[serde(default)]
    pub spend_control: Option<SpendControlInfo>,
    #[serde(default)]
    pub additional_rate_limits: Option<Vec<AdditionalRateLimit>>,
    #[serde(default, deserialize_with = "reached_type")]
    pub rate_limit_reached_type: String,
    #[serde(default, rename = "rate_limit_reset_credits")]
    pub reset_credits: Option<ResetCreditInfo>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RateLimitInfo {
    #[serde(default)]
    pub allowed: Option<bool>,
    #[serde(default)]
    pub limit_reached: Option<bool>,
    #[serde(default)]
    pub primary_window: Option<WindowInfo>,
    #[serde(default)]
    pub secondary_window: Option<WindowInfo>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WindowInfo {
    /// Go optionalNumber: accepts malformed future wire values without failing
    /// the whole quota refresh. Only an explicitly reported JSON number is
    /// percentage evidence; null, missing, strings and objects stay Not
    /// reported.
    #[serde(default, deserialize_with = "optional_number")]
    pub used_percent: Option<f64>,
    #[serde(default)]
    pub limit_window_seconds: Option<i64>,
    #[serde(default)]
    pub reset_after_seconds: Option<i64>,
    #[serde(default)]
    pub reset_at: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CreditInfo {
    #[serde(default)]
    pub has_credits: Option<bool>,
    #[serde(default)]
    pub unlimited: Option<bool>,
    #[serde(default)]
    pub balance: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SpendControlInfo {
    #[serde(default)]
    pub reached: Option<bool>,
    #[serde(default)]
    pub individual_limit: Option<SpendLimitInfo>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SpendLimitInfo {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub limit: Option<String>,
    #[serde(default)]
    pub used: Option<String>,
    #[serde(default)]
    pub remaining: Option<String>,
    #[serde(default)]
    pub remaining_percent: Option<i64>,
    #[serde(default)]
    pub reset_after_seconds: Option<i64>,
    #[serde(default)]
    pub reset_at: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AdditionalRateLimit {
    #[serde(default)]
    pub limit_name: Option<String>,
    #[serde(default)]
    pub metered_feature: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitInfo>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResetCreditInfo {
    #[serde(default)]
    pub available_count: Option<i64>,
}

/// GET /wham/rate-limit-reset-credits, read for availability and expiry only:
/// the records carry identifiers and prose the dashboard does not need, so no
/// upstream credit identifier enters runtime state or a public response.
#[derive(Debug, Default, Deserialize)]
pub struct ResetCreditDetailsResponse {
    #[serde(default)]
    pub available_count: Option<i64>,
    #[serde(default)]
    pub credits: Option<Vec<ResetCreditDetail>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResetCreditDetail {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

fn optional_number<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(value.as_f64())
}

/// Go codexReachedType: an object's `type`, or a bare string, sanitized; any
/// other shape is no reached type rather than a failed read.
fn reached_type<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let text = match &value {
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some(kind) if !kind.is_empty() => kind,
            _ => "",
        },
        Value::String(text) => text.as_str(),
        _ => "",
    };
    Ok(clean_metadata_token(text))
}

// ---- Building the snapshot ----

pub fn quota_from_usage(usage: &UsageResponse, now: Timestamp) -> AccountQuota {
    let base = quota_limit_from_rate_limit("codex", "", usage.rate_limit.as_ref(), now);
    let mut quota = AccountQuota {
        limit_id: base.limit_id,
        limit_name: base.limit_name,
        primary: base.primary,
        secondary: base.secondary,
        windows: base.windows,
        allowed: base.allowed,
        limit_reached: base.limit_reached,
        exhausted: base.exhausted,
        exhaustion_reason: base.exhaustion_reason,
        provenance: "chatgpt_wham_usage".to_string(),
        observed_at: now,
        ..AccountQuota::default()
    };
    if let Some(credits) = &usage.credits {
        let balance = credits
            .balance
            .as_deref()
            .map(|balance| clean_metadata_text(balance, 80))
            .filter(|balance| !balance.is_empty());
        quota.credits = Some(QuotaCredits {
            has_credits: credits.has_credits.unwrap_or(false),
            unlimited: credits.unlimited.unwrap_or(false),
            balance,
            ..QuotaCredits::default()
        });
    }
    if let Some(spend) = &usage.spend_control {
        let mut limit = QuotaSpendControl {
            reached: spend.reached.unwrap_or(false),
            ..QuotaSpendControl::default()
        };
        if let Some(details) = &spend.individual_limit {
            let text = |value: &Option<String>| {
                clean_metadata_text(value.as_deref().unwrap_or_default(), 80)
            };
            limit.source = clean_metadata_token(details.source.as_deref().unwrap_or_default());
            limit.limit = text(&details.limit);
            limit.used = text(&details.used);
            limit.remaining = text(&details.remaining);
            limit.remaining_percent = details.remaining_percent.map(|value| value.clamp(0, 100));
            limit.reset_at =
                normalized_reset_at(details.reset_at, details.reset_after_seconds, now);
        }
        if limit.reached {
            quota.exhausted = true;
            quota.exhaustion_reason = "spend_control_reached".to_string();
        }
        quota.individual_limit = Some(limit);
    }
    quota.rate_limit_reached_type = clean_metadata_token(&usage.rate_limit_reached_type);
    if !quota.rate_limit_reached_type.is_empty() {
        quota.exhausted = true;
        quota.exhaustion_reason = quota.rate_limit_reached_type.clone();
    }
    if let Some(additional) = &usage.additional_rate_limits {
        for limit in additional {
            quota.additional_limits.push(quota_limit_from_rate_limit(
                &clean_metadata_token(limit.metered_feature.as_deref().unwrap_or_default()),
                &clean_metadata_text(limit.limit_name.as_deref().unwrap_or_default(), 120),
                limit.rate_limit.as_ref(),
                now,
            ));
        }
    }
    if let Some(code_review) = &usage.code_review_rate_limit {
        quota.additional_limits.push(quota_limit_from_rate_limit(
            "code_review",
            "Code review",
            Some(code_review),
            now,
        ));
    }
    quota.additional_limits =
        drop_retired_model_meters(std::mem::take(&mut quota.additional_limits));
    if let Some(reset) = &usage.reset_credits {
        quota.reset_credits = Some(QuotaResetCredits {
            available_count: reset.available_count,
            ..QuotaResetCredits::default()
        });
    }
    quota.hourly = quota_window_by_duration(&quota, FIVE_HOUR_WINDOW_MINUTES);
    quota.weekly = quota_window_by_duration(&quota, WEEKLY_WINDOW_MINUTES);
    quota
}

/// Codex rolling snapshots may omit optional metadata while still providing
/// fresh windows. Missing/null credits, spend control, reset credits and
/// additional limits are not clear signals, so the last verified value stays.
pub fn merge_sparse_quota(
    prior: Option<&AccountQuota>,
    mut current: AccountQuota,
    usage: &UsageResponse,
) -> AccountQuota {
    let Some(prior) = prior else {
        return current;
    };
    if usage.credits.is_none() {
        current.credits = prior.credits.clone();
    }
    if usage.spend_control.is_none() {
        current.individual_limit = prior.individual_limit.clone();
    }
    if usage.reset_credits.is_none() {
        current.reset_credits = prior.reset_credits.clone();
    }
    if usage.additional_rate_limits.is_none() && usage.code_review_rate_limit.is_none() {
        // The prior snapshot may predate a retirement, so it is filtered too;
        // otherwise a sparse refresh would carry a retired meter forward
        // forever.
        current.additional_limits = drop_retired_model_meters(prior.additional_limits.clone());
    }
    current
}

pub fn quota_limit_from_rate_limit(
    limit_id: &str,
    limit_name: &str,
    rate_limit: Option<&RateLimitInfo>,
    now: Timestamp,
) -> QuotaLimit {
    let mut result = QuotaLimit {
        limit_id: limit_id.to_string(),
        limit_name: limit_name.to_string(),
        ..QuotaLimit::default()
    };
    let Some(rate_limit) = rate_limit else {
        return result;
    };
    result.allowed = rate_limit.allowed;
    result.limit_reached = rate_limit.limit_reached;
    result.primary = normalize_quota_window("primary", rate_limit.primary_window.as_ref(), now);
    result.secondary =
        normalize_quota_window("secondary", rate_limit.secondary_window.as_ref(), now);
    for window in [&result.primary, &result.secondary] {
        if window.observed || window.present {
            result.windows.push(window.clone());
        }
    }
    if rate_limit.limit_reached == Some(true) {
        result.exhausted = true;
        result.exhaustion_reason = "rate_limit_reached".to_string();
    } else if rate_limit.allowed == Some(false) {
        result.exhausted = true;
        result.exhaustion_reason = "not_allowed".to_string();
    }
    result
}

pub fn normalize_quota_window(
    role: &str,
    window: Option<&WindowInfo>,
    now: Timestamp,
) -> QuotaWindow {
    let Some(window) = window else {
        return QuotaWindow {
            role: role.to_string(),
            ..QuotaWindow::default()
        };
    };
    let mut result = QuotaWindow {
        role: role.to_string(),
        observed: true,
        ..QuotaWindow::default()
    };
    if let Some(seconds) = window.limit_window_seconds.filter(|seconds| *seconds > 0) {
        result.window_minutes = Some((seconds + 59) / 60);
    }
    result.label = quota_window_label(result.window_minutes);
    result.reset_at = normalized_reset_at(window.reset_at, window.reset_after_seconds, now);
    if let Some(used) = window.used_percent {
        let used = used.clamp(0.0, 100.0);
        let remaining = 100.0 - used;
        result.used_percent = Some(used);
        result.remaining_percent = Some(remaining);
        #[allow(clippy::cast_possible_truncation)]
        {
            result.percentage = remaining as i64;
        }
        result.present = true;
    }
    result
}

pub fn normalized_reset_at(
    reset_at: Option<i64>,
    reset_after_seconds: Option<i64>,
    now: Timestamp,
) -> Option<i64> {
    if let Some(reset_at) = reset_at.filter(|value| *value > 0) {
        return Some(reset_at);
    }
    reset_after_seconds
        .filter(|value| *value >= 0)
        .map(|after| now.add(seconds(after)).unix())
}

pub fn quota_window_label(window_minutes: Option<i64>) -> String {
    let Some(minutes) = window_minutes.filter(|minutes| *minutes > 0) else {
        return "Window".to_string();
    };
    match minutes {
        300 => "5h".to_string(),
        10080 => "Week".to_string(),
        _ if minutes < 60 => format!("{minutes}m"),
        _ if minutes % 1440 == 0 => format!("{}d", minutes / 1440),
        _ if minutes % 60 == 0 => format!("{}h", minutes / 60),
        _ => format!("{}h {}m", minutes / 60, minutes % 60),
    }
}

/// Whether a reported window must be treated as consumed. Every routing and
/// display decision that used to compare against zero goes through here, so
/// the floor cannot apply in one place and not another: a window that blocks
/// routing must also read as exhausted on the dashboard and count as spent in
/// the capacity bars.
pub fn quota_window_spent(window: &QuotaWindow, floor_percent: f64) -> bool {
    quota_window_remaining(window) <= floor_percent
}

pub fn quota_window_remaining(window: &QuotaWindow) -> f64 {
    match window.remaining_percent {
        Some(remaining) => remaining.clamp(0.0, 100.0),
        #[allow(clippy::cast_precision_loss)]
        None => window.percentage.clamp(0, 100) as f64,
    }
}

pub fn quota_window_by_duration(quota: &AccountQuota, window_minutes: i64) -> QuotaWindow {
    quota_reported_windows(quota)
        .into_iter()
        .find(|window| window.window_minutes == Some(window_minutes))
        .unwrap_or_else(|| QuotaWindow {
            label: quota_window_label(Some(window_minutes)),
            ..QuotaWindow::default()
        })
}

/// Derives a Business seat tier from the reported quota windows. No endpoint
/// this pool calls carries a Standard/Premium field, and the pinned Codex
/// client model has none either, but OpenAI documents that a Premium seat
/// carries no five-hour usage limit. A Business account that reports quota
/// windows and none of them is the five-hour window is therefore Premium; one
/// that still reports it is Standard.
///
/// The result is always marked inferred. It is display evidence only: it must
/// never grant models, capacity, or routing preference, and an authoritative
/// upstream seat value always wins. Requiring at least one reported window
/// keeps a sparse or failed refresh, which reports nothing at all, from
/// silently promoting a Standard seat to Premium.
pub fn business_seat_inference_from_quota(
    plan_family: &str,
    quota: &AccountQuota,
) -> Option<&'static str> {
    if plan_family != "business" {
        return None;
    }
    let mut reported = false;
    for window in quota_reported_windows(quota) {
        if !window.present {
            continue;
        }
        reported = true;
        if window.window_minutes == Some(FIVE_HOUR_WINDOW_MINUTES) {
            return Some("standard");
        }
    }
    reported.then_some("premium")
}

/// Refreshes an inferred seat without ever overwriting an authoritative one.
/// When this refresh reports no window at all the previous inference is kept
/// rather than cleared, so a single sparse refresh does not make the seat flap
/// between values on the dashboard.
pub fn apply_business_seat_inference(snapshot: &mut QuotaSnapshot, quota: &AccountQuota) {
    if !snapshot.seat_type.is_empty() && !snapshot.seat_type_inferred {
        return;
    }
    let Some(seat) = business_seat_inference_from_quota(&snapshot.plan_family, quota) else {
        return;
    };
    snapshot.seat_type = seat.to_string();
    snapshot.seat_type_inferred = true;
    snapshot.quota_policy = with_quota_policy(
        &snapshot.quota_policy,
        "no_five_hour_cap",
        seat == "premium",
    );
}

/// Adds or removes a single policy token while preserving any other policy
/// already recorded, so a future upstream-provided policy is not dropped by
/// this pool's own derived one.
pub fn with_quota_policy(policies: &[String], policy: &str, present: bool) -> Vec<String> {
    let mut result: Vec<String> = policies
        .iter()
        .filter(|existing| *existing != policy)
        .cloned()
        .collect();
    if present {
        result.push(policy.to_string());
    }
    result
}

pub fn quota_reported_windows(quota: &AccountQuota) -> Vec<QuotaWindow> {
    if !quota.windows.is_empty() {
        return quota.windows.clone();
    }
    let reported = |window: &&QuotaWindow| {
        window.observed || window.present || window.window_minutes.is_some()
    };
    let result: Vec<QuotaWindow> = [&quota.primary, &quota.secondary]
        .into_iter()
        .filter(reported)
        .cloned()
        .collect();
    if !result.is_empty() {
        return result;
    }
    // Persisted snapshots from older releases may only contain these aliases.
    [&quota.hourly, &quota.weekly]
        .into_iter()
        .filter(reported)
        .cloned()
        .collect()
}

pub fn remaining_quota_hint(quota: &AccountQuota) -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    let values: Vec<i64> = quota_reported_windows(quota)
        .iter()
        .filter(|window| window.present)
        .map(|window| quota_window_remaining(window) as i64)
        .collect();
    match values.iter().min() {
        Some(least) => (*least).clamp(0, 100),
        None if quota.exhausted => 0,
        None => 100,
    }
}

pub fn quota_window_evidence_active(window: &QuotaWindow, now: Timestamp) -> bool {
    window.reset_at.is_none_or(|reset_at| now.unix() < reset_at)
}

/// Whether a per-model meter describes exactly the requested model. Upstream
/// identifies these meters by an internal codename in the id, such as
/// codex_bengalfox, and puts the model in the display name, so matching the id
/// alone silently ignores every meter upstream actually sends. Both sides are
/// compared as sanitized tokens, and a meter that names anything other than
/// this model stays display metadata.
pub fn additional_limit_names_model(limit: &QuotaLimit, model: &str) -> bool {
    let model = clean_metadata_token(model);
    !model.is_empty()
        && (clean_metadata_token(&limit.limit_id) == model
            || clean_metadata_token(&limit.limit_name) == model)
}

/// Removes per-model meters that name a retired model. This is the one
/// exception to keeping every upstream-reported meter visible: the model
/// behind it can no longer be requested, so the meter is neither an
/// entitlement to route on nor anything an operator can act on. Only a meter
/// naming exactly a retired slug is dropped; feature meters such as code review
/// and meters for live models stay.
pub fn drop_retired_model_meters(limits: Vec<QuotaLimit>) -> Vec<QuotaLimit> {
    limits
        .into_iter()
        .filter(|limit| {
            !RETIRED_CODEX_MODEL_SLUGS
                .iter()
                .any(|model| additional_limit_names_model(limit, model))
        })
        .collect()
}

pub fn quota_explicitly_blocks_routing(
    quota: &AccountQuota,
    model: &str,
    now: Timestamp,
    floor_percent: f64,
) -> bool {
    if quota.exhausted {
        return true;
    }
    let blocks = |window: &QuotaWindow| {
        window.present
            && quota_window_spent(window, floor_percent)
            && quota_window_evidence_active(window, now)
    };
    if quota_reported_windows(quota).iter().any(blocks) {
        return true;
    }
    for additional in &quota.additional_limits {
        // Unknown/model-specific buckets are display metadata unless the
        // meter names the requested model. Do not turn a new future meter into
        // an account-wide authorization rule.
        if !additional_limit_names_model(additional, model) {
            continue;
        }
        if additional.exhausted {
            return true;
        }
        // A model-specific meter bills the same way, so the floor holds here
        // too: an almost-spent meter would otherwise route one request that
        // charges credits before the meter finally reads zero.
        if additional.windows.iter().any(blocks) {
            return true;
        }
    }
    false
}

pub fn quota_telemetry_state(snapshot: &QuotaSnapshot, now: Timestamp) -> &'static str {
    if snapshot.quota_error.is_some() {
        return "refresh_unavailable";
    }
    let mut observed_at = snapshot.last_successful_refresh_at;
    if observed_at.is_zero() {
        observed_at = snapshot.usage_updated_at;
    }
    if observed_at.is_zero() {
        return "not_reported";
    }
    if now.sub(&observed_at) <= minutes(QUOTA_TELEMETRY_FRESHNESS_MINUTES) {
        "fresh"
    } else {
        "stale"
    }
}

pub fn quota_snapshot_for_display(snapshot: &QuotaSnapshot, now: Timestamp) -> QuotaSnapshot {
    let mut snapshot = snapshot.clone();
    snapshot.freshness = quota_telemetry_state(&snapshot, now).to_string();
    if snapshot.provenance.is_empty()
        && let Some(quota) = &snapshot.quota
    {
        snapshot.provenance = quota.provenance.clone();
    }
    snapshot
}

pub fn nearest_available_reset_credit_expiration(
    details: &ResetCreditDetailsResponse,
    now: Timestamp,
) -> Option<i64> {
    details
        .credits
        .iter()
        .flatten()
        .filter(|credit| {
            credit
                .status
                .as_deref()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("available")
        })
        .filter_map(|credit| {
            Timestamp::parse(credit.expires_at.as_deref().unwrap_or_default().trim()).ok()
        })
        .filter(|expires_at| expires_at.after(&now))
        .map(|expires_at| expires_at.unix())
        .min()
}

/// Turns upstream's verdict on a consume attempt into operator-visible
/// wording, and whether a credit went to a reset. "reset" spends a credit and
/// "already_redeemed" reports that a resent attempt's earlier send did; the
/// others report why nothing happened, and each needs its own wording because
/// the remedy differs.
///
/// Upstream spells these in snake_case: reset, nothing_to_reset, no_credit and
/// already_redeemed. The camelCase spellings are the Codex client's rename of
/// the same enum for its own JSON-RPC surface; they are accepted so a verdict
/// is never reported as unrecognized if this pool ever reads that surface
/// instead. Matching is case-insensitive because this pool sanitizes metadata
/// to lowercase before it gets here.
pub fn reset_credit_outcome_message(outcome: &str) -> (&'static str, bool) {
    match outcome.trim().to_lowercase().as_str() {
        "reset" => (
            "A reset credit was consumed and the eligible rate-limit windows were reset.",
            true,
        ),
        "nothing_to_reset" | "nothingtoreset" => (
            "No rate-limit window is currently eligible for a reset, so no credit was spent.",
            false,
        ),
        "no_credit" | "nocredit" | "nocreditsavailable" | "nocredits" => {
            ("This account has no earned reset credits available.", false)
        }
        // Only a resent attempt meets this: the earlier send went through and
        // its answer was lost. Codex reports it as the reset it is (tui
        // chatwidget/usage.rs treats Reset and AlreadyRedeemed alike).
        "already_redeemed" | "alreadyredeemed" => (
            "The reset already went through on the earlier attempt, so no further credit was spent.",
            true,
        ),
        _ => ("Upstream returned an unrecognized reset outcome.", false),
    }
}

// ---- Upstream error bodies ----

fn nested_str<'a>(payload: &'a serde_json::Map<String, Value>, object: &str, key: &str) -> &'a str {
    payload
        .get(object)
        .and_then(Value::as_object)
        .and_then(|object| object.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn json_object(body: &[u8]) -> Option<serde_json::Map<String, Value>> {
    serde_json::from_slice::<serde_json::Map<String, Value>>(body).ok()
}

/// Whether an upstream 5xx is the local gateway refusing a model it has no
/// provider for. The phrase is matched narrowly and deliberately: a broader
/// test on a 5xx body would swallow genuine upstream failures and stop the
/// pool from failing over when it should.
pub fn gateway_unknown_model(body: &[u8]) -> bool {
    let Some(payload) = json_object(body) else {
        return false;
    };
    let mut message = nested_str(&payload, "error", "message");
    if message.is_empty() {
        message = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
    }
    message
        .to_lowercase()
        .contains("unknown provider for model")
}

pub fn extract_upstream_error_code(body: &[u8]) -> String {
    let Some(payload) = json_object(body) else {
        return String::new();
    };
    for code in [
        nested_str(&payload, "detail", "code"),
        nested_str(&payload, "error", "code"),
        payload
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ] {
        if !code.is_empty() {
            return sanitized_error_code(code);
        }
    }
    String::new()
}

pub fn sanitized_error_code(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() || value.len() > 64 {
        return String::new();
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        value.to_string()
    } else {
        String::new()
    }
}

/// Upstream's own explanation for a rejected request. It is for the log only,
/// never for a response: the text is bounded and stripped of control
/// characters, but it is still upstream prose.
pub fn upstream_error_detail(body: &[u8]) -> String {
    let Some(payload) = json_object(body) else {
        return String::new();
    };
    let detail = payload
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    [
        detail,
        nested_str(&payload, "detail", "message"),
        nested_str(&payload, "error", "message"),
        message,
    ]
    .into_iter()
    .map(|candidate| clean_metadata_text(candidate, 200))
    .find(|text| !text.is_empty())
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::state::QuotaErrorInfo;
    use crate::pool::time::hours;
    use pretty_assertions::assert_eq;

    const FLOOR: f64 = 1.0;

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix)
    }

    fn decode(payload: &str) -> AccountQuota {
        let usage: UsageResponse =
            serde_json::from_str(payload).unwrap_or_else(|error| panic!("{payload}: {error}"));
        quota_from_usage(&usage, at(1_800_000_000))
    }

    fn present(window_minutes: i64) -> QuotaWindow {
        QuotaWindow {
            window_minutes: Some(window_minutes),
            present: true,
            observed: true,
            ..QuotaWindow::default()
        }
    }

    // Go TestBusinessSeatInferenceFromQuotaWindows.
    #[test]
    fn business_seat_inference_from_quota_windows() {
        let quota = |windows: Vec<QuotaWindow>| AccountQuota {
            windows,
            ..AccountQuota::default()
        };
        let cases = [
            (
                "premium has no five hour window",
                "business",
                quota(vec![present(10080)]),
                Some("premium"),
            ),
            (
                "standard still reports it",
                "business",
                quota(vec![present(300), present(10080)]),
                Some("standard"),
            ),
            (
                "no reported window is not evidence",
                "business",
                quota(vec![QuotaWindow {
                    window_minutes: Some(10080),
                    ..QuotaWindow::default()
                }]),
                None,
            ),
            (
                "pro is never a business seat",
                "pro",
                quota(vec![present(10080)]),
                None,
            ),
            (
                "plus is never a business seat",
                "plus",
                quota(vec![present(10080)]),
                None,
            ),
        ];
        for (name, family, quota, want) in cases {
            assert_eq!(
                business_seat_inference_from_quota(family, &quota),
                want,
                "{name}"
            );
        }
    }

    // Go TestBusinessSeatInferenceNeverOverridesAuthoritativeSeat.
    #[test]
    fn business_seat_inference_never_overrides_authoritative_seat() {
        let week_only = AccountQuota {
            windows: vec![present(10080)],
            ..AccountQuota::default()
        };
        let mut snapshot = QuotaSnapshot {
            plan_family: "business".into(),
            seat_type: "standard".into(),
            ..QuotaSnapshot::default()
        };
        apply_business_seat_inference(&mut snapshot, &week_only);
        assert_eq!(
            (snapshot.seat_type.as_str(), snapshot.seat_type_inferred),
            ("standard", false)
        );

        let mut inferred = QuotaSnapshot {
            plan_family: "business".into(),
            seat_type: "standard".into(),
            seat_type_inferred: true,
            ..QuotaSnapshot::default()
        };
        apply_business_seat_inference(&mut inferred, &week_only);
        assert_eq!(
            (inferred.seat_type.as_str(), inferred.seat_type_inferred),
            ("premium", true)
        );
        assert_eq!(inferred.quota_policy, vec!["no_five_hour_cap".to_string()]);

        // Downgrading back to standard drops the policy.
        let five_hour = AccountQuota {
            windows: vec![present(300)],
            ..AccountQuota::default()
        };
        apply_business_seat_inference(&mut inferred, &five_hour);
        assert_eq!(inferred.seat_type, "standard");
        assert!(inferred.quota_policy.is_empty());

        // A refresh that reports nothing keeps the last inference.
        apply_business_seat_inference(&mut inferred, &AccountQuota::default());
        assert_eq!(
            (inferred.seat_type.as_str(), inferred.seat_type_inferred),
            ("standard", true)
        );
    }

    // Go TestAdditionalLimitNamesModel.
    #[test]
    fn additional_limit_names_model_cases() {
        let spark = QuotaLimit {
            limit_id: "codex_bengalfox".into(),
            limit_name: "GPT-5.3-Codex-Spark".into(),
            ..QuotaLimit::default()
        };
        assert!(additional_limit_names_model(&spark, "gpt-5.3-codex-spark"));
        assert!(!additional_limit_names_model(&spark, "gpt-6-astra"));
        let unrelated = QuotaLimit {
            limit_id: "base_model_inference".into(),
            limit_name: "gpt-reserve".into(),
            ..QuotaLimit::default()
        };
        assert!(!additional_limit_names_model(
            &unrelated,
            "gpt-5.3-codex-spark"
        ));
        let by_id = QuotaLimit {
            limit_id: "gpt-5.3-codex-spark".into(),
            ..QuotaLimit::default()
        };
        assert!(additional_limit_names_model(&by_id, "gpt-5.3-codex-spark"));
    }

    // Go TestExhaustedModelMeterBlocksRouting.
    #[test]
    fn exhausted_model_meter_blocks_routing() {
        let now = Timestamp::now();
        let quota = AccountQuota {
            additional_limits: vec![QuotaLimit {
                limit_id: "codex_bengalfox".into(),
                limit_name: "GPT-5.3-Codex-Spark".into(),
                windows: vec![QuotaWindow {
                    label: "5h".into(),
                    remaining_percent: Some(0.0),
                    reset_at: Some(now.add(hours(1)).unix()),
                    present: true,
                    observed: true,
                    ..QuotaWindow::default()
                }],
                ..QuotaLimit::default()
            }],
            ..AccountQuota::default()
        };
        assert!(quota_explicitly_blocks_routing(
            &quota,
            "gpt-5.3-codex-spark",
            now,
            FLOOR
        ));
        assert!(!quota_explicitly_blocks_routing(
            &quota,
            "gpt-6-astra",
            now,
            FLOOR
        ));
    }

    // Go TestResetCreditOutcomeMessages.
    #[test]
    fn reset_credit_outcome_messages() {
        for spelling in [
            "reset",
            "Reset",
            " reset ",
            "already_redeemed",
            "alreadyRedeemed",
        ] {
            assert!(reset_credit_outcome_message(spelling).1, "{spelling}");
        }
        assert_ne!(
            reset_credit_outcome_message("nothing_to_reset").0,
            reset_credit_outcome_message("reset").0
        );
        for outcome in [
            "nothing_to_reset",
            "nothingToReset",
            "no_credit",
            "noCreditsAvailable",
            "somethingNew",
            "",
        ] {
            let (message, consumed) = reset_credit_outcome_message(outcome);
            assert!(!consumed, "{outcome}");
            assert!(!message.is_empty(), "{outcome}");
        }
    }

    // Go TestNearestAvailableResetCreditExpiration.
    #[test]
    fn nearest_available_reset_credit_expiration_cases() {
        let now = Timestamp::parse("2026-09-04T12:00:00Z").unwrap_or_default();
        let nearest = now.add(hours(24));
        let credit = |status: &str, expires_at: String| ResetCreditDetail {
            status: Some(status.into()),
            expires_at: Some(expires_at),
        };
        let details = ResetCreditDetailsResponse {
            available_count: None,
            credits: Some(vec![
                credit("used", now.add(hours(1)).to_string()),
                credit("available", now.add(hours(-1)).to_string()),
                credit("available", now.add(hours(72)).to_string()),
                credit("AVAILABLE", nearest.to_string()),
                credit("available", "not-a-date".into()),
            ]),
        };
        assert_eq!(
            nearest_available_reset_credit_expiration(&details, now),
            Some(nearest.unix())
        );
    }

    // Go TestSparseQuotaMergeRetainsOptionalMetadata.
    #[test]
    fn sparse_quota_merge_retains_optional_metadata() {
        let prior = AccountQuota {
            credits: Some(QuotaCredits {
                has_credits: true,
                balance: Some("9.00".into()),
                ..QuotaCredits::default()
            }),
            individual_limit: Some(QuotaSpendControl {
                limit: "100".into(),
                remaining_percent: Some(60),
                ..QuotaSpendControl::default()
            }),
            additional_limits: vec![QuotaLimit {
                limit_id: "gpt-review".into(),
                ..QuotaLimit::default()
            }],
            reset_credits: Some(QuotaResetCredits {
                available_count: Some(3),
                expires_at: Some(1_800_000_500),
                ..QuotaResetCredits::default()
            }),
            ..AccountQuota::default()
        };
        let current = AccountQuota {
            limit_id: "codex".into(),
            ..AccountQuota::default()
        };
        let merged = merge_sparse_quota(Some(&prior), current, &UsageResponse::default());
        assert_eq!(
            merged.credits.and_then(|c| c.balance).as_deref(),
            Some("9.00")
        );
        assert!(merged.individual_limit.is_some());
        assert_eq!(merged.additional_limits.len(), 1);
        assert_eq!(
            merged.reset_credits.and_then(|r| r.expires_at),
            Some(1_800_000_500)
        );
    }

    // Go TestQuotaTelemetryFreshnessStates.
    #[test]
    fn quota_telemetry_freshness_states() {
        let now = at(1_800_000_000);
        let freshness = minutes(QUOTA_TELEMETRY_FRESHNESS_MINUTES);
        assert_eq!(
            quota_telemetry_state(&QuotaSnapshot::default(), now),
            "not_reported"
        );
        let snapshot = |refreshed: Timestamp| QuotaSnapshot {
            last_successful_refresh_at: refreshed,
            ..QuotaSnapshot::default()
        };
        assert_eq!(
            quota_telemetry_state(&snapshot(now.add(-freshness)), now),
            "fresh"
        );
        assert_eq!(
            quota_telemetry_state(&snapshot(now.add(-freshness - seconds(1))), now),
            "stale"
        );
        let failed = QuotaSnapshot {
            quota_error: Some(QuotaErrorInfo {
                code: "upstream_status".into(),
                ..QuotaErrorInfo::default()
            }),
            ..snapshot(now)
        };
        assert_eq!(quota_telemetry_state(&failed, now), "refresh_unavailable");
    }

    // Go TestExtractUpstreamErrorCodeRejectsUnsafeValues.
    #[test]
    fn extract_upstream_error_code_rejects_unsafe_values() {
        assert_eq!(
            extract_upstream_error_code(br#"{"error":{"code":"invalid_token"}}"#),
            "invalid_token"
        );
        assert_eq!(
            extract_upstream_error_code(br#"{"error":{"code":"secret token value"}}"#),
            ""
        );
        assert_eq!(
            extract_upstream_error_code(br#"{"detail":{"code":"token_revoked"}}"#),
            "token_revoked"
        );
        assert!(gateway_unknown_model(
            br#"{"error":{"message":"Unknown provider for model x"}}"#
        ));
        assert_eq!(
            upstream_error_detail(br#"{"detail":"bad\u0007 field"}"#),
            "bad field"
        );
    }

    // Go TestRetiredModelMetersAreDropped (the default lineup half is in the
    // model catalog tests).
    #[test]
    fn retired_model_meters_are_dropped() {
        let limit = |id: &str, name: &str| QuotaLimit {
            limit_id: id.into(),
            limit_name: name.into(),
            ..QuotaLimit::default()
        };
        let kept = drop_retired_model_meters(vec![
            limit("codex_bengalfox", "GPT-5.3-Codex-Spark"),
            limit("code_review", "Code review"),
            limit("codex_live", "gpt-6-astra"),
            limit("gpt-5.2-codex", ""),
        ]);
        let ids: Vec<&str> = kept.iter().map(|limit| limit.limit_id.as_str()).collect();
        assert_eq!(ids, ["code_review", "codex_live"]);
    }

    // Go TestQuotaFromUsageTreatsReachedLimitWithoutWindowsAsExhausted.
    #[test]
    fn reached_limit_without_windows_is_exhausted() {
        let quota = decode(r#"{"rate_limit":{"limit_reached":true}}"#);
        assert!(quota.exhausted);
        assert_eq!(quota.exhaustion_reason, "rate_limit_reached");
        assert!(!quota.hourly.present && !quota.weekly.present);
        assert_eq!(remaining_quota_hint(&quota), 0);
    }

    // Go TestQuotaWindowNormalizationRequiresExplicitNumericPercentAndUsesDurationLabels.
    #[test]
    fn window_normalization_requires_explicit_numeric_percent() {
        for payload in [
            r#"{"rate_limit":{"primary_window":{"limit_window_seconds":900}}}"#,
            r#"{"rate_limit":{"primary_window":{"used_percent":null,"limit_window_seconds":900}}}"#,
            r#"{"rate_limit":{"primary_window":{"used_percent":"unknown","limit_window_seconds":900}}}"#,
            r#"{"rate_limit":{"primary_window":{"used_percent":{},"limit_window_seconds":900}}}"#,
        ] {
            let quota = decode(payload);
            assert!(
                quota.primary.observed && !quota.primary.present,
                "{payload}"
            );
            assert_eq!(quota.primary.label, "15m");
        }
        let quota = decode(
            r#"{"rate_limit":{"primary_window":{"used_percent":-7.5,"limit_window_seconds":3600},"secondary_window":{"used_percent":125,"limit_window_seconds":604800}}}"#,
        );
        assert_eq!(quota.primary.used_percent, Some(0.0));
        assert_eq!(quota_window_remaining(&quota.primary), 100.0);
        assert_eq!(quota.primary.label, "1h");
        assert_eq!(quota.secondary.used_percent, Some(100.0));
        assert_eq!(quota_window_remaining(&quota.secondary), 0.0);
        assert_eq!(quota.secondary.label, "Week");
    }

    // Go TestQuotaWindowsRemainIndependentWhenOneIsExhausted.
    #[test]
    fn windows_remain_independent_when_one_is_exhausted() {
        let quota = decode(
            r#"{"rate_limit":{
                "primary_window":{"used_percent":30,"limit_window_seconds":18000,"reset_at":1800000100},
                "secondary_window":{"used_percent":100,"limit_window_seconds":604800,"reset_at":1800000200}}}"#,
        );
        assert_eq!(quota_window_remaining(&quota.primary), 70.0);
        assert_eq!(quota.primary.reset_at, Some(1_800_000_100));
        assert_eq!(quota_window_remaining(&quota.secondary), 0.0);
        assert_eq!(quota.secondary.reset_at, Some(1_800_000_200));
        assert_eq!(quota.hourly.reset_at, Some(1_800_000_100));
        assert_eq!(quota.weekly.reset_at, Some(1_800_000_200));
    }

    // Go TestQuotaNormalizationIncludesCreditsSpendAdditionalAndResetCredits.
    #[test]
    fn normalization_includes_credits_spend_additional_and_reset_credits() {
        let quota = decode(
            r#"{
            "plan_type":"team",
            "rate_limit":{"allowed":false,"limit_reached":true},
            "credits":{"has_credits":true,"unlimited":true,"balance":"12.50"},
            "spend_control":{"reached":false,"individual_limit":{"source":"workspace","limit":"25000","used":"8000","remaining":"17000","remaining_percent":68,"reset_at":1800000500}},
            "rate_limit_reached_type":{"type":"workspace_member_credits_depleted"},
            "additional_rate_limits":[{"limit_name":"Review model","metered_feature":"gpt-review","rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":900}}}],
            "rate_limit_reset_credits":{"available_count":2}
        }"#,
        );
        assert_eq!(quota.limit_id, "codex");
        let credits = quota.credits.clone().unwrap_or_default();
        assert!(credits.has_credits && credits.unlimited);
        assert_eq!(credits.balance.as_deref(), Some("12.50"));
        assert_eq!(
            quota
                .individual_limit
                .as_ref()
                .and_then(|l| l.remaining_percent),
            Some(68)
        );
        assert_eq!(
            quota.rate_limit_reached_type,
            "workspace_member_credits_depleted"
        );
        assert!(quota.exhausted);
        assert_eq!(quota.exhaustion_reason, "workspace_member_credits_depleted");
        assert_eq!(quota.additional_limits.len(), 1);
        assert_eq!(quota.additional_limits[0].limit_id, "gpt-review");
        assert_eq!(quota.additional_limits[0].primary.label, "15m");
        assert_eq!(quota.reset_credits.and_then(|r| r.available_count), Some(2));
    }

    // The wire types fail where Go's typed decode fails, and read leniently
    // where Go's does.
    #[test]
    fn usage_decodes_as_go_does() {
        assert!(
            serde_json::from_str::<UsageResponse>(r#"{"rate_limit":{"allowed":"yes"}}"#).is_err()
        );
        assert!(
            serde_json::from_str::<UsageResponse>(
                r#"{"rate_limit":{"primary_window":{"limit_window_seconds":1.5}}}"#
            )
            .is_err()
        );
        let usage: UsageResponse = serde_json::from_str(
            r#"{"plan_type":null,"rate_limit_reached_type":"Rate_Limit_Reached","additional_rate_limits":[]}"#,
        )
        .unwrap_or_default();
        assert_eq!(usage.plan_type, None);
        assert_eq!(usage.rate_limit_reached_type, "rate_limit_reached");
        assert_eq!(
            usage.additional_rate_limits.map(|limits| limits.len()),
            Some(0)
        );
        let usage: UsageResponse =
            serde_json::from_str(r#"{"rate_limit_reached_type":7}"#).unwrap_or_default();
        assert_eq!(usage.rate_limit_reached_type, "");
        assert_eq!(quota_window_label(Some(90)), "1h 30m");
        assert_eq!(quota_window_label(Some(2880)), "2d");
    }
}
