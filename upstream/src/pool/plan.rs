//! Account plan metadata: plan families, Pro tiers, weekly weights, the
//! sanitizers for upstream metadata, and the names the dashboard shows.
//!
//! A port of the plan helpers in the Go pool's main.go (normalizePlanType,
//! planFamilyFromRaw, accountWeeklyWeight and their neighbours). The comments
//! that explain why a mapping exists are carried over with it: each one records
//! an account that was once weighed or named wrongly.

use super::state::Account;
use serde_json::Map;
use serde_json::Value;

/// baseWeeklyWeight is the unit every weekly allowance is measured against. An
/// account with no multiplier evidence weighs exactly one, so a pool of
/// ordinary subscriptions normalises to a plain average.
pub const BASE_WEEKLY_WEIGHT: f64 = 1.0;

/// OpenAI's published Premium-to-Standard usage ratio for Business seats.
///
/// Unlike a Pro multiplier, which the account itself reports through
/// planLimit, no endpoint this pool calls reports a Business seat multiplier
/// at all. This number is therefore an assumption read off a public pricing
/// page, not telemetry, and a deployment whose contract differs must be able
/// to correct it without a rebuild (CODEX_POOL_PREMIUM_SEAT_MULTIPLIER). Every
/// reading it contributes to is flagged as assumed so it can never be
/// presented with the same authority as a reported multiplier.
pub const DEFAULT_PREMIUM_SEAT_WEIGHT: f64 = 5.0;

/// OpenAI's published Pro 100 (raw plan prolite) allowance relative to Plus,
/// an assumption from public pricing flagged as such wherever it contributes.
pub const PRO_LITE_WEIGHT: f64 = 5.0;

/// The published Pro 200 allowance (10x Plus since 2026-09-29), assumed for a
/// Pro account that reports no multiplier. Codex itself labels raw plan "pro"
/// as Pro 200 (tui subscription.rs).
pub const PRO_DEFAULT_WEIGHT: f64 = 10.0;

/// The published Pro 500 (raw plan promax) allowance.
pub const PRO_MAX_WEIGHT: f64 = 25.0;

pub const MAX_OWNER_NOTE_RUNES: usize = 80;

/// Parses CODEX_POOL_PREMIUM_SEAT_MULTIPLIER. An invalid setting is a startup
/// error rather than a silent fallback, because a wrong multiplier silently
/// distorts every weighted reading it touches.
pub fn premium_seat_weight_from_env(raw: Option<&str>) -> Result<f64, String> {
    let raw = raw.map(str::trim).unwrap_or_default();
    if raw.is_empty() {
        return Ok(DEFAULT_PREMIUM_SEAT_WEIGHT);
    }
    match raw.parse::<f64>() {
        Ok(value) if value > 0.0 && value.is_finite() => Ok(value),
        _ => Err("CODEX_POOL_PREMIUM_SEAT_MULTIPLIER must be a positive number".to_string()),
    }
}

/// How many base allowances one account's weekly budget is worth, and whether
/// that figure is assumed rather than reported.
///
/// A Pro slot reports its own multiplier, so that value is authoritative. A
/// Business Premium seat reports none, so the assumed ratio applies and the
/// caller must surface it as an assumption. Everything else weighs one:
/// absent evidence, an account counts once rather than being guessed upward.
pub fn account_weekly_weight(
    plan_family: &str,
    plan_limit: &str,
    seat_type: &str,
    premium_seat_weight: f64,
) -> (f64, bool) {
    match normalize_plan_type(plan_family).as_str() {
        "pro" => {
            match clean_plan_limit(plan_limit).as_str() {
                "5x" => return (5.0, false),
                "10x" => return (10.0, false),
                "20x" => return (20.0, false),
                "25x" => return (25.0, false),
                _ => {}
            }
            // Upstream reports no multiplier for this Pro account. Pro 100 has
            // its own raw plan (prolite), so a plain Pro without one is weighed
            // as Pro 200, OpenAI's published 10x, and flagged as assumed like
            // Pro 100 and Premium seats. Weighing it as one understated a 10x
            // account as a single base allowance. If it is really Pro 500 this
            // still errs low.
            (PRO_DEFAULT_WEIGHT, true)
        }
        // Pro Lite is the Pro 100 tier, which OpenAI prices at 5x Plus. Like
        // the Premium seat ratio this is read off a pricing page rather than
        // reported by the account, so it is flagged as assumed. It is not a
        // "generic Pro" guess: the raw plan names the tier exactly, and weighing
        // it as one understated a 5x account as a single base allowance.
        "pro_lite" => (PRO_LITE_WEIGHT, true),
        // The Pro 500 tier, which OpenAI prices at 25x Plus. The raw plan names
        // the tier exactly, as prolite does for Pro 100; the multiplier is read
        // off the pricing page, so it is flagged as assumed.
        "pro_max" => (PRO_MAX_WEIGHT, true),
        "business" if clean_metadata_token(seat_type) == "premium" => (premium_seat_weight, true),
        _ => (BASE_WEEKLY_WEIGHT, false),
    }
}

