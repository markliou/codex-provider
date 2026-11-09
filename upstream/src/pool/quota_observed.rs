//! Keeping an account's quota current when other clients spend it too.
//!
//! The owner's accounts can serve more than one pool at once (one person on
//! two projects), besides their owners' own Codex. What those spend shows up
//! here only in upstream's figures, so the pool reads them three ways (SPEC.md
//! 9.9):
//!
//! - from every response, as a native Codex does, at no extra request
//!   (`note_rate_limit_headers`, `note_rate_limit_event`; quota.rs
//!   `apply_rate_limit_snapshots`);
//! - once just before sending to an account whose figures are stale
//!   (`select_account_checked`), so routing does not pick an account
//!   another pool drained on figures from hours ago;
//! - by the background poll, which also asks idle accounts now
//!   (quota_schedule.rs).
//!
//! The second and third are the pool's own usage calls (own_call.rs). Without
//! the second, a drained account was found out only by upstream refusing a
//! turn with `usage_limit_reached`, after which the request failed over.

use super::app::App;
use super::plan;
use super::quota;
use super::routing::RoutingDecision;
use super::routing::SelectError;
use super::state::Account;
use super::time::Timestamp;
use super::time::minutes;
use http::HeaderMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// How long a request waits for the usage read before it goes on with the
/// figures it has (`Tuning::placement_check_wait`, the default). The read
/// itself keeps running (it may be rotating the account's token, which must
/// never be cut off).
pub const PLACEMENT_CHECK_WAIT: Duration = Duration::from_secs(5);
/// Reads per request: a request whose candidates are all stale reads at most
/// this many before it is sent on the figures it has.
pub const PLACEMENT_CHECKS_PER_REQUEST: usize = 2;

impl App {
    /// The `x-codex-*` rate-limit headers of a Codex backend response
    /// (codex-api sse/responses.rs, `parse_all_rate_limits` on every
    /// response; api_bridge.rs reads the same family on a 429).
    pub fn note_rate_limit_headers(&self, account_id: &str, headers: &HeaderMap) {
        let snapshots = crate::rate_limits::parse_all_rate_limits(headers);
        self.note_rate_limits(account_id, &snapshots);
    }

    /// A WebSocket event; only `codex.rate_limits` is read
    /// (endpoint/responses_websocket.rs, `parse_rate_limit_event`).
    pub fn note_rate_limit_event(&self, account_id: &str, message: &[u8]) {
        let Ok(text) = std::str::from_utf8(message) else {
            return;
        };
        // Every event passes here; only this one is parsed.
        if !text.contains("codex.rate_limits") {
            return;
        }
        if let Some(snapshot) = crate::rate_limits::parse_rate_limit_event(text) {
            self.note_rate_limits(account_id, &[snapshot]);
        }
    }

    fn note_rate_limits(
        &self,
        account_id: &str,
        snapshots: &[codex_protocol::protocol::RateLimitSnapshot],
    ) {
        if snapshots.is_empty() {
            return;
        }
        let now = Timestamp::now();
        let mut inner = self.write();
        let Some(index) = inner.account_index(account_id) else {
            return;
        };
        if !plan::is_codex_device_auth(&inner.config.accounts()[index]) {
            return;
        }
        let mut snapshot = inner.quota_snapshot(account_id);
        let Some(current) =
            quota::apply_rate_limit_snapshots(snapshot.quota.as_ref(), snapshots, now)
        else {
            return;
        };
        let changed = snapshot.quota.as_ref().is_none_or(|prior| {
            prior.windows != current.windows
                || prior.additional_limits != current.additional_limits
                || prior.credits != current.credits
        });
        let has_window = quota::quota_reported_windows(&current)
            .iter()
            .any(|window| window.present);
        if has_window {
            inner.config.accounts_mut()[index].remaining_quota =
                Some(quota::remaining_quota_hint(&current));
        }
        snapshot.account_id = account_id.to_string();
        snapshot.quota = Some(current);
        snapshot.observed_at = now;
        snapshot.usage_updated_at = now;
        inner
            .state
            .quotas_mut()
            .insert(account_id.to_string(), snapshot);
        // The figures repeat on every response; the file is written only when
        // they moved. A restart then loses at most the freshness instant.
        if changed {
            let _ = inner.save();
        }
    }

    /// Selects the account for a route, first reading the usage of a
    /// candidate whose figures are stale and selecting again on what came
    /// back. Every place that picks an account for a request goes through
    /// here, so a stale account is never sent to unread.
    pub async fn select_account_checked(
        self: &Arc<Self>,
        route: &mut RoutingDecision,
        model: &str,
        excluded: &HashMap<String, bool>,
    ) -> Result<Account, SelectError> {
        let wait = self.tuning.placement_check_wait;
        let mut checks = 0;
        loop {
            let candidate = self.select_account_for_route(route, model, excluded)?;
            if wait.is_zero()
                || checks >= PLACEMENT_CHECKS_PER_REQUEST
                || !self.claim_placement_check(&candidate)
            {
                return Ok(candidate);
            }
            checks += 1;
            let app = self.clone();
            let id = candidate.id.clone();
            let read = tokio::spawn(async move {
                let result = app.refresh_account_quota(&id).await;
                app.note_quota_read_elsewhere(&id);
                if let Err(error) = result {
                    app.logger
                        .printf(&format!("usage read before use failed for {id}: {error}"));
                }
            });
            let _ = tokio::time::timeout(wait, read).await;
        }
    }

    /// Whether to read this account's usage before sending to it now, and
    /// if so, records that it is being read. Only a Codex sign-in the poll
    /// would read, whose figures are older than the freshness window, and
    /// that was not read this way within that window, so an account whose
    /// read fails is not asked again on every request.
    pub fn claim_placement_check(&self, item: &Account) -> bool {
        if !self.upstream_enabled() || self.credential_refused(item) {
            return false;
        }
        let now = Timestamp::now();
        {
            let inner = self.read();
            if !plan::is_codex_device_auth(item) || inner.pi_only(item) {
                return false;
            }
            let snapshot = inner.quota_snapshot(&item.id);
            if !matches!(
                quota::quota_telemetry_state(&snapshot, now),
                "stale" | "not_reported"
            ) {
                return false;
            }
        }
        let mut checks = self
            .placement_checks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let window = minutes(quota::QUOTA_TELEMETRY_FRESHNESS_MINUTES);
        if checks
            .get(&item.id)
            .is_some_and(|last| now.sub(last) < window)
        {
            return false;
        }
        checks.retain(|_, last| now.sub(last) < window);
        checks.insert(item.id.clone(), now);
        true
    }

    /// Lets the background poll count a read made elsewhere as its own
    /// poll, so the account is not asked again right after.
    pub fn note_quota_read_elsewhere(&self, account_id: &str) {
        self.quota_reads_elsewhere
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((account_id.to_string(), std::time::Instant::now()));
    }

    pub fn take_quota_reads_elsewhere(&self) -> Vec<(String, std::time::Instant)> {
        std::mem::take(
            &mut *self
                .quota_reads_elsewhere
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}
