//! An account's status as the dashboard and the response headers show it.
//!
//! A port of accountStatusLocked and addCurrentAccountResponseHeaders in
//! the Go pool's main.go. The order of the checks is the point: each comment
//! says which status must win over which.

use super::app::App;
use super::app::Inner;
use super::app::quota_error_blocks_routing;
use super::plan;
use super::proxy::safe_header_value;
use super::quota;
use super::state::Account;
use super::state::Cooldown;
use super::time::Timestamp;

pub fn active_cooldowns(values: &[Cooldown], now: Timestamp) -> Vec<Cooldown> {
    values
        .iter()
        .filter(|item| item.next_retry_at.after(&now))
        .cloned()
        .collect()
}

impl Inner {
    /// The account's status and its reason.
    pub fn account_status(&self, item: &Account, now: Timestamp) -> (&'static str, String) {
        if self.account_login_in_progress(&item.id) {
            return ("authenticating", "Sign-in repair is in progress".into());
        }
        if item.pending_auth_verification {
            return (
                "missing_auth",
                "Sign-in repair must be completed before routing resumes".into(),
            );
        }
        if !item.enabled {
            return ("disabled", "Account is disabled".into());
        }
        if !item.in_pool {
            return ("standby", "Account is not in the pool".into());
        }
        // Disabled/out-of-pool device-auth slots must be cheap to render. Only
        // read auth.json for accounts that can actually participate in
        // routing; staging slots otherwise serialize dashboard/status requests
        // behind repeated missing-auth retries while the state lock is held.
        if plan::is_codex_device_auth(item) && self.codex_auth(item).is_err() {
            return ("missing_auth", "Device auth login is required".into());
        }
        let snapshot = self.quota_snapshot(&item.id);
        // A proven credential failure is more direct and actionable than
        // duplicate identity. Keep it ahead of duplicate classification so an
        // in-pool slot that needs Repair is counted as Unavailable rather than
        // falsely inflating the Out-of-pool/Duplicate cards. Transient quota
        // polling failures remain diagnostic history rather than availability
        // gates; quota_error_blocks_routing only accepts errors that prove the
        // credential itself cannot route requests.
        if quota_error_blocks_routing(snapshot.quota_error.as_ref()) {
            let mut reason = "Quota refresh failed".to_string();
            if let Some(error) = &snapshot.quota_error
                && !error.code.is_empty()
            {
                reason.push_str(&format!(": {}", error.code));
            }
            return ("error", reason);
        }
        // Duplicate identity before cooldown/quota, so the operator sees the
        // structural reason this slot is not routable. The primary slot owns
        // the upstream identity; runtime cooldown and quota are evaluated on
        // that slot.
        let primary = self.duplicate_upstream_account_primary(item, now);
        if !primary.is_empty() {
            return (
                "duplicate",
                format!("Duplicate upstream account; routing uses {primary}"),
            );
        }
        let cooldowns = active_cooldowns(
            self.state
                .cooldowns()
                .get(&item.id)
                .map_or(&[][..], Vec::as_slice),
            now,
        );
        if let Some(first) = cooldowns.first() {
            return ("cooldown", first.reason.clone());
        }
        // Last failure and transient quota polling failures are diagnostic
        // history, not availability gates. Routing only excludes active
        // cooldowns, missing auth, exhausted quota, and explicit credential
        // errors; otherwise a usage API outage can hide a healthy Pro fallback
        // and create a false 503 exactly when a non-Pro account runs out.
        if let Some(account_quota) = &snapshot.quota {
            if quota::quota_explicitly_blocks_routing(
                account_quota,
                "",
                now,
                self.tuning.quota_floor_percent,
            ) {
                let mut reason = "Quota exhausted".to_string();
                if !account_quota.exhaustion_reason.is_empty() {
                    reason.push_str(&format!(": {}", account_quota.exhaustion_reason));
                }
                return ("exhausted", reason);
            }
            let protection = self.quota_protection_status(item, &snapshot, now);
            if protection.blocked {
                return ("protected", protection.message);
            }
            if quota::remaining_quota_hint(account_quota) <= 20 {
                return ("low", "Quota window is at or below 20%".into());
            }
        }
        if let Some(remaining) = item.remaining_quota {
            if remaining <= 0 {
                return ("exhausted", "Remaining quota is exhausted".into());
            }
            if remaining <= 20 {
                return ("low", "Remaining quota is at or below 20%".into());
            }
        }
        ("ready", "Ready".into())
    }
}

