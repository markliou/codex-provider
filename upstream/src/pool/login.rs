//! Signing a slot in with the bundled Codex CLI's device-auth flow.
//!
//! A port of the login-job half of main.go (startLoginJobWithPolicyLocked,
//! runLoginJob and their helpers). `codex login --device-auth` writes the
//! slot's auth.json itself, so the credential is made by Codex as for any
//! native Codex. Around it the pool keeps a durable routing gate
//! (pending_auth_verification) from the moment a sign-in starts until the
//! resulting identity is verified: the CLI may already be rewriting
//! auth.json, and identity-scoped history (cache statistics, routes,
//! affinity) may only survive when the same upstream account signed in again.
//! One login runs at a time across the whole pool.

use super::app::App;
use super::app::Inner;
use super::app::LoginJob;
use super::app::login_job_active_status;
use super::http_io::Cancel;
use super::metadata::CodexAuthInfo;
use super::plan;
use super::quota_refresh::RefreshError;
use super::state::Account;
use super::time::Timestamp;
use super::time::minutes;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::io::AsyncBufReadExt;

/// Why a sign-in could not start.
#[derive(Debug, PartialEq)]
pub enum LoginStartError {
    /// Another sign-in is running somewhere in the pool, or an owner's job is
    /// running for the slot a public repair asked for.
    AnotherJobInProgress,
    /// A public repair without a previously verified upstream account.
    RepairIdentityMismatch,
    Storage(String),
    AccountGone,
    /// The upstream switch is off: a sign-in is the bundled Codex talking to
    /// OpenAI, so it is not started (SPEC.md, upstream switch).
    UpstreamClosed,
}

impl std::fmt::Display for LoginStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AnotherJobInProgress => {
                f.write_str("another Codex device auth login is already in progress")
            }
            Self::RepairIdentityMismatch => {
                f.write_str("public sign-in repair identity did not match the existing account")
            }
            Self::Storage(detail) => write!(f, "persist pending auth verification: {detail}"),
            Self::AccountGone => f.write_str("account no longer exists"),
            Self::UpstreamClosed => f.write_str(crate::transport::UPSTREAM_CLOSED),
        }
    }
}

/// The cancel handle of each running sign-in.
#[derive(Default)]
pub struct LoginCancels {
    cancels: Mutex<HashMap<String, Cancel>>,
}

impl LoginCancels {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Cancel>> {
        self.cancels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub fn login_job_cancellation_requested_status(status: &str) -> bool {
    status == "cancelling" || status == "cancelled"
}

/// A slot that has signed in before, or whose metadata says so.
pub fn account_has_prior_identity(item: &Account) -> bool {
    !item.account_id.trim().is_empty()
        || !plan::normalize_email(&item.email).is_empty()
        || !plan::clean_organization_name(&item.organization_name).is_empty()
        || !item.last_login_at.is_zero()
}

/// A repair may inherit identity-scoped metrics and routes only when the
/// durable upstream account id is reproduced exactly. Email and display
/// organization are intentionally not fallback identity keys: one user can
/// access several ChatGPT workspaces, so an unverifiable legacy slot resets
/// once rather than risk crossing an account boundary.
pub fn reauthentication_changed_upstream_identity(
    auth: &CodexAuthInfo,
    expected_upstream_account_id: &str,
    reauthentication: bool,
) -> bool {
    if !reauthentication {
        return false;
    }
    let previous = expected_upstream_account_id.trim();
    let current = auth.account_id.trim();
    previous.is_empty() || current.is_empty() || current != previous
}

/// What the sign-in child may see: its home, PATH, and the proxy and CA
/// settings every outbound client gets. Never the pool's API keys, admin
/// hash or upstream keys.
pub fn codex_login_env(
    codex_home: &Path,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let account_home = codex_home.parent().unwrap_or(codex_home);
    let mut env = vec![
        ("CODEX_HOME".to_string(), codex_home.display().to_string()),
        ("HOME".to_string(), account_home.display().to_string()),
        (
            "PATH".to_string(),
            lookup("PATH")
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| {
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()
                }),
        ),
    ];
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CODEX_CA_CERTIFICATE",
        "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
        "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
    ] {
        if let Some(value) = lookup(name).filter(|value| !value.is_empty()) {
            env.push((name.to_string(), value));
        }
    }
    env
}