pub fn clean_plan_limit(value: &str) -> String {
    let value = value.trim().to_lowercase();
    match value.as_str() {
        "5x" | "10x" | "20x" | "25x" => value,
        _ => String::new(),
    }
}

pub fn compact_plan_limit_text(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .filter(char::is_ascii_lowercase_or_digit)
        .collect()
}

trait AsciiLowerOrDigit {
    fn is_ascii_lowercase_or_digit(&self) -> bool;
}

impl AsciiLowerOrDigit for char {
    fn is_ascii_lowercase_or_digit(&self) -> bool {
        self.is_ascii_lowercase() || self.is_ascii_digit()
    }
}

pub fn plan_limit_from_map(values: Option<&Map<String, Value>>) -> String {
    let Some(values) = values else {
        return String::new();
    };
    const DIRECT: &[&str] = &[
        "plan_limit",
        "planLimit",
        "codex_plan_limit",
        "codexPlanLimit",
        "usage_tier",
        "usageTier",
        "quota_tier",
        "quotaTier",
        "rate_limit_tier",
        "rateLimitTier",
        "plan_tier",
        "planTier",
        "pro_tier",
        "proTier",
    ];
    const MULTIPLIERS: &[&str] = &[
        "multiplier",
        "usage_multiplier",
        "usageMultiplier",
        "quota_multiplier",
        "quotaMultiplier",
        "rate_limit_multiplier",
        "rateLimitMultiplier",
        "codex_rate_limit_multiplier",
        "codexRateLimitMultiplier",
    ];
    const NESTED: &[&str] = &[
        "plan",
        "subscription",
        "subscriptions",
        "entitlement",
        "account",
        "accounts",
        "items",
        "data",
        "billing",
        "quota",
        "rate_limit",
        "limits",
        "codex",
    ];
    for key in DIRECT.iter().chain(MULTIPLIERS).chain(NESTED) {
        let limit = plan_limit_from_value(values.get(*key));
        if !limit.is_empty() {
            return limit;
        }
    }
    String::new()
}

pub fn plan_limit_from_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => clean_plan_limit(text),
        Some(Value::Number(number)) => {
            if let Some(whole) = number.as_i64() {
                return plan_limit_from_number(whole);
            }
            // Go decodes JSON numbers as float64 and accepts a whole one.
            match number.as_f64() {
                Some(float) if float.fract() == 0.0 && float.abs() < 1e15 =>
                {
                    #[allow(clippy::cast_possible_truncation)]
                    plan_limit_from_number(float as i64)
                }
                _ => String::new(),
            }
        }
        Some(Value::Object(map)) => plan_limit_from_map(Some(map)),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| plan_limit_from_value(Some(item)))
            .find(|limit| !limit.is_empty())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

pub fn plan_limit_from_number(value: i64) -> String {
    match value {
        5 | 10 | 20 | 25 => format!("{value}x"),
        _ => String::new(),
    }
}

