//! Reading an account's allowance from upstream, as a native Codex reads it.
//!
//! A port of refreshAccountQuotaWithExpectedIdentity and its helpers in
//! the Go pool's main.go. Every upstream call here is one of the pool's own
//! calls composed as Codex composes it (own_call.rs): the usage read is
//! backend-client rate_limit_resets.rs get_rate_limit_status, the workspace
//! lookup is client.rs get_accounts_check, and the credit listing is
//! rate_limit_resets.rs list_rate_limit_reset_credits.

use super::app::App;
use super::metadata::CodexAuthInfo;
use super::metadata::SubscriptionMetadata;
use super::own_call::OwnCall;
use super::plan;
use super::quota;
use super::quota::ResetCreditDetailsResponse;
use super::quota::UsageResponse;
use super::state::AccountQuota;
use super::state::QuotaErrorInfo;
use super::state::QuotaResetCredits;
use super::state::QuotaSnapshot;
use super::stream::MAX_REQUEST_BODY;
use super::stream::SseReader;
use super::time::Timestamp;
use super::time::hours;
use std::time::Duration;

const QUOTA_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
/// Reset-credit expiry is descriptive metadata from a separate endpoint, not
/// routing evidence. It is refreshed far less often than quota windows so the
/// dashboard can show the date without doubling upstream polling.
const RESET_CREDIT_DETAILS_REFRESH_HOURS: i64 = 6;

/// Why a quota refresh did not produce a snapshot.
#[derive(Debug, PartialEq)]
pub enum RefreshError {
    /// The message the caller may show; the stored quota error carries the
    /// sanitized code.
    Failed(String),
    /// A public sign-in repair produced a different upstream account than
    /// the slot's. Public repair is not an account-replacement API.
    RepairIdentityMismatch,
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(message) => f.write_str(message),
            Self::RepairIdentityMismatch => {
                f.write_str("public sign-in repair identity did not match the existing account")
            }
        }
    }
}

fn failed(message: impl Into<String>) -> RefreshError {
    RefreshError::Failed(message.into())
}

impl App {
    pub async fn refresh_account_quota(
        &self,
        account_id: &str,
    ) -> Result<QuotaSnapshot, RefreshError> {
        self.refresh_account_quota_with_expected_identity(account_id, "")
            .await
    }

    pub async fn refresh_account_quota_for_repair(
        &self,
        account_id: &str,
        expected_upstream_account_id: &str,
    ) -> Result<QuotaSnapshot, RefreshError> {
        self.refresh_account_quota_with_expected_identity(
            account_id,
            expected_upstream_account_id.trim(),
        )
        .await
    }

    pub async fn refresh_account_quota_with_expected_identity(
        &self,
        account_id: &str,
        expected_upstream_account_id: &str,
    ) -> Result<QuotaSnapshot, RefreshError> {
        match tokio::time::timeout(
            QUOTA_REFRESH_TIMEOUT,
            self.refresh_quota_once(account_id, expected_upstream_account_id),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(failed("quota refresh cancelled")),
        }
    }