fn regex(pattern: &str) -> regex_lite::Regex {
    regex_lite::Regex::new(pattern)
        .unwrap_or_else(|error| panic!("login pattern {pattern}: {error}"))
}

pub fn strip_ansi(value: &str) -> String {
    regex(r"\x1b\[[0-9;]*[A-Za-z]")
        .replace_all(value, "")
        .into_owned()
}

/// The verification link and user code in the CLI's output.
pub fn parse_device_auth_prompt(output: &str) -> (String, String) {
    let url = regex(r"https?://[^\s]+");
    let code = regex(r"[A-Z0-9]{4,}(-[A-Z0-9]{4,})+");
    let (mut verification_url, mut user_code) = (String::new(), String::new());
    for line in output.split('\n') {
        let line = line.trim();
        if verification_url.is_empty()
            && let Some(found) = url.find(line)
        {
            verification_url = found.as_str().trim_end_matches(['.', ',', ')']).to_string();
        }
        if user_code.is_empty()
            && let Some(found) = code.find(line)
        {
            user_code = found.as_str().to_string();
        }
    }
    (verification_url, user_code)
}

/// The CLI's output as a job error: any line that looks like a credential is
/// dropped, and the rest is bounded.
pub fn redact_login_output(value: &str) -> String {
    let secretish = regex(
        r"(?i)(access[_ -]?token|refresh[_ -]?token|id[_ -]?token|authorization|bearer|api[_ -]?key|cookie|session[_ -]?cookie|CODEX_POOL_[A-Z0-9_]+)",
    );
    let jwt_like = regex(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}");
    let lines: Vec<&str> = value
        .split('\n')
        .map(|line| {
            if secretish.is_match(line) || jwt_like.is_match(line) {
                "[REDACTED]"
            } else {
                line
            }
        })
        .collect();
    let result = lines.join("\n").trim().to_string();
    const MAX: usize = 1200;
    if result.len() > MAX {
        let mut end = MAX;
        while !result.is_char_boundary(end) {
            end -= 1;
        }
        return format!("{}...", &result[..end]);
    }
    result
}

impl Inner {
    pub fn has_prior_identity(&self, item: &Account) -> bool {
        if account_has_prior_identity(item) {
            return true;
        }
        let id = item.id.as_str();
        let state = &self.state;
        state
            .prompt_cache()
            .values()
            .chain(state.prompt_cache_baseline().values())
            .any(|stat| stat.account_id == id)
            || state
                .sticky_sessions()
                .values()
                .any(|binding| binding.account_id == id)
            || state
                .response_bindings()
                .values()
                .any(|binding| binding.account_id == id)
            || state
                .thread_bindings()
                .values()
                .any(|binding| binding.account_id == id)
            || state
                .lineage_bindings()
                .values()
                .any(|binding| binding.account_id == id)
            || state
                .routing_cache_events()
                .iter()
                .any(|event| event.account_id == id || event.failover_from_account_id == id)
            || self
                .throughput_buckets
                .iter()
                .any(|bucket| bucket.account_id == id)
    }

    /// Applies a verified sign-in to the slot. Returns whether identity-scoped
    /// history was reset: metrics and route bindings belong to an upstream
    /// identity, not to the reusable local credential directory, so a
    /// different (or unverifiable) account starts clean, or the dashboard
    /// would attribute the old account's history to the new one and old
    /// threads could be sent across an account boundary.
    pub fn apply_completed_device_auth(
        &mut self,
        account_id: &str,
        auth: &CodexAuthInfo,
        expected_upstream_account_id: &str,
        reauthentication: bool,
        now: Timestamp,
    ) -> bool {
        let history_reset = reauthentication_changed_upstream_identity(
            auth,
            expected_upstream_account_id,
            reauthentication,
        );
        if history_reset {
            self.clear_account_identity_scoped_state(account_id);
        }
        let Some(index) = self.account_index(account_id) else {
            return history_reset;
        };
        let item = &mut self.config.accounts_mut()[index];
        if history_reset {
            item.email = plan::normalize_email(&auth.email);
            item.account_id = auth.account_id.trim().to_string();
            item.organization_name = plan::clean_organization_name(&auth.organization_name);
            item.raw_plan_type = plan::clean_raw_plan_type(&auth.plan_type);
            item.plan_family = plan::plan_family_from_raw(&item.raw_plan_type).to_string();
            item.plan_type.clone_from(&item.plan_family);
            item.plan_limit = plan::clean_plan_limit(&auth.plan_limit);
            item.plan_rank = plan::plan_rank(&item.plan_family);
            item.seat_type.clear();
            item.seat_type_raw.clear();
            item.seat_type_inferred = false;
            item.quota_policy.clear();
        } else {
            if !auth.email.is_empty() {
                item.email = plan::normalize_email(&auth.email);
            }
            if !auth.account_id.is_empty() {
                item.account_id = auth.account_id.trim().to_string();
            }
            if !auth.organization_name.is_empty() {
                item.organization_name = plan::clean_organization_name(&auth.organization_name);
            }
            if !auth.plan_type.is_empty() {
                item.raw_plan_type = plan::clean_raw_plan_type(&auth.plan_type);
                item.plan_family = plan::plan_family_from_raw(&item.raw_plan_type).to_string();
                item.plan_type.clone_from(&item.plan_family);
                item.plan_rank = plan::plan_rank(&item.plan_family);
            }
            if !auth.plan_limit.is_empty() {
                item.plan_limit = plan::clean_plan_limit(&auth.plan_limit);
            }
        }
        item.label = plan::management_credential_display_name(item);
        item.last_login_at = now;
        item.updated_at = now;
        self.clear_account_runtime_state(account_id);
        history_reset
    }

