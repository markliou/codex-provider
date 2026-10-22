//! Spending a reset credit the way a native Codex spends one.
//!
//! A port of reset_credit.go and the quota/reset-credit account action. A
//! native Codex resets from its /usage picker (tui chatwidget/usage.rs and
//! reset_credits.rs, through app-server account_processor/rate_limit_resets.rs
//! and backend-client rate_limit_resets.rs): it lists the account's credits,
//! offers the available ones soonest-expiring first with the first selected,
//! and spends the chosen one by id under a fresh idempotency key. A failed
//! attempt's "Try again" resends the same key and the same credit, so a spend
//! that went through but lost its answer comes back already_redeemed instead
//! of spending a second credit. The pool once let upstream pick the credit and
//! minted a new key on every click: a credit about to expire could outlive the
//! reset meant to use it, and a retried click could spend twice.

use super::app::App;
use super::http_io::ResponseWriter;
use super::http_io::write_json;
use super::http_io::write_openai_error;
use super::metadata::CodexAuthInfo;
use super::own_call::OwnCall;
use super::stream::MAX_REQUEST_BODY;
use super::stream::SseReader;
use super::time::Timestamp;
use super::time::minutes;
use http::StatusCode;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// app-server rate_limit_resets.rs RATE_LIMIT_RESET_DETAILS_REQUEST_TIMEOUT
/// and RATE_LIMIT_RESET_REQUEST_TIMEOUT.
const RESET_CREDIT_DETAILS_TIMEOUT: Duration = Duration::from_secs(5);
const RESET_CREDIT_CONSUME_TIMEOUT: Duration = Duration::from_secs(10);
const QUOTA_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a failed attempt stays the one a retry resends. Codex keeps it
/// while its "Try again" prompt is open; the pool has no prompt, so it keeps
/// it for a while an operator would plausibly still be retrying.
const RESET_CREDIT_RETRY_WINDOW_MINUTES: i64 = 10;

/// A spend that failed without an upstream verdict. The credit id is
/// upstream's identifier: it lives in memory for the retry only, never in
/// runtime state, a log line or a response.
#[derive(Clone, Debug, Default)]
pub struct ResetCreditAttempt {
    pub redeem_request_id: String,
    pub credit_id: String,
    pub at: Timestamp,
}

#[derive(Default)]
pub struct ResetCreditAttempts {
    pending: Mutex<HashMap<String, ResetCreditAttempt>>,
}

impl ResetCreditAttempts {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, ResetCreditAttempt>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The failed attempt a retry on this account must resend.
    pub fn resume(&self, account_id: &str, now: Timestamp) -> Option<ResetCreditAttempt> {
        let mut pending = self.pending();
        match pending.get(account_id) {
            Some(attempt) if now.sub(&attempt.at) <= minutes(RESET_CREDIT_RETRY_WINDOW_MINUTES) => {
                Some(attempt.clone())
            }
            _ => {
                pending.remove(account_id);
                None
            }
        }
    }

    pub fn keep(&self, account_id: &str, attempt: ResetCreditAttempt) {
        self.pending().insert(account_id.to_string(), attempt);
    }

    pub fn clear(&self, account_id: &str) {
        self.pending().remove(account_id);
    }
}

/// backend-client types.rs RateLimitResetCreditDetails as far as the picker
/// reads it. The id is decoded only here, to spend the credit, and goes no
/// further than the consume request.
#[derive(Debug, Default, Deserialize)]
pub struct ResetCreditListing {
    #[serde(default)]
    pub available_count: i64,
    #[serde(default)]
    pub credits: Vec<ListedCredit>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ListedCredit {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub granted_at: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Go's time.Parse(time.RFC3339, ...): an RFC 3339 instant.
fn rfc3339_unix(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.timestamp())
}

/// The credit a native Codex's picker selects first: available credits sorted
/// by expiry, a credit without one last, ties in upstream's order (tui
/// reset_credits.rs reset_credit_options, a stable sort). "" means Codex would
/// spend without naming a credit: the listing failed, or a timestamp in it did
/// not parse (app-server rejects the whole listing then), or it offers nothing,
/// and the picker falls back to an option without an id.
pub fn native_reset_credit_choice(listing: &ResetCreditListing) -> String {
    let mut available: Vec<(&str, i64)> = Vec::new();
    for credit in &listing.credits {
        if rfc3339_unix(&credit.granted_at).is_none() {
            return String::new();
        }
        let expires = match &credit.expires_at {
            Some(value) => match rfc3339_unix(value) {
                Some(at) => at,
                None => return String::new(),
            },
            None => i64::MAX,
        };
        if credit.status == "available" {
            available.push((&credit.id, expires));
        }
    }
    // The picker shows at most available_count credits.
    if listing.available_count <= 0 || available.is_empty() {
        return String::new();
    }
    available.sort_by_key(|(_, expires)| *expires);
    available[0].0.to_string()
}

/// The picker's answer to no_credit for a named credit (tui
/// chatwidget/usage.rs finish_rate_limit_reset_consume).
pub fn reset_credit_no_longer_available(outcome: &str, credit_id: &str) -> bool {
    matches!(
        outcome.trim().to_ascii_lowercase().as_str(),
        "no_credit" | "nocredit" | "nocreditsavailable" | "nocredits"
    ) && !credit_id.is_empty()
}

impl App {
    /// Lists the account's credits as the picker does before a spend, with
    /// the picker's timeout, and returns the credit it would select.
    pub async fn choose_reset_credit(&self, account_id: &str, auth: &CodexAuthInfo) -> String {
        let call = OwnCall::ResetCredits(self.codex_backend_call(
            account_id,
            self.settings.codex_reset_credits_url(),
            auth,
        ));
        let listed = tokio::time::timeout(RESET_CREDIT_DETAILS_TIMEOUT, async {
            let response = self.send_own_call(&call).await.ok()?;
            if !response.status.is_success() {
                return None;
            }
            let body = SseReader::new(response.body)
                .read_all(MAX_REQUEST_BODY)
                .await
                .ok()?;
            serde_json::from_slice::<ResetCreditListing>(&body).ok()
        })
        .await;
        match listed {
            Ok(Some(listing)) => native_reset_credit_choice(&listing),
            _ => String::new(),
        }
    }