    async fn refresh_quota_once(
        &self,
        account_id: &str,
        expected_upstream_account_id: &str,
    ) -> Result<QuotaSnapshot, RefreshError> {
        let (item, prior_reset_credits) = {
            let inner = self.read();
            let Some(item) = inner.account(account_id).cloned() else {
                return Err(failed("account not found"));
            };
            let prior = inner
                .state
                .quotas()
                .get(account_id)
                .and_then(|snapshot| snapshot.quota.as_ref())
                .and_then(|quota| quota.reset_credits.clone());
            (item, prior)
        };
        if !plan::is_codex_device_auth(&item) {
            return Err(failed(
                "quota refresh is only available for Codex device-auth accounts",
            ));
        }
        let mut auth = match self.refreshed_codex_auth(&item).await {
            Ok(auth) => auth,
            Err(error) => {
                let code = if error.auth_failed {
                    "account_auth_failed"
                } else {
                    "token_refresh_unavailable"
                };
                self.save_quota_error(account_id, code, "refresh codex token failed");
                return Err(failed("refresh codex token failed"));
            }
        };
        let mut verified_upstream_account_id = auth.account_id.trim().to_string();
        if auth.account_id.is_empty() {
            auth.account_id.clone_from(&item.account_id);
        }

        // Codex's own rate-limit read (backend-client rate_limit_resets.rs
        // get_rate_limit_status): GET /wham/usage with the backend-client
        // headers.
        let call = OwnCall::Usage(self.codex_backend_call(
            account_id,
            self.settings.codex_usage_url(),
            &auth,
        ));
        let Ok(response) = self.send_own_call(&call).await else {
            self.save_quota_error(account_id, "request_failed", "quota request failed");
            return Err(failed("quota request failed"));
        };
        let status = response.status;
        let Ok(body) = SseReader::new(response.body)
            .read_all(MAX_REQUEST_BODY)
            .await
        else {
            self.save_quota_error(
                account_id,
                "read_failed",
                "quota response could not be read",
            );
            return Err(failed("quota response could not be read"));
        };
        if !status.is_success() {
            let mut code = quota::extract_upstream_error_code(&body);
            let mut message = format!("quota API returned status {}", status.as_u16());
            if !code.is_empty() {
                message.push_str(&format!(" [{code}]"));
            }
            if super::credentials::usage_refuses_credential(status.as_u16(), &code) {
                self.record_refused_credential(account_id, &code);
            }
            let auth_status =
                status == http::StatusCode::UNAUTHORIZED || status == http::StatusCode::FORBIDDEN;
            let blocks = super::app::quota_error_blocks_routing(Some(&QuotaErrorInfo {
                code: code.clone(),
                ..QuotaErrorInfo::default()
            }));
            if auth_status && !blocks {
                code = "account_auth_failed".to_string();
            }
            let code = if code.is_empty() {
                "upstream_status".to_string()
            } else {
                code
            };
            self.save_quota_error(account_id, &code, &message);
            return Err(failed(message));
        }
        self.clear_refused_credential(account_id);
        let Ok(usage) = serde_json::from_slice::<UsageResponse>(&body) else {
            self.save_quota_error(
                account_id,
                "decode_failed",
                "quota response JSON could not be decoded",
            );
            return Err(failed("quota response JSON could not be decoded"));
        };
        let usage_fields =
            serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&body).ok();
        let now = Timestamp::now();
        let mut current = quota::quota_from_usage(&usage, now);
        self.enrich_reset_credit_expiration(
            account_id,
            &auth,
            &mut current,
            prior_reset_credits.as_ref(),
            now,
        )
        .await;
        let mut plan_raw = plan::clean_raw_plan_type(&choose(
            usage.plan_type.as_deref(),
            usage.subscription_plan.as_deref(),
        ));
        let mut family = plan::plan_family_from_raw(&plan_raw).to_string();
        let mut plan_limit = plan::plan_limit_from_map(usage_fields.as_ref());
        // auth.plan_limit is mostly this pool's own earlier output: the
        // multiplier is stored with the account and read back on the next
        // refresh. Using it before the account metadata made a recorded
        // multiplier self-perpetuating, so a Pro 200 account kept showing 20x
        // after upstream cut the tier to 10x. It is only a last resort, used
        // when the metadata lookup could not run; a metadata answer, including
        // "no multiplier", always wins over it.
        let stored_plan_limit = plan::clean_plan_limit(&auth.plan_limit);
        let mut organization_name =
            plan::clean_organization_name(&plan::organization_name_from_map(usage_fields.as_ref()));
        if organization_name.is_empty() && !auth.organization_name.is_empty() {
            organization_name = plan::clean_organization_name(&auth.organization_name);
        }
        let mut metadata_resolved = false;
        if (plan_raw.is_empty()
            || organization_name.is_empty()
            || plan::organization_scoped_plan(&family))
            && let Ok(metadata) = self
                .fetch_codex_subscription_metadata(account_id, &auth)
                .await
        {
            metadata_resolved = true;
            if !metadata.account_id.is_empty() {
                auth.account_id.clone_from(&metadata.account_id);
                verified_upstream_account_id = metadata.account_id.trim().to_string();
            }
            if !metadata.raw_plan_type.is_empty() {
                plan_raw.clone_from(&metadata.raw_plan_type);
            }
            if !metadata.plan_family.is_empty() && metadata.plan_family != "unknown" {
                family.clone_from(&metadata.plan_family);
            }
            if plan_limit.is_empty() {
                plan_limit.clone_from(&metadata.plan_limit);
            }
            if (!metadata.organization_name.is_empty() && plan::organization_scoped_plan(&family))
                || organization_name.is_empty()
            {
                organization_name.clone_from(&metadata.organization_name);
            }
        }
        if plan_limit.is_empty() && !metadata_resolved {
            plan_limit = stored_plan_limit;
        }
        if !family.is_empty() && family != "unknown" && !plan::organization_scoped_plan(&family) {
            organization_name.clear();
        }
        if !expected_upstream_account_id.is_empty()
            && verified_upstream_account_id != expected_upstream_account_id
        {
            // Checked after subscription/workspace enrichment but before any
            // config or runtime mutation, so a visitor cannot transfer a slot
            // or its history.
            return Err(RefreshError::RepairIdentityMismatch);
        }