    pub fn latest_public_repair_job_for_account(&self, account_id: &str) -> Option<&LoginJob> {
        self.jobs
            .values()
            .filter(|job| job.account_id == account_id && job.public_repair)
            .fold(None, |latest: Option<&LoginJob>, job| match latest {
                Some(latest) if !job.started_at.after(&latest.started_at) => Some(latest),
                _ => Some(job),
            })
    }

    pub fn public_repair_job_active(&self, account_id: &str) -> bool {
        self.active_login_job_for_account(account_id)
            .is_some_and(|job| job.public_repair)
    }

    /// Whether the public dashboard may offer a sign-in repair. Public repair
    /// must prove continuity with a prior verified identity; empty or legacy
    /// identity metadata requires the authenticated owner.
    pub fn public_repair_available(&self, item: &Account) -> bool {
        if !plan::is_codex_device_auth(item) || !item.enabled || !item.in_pool {
            return false;
        }
        let expected = if item.pending_auth_verification {
            &item.pending_auth_expected_account_id
        } else {
            &item.account_id
        };
        if expected.trim().is_empty() {
            return false;
        }
        if let Some(job) = self.active_login_job_for_account(&item.id) {
            return job.public_repair;
        }
        if item.pending_auth_verification || self.codex_auth(item).is_err() {
            return true;
        }
        super::app::quota_error_blocks_routing(
            self.state
                .quotas()
                .get(&item.id)
                .and_then(|snapshot| snapshot.quota_error.as_ref()),
        )
    }

    pub fn login_job_status(&self, job_id: &str) -> String {
        self.jobs
            .get(job_id)
            .map(|job| job.status.clone())
            .unwrap_or_default()
    }

    fn update_login_job(
        &mut self,
        job_id: &str,
        status: &str,
        verification_url: &str,
        user_code: &str,
        message: &str,
    ) {
        let Some(job) = self.jobs.get_mut(job_id) else {
            return;
        };
        if login_job_cancellation_requested_status(&job.status)
            || job.status == "completed"
            || job.status == "failed"
        {
            return;
        }
        let now = Timestamp::now();
        job.status = status.to_string();
        if !verification_url.is_empty() {
            job.verification_url = verification_url.to_string();
        }
        if !user_code.is_empty() {
            job.user_code = user_code.to_string();
            if job.code_expires_at.is_zero() {
                job.code_expires_at = now.add(minutes(15));
            }
        }
        job.message = message.to_string();
        job.updated_at = now;
    }
}

/// The complete unauthenticated job projection. Raw job and account ids, CLI
/// output, internal errors and owner-originated login jobs stay private.
pub fn public_repair_job(job: &LoginJob) -> serde_json::Value {
    let mut result = serde_json::json!({
        "status": job.status,
        "reauthentication": job.reauthentication,
        "historyReset": job.history_reset,
    });
    if !job.verification_url.is_empty() {
        result["verificationUrl"] = serde_json::Value::String(job.verification_url.clone());
    }
    if !job.user_code.is_empty() {
        result["userCode"] = serde_json::Value::String(job.user_code.clone());
    }
    for (name, at) in [
        ("codeExpiresAt", job.code_expires_at),
        ("startedAt", job.started_at),
        ("updatedAt", job.updated_at),
        ("completedAt", job.completed_at),
    ] {
        if !at.is_zero() {
            result[name] = serde_json::to_value(at).unwrap_or_default();
        }
    }
    result
}