pub fn normalize_plan_type(value: &str) -> String {
    let value = value.trim().to_lowercase().replace(['-', ' '], "_");
    match value.as_str() {
        "free" | "go" | "plus" | "pro" | "pro_lite" | "pro_max" | "team" | "business"
        | "enterprise" | "edu" => value,
        "chatgpt_plus" => "plus".to_string(),
        "chatgpt_pro" => "pro".to_string(),
        "chatgpt_team" => "team".to_string(),
        "chatgpt_business" => "business".to_string(),
        "chatgpt_enterprise" => "enterprise".to_string(),
        "chatgpt_edu" => "edu".to_string(),
        "" => "unknown".to_string(),
        _ => {
            let compact = compact_plan_limit_text(&value);
            for candidate in [
                "enterprise",
                "business",
                "team",
                "plus",
                "pro",
                "free",
                "edu",
            ] {
                if compact.contains(candidate) {
                    return candidate.to_string();
                }
            }
            value
        }
    }
}

/// Go's strings.Fields joined by single spaces.
pub fn collapse_fields(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn truncate_runes(value: &str, max_runes: usize) -> String {
    value.chars().take(max_runes).collect()
}

pub fn clean_metadata_text(value: &str, max_runes: usize) -> String {
    let kept: String = value.trim().chars().filter(|c| !c.is_control()).collect();
    let value = collapse_fields(&kept);
    if max_runes > 0 && value.chars().count() > max_runes {
        return truncate_runes(&value, max_runes);
    }
    value
}

pub fn clean_metadata_token(value: &str) -> String {
    let value = value.trim().to_lowercase();
    if value.is_empty() || value.len() > 80 {
        return String::new();
    }
    let allowed = value.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '_' | '-' | '.')
    });
    if allowed { value } else { String::new() }
}

pub fn clean_raw_plan_type(value: &str) -> String {
    clean_metadata_token(value)
}

pub fn plan_family_from_raw(value: &str) -> &'static str {
    match clean_raw_plan_type(value).as_str() {
        "free" => "free",
        "go" => "go",
        "plus" | "chatgptplus" | "chatgpt_plus" => "plus",
        "pro" | "chatgptpro" | "chatgpt_pro" | "chatgptproplan" => "pro",
        // chatgptprolite is the web app's name for it, still stored on slots
        // whose plan was last read from the web app's account metadata.
        "prolite" | "chatgptprolite" | "pro_lite" => "pro_lite",
        // promax is the Pro 500 tier: Codex names it so (protocol auth.rs
        // KnownPlan ProMax, tui subscription.rs "Pro 500"). Unmapped, a Pro 500
        // account was "unknown" and weighed as one base allowance instead of 25.
        "promax" | "chatgptpromax" | "pro_max" => "pro_max",
        // self_serve_business_prolite appeared in ID-token claims in September
        // 2026 on the Business seats inferred as Premium (Premium is priced like
        // Pro 100, hence "prolite"). It is mapped to the Business family only;
        // it is deliberately not treated as authoritative seat evidence,
        // because no upstream documentation defines it and the quota-shape
        // inference already covers these seats. Without this entry a re-login,
        // which takes the plan from the token, showed the account as "Unknown
        // tier" and dropped it out of Business seat inference until the next
        // quota refresh.
        "team"
        | "chatgptteam"
        | "chatgpt_team"
        | "chatgptteamplan"
        | "self_serve_business_usage_based"
        | "self_serve_business_prolite" => "business",
        "business" | "enterprise_cbp_usage_based" | "enterprise" | "hc" => "enterprise",
        "education" | "edu" => "edu",
        _ => "unknown",
    }
}