        let mut inner = self.write();
        let Some(index) = inner.account_index(account_id) else {
            return Err(failed("account no longer exists"));
        };
        let prior = inner
            .state
            .quotas()
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        let current = quota::merge_sparse_quota(prior.quota.as_ref(), current, &usage);
        let mut snapshot = QuotaSnapshot {
            account_id: account_id.to_string(),
            organization_name: prior.organization_name.clone(),
            raw_plan_type: prior.raw_plan_type.clone(),
            plan_family: prior.plan_family.clone(),
            plan_type: prior.plan_type.clone(),
            plan_limit: prior.plan_limit.clone(),
            seat_type: prior.seat_type.clone(),
            seat_type_raw: prior.seat_type_raw.clone(),
            seat_type_inferred: prior.seat_type_inferred,
            quota_policy: prior.quota_policy,
            quota: Some(current.clone()),
            observed_at: now,
            last_successful_refresh_at: now,
            usage_updated_at: now,
            freshness: "fresh".to_string(),
            provenance: "chatgpt_wham_usage".to_string(),
            ..QuotaSnapshot::default()
        };
        let item = &mut inner.config.accounts_mut()[index];
        if snapshot.raw_plan_type.is_empty() {
            snapshot.raw_plan_type = plan::clean_raw_plan_type(&item.raw_plan_type);
        }
        if snapshot.plan_family.is_empty() {
            snapshot.plan_family = plan::effective_plan_family(item);
        }
        if snapshot.plan_type.is_empty() {
            snapshot.plan_type.clone_from(&snapshot.plan_family);
        }
        if snapshot.plan_limit.is_empty() {
            snapshot.plan_limit = plan::clean_plan_limit(&item.plan_limit);
        }
        if snapshot.organization_name.is_empty() {
            snapshot.organization_name = plan::clean_organization_name(&item.organization_name);
        }
        if !plan_raw.is_empty() {
            snapshot.raw_plan_type.clone_from(&plan_raw);
            snapshot.plan_family.clone_from(&family);
            snapshot.plan_type.clone_from(&family);
            // A generic Pro plan is not multiplier evidence. An exact
            // multiplier may still arrive from its own supported field in this
            // same full refresh.
            snapshot.plan_limit.clone_from(&plan_limit);
            item.raw_plan_type.clone_from(&plan_raw);
            item.plan_family.clone_from(&family);
            item.plan_type.clone_from(&family);
            item.plan_rank = plan::plan_rank(&family);
        } else if !plan_limit.is_empty() {
            snapshot.plan_limit.clone_from(&plan_limit);
        }
        quota::apply_business_seat_inference(&mut snapshot, &current);
        item.plan_limit.clone_from(&snapshot.plan_limit);
        item.seat_type.clone_from(&snapshot.seat_type);
        item.seat_type_raw.clone_from(&snapshot.seat_type_raw);
        item.seat_type_inferred = snapshot.seat_type_inferred;
        item.quota_policy.clone_from(&snapshot.quota_policy);
        if !organization_name.is_empty()
            || metadata_resolved
            || (!plan_raw.is_empty() && !plan::organization_scoped_plan(&family))
        {
            snapshot.organization_name.clone_from(&organization_name);
        }
        if !auth.email.is_empty() {
            item.email = plan::normalize_email(&auth.email);
        }
        if !auth.account_id.is_empty() {
            item.account_id.clone_from(&auth.account_id);
        }
        if plan::organization_scoped_plan(&snapshot.plan_family) {
            item.organization_name
                .clone_from(&snapshot.organization_name);
        } else {
            item.organization_name.clear();
            snapshot.organization_name.clear();
        }
        item.label = plan::management_credential_display_name(item);
        let has_window = quota::quota_reported_windows(&current)
            .iter()
            .any(|window| window.present);
        item.remaining_quota = if has_window {
            Some(quota::remaining_quota_hint(&current))
        } else if current.exhausted {
            Some(0)
        } else {
            None
        };
        item.updated_at = now;
        inner
            .state
            .quotas_mut()
            .insert(account_id.to_string(), snapshot.clone());
        inner.save().map_err(|error| failed(error.to_string()))?;
        Ok(snapshot)
    }

    pub fn save_quota_error(&self, account_id: &str, code: &str, message: &str) {
        let mut inner = self.write();
        if inner.account(account_id).is_none() {
            return;
        }
        let mut prior = inner
            .state
            .quotas()
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        prior.account_id = account_id.to_string();
        prior.quota_error = Some(QuotaErrorInfo {
            code: code.to_string(),
            message: message.to_string(),
            timestamp: Timestamp::now(),
            ..QuotaErrorInfo::default()
        });
        inner
            .state
            .quotas_mut()
            .insert(account_id.to_string(), prior);
        let _ = inner.save();
    }

    /// The workspace and plan the credential acts as (backend-client
    /// client.rs get_accounts_check).
    pub async fn fetch_codex_subscription_metadata(
        &self,
        account_id: &str,
        auth: &CodexAuthInfo,
    ) -> Result<SubscriptionMetadata, String> {
        if auth.access_token.trim().is_empty() {
            return Err("codex access token is missing".to_string());
        }
        let call = OwnCall::AccountsCheck(self.codex_backend_call(
            account_id,
            self.settings.codex_accounts_check_url(),
            auth,
        ));
        let response = self
            .send_own_call(&call)
            .await
            .map_err(|_| "accounts/check request failed".to_string())?;
        if !response.status.is_success() {
            return Err(format!(
                "accounts/check returned status {}",
                response.status.as_u16()
            ));
        }
        let body = SseReader::new(response.body)
            .read_all(MAX_REQUEST_BODY)
            .await?;
        let payload: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        super::metadata::subscription_metadata_from_value(&payload, &auth.account_id)
            .ok_or_else(|| "accounts/check did not include account metadata".to_string())
    }

    pub async fn enrich_reset_credit_expiration(
        &self,
        account_id: &str,
        auth: &CodexAuthInfo,
        quota: &mut AccountQuota,
        prior: Option<&QuotaResetCredits>,
        now: Timestamp,
    ) {
        let Some(current) = quota.reset_credits.as_mut() else {
            return;
        };
        let Some(count) = current.available_count.filter(|count| *count > 0) else {
            return;
        };
        let same_count = prior.is_some_and(|prior| prior.available_count == Some(count));
        let prior_future = prior
            .and_then(|prior| prior.expires_at)
            .filter(|expires_at| *expires_at > now.unix());
        let details_fresh = same_count
            && prior.is_some_and(|prior| {
                !prior.details_observed_at.is_zero()
                    && now.sub(&prior.details_observed_at)
                        < hours(RESET_CREDIT_DETAILS_REFRESH_HOURS)
            })
            && prior_future.is_some();
        if details_fresh {
            current.expires_at = prior_future;
            current.details_observed_at = prior
                .map(|prior| prior.details_observed_at)
                .unwrap_or_default();
            return;
        }
        match self
            .fetch_codex_reset_credit_expiration(account_id, auth, now)
            .await
        {
            Ok(expires_at) => {
                current.expires_at = expires_at;
                current.details_observed_at = now;
            }
            // Expiry lookup is optional display metadata. A transient failure
            // must not fail quota refresh or affect routing; retain a
            // still-future date and retry on the next ordinary refresh instead
            // of creating a new polling loop.
            Err(_) if same_count && prior_future.is_some() => {
                current.expires_at = prior_future;
                current.details_observed_at = prior
                    .map(|prior| prior.details_observed_at)
                    .unwrap_or_default();
            }
            Err(_) => {}
        }
    }

    async fn fetch_codex_reset_credit_expiration(
        &self,
        account_id: &str,
        auth: &CodexAuthInfo,
        now: Timestamp,
    ) -> Result<Option<i64>, String> {
        // Codex's own listing (backend-client rate_limit_resets.rs
        // list_rate_limit_reset_credits) with the backend-client headers.
        let call = OwnCall::ResetCredits(self.codex_backend_call(
            account_id,
            self.settings.codex_reset_credits_url(),
            auth,
        ));
        let response = self
            .send_own_call(&call)
            .await
            .map_err(|_| "fetch reset-credit details failed".to_string())?;
        if !response.status.is_success() {
            return Err(format!(
                "reset-credit details returned status {}",
                response.status.as_u16()
            ));
        }
        let body = SseReader::new(response.body)
            .read_all(MAX_REQUEST_BODY)
            .await?;
        let details: ResetCreditDetailsResponse = serde_json::from_slice(&body)
            .map_err(|error| format!("decode reset-credit details: {error}"))?;
        Ok(quota::nearest_available_reset_credit_expiration(
            &details, now,
        ))
    }
}