impl App {
    pub fn start_login_job(
        self: &Arc<Self>,
        account_id: &str,
    ) -> Result<LoginJob, LoginStartError> {
        self.start_login_job_with_policy(account_id, false)
    }

    /// A public repair cannot safely initialize or replace a slot: without a
    /// previously verified upstream account id, any visitor could bind their
    /// own account to the credential directory, so the owner-only flow is
    /// required.
    pub fn start_public_repair_job(
        self: &Arc<Self>,
        account_id: &str,
    ) -> Result<LoginJob, LoginStartError> {
        {
            let inner = self.read();
            let item = inner
                .account(account_id)
                .ok_or(LoginStartError::AccountGone)?;
            let expected = if item.pending_auth_verification {
                &item.pending_auth_expected_account_id
            } else {
                &item.account_id
            };
            if expected.trim().is_empty() {
                return Err(LoginStartError::RepairIdentityMismatch);
            }
        }
        self.start_login_job_with_policy(account_id, true)
    }

    fn start_login_job_with_policy(
        self: &Arc<Self>,
        account_id: &str,
        public_repair: bool,
    ) -> Result<LoginJob, LoginStartError> {
        if !self.settings.upstream_enabled {
            return Err(LoginStartError::UpstreamClosed);
        }
        let mut inner = self.write();
        let item = inner
            .account(account_id)
            .cloned()
            .ok_or(LoginStartError::AccountGone)?;
        let active: Vec<LoginJob> = inner
            .jobs
            .values()
            .filter(|job| login_job_active_status(&job.status))
            .cloned()
            .collect();
        if let Some(job) = active.first() {
            if job.account_id != item.id || public_repair && !job.public_repair {
                // Never expose or adopt an owner-originated login job on the
                // unauthenticated page; it may have been started to replace
                // the slot and its device code belongs to the owner's session.
                return Err(LoginStartError::AnotherJobInProgress);
            }
            if let Some(index) = inner.account_index(&item.id)
                && !inner.config.accounts()[index].pending_auth_verification
            {
                let slot = &mut inner.config.accounts_mut()[index];
                slot.pending_auth_verification = true;
                slot.pending_auth_expected_account_id = slot.account_id.trim().to_string();
                slot.updated_at = Timestamp::now();
                // The config write may have succeeded even when the runtime
                // write failed. Stay blocked in memory too; the active CLI may
                // already be rewriting auth.json.
                inner
                    .save()
                    .map_err(|error| LoginStartError::Storage(error.to_string()))?;
            }
            return Ok(job.clone());
        }
        let home = inner.account_codex_home(&item.id);
        let now = Timestamp::now();
        let job_id = format!(
            "job-login-{}-{}-{}",
            item.id,
            now.unix(),
            super::crypto::random_id()
        );
        // Retrying after a failed job or restart compares against the identity
        // captured before the first credential rewrite, not metadata that may
        // have been partially saved by that unfinished attempt.
        let expected = if item.pending_auth_verification {
            item.pending_auth_expected_account_id.trim()
        } else {
            item.account_id.trim()
        }
        .to_string();
        let reauthentication = inner.has_prior_identity(&item);
        let index = inner
            .account_index(&item.id)
            .ok_or(LoginStartError::AccountGone)?;
        {
            let slot = &mut inner.config.accounts_mut()[index];
            if !slot.pending_auth_verification {
                slot.pending_auth_verification = true;
                slot.pending_auth_expected_account_id.clone_from(&expected);
            }
            slot.updated_at = now;
        }
        // The gate is not rolled back in memory on failure: save may already
        // have committed config.json before runtime.json failed.
        inner
            .save()
            .map_err(|error| LoginStartError::Storage(error.to_string()))?;
        let job = LoginJob {
            id: job_id.clone(),
            kind: "account_login".to_string(),
            status: "running".to_string(),
            account_id: item.id.clone(),
            public_repair,
            reauthentication,
            message: "Starting Codex device auth login".to_string(),
            started_at: now,
            updated_at: now,
            ..LoginJob::default()
        };
        inner.jobs.insert(job_id.clone(), job.clone());
        drop(inner);
        let cancel = Cancel::default();
        self.login_cancels
            .map()
            .insert(job_id.clone(), cancel.clone());
        // The upstream account id is durable metadata from the prior
        // successful login. It travels with the job so completion can tell a
        // same-account credential repair from a slot replaced with another
        // upstream identity.
        let app = self.clone();
        let account = item.id;
        tokio::spawn(async move {
            app.run_login_job(
                cancel,
                job_id,
                account,
                home,
                expected,
                reauthentication,
                public_repair,
            )
            .await;
        });
        Ok(job)
    }