pub fn normalize_account_plan_metadata(item: &mut Account) {
    item.raw_plan_type = clean_raw_plan_type(&item.raw_plan_type);
    if !item.raw_plan_type.is_empty() {
        item.plan_family = plan_family_from_raw(&item.raw_plan_type).to_string();
    } else if !item.plan_family.trim().is_empty() {
        item.plan_family = normalize_plan_type(&item.plan_family);
    } else {
        // Legacy config stored only the already-normalized planType. Do not
        // apply raw-wire remapping here or an old Business family would become
        // Enterprise merely because the new rawPlanType field was absent.
        item.plan_family = normalize_plan_type(&item.plan_type);
    }
    item.plan_type = item.plan_family.clone();
    match clean_metadata_token(&item.seat_type).as_str() {
        seat @ ("standard" | "premium") => item.seat_type = seat.to_string(),
        _ => {
            item.seat_type = String::new();
            item.seat_type_inferred = false;
        }
    }
    item.seat_type_raw = clean_metadata_token(&item.seat_type_raw);
    item.quota_policy = item
        .quota_policy
        .iter()
        .map(|policy| clean_metadata_token(policy))
        .filter(|policy| !policy.is_empty())
        .collect();
}

pub fn effective_plan_family(item: &Account) -> String {
    let family = item.plan_family.trim();
    if !family.is_empty() {
        return normalize_plan_type(family);
    }
    normalize_plan_type(&item.plan_type)
}

pub fn plan_rank(plan: &str) -> i64 {
    match normalize_plan_type(plan).as_str() {
        "enterprise" => 500,
        "team" | "business" => 400,
        "pro_max" => 320,
        "pro" => 300,
        "pro_lite" => 250,
        "plus" => 200,
        "edu" => 150,
        "free" => 100,
        _ => 0,
    }
}

/// Names a Pro account's tier from its reported multiplier, using OpenAI's
/// September 2026 names. The multiplier is kept next to the name because the
/// two do not map one-to-one: Pro 200 is 10x for new subscribers but 20x for
/// existing ones until 2026-10-29, and 5x is also what the separate prolite
/// plan (Pro 100) provides.
pub fn pro_tier_display_name(limit: &str) -> &'static str {
    match limit {
        "5x" => "Pro 100",
        "10x" | "20x" => "Pro 200",
        "25x" => "Pro 500",
        _ => "Pro",
    }
}

pub fn plan_display_name(plan: &str) -> String {
    let normalized = normalize_plan_type(plan);
    match normalized.as_str() {
        "free" => "Free".to_string(),
        "plus" => "Plus".to_string(),
        "pro" => "Pro".to_string(),
        // OpenAI now sells Pro as Pro 100 / 200 / 500; the prolite plan is the
        // $100 tier.
        "pro_lite" => "Pro 100".to_string(),
        "pro_max" => "Pro 500".to_string(),
        "go" => "Go".to_string(),
        "team" => "Team".to_string(),
        "business" => "Business".to_string(),
        "enterprise" => "Enterprise".to_string(),
        "edu" => "Edu".to_string(),
        "unknown" => "Unknown tier".to_string(),
        _ => {
            let mut chars = normalized.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

pub fn account_plan_display_name(item: &Account, with_account_suffix: bool) -> String {
    let plan = effective_plan_family(item);
    let mut name = plan_display_name(&plan);
    if plan == "pro" {
        let limit = clean_plan_limit(&item.plan_limit);
        name = if limit.is_empty() {
            // No reported multiplier: shown as the assumed Pro 200 tier, with
            // no multiplier beside it, so it never reads as a reported value.
            "Pro 200".to_string()
        } else {
            format!("{} {limit}", pro_tier_display_name(&limit))
        };
    }
    if with_account_suffix {
        name.push_str(" account");
    }
    let organization_name = public_organization_name(&effective_organization_name(item));
    if !organization_name.is_empty() && organization_scoped_plan(&plan) {
        return format!("{name} · {organization_name}");
    }
    name
}

pub fn organization_scoped_plan(plan: &str) -> bool {
    matches!(
        normalize_plan_type(plan).as_str(),
        "team" | "business" | "enterprise" | "edu"
    )
}

pub fn is_codex_device_auth(item: &Account) -> bool {
    item.auth_type == "codex_device_auth"
}

pub fn quota_metering_kind(item: &Account) -> &'static str {
    if is_codex_device_auth(item) {
        "chatgpt_subscription"
    } else {
        "api_metered"
    }
}

pub fn generated_account_id_base(item: &Account) -> &'static str {
    if is_codex_device_auth(item) {
        "acct-credential"
    } else if item.auth_type == "provider_api_key" {
        "acct-provider"
    } else {
        "acct-account"
    }
}

pub fn valid_account_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn allowed_model(item: &Account, model: &str) -> bool {
    if item
        .excluded_models
        .iter()
        .any(|excluded| excluded == model)
    {
        return false;
    }
    item.allowed_models.is_empty() || item.allowed_models.iter().any(|allowed| allowed == model)
}

pub fn normal_wire_api(value: &str) -> &'static str {
    match value.to_lowercase().as_str() {
        "chat_completions" | "chat-completions" | "openai_chat" | "openai-chat" | "chat" => {
            "chat_completions"
        }
        _ => "responses",
    }
}