/// Go chooseString: the value unless it is blank, else the fallback.
fn choose(value: Option<&str>, fallback: Option<&str>) -> String {
    match value {
        Some(value) if !value.trim().is_empty() => value.to_string(),
        _ => fallback.unwrap_or_default().to_string(),
    }
}

impl App {
    /// The accounts the poller reads: every Codex device-auth slot, in or out
    /// of the pool.
    pub fn quota_poll_accounts(&self) -> Vec<super::state::Account> {
        self.read()
            .config
            .accounts()
            .iter()
            .filter(|item| plan::is_codex_device_auth(item))
            .cloned()
            .collect()
    }

    /// Polls every account once, in order. The background poll does not use
    /// it: it asks one account at a time on its own schedule.
    pub async fn refresh_all_codex_quotas(&self) {
        for item in self.quota_poll_accounts() {
            self.poll_account_quota(&item).await;
        }
    }

    pub async fn poll_account_quota(&self, item: &super::state::Account) {
        // Upstream refused these tokens for good; asking again with them is a
        // request a native Codex would not send (credentials.rs).
        if self.credential_refused(item) {
            return;
        }
        match self.refresh_account_quota(&item.id).await {
            Ok(snapshot) => {
                self.prime_unanchored_quota_window(&item.id, &snapshot)
                    .await
            }
            Err(error) => self
                .logger
                .printf(&format!("quota refresh skipped for {}: {error}", item.id)),
        }
    }