    pub fn login_job_cancellation_requested(&self, job_id: &str) -> bool {
        login_job_cancellation_requested_status(&self.read().login_job_status(job_id))
    }

    /// Requests cancellation. The job stays "cancelling" until the process
    /// has exited: marking it terminal here would let a second login start
    /// while the first process is still tearing down.
    pub fn cancel_login_job(&self, job_id: &str) -> Result<LoginJob, String> {
        let mut inner = self.write();
        let Some(job) = inner.jobs.get_mut(job_id) else {
            return Err("job not found".to_string());
        };
        if matches!(
            job.status.as_str(),
            "completed" | "failed" | "cancelled" | "cancelling"
        ) {
            return Ok(job.clone());
        }
        job.status = "cancelling".to_string();
        job.message = "Cancelling Codex device auth login".to_string();
        job.error.clear();
        job.updated_at = Timestamp::now();
        let job = job.clone();
        drop(inner);
        if let Some(cancel) = self.login_cancels.map().remove(job_id) {
            cancel.cancel();
        }
        Ok(job)
    }

    pub fn cancel_login_jobs_for_account(&self, inner: &mut Inner, account_id: &str) {
        let now = Timestamp::now();
        for (job_id, job) in inner.jobs.iter_mut() {
            if job.account_id != account_id || !login_job_active_status(&job.status) {
                continue;
            }
            job.status = "cancelling".to_string();
            job.message =
                "Cancelling Codex device auth login because the account was removed".to_string();
            job.error.clear();
            job.updated_at = now;
            if let Some(cancel) = self.login_cancels.map().remove(job_id) {
                cancel.cancel();
            }
        }
    }