impl App {
    /// The X-Codex-Pool-* headers that tell a client which account served it.
    pub fn current_account_response_headers(&self, account_id: &str) -> Vec<(String, String)> {
        let inner = self.read();
        let Some(index) = inner.account_index(account_id) else {
            return Vec::new();
        };
        let item = inner.config.accounts()[index].clone();
        let snapshot = inner.quota_snapshot(&item.id);
        let mut display = item.clone();
        if !snapshot.organization_name.is_empty() {
            display
                .organization_name
                .clone_from(&snapshot.organization_name);
        }
        if !snapshot.plan_type.is_empty() {
            display.plan_type.clone_from(&snapshot.plan_type);
        }
        if !snapshot.plan_limit.is_empty() {
            display.plan_limit.clone_from(&snapshot.plan_limit);
        }
        let (status, _) = inner.account_status(&item, Timestamp::now());
        drop(inner);
        let display_name = plan::public_credential_display_name(&display, Some(index));
        let organization =
            plan::public_organization_name(&plan::effective_organization_name(&display));
        let plan_type = plan::normalize_plan_type(&display.plan_type);
        let plan_display = plan::account_plan_display_name(&display, false);
        let mut headers = Vec::new();
        let mut push = |name: &str, value: String| headers.push((name.to_string(), value));
        if !display_name.is_empty() {
            push("x-codex-pool-account", safe_header_value(&display_name));
        }
        if !plan_type.is_empty() && plan_type != "unknown" {
            push("x-codex-pool-plan", safe_header_value(&plan_display));
        }
        if !organization.is_empty() {
            push(
                "x-codex-pool-organization",
                safe_header_value(&organization),
            );
        }
        if !status.is_empty() {
            push("x-codex-pool-account-status", safe_header_value(status));
        }
        if let Some(account_quota) = &snapshot.quota {
            push(
                "x-codex-pool-quota-remaining",
                quota::remaining_quota_hint(account_quota).to_string(),
            );
            if account_quota.hourly.present {
                push(
                    "x-codex-pool-quota-hourly-remaining",
                    account_quota.hourly.percentage.to_string(),
                );
            }
            if account_quota.weekly.present {
                push(
                    "x-codex-pool-quota-weekly-remaining",
                    account_quota.weekly.percentage.to_string(),
                );
            }
        }
        if !snapshot.usage_updated_at.is_zero() {
            // Go's time.RFC3339: seconds only.
            push(
                "x-codex-pool-quota-updated-at",
                snapshot
                    .usage_updated_at
                    .0
                    .format("%Y-%m-%dT%H:%M:%S%:z")
                    .to_string()
                    .replace("+00:00", "Z"),
            );
        }
        headers
    }
}

pub fn to_value<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

impl Inner {
    /// What /v1/codex-pool/status shows about the account serving a session:
    /// display names and plan metadata only, never local or upstream ids.
    pub fn current_account_status(
        &self,
        item: &Account,
        index: usize,
        now: Timestamp,
    ) -> serde_json::Value {
        let (status, reason) = self.account_status(item, now);
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
        // first successful refresh it carries none. Overwriting
        // unconditionally would then erase the seat already persisted on the
        // slot and blank the row.
        if !snapshot.seat_type.is_empty() {
            display.seat_type.clone_from(&snapshot.seat_type);
            display.seat_type_inferred = snapshot.seat_type_inferred;
        }
        if !snapshot.seat_type_raw.is_empty() {
            display.seat_type_raw.clone_from(&snapshot.seat_type_raw);
        }
        display.quota_policy.clone_from(&snapshot.quota_policy);
        let remaining = display
            .remaining_quota
            .or_else(|| snapshot.quota.as_ref().map(quota::remaining_quota_hint));
        let metadata = plan::credential_metadata(&display);
        let name = plan::public_credential_display_name(&display, Some(index));
        serde_json::json!({
            "label": name,
            "displayName": name,
            "credentialMetadata": metadata,
            "email": metadata["email"],
            "organizationName": metadata["organizationName"],
            "planType": metadata["planType"],
            "planLimit": metadata["planLimit"],
            "planDisplayName": metadata["planDisplayName"],
            "planRank": metadata["planRank"],
            "status": status,
            "statusReason": reason,
            "available": status == "ready" || status == "low",
            "remainingQuota": remaining,
            "quota": to_value(&snapshot.quota),
            "usageUpdatedAt": snapshot.usage_updated_at,
            "lastSuccessfulRefreshAt": snapshot.last_successful_refresh_at,
            "quotaFreshness": snapshot.freshness,
            "quotaProvenance": snapshot.provenance,
            "quotaError": to_value(&snapshot.quota_error),
            "quotaProtection": to_value(&self.quota_protection_status(item, &snapshot, now)),
            "quotaMetering": plan::quota_metering_kind(item),
        })
    }
}