    /// Spends one earned reset credit on an account. This is irreversible and
    /// costs real entitlement, so it is never automatic: only an explicit
    /// operator action reaches here.
    ///
    /// The redeem request id is generated per attempt and reused for nothing
    /// else. A retry of the same logical attempt must carry the same id or
    /// upstream would spend a second credit, so the caller owns it.
    pub async fn consume_codex_reset_credit(
        &self,
        account_id: &str,
        auth: &CodexAuthInfo,
        redeem_request_id: &str,
        credit_id: &str,
    ) -> Result<String, String> {
        // Upstream reads this key as redeem_request_id and requires it; credit_id
        // names the credit Codex's picker selected and is left out only where
        // Codex leaves it out. Codex's own redemption (backend-client
        // rate_limit_resets.rs consume_rate_limit_reset_credit_request).
        let call = OwnCall::ConsumeResetCredit {
            backend: self.codex_backend_call(
                account_id,
                self.settings.codex_reset_credit_consume_url(),
                auth,
            ),
            redeem_request_id: redeem_request_id.to_string(),
            credit_id: Some(credit_id.to_string()).filter(|id| !id.is_empty()),
        };
        let response = self
            .send_own_call(&call)
            .await
            .map_err(|error| format!("consume reset credit: {error}"))?;
        let status = response.status;
        let payload = SseReader::new(response.body)
            .read_all(MAX_REQUEST_BODY)
            .await
            .map_err(|error| format!("read reset-credit consume response: {error}"))?;
        if !status.is_success() {
            // Report only the sanitized upstream code. The raw body may carry
            // account or token detail that must not reach the admin surface.
            // The log keeps upstream's own explanation, sanitized and bounded:
            // a bare status cannot say which part of the request upstream
            // refused.
            let detail = super::quota::upstream_error_detail(&payload);
            if !detail.is_empty() {
                self.logger.printf(&format!(
                    "reset credit consume rejected with status {}: {detail}",
                    status.as_u16()
                ));
            }
            let code = super::quota::extract_upstream_error_code(&payload);
            if !code.is_empty() {
                return Err(format!(
                    "upstream returned status {} [{code}]",
                    status.as_u16()
                ));
            }
            return Err(format!("upstream returned status {}", status.as_u16()));
        }
        // Upstream names its verdict "code". "outcome" is the Codex client's
        // own spelling for the same enum on its JSON-RPC surface, and reading
        // that name here would leave every verdict empty and unrecognized.
        #[derive(Deserialize)]
        struct Decoded {
            #[serde(default)]
            code: String,
        }
        let decoded: Decoded = serde_json::from_slice(&payload)
            .map_err(|error| format!("decode reset-credit consume response: {error}"))?;
        Ok(super::plan::clean_metadata_token(&decoded.code))
    }
}

/// The quota/reset-credit account action. Spending a credit is irreversible
/// and consumes real entitlement, so it stays an explicit management action:
/// never on the public dashboard, never automatic, and refused outright when
/// the account has none.
pub async fn handle_reset_credit(app: &App, id: &str, writer: &mut dyn ResponseWriter) {
    let (item, available) = {
        let inner = app.read();
        let Some(item) = inner.account(id).cloned() else {
            write_openai_error(
                writer,
                StatusCode::NOT_FOUND,
                "not_found",
                "account not found",
            );
            return;
        };
        let available = inner
            .state
            .quotas()
            .get(&item.id)
            .and_then(|snapshot| snapshot.quota.as_ref())
            .and_then(|quota| quota.reset_credits.as_ref())
            .and_then(|credits| credits.available_count)
            .unwrap_or_default();
        (item, available)
    };
    if !super::plan::is_codex_device_auth(&item) {
        write_openai_error(
            writer,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "reset credits are only available for Codex accounts",
        );
        return;
    }
    if available <= 0 {
        write_openai_error(
            writer,
            StatusCode::CONFLICT,
            "no_reset_credits",
            "this account has no earned reset credits available",
        );
        return;
    }
    // One deadline covers the sign-in, the listing and the spend, as Go's
    // request context did; the listing and the spend also keep the picker's
    // own shorter timeouts.
    let deadline = tokio::time::Instant::now() + QUOTA_REFRESH_TIMEOUT;
    let Ok(Ok(auth)) = tokio::time::timeout_at(deadline, app.refreshed_codex_auth(&item)).await
    else {
        write_openai_error(
            writer,
            StatusCode::BAD_GATEWAY,
            "account_auth_failed",
            "unable to authenticate this account with upstream",
        );
        return;
    };
    // A retry after a failed attempt resends that attempt, as Codex's "Try
    // again" does; otherwise the credit is the one Codex's picker selects,
    // under a fresh key.
    let resumed = app.reset_attempts.resume(&item.id, Timestamp::now());
    let retrying = resumed.is_some();
    let mut attempt = match resumed {
        Some(attempt) => attempt,
        None => {
            let credit_id =
                tokio::time::timeout_at(deadline, app.choose_reset_credit(&item.id, &auth))
                    .await
                    .unwrap_or_default();
            ResetCreditAttempt {
                redeem_request_id: super::crypto::random_uuid(),
                credit_id,
                at: Timestamp::now(),
            }
        }
    };
    attempt.at = Timestamp::now();
    let consume_deadline = deadline.min(tokio::time::Instant::now() + RESET_CREDIT_CONSUME_TIMEOUT);
    let consumed = tokio::time::timeout_at(
        consume_deadline,
        app.consume_codex_reset_credit(
            &item.id,
            &auth,
            &attempt.redeem_request_id,
            &attempt.credit_id,
        ),
    )
    .await
    .unwrap_or_else(|_| Err("consume reset credit: context deadline exceeded".to_string()));
    let outcome = match consumed {
        Ok(outcome) => outcome,
        Err(error) => {
            app.reset_attempts.keep(&item.id, attempt);
            app.logger.printf(&format!(
                "reset credit consume failed for {} (retry={retrying}): {error}",
                item.id
            ));
            write_openai_error(
                writer,
                StatusCode::BAD_GATEWAY,
                "reset_credit_failed",
                &format!(
                    "{error}; trying again resends the same reset, so it cannot spend a second credit"
                ),
            );
            return;
        }
    };
    // Any verdict ends the attempt: the next spend starts from the picker.
    app.reset_attempts.clear(&item.id);
    let (mut message, consumed) = super::quota::reset_credit_outcome_message(&outcome);
    if reset_credit_no_longer_available(&outcome, &attempt.credit_id) {
        message = "That reset is no longer available. Refresh to see your current resets.";
    }
    // Refresh regardless of outcome. A successful reset changes the windows
    // the dashboard shows, and a refusal such as already_redeemed means the
    // local snapshot is the stale half of the disagreement.
    let snapshot = match app.refresh_account_quota(&item.id).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            app.logger.printf(&format!(
                "quota refresh after reset credit failed for {}: {error}",
                item.id
            ));
            super::state::QuotaSnapshot::default()
        }
    };
    let snapshot = super::quota::quota_snapshot_for_display(&snapshot, Timestamp::now());
    write_json(
        writer,
        StatusCode::OK,
        &serde_json::json!({
            "ok": true,
            "accountId": id,
            "outcome": outcome,
            "consumed": consumed,
            "message": message,
            "quota": super::status::to_value(&snapshot.quota),
        }),
    );
}