pub fn normalize_email(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Go slices the local part by bytes; a cut through a multi-byte character
/// becomes U+FFFD when the value is written as JSON, as here.
pub fn masked_public_email(value: &str) -> String {
    let value = normalize_email(value);
    let parts: Vec<&str> = value.split('@').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return String::new();
    }
    let local = parts[0].as_bytes();
    let take = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let masked = if local.len() <= 2 {
        format!("{}***", take(&local[..1]))
    } else if local.len() <= 4 {
        format!("{}***", take(&local[..2]))
    } else {
        format!(
            "{}***{}",
            take(&local[..2]),
            take(&local[local.len() - 2..])
        )
    };
    format!("{masked}@{}", parts[1])
}

fn is_email_local_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

fn is_email_domain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-')
}

/// Masks every address matching `[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}`
/// (Go emailInDisplayPattern), leftmost-longest as Go's regexp does here.
fn mask_emails_in(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        if let Some(end) = email_match_at(&chars, index) {
            let matched: String = chars[index..end].iter().collect();
            out.push_str(&masked_public_email(&matched));
            index = end;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

fn email_match_at(chars: &[char], start: usize) -> Option<usize> {
    let mut at = start;
    while at < chars.len() && is_email_local_char(chars[at]) {
        at += 1;
    }
    if at == start || at >= chars.len() || chars[at] != '@' {
        return None;
    }
    let domain_start = at + 1;
    let mut end = domain_start;
    while end < chars.len() && is_email_domain_char(chars[end]) {
        end += 1;
    }
    // The longest domain whose tail is `.` and two or more letters.
    let mut candidate = end;
    while candidate > domain_start {
        let mut letters = 0;
        while letters < candidate - domain_start
            && chars[candidate - 1 - letters].is_ascii_alphabetic()
        {
            letters += 1;
        }
        let dot = candidate.checked_sub(letters + 1);
        if letters >= 2 && dot.is_some_and(|dot| dot > domain_start && chars[dot] == '.') {
            return Some(candidate);
        }
        candidate -= 1;
    }
    None
}

pub fn public_organization_name(value: &str) -> String {
    let value = clean_organization_name(value);
    if value.is_empty() {
        return value;
    }
    mask_emails_in(&value)
}

/// Notes are rendered on an unauthenticated page. Collapse whitespace and
/// remove control characters at the storage boundary so every UI/API caller
/// receives the same compact plain-text value.
pub fn normalized_owner_note(value: &str) -> String {
    let safe: String = value
        .chars()
        .filter(|r| !r.is_control() || r.is_whitespace())
        .collect();
    collapse_fields(&safe)
}

pub fn clean_owner_note(value: &str) -> String {
    truncate_runes(&normalized_owner_note(value), MAX_OWNER_NOTE_RUNES)
}

pub fn validate_owner_note(value: &str) -> Result<String, String> {
    if value.chars().any(|r| r.is_control() && !r.is_whitespace()) {
        return Err("account note must contain plain text only".to_string());
    }
    let value = normalized_owner_note(value);
    if value.chars().count() > MAX_OWNER_NOTE_RUNES {
        return Err(format!(
            "account note must be {MAX_OWNER_NOTE_RUNES} characters or fewer"
        ));
    }
    Ok(value)
}

pub fn effective_organization_name(item: &Account) -> String {
    clean_organization_name(&item.organization_name)
}

pub fn clean_organization_name(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    let kept: String = value.chars().filter(|c| !c.is_control()).collect();
    let value = collapse_fields(&kept);
    if value.chars().count() > 120 {
        return truncate_runes(&value, 120);
    }
    value
}

pub fn claim_string<'a>(claims: &'a Map<String, Value>, name: &str) -> &'a str {
    claims.get(name).and_then(Value::as_str).unwrap_or_default()
}