    /// The background poll (Go startQuotaRefresher): one account at a time,
    /// each on its own randomized cadence (quota_schedule.rs), never a fixed
    /// beat a backend could recognize. It stops when `stop` is cancelled.
    pub fn start_quota_refresher(
        self: &std::sync::Arc<Self>,
        stop: super::http_io::Cancel,
    ) -> tokio::task::JoinHandle<()> {
        let app = self.clone();
        tokio::spawn(async move {
            let mut schedule = super::quota_schedule::QuotaPollSchedule::new(|| {
                f64::from(u32::from_le_bytes(super::crypto::random_bytes::<4>()))
                    / f64::from(u32::MAX)
            });
            loop {
                let accounts = app.quota_poll_accounts();
                let ids: Vec<String> = accounts.iter().map(|item| item.id.clone()).collect();
                let (instant, now) = (std::time::Instant::now(), Timestamp::now());
                if let Some(id) = schedule.due(instant, &ids, |id| app.account_in_use(id, now)) {
                    if let Some(item) = accounts.iter().find(|item| item.id == id) {
                        app.poll_account_quota(item).await;
                    }
                    schedule.polled(&id, std::time::Instant::now());
                    if let Some(at) = app.prime_grace_due(&id) {
                        let wait = at.sub(&Timestamp::now()).to_std().unwrap_or_default();
                        schedule.poll_by(&id, std::time::Instant::now() + wait);
                    }
                }
                if !stop.sleep(schedule.pause()).await {
                    return;
                }
            }
        })
    }
}