    fn finish_login_job(
        &self,
        job_id: &str,
        status: &str,
        verification_url: &str,
        user_code: &str,
        message: &str,
    ) {
        let mut inner = self.write();
        let Some(job) = inner.jobs.get_mut(job_id) else {
            return;
        };
        if login_job_cancellation_requested_status(&job.status) && status != "cancelled" {
            return;
        }
        let now = Timestamp::now();
        job.status = status.to_string();
        job.message = message.to_string();
        job.error = if status == "failed" {
            message.to_string()
        } else {
            String::new()
        };
        if !verification_url.is_empty() {
            job.verification_url = verification_url.to_string();
        }
        if !user_code.is_empty() {
            job.user_code = user_code.to_string();
            if job.code_expires_at.is_zero() {
                job.code_expires_at = now.add(minutes(15));
            }
        }
        job.completed_at = now;
        job.updated_at = now;
        drop(inner);
        self.login_cancels.map().remove(job_id);
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_login_job(
        self: Arc<Self>,
        cancel: Cancel,
        job_id: String,
        account_id: String,
        codex_home: std::path::PathBuf,
        expected: String,
        reauthentication: bool,
        public_repair: bool,
    ) {
        let cancelled =
            |app: &App| cancel.is_cancelled() || app.login_job_cancellation_requested(&job_id);
        let this: &App = &self;
        if let Err(error) = std::fs::create_dir_all(&codex_home) {
            self.finish_login_job(
                &job_id,
                "failed",
                "",
                "",
                &format!("create CODEX_HOME: {error}"),
            );
            return;
        }
        let (output, exit) = self.run_codex_login(&cancel, &job_id, &codex_home).await;
        let (verification_url, user_code) = parse_device_auth_prompt(&output);
        if cancelled(this) {
            self.finish_login_job(
                &job_id,
                "cancelled",
                &verification_url,
                &user_code,
                "Codex device auth login cancelled",
            );
            return;
        }
        if let Err(error) = exit {
            let message = if output.trim().is_empty() {
                error
            } else {
                output.trim().to_string()
            };
            self.finish_login_job(
                &job_id,
                "failed",
                &verification_url,
                &user_code,
                &redact_login_output(&message),
            );
            return;
        }
        let mut refresh_account = false;
        let mut continue_login = false;
        let mut activate_after_finalize = false;
        let mut history_reset = false;
        let mut finalize_error = "";
        {
            let mut inner = self.write();
            let running = inner
                .jobs
                .get(&job_id)
                .is_some_and(|job| !login_job_cancellation_requested_status(&job.status));
            if running {
                let now = Timestamp::now();
                if let Some(job) = inner.jobs.get_mut(&job_id) {
                    job.status = "finalizing".to_string();
                    job.message = "Refreshing account quota".to_string();
                    job.error.clear();
                    job.verification_url.clone_from(&verification_url);
                    job.user_code.clone_from(&user_code);
                    job.updated_at = now;
                }
                continue_login = true;
                if let Some(item) = inner.account(&account_id).cloned() {
                    match inner.codex_auth(&item) {
                        Ok(auth) => {
                            activate_after_finalize = item.pending_pool_activation;
                            if public_repair
                                && reauthentication_changed_upstream_identity(
                                    &auth,
                                    &expected,
                                    reauthentication,
                                )
                            {
                                // Public callers may repair only the same
                                // upstream identity. Fail before mutating
                                // metadata or history; the durable gate stays
                                // set so the legitimate user can retry.
                                finalize_error =
                                    "Authenticated account did not match this credential";
                            } else {
                                history_reset = inner.apply_completed_device_auth(
                                    &account_id,
                                    &auth,
                                    &expected,
                                    reauthentication,
                                    now,
                                );
                                if let Some(job) = inner.jobs.get_mut(&job_id) {
                                    job.history_reset = history_reset;
                                }
                                if inner.save().is_err() {
                                    finalize_error =
                                        "Unable to persist authenticated account state";
                                } else {
                                    refresh_account = true;
                                }
                            }
                        }
                        // A successful CLI exit is not enough to inherit
                        // identity-scoped state. If the resulting credential
                        // cannot be read and its upstream account id verified,
                        // fail closed instead of reporting that history was
                        // preserved.
                        Err(_) => finalize_error = "Unable to verify the authenticated account",
                    }
                }
            }
        }
        if !finalize_error.is_empty() {
            self.finish_login_job(
                &job_id,
                "failed",
                &verification_url,
                &user_code,
                finalize_error,
            );
            return;
        }
        if !continue_login || self.login_job_cancellation_requested(&job_id) {
            if self.login_job_cancellation_requested(&job_id) {
                self.finish_login_job(
                    &job_id,
                    "cancelled",
                    &verification_url,
                    &user_code,
                    "Codex device auth login cancelled",
                );
            }
            return;
        }
        if refresh_account {
            let refreshed = tokio::select! {
                refreshed = async {
                    if public_repair {
                        self.refresh_account_quota_for_repair(&account_id, &expected).await
                    } else {
                        self.refresh_account_quota(&account_id).await
                    }
                } => Some(refreshed),
                () = cancel.cancelled() => None,
            };
            match refreshed {
                Some(Err(RefreshError::RepairIdentityMismatch)) => {
                    self.finish_login_job(
                        &job_id,
                        "failed",
                        &verification_url,
                        &user_code,
                        "Authenticated account did not match this credential",
                    );
                    return;
                }
                Some(Err(error)) => self.logger.printf(&format!(
                    "quota refresh after login skipped for {account_id}: {error}"
                )),
                _ => {}
            }
        }
        if cancelled(this) {
            self.finish_login_job(
                &job_id,
                "cancelled",
                &verification_url,
                &user_code,
                "Codex device auth login cancelled",
            );
            return;
        }
        {
            let mut inner = self.write();
            match inner.account_index(&account_id) {
                None => finalize_error = "Account no longer exists",
                Some(index) => {
                    // Quota and subscription metadata can resolve a more
                    // specific workspace id than auth.json. The final durable
                    // identity is checked again before the routing gate opens,
                    // so later metadata enrichment cannot smuggle old affinity
                    // or history across an account boundary.
                    let current = CodexAuthInfo {
                        account_id: inner.config.accounts()[index].account_id.clone(),
                        ..CodexAuthInfo::default()
                    };
                    let final_identity_reset = reauthentication_changed_upstream_identity(
                        &current,
                        &expected,
                        reauthentication,
                    );
                    if public_repair && final_identity_reset {
                        // Public repair still requires an exact final identity;
                        // never release the gate or convert this slot for a
                        // visitor.
                        finalize_error = "Authenticated account did not match this credential";
                    } else if final_identity_reset && !history_reset {
                        inner.clear_account_identity_scoped_state(&account_id);
                        history_reset = true;
                        if let Some(job) = inner.jobs.get_mut(&job_id) {
                            job.history_reset = true;
                        }
                    }
                    if finalize_error.is_empty() {
                        let slot = &mut inner.config.accounts_mut()[index];
                        slot.pending_auth_verification = false;
                        slot.pending_auth_expected_account_id.clear();
                        if activate_after_finalize {
                            slot.enabled = true;
                            slot.in_pool = true;
                            slot.pending_pool_activation = false;
                        }
                        slot.updated_at = Timestamp::now();
                        if inner.save_completed_auth_verification().is_err() {
                            // Keep the in-memory gate aligned with the durable
                            // marker written at job start: a storage failure
                            // must not release a credential whose identity
                            // transition was not persisted.
                            let slot = &mut inner.config.accounts_mut()[index];
                            slot.pending_auth_verification = true;
                            slot.pending_auth_expected_account_id.clone_from(&expected);
                            finalize_error = "Unable to persist completed sign-in repair";
                        }
                    }
                }
            }
        }
        if !finalize_error.is_empty() {
            self.finish_login_job(
                &job_id,
                "failed",
                &verification_url,
                &user_code,
                finalize_error,
            );
            return;
        }
        let message = if history_reset {
            "Codex device auth login completed; account identity changed and cache/affinity history was reset"
        } else {
            "Codex device auth login completed"
        };
        self.finish_login_job(&job_id, "completed", &verification_url, &user_code, message);
    }

    /// Runs `codex login --device-auth` for the slot's home, publishing the
    /// device prompt as soon as the CLI prints it. Returns the output (ANSI
    /// stripped) and how the process ended. Cancelling kills the process
    /// group, as Go's CommandContext with Setpgid did.
    async fn run_codex_login(
        self: &Arc<Self>,
        cancel: &Cancel,
        job_id: &str,
        codex_home: &Path,
    ) -> (String, Result<(), String>) {
        let mut command = tokio::process::Command::new(&self.settings.codex_cli);
        command
            .args([
                "-c",
                "cli_auth_credentials_store=\"file\"",
                "login",
                "--device-auth",
            ])
            .env_clear()
            .envs(codex_login_env(codex_home, |name| std::env::var(name).ok()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return (String::new(), Err(format!("start Codex CLI: {error}"))),
        };
        let output = Arc::new(Mutex::new(String::new()));
        let mut readers = Vec::new();
        for stream in [
            child
                .stdout
                .take()
                .map(|out| Box::new(out) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child
                .stderr
                .take()
                .map(|err| Box::new(err) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        ]
        .into_iter()
        .flatten()
        {
            let output = output.clone();
            let job_id = job_id.to_string();
            let app = self.clone();
            readers.push(tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let text = {
                        let mut output = output
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        output.push_str(&strip_ansi(&line));
                        output.push('\n');
                        output.clone()
                    };
                    let (url, code) = parse_device_auth_prompt(&text);
                    if !url.is_empty() || !code.is_empty() {
                        app.write().update_login_job(
                            &job_id,
                            "waiting_for_user",
                            &url,
                            &code,
                            "Open the verification URL and enter the code.",
                        );
                    }
                }
            }));
        }
        let pid = child.id();
        let exit = tokio::select! {
            status = child.wait() => match status {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => Err(status.to_string()),
                Err(error) => Err(error.to_string()),
            },
            () = cancel.cancelled() => {
                kill_process_group(pid);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
                Err("cancelled".to_string())
            }
        };
        for reader in readers {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), reader).await;
        }
        let text = output
            .lock()
            .map(|output| output.clone())
            .unwrap_or_default();
        (text, exit)
    }
}

/// SIGKILL to the child's whole process group (the CLI may have spawned
/// helpers), ignoring a group that is already gone.
fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) else {
        return;
    };
    // SAFETY: kill(2) with a negative pid signals the process group; it has
    // no memory-safety preconditions.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}