pub fn organization_name_from_map(values: Option<&Map<String, Value>>) -> String {
    let Some(values) = values else {
        return String::new();
    };
    for key in [
        "organization_name",
        "organization_display_name",
        "org_name",
        "org_display_name",
        "workspace_name",
        "workspace_display_name",
        "team_name",
        "team_display_name",
        "account_name",
        "account_display_name",
        "chatgpt_organization_name",
        "chatgpt_org_name",
        "chatgpt_workspace_name",
        "chatgpt_account_name",
    ] {
        let value = clean_organization_name(claim_string(values, key));
        if !value.is_empty() {
            return value;
        }
    }
    if claim_string(values, "structure").eq_ignore_ascii_case("workspace") {
        let value = organization_name_from_nested_map(Some(values));
        if !value.is_empty() {
            return value;
        }
    }
    for key in ["organization", "org", "workspace", "team"] {
        let value = organization_name_from_nested_map(values.get(key).and_then(Value::as_object));
        if !value.is_empty() {
            return value;
        }
    }
    for key in [
        "account",
        "accounts",
        "subscription",
        "subscriptions",
        "entitlement",
        "billing",
        "items",
        "data",
    ] {
        let value = organization_name_from_map(values.get(key).and_then(Value::as_object));
        if !value.is_empty() {
            return value;
        }
        if let Some(items) = values.get(key).and_then(Value::as_array) {
            for item in items {
                let value = organization_name_from_map(item.as_object());
                if !value.is_empty() {
                    return value;
                }
            }
        }
    }
    String::new()
}