/// The quota/refresh account action.
pub async fn handle_quota_refresh(app: &App, id: &str, writer: &mut dyn ResponseWriter) {
    let Some(account_id) = app.read().account(id).map(|item| item.id.clone()) else {
        write_openai_error(
            writer,
            StatusCode::NOT_FOUND,
            "not_found",
            "account not found",
        );
        return;
    };
    let snapshot = match app.refresh_account_quota(&account_id).await {
        Ok(snapshot) => super::quota::quota_snapshot_for_display(&snapshot, Timestamp::now()),
        Err(error) => {
            write_openai_error(
                writer,
                StatusCode::BAD_GATEWAY,
                "quota_refresh_failed",
                &error.to_string(),
            );
            return;
        }
    };
    write_json(
        writer,
        StatusCode::OK,
        &serde_json::json!({
            "ok": true,
            "accountId": id,
            "quota": super::status::to_value(&snapshot.quota),
            "organizationName": super::plan::public_organization_name(&snapshot.organization_name),
            "rawPlanType": snapshot.raw_plan_type,
            "planFamily": snapshot.plan_family,
            "planType": snapshot.plan_type,
            "planLimit": snapshot.plan_limit,
            "seatType": snapshot.seat_type,
            "seatTypeRaw": snapshot.seat_type_raw,
            "quotaPolicy": snapshot.quota_policy,
            "usageUpdatedAt": snapshot.usage_updated_at,
            "lastSuccessfulRefreshAt": snapshot.last_successful_refresh_at,
            "freshness": snapshot.freshness,
            "provenance": snapshot.provenance,
        }),
    );
}