pub fn organization_name_from_nested_map(values: Option<&Map<String, Value>>) -> String {
    let Some(values) = values else {
        return String::new();
    };
    for key in ["display_name", "name", "title"] {
        let value = clean_organization_name(claim_string(values, key));
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

pub fn credential_metadata(item: &Account) -> Value {
    let plan_family = effective_plan_family(item);
    let seat_type = clean_metadata_token(&item.seat_type);
    let seat_display = match seat_type.as_str() {
        "standard" => "Standard".to_string(),
        "premium" => "Premium".to_string(),
        _ => {
            let raw = clean_metadata_token(&item.seat_type_raw);
            if raw.is_empty() {
                "Not reported".to_string()
            } else {
                raw
            }
        }
    };
    serde_json::json!({
        "email": masked_public_email(&item.email),
        "organizationName": public_organization_name(&effective_organization_name(item)),
        "rawPlanType": clean_raw_plan_type(&item.raw_plan_type),
        "planFamily": plan_family,
        "planType": plan_family,
        "planLimit": clean_plan_limit(&item.plan_limit),
        "planDisplayName": account_plan_display_name(item, false),
        "planRank": item.plan_rank,
        "seatType": seat_type,
        "seatTypeRaw": clean_metadata_token(&item.seat_type_raw),
        "seatTypeDisplay": seat_display,
        "seatTypeInferred": item.seat_type_inferred && !seat_type.is_empty(),
        "quotaPolicy": item.quota_policy,
    })
}

pub fn management_credential_display_name(item: &Account) -> String {
    let email = masked_public_email(&item.email);
    if !email.is_empty() {
        return email;
    }
    let label = credential_label(item);
    if !label.is_empty() {
        return label;
    }
    if !item.id.trim().is_empty() {
        return item.id.clone();
    }
    "Credential".to_string()
}

/// `index` is the account's position, or None where Go passes -1.
pub fn public_credential_display_name(item: &Account, index: Option<usize>) -> String {
    let email = masked_public_email(&item.email);
    if !email.is_empty() {
        return email;
    }
    let label = credential_label(item);
    if !label.is_empty() && label != item.id {
        return label;
    }
    match index {
        Some(index) => format!("Credential {}", index + 1),
        None => "Credential".to_string(),
    }
}

pub fn credential_label(item: &Account) -> String {
    let label = item.label.trim();
    if label.is_empty() || metadata_derived_account_label(item, label) {
        return String::new();
    }
    label.to_string()
}

pub fn metadata_derived_account_label(item: &Account, label: &str) -> bool {
    let label = label.trim();
    if label.is_empty() {
        return false;
    }
    if label.contains('@') {
        return true;
    }
    let mut generated = vec![
        legacy_account_display_name(item),
        account_plan_display_name(item, false),
        account_plan_display_name(item, true),
        plan_display_name(&item.plan_type),
        format!("{} account", plan_display_name(&item.plan_type)),
    ];
    // Labels generated before the Pro 100/200/500 rename still count as
    // metadata-derived, so they keep refreshing instead of freezing as if an
    // operator had typed them.
    let family = effective_plan_family(item);
    if family == "pro_lite" {
        generated.push("Pro Lite".to_string());
        generated.push("Pro Lite account".to_string());
    }
    let limit = clean_plan_limit(&item.plan_limit);
    if !limit.is_empty() && family == "pro" {
        generated.push(format!("Pro {limit}"));
        generated.push(format!("Pro {limit} account"));
    }
    generated
        .iter()
        .any(|value| !value.is_empty() && go_equal_fold(label, value))
}

/// Go's strings.EqualFold (simple Unicode case folding).
pub fn go_equal_fold(left: &str, right: &str) -> bool {
    left.chars().count() == right.chars().count()
        && left
            .chars()
            .zip(right.chars())
            .all(|(a, b)| a == b || a.to_lowercase().eq(b.to_lowercase()))
}

pub fn legacy_account_display_name(item: &Account) -> String {
    let plan = account_plan_display_name(item, false);
    let email = item.email.trim();
    let known = normalize_plan_type(&item.plan_type) != "unknown";
    if !email.is_empty() && known {
        return format!("{email} · {plan}");
    }
    if !email.is_empty() {
        return email.to_string();
    }
    if known {
        return plan;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // Go TestAccountWeeklyWeight.
    #[test]
    fn account_weekly_weight_cases() {
        let cases: &[(&str, &str, &str, &str, f64, bool)] = &[
            ("pro reports its multiplier", "pro", "20x", "", 20.0, false),
            ("pro 5x", "pro", "5x", "", 5.0, false),
            ("pro 10x", "pro", "10x", "", 10.0, false),
            ("pro 500 reports 25x", "pro", "25x", "", 25.0, false),
            (
                "pro lite is the assumed pro 100 ratio",
                "pro_lite",
                "",
                "",
                PRO_LITE_WEIGHT,
                true,
            ),
            (
                "pro without a multiplier is the assumed pro 200 ratio",
                "pro",
                "",
                "",
                PRO_DEFAULT_WEIGHT,
                true,
            ),
            (
                "unsupported multiplier text falls back to the assumed ratio",
                "pro",
                "50x",
                "",
                PRO_DEFAULT_WEIGHT,
                true,
            ),
            (
                "premium seat uses the assumed ratio",
                "business",
                "",
                "premium",
                DEFAULT_PREMIUM_SEAT_WEIGHT,
                true,
            ),
            (
                "standard seat is the base unit",
                "business",
                "",
                "standard",
                1.0,
                false,
            ),
            (
                "business with no seat evidence",
                "business",
                "",
                "",
                1.0,
                false,
            ),
            ("plus", "plus", "", "", 1.0, false),
            (
                "a seat type on a non-business plan is not a multiplier",
                "plus",
                "",
                "premium",
                1.0,
                false,
            ),
        ];
        for (name, family, limit, seat, want, want_assumed) in cases {
            assert_eq!(
                account_weekly_weight(family, limit, seat, DEFAULT_PREMIUM_SEAT_WEIGHT),
                (*want, *want_assumed),
                "{name}"
            );
        }
    }

    // Go TestSeptember2026PlanNames.
    #[test]
    fn september_2026_plan_names() {
        assert_eq!(
            plan_family_from_raw("self_serve_business_prolite"),
            "business"
        );
        assert_eq!(plan_display_name("pro_lite"), "Pro 100");
        for (limit, want) in [
            ("5x", "Pro 100"),
            ("10x", "Pro 200"),
            ("20x", "Pro 200"),
            ("25x", "Pro 500"),
            ("", "Pro"),
        ] {
            assert_eq!(pro_tier_display_name(limit), want, "{limit}");
        }
        assert_eq!(plan_limit_from_number(25), "25x");
        let lite = Account {
            plan_family: "pro_lite".into(),
            plan_type: "pro_lite".into(),
            ..Account::default()
        };
        assert!(metadata_derived_account_label(&lite, "Pro Lite account"));
        let pro = Account {
            plan_family: "pro".into(),
            plan_type: "pro".into(),
            plan_limit: "20x".into(),
            ..Account::default()
        };
        assert!(metadata_derived_account_label(&pro, "Pro 20x"));
    }

    // Go TestPlanMetadataKeepsRawFamilySeatAndMultiplierSeparate (the plan
    // part; the accounts/check part is in the metadata module).
    #[test]
    fn plan_metadata_keeps_raw_family_and_multiplier_separate() {
        for (raw, family) in [
            ("team", "business"),
            ("self_serve_business_usage_based", "business"),
            ("business", "enterprise"),
            ("enterprise_cbp_usage_based", "enterprise"),
            ("prolite", "pro_lite"),
            ("future_plan", "unknown"),
        ] {
            assert_eq!(plan_family_from_raw(raw), family, "{raw}");
        }
        // 25x is Pro 500 (September 2026); 15x and 50x are still no tier.
        for value in ["15x", "50x", "premium", "plan-5x-extra", "pro20x"] {
            assert_eq!(clean_plan_limit(value), "", "{value}");
        }
        for value in ["5x", "10x", "20x", "25x"] {
            assert_eq!(clean_plan_limit(value), value);
        }
        let item = Account {
            raw_plan_type: "team".into(),
            plan_family: "business".into(),
            plan_type: "business".into(),
            ..Account::default()
        };
        let projected = credential_metadata(&item);
        assert_eq!(projected["seatType"], "");
        assert_eq!(projected["seatTypeDisplay"], "Not reported");
    }

    #[test]
    fn masks_emails_as_go_does() {
        assert_eq!(
            masked_public_email("Alice.Example@Example.test"),
            "al***le@example.test"
        );
        assert_eq!(masked_public_email("ab@example.test"), "a***@example.test");
        assert_eq!(
            masked_public_email("abcd@example.test"),
            "ab***@example.test"
        );
        assert_eq!(masked_public_email("not-an-email"), "");
        assert_eq!(
            public_organization_name("  Team of owner.name@corp.example.com\u{7}  "),
            "Team of ow***me@corp.example.com"
        );
        assert_eq!(public_organization_name("x@y.z1"), "x@y.z1");
    }

    #[test]
    fn owner_notes_are_plain_text() {
        assert_eq!(normalized_owner_note("  a \n\t b  "), "a b");
        assert!(validate_owner_note("bad\u{7}").is_err());
        assert!(validate_owner_note(&"x".repeat(81)).is_err());
        assert_eq!(clean_owner_note(&"y".repeat(90)).chars().count(), 80);
    }

    #[test]
    fn normalizes_plan_types() {
        for (input, want) in [
            ("ChatGPT-Plus", "plus"),
            ("Pro Lite", "pro_lite"),
            ("", "unknown"),
            ("my-enterprise-plan", "enterprise"),
            ("weird", "weird"),
        ] {
            assert_eq!(normalize_plan_type(input), want, "{input}");
        }
        assert_eq!(plan_display_name("weird"), "Weird");
        assert_eq!(plan_rank("pro_max"), 320);
    }
}
