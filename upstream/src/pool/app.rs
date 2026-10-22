//! The pool: its settings, its state under one lock, and the rules every
//! routing decision reads.
//!
//! A port of the `app` type in cmd/codex-pool/main.go. Go guards config and
//! runtime state with one RWMutex and names every method that expects it held
//! `...Locked`; here that state is `Inner`, reached through `App::read` and
//! `App::write`, and the `...Locked` methods are methods of `Inner`. The lock
//! is never held across an await (RUST_PORT.md 5), and a panic while it is held
//! does not poison it: one request's failure must not stop the pool.

use super::gosort::slice_stable;
use super::metadata::CodexAuthFile;
use super::metadata::CodexAuthInfo;
use super::metadata::codex_auth_info_from_file;
use super::plan;
use super::plan::is_codex_device_auth;
use super::quota;
use super::settings::DEFAULT_MODEL;
use super::settings::Settings;
use super::state::Account;
use super::state::Config;
use super::state::QuotaSnapshot;
use super::state::QuotaWindow;
use super::state::RuntimeState;
use super::time::Timestamp;
use super::tuning::Tuning;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;
use std::sync::RwLockWriteGuard;
use std::time::Duration;

/// How often an auth.json read is tried before it counts as broken, and the
/// wait between tries: the Codex CLI and the pool's own refresh rewrite it
/// while requests are selecting accounts.
const CODEX_AUTH_READ_ATTEMPTS: usize = 20;
const CODEX_AUTH_READ_RETRY_DELAY: Duration = Duration::from_millis(50);

/// A device-auth login (cmd/codex-pool loginJob). Only what routing reads is
/// here yet; the login flow itself is ported with the account work (4.4).
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginJob {
    #[serde(rename = "jobId")]
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub account_id: String,
    /// Process-local policy, never API output: a public repair job may expose
    /// only a redacted projection and must not accept a different upstream
    /// identity, unlike the authenticated owner's replacement flow.
    #[serde(skip)]
    pub public_repair: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reauthentication: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub history_reset: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub verification_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user_code: String,
    pub code_expires_at: Timestamp,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub started_at: Timestamp,
    pub updated_at: Timestamp,
    pub completed_at: Timestamp,
}

pub fn login_job_active_status(status: &str) -> bool {
    matches!(
        status,
        "running" | "waiting_for_user" | "finalizing" | "cancelling"
    )
}

pub fn login_job_cancellation_requested_status(status: &str) -> bool {
    status == "cancelling" || status == "cancelled"
}

#[derive(Debug)]
pub enum AuthError {
    /// No auth.json: an empty onboarding slot, expected and classified fast.
    Missing(String),
    Invalid(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(message) | Self::Invalid(message) => f.write_str(message),
        }
    }
}

/// The state Go guards with `a.mu`.
pub struct Inner {
    pub settings: Arc<Settings>,
    pub tuning: Tuning,
    pub config: Config,
    pub state: RuntimeState,
    pub preserve_pro_quota: bool,
    pub lineage_affinity: bool,
    pub quota_prime: bool,
    pub jobs: BTreeMap<String, LoginJob>,
}

pub struct App {
    pub settings: Arc<Settings>,
    pub tuning: Tuning,
    inner: RwLock<Inner>,
}

impl App {
    pub fn new(settings: Settings, tuning: Tuning, config: Config, state: RuntimeState) -> Self {
        let settings = Arc::new(settings);
        let inner = Inner {
            preserve_pro_quota: config
                .preserve_pro_quota
                .unwrap_or(settings.preserve_pro_quota),
            lineage_affinity: config.lineage_affinity.unwrap_or(settings.lineage_affinity),
            quota_prime: config.quota_prime.unwrap_or(settings.quota_prime),
            settings: settings.clone(),
            tuning,
            config,
            state,
            jobs: BTreeMap::new(),
        };
        Self {
            settings,
            tuning,
            inner: RwLock::new(inner),
        }
    }

    /// Go testApp: a fresh pool over a temporary directory with these
    /// accounts, the default model "gpt-test" and the alias "alias".
    pub fn for_test(data_dir: &Path, accounts: Vec<Account>) -> Self {
        let mut config = Config {
            default_model: "gpt-test".to_string(),
            accounts: Some(accounts),
            ..Config::default()
        };
        config
            .model_aliases_mut()
            .insert("alias".to_string(), "gpt-test".to_string());
        let tuning = Tuning {
            // Capacity retries wait seconds in production; tests pace them down.
            capacity_retry_backoff: Duration::from_millis(1),
            capacity_retry_max_wait: Duration::from_millis(4),
            ..Tuning::default()
        };
        let _ = std::fs::create_dir_all(data_dir.join("state"));
        Self::new(
            Settings::for_test(data_dir.to_path_buf()),
            tuning,
            config,
            initialized_state(RuntimeState::default()),
        )
    }

    pub fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Go load(): config.json and runtime.json from the data directory, with
    /// the same normalization of every account.
    pub fn load(
        settings: Settings,
        tuning: Tuning,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let data_dir = settings.data_dir.clone();
        std::fs::create_dir_all(data_dir.join("state"))
            .map_err(|error| format!("create data directory: {error}"))?;
        let mut config = read_json::<Config>(&data_dir.join("config.json"))
            .map_err(|error| format!("read config: {error}"))?
            .unwrap_or_default();
        if config.default_model.trim().is_empty() {
            config.default_model = DEFAULT_MODEL.to_string();
        }
        if let Some(model) = &settings.default_model_override {
            config.default_model.clone_from(model);
        }
        let state = read_json::<RuntimeState>(&data_dir.join("state").join("runtime.json"))
            .map_err(|error| format!("read runtime state: {error}"))?
            .unwrap_or_default();
        config.model_aliases_mut();
        let app = Self::new(settings, tuning, config, initialized_state(state));
        {
            let mut inner = app.write();
            let now = Timestamp::now();
            let migrated = inner
                .state
                .throughput_buckets
                .take()
                .is_some_and(|buckets| !buckets.is_empty());
            if inner.prune_expired_runtime_state(now) || migrated {
                let _ = inner.save();
            }
            let homes: Vec<(usize, PathBuf)> = inner
                .config
                .accounts()
                .iter()
                .enumerate()
                .map(|(index, item)| (index, inner.account_codex_home(&item.id)))
                .collect();
            for (index, home) in homes {
                let item = &mut inner.config.accounts_mut()[index];
                normalize_loaded_account(item, &home);
            }
            if inner.config.accounts().is_empty()
                && let Some(base_url) =
                    env("CODEX_POOL_UPSTREAM_BASE_URL").filter(|url| !url.is_empty())
            {
                let now = Timestamp::now();
                inner.config.accounts = Some(vec![Account {
                    id: "provider-default".to_string(),
                    label: "Default provider".to_string(),
                    auth_type: "provider_api_key".to_string(),
                    enabled: true,
                    in_pool: true,
                    priority: 100,
                    upstream_base_url: base_url.trim_end_matches('/').to_string(),
                    upstream_api_key: env("CODEX_POOL_UPSTREAM_API_KEY").unwrap_or_default(),
                    wire_api: plan::normal_wire_api(
                        &env("CODEX_POOL_UPSTREAM_WIRE_API").unwrap_or_default(),
                    )
                    .to_string(),
                    created_at: now,
                    updated_at: now,
                    ..Account::default()
                }]);
                inner.config.created_at = now;
                inner.save().map_err(|error| error.to_string())?;
            }
        }
        Ok(app)
    }

    /// Go proxyAttemptLimit: one attempt per account, or the configured cap.
    pub fn proxy_attempt_limit(&self) -> usize {
        let inner = self.read();
        let count = inner.config.accounts().len();
        if count == 0 {
            return 1;
        }
        let cap = self.settings.max_retry_accounts;
        if cap > 0 && cap < count { cap } else { count }
    }
}

/// Go load() makes these maps non-nil, so they are written as `{}`.
fn initialized_state(mut state: RuntimeState) -> RuntimeState {
    state.sticky_sessions_mut();
    state.response_bindings_mut();
    state.thread_bindings_mut();
    state.cooldowns_mut();
    state.health_mut();
    state.quotas_mut();
    state.quota_window_primes_mut();
    state.prompt_cache_mut();
    state
}

fn normalize_loaded_account(item: &mut Account, codex_home: &Path) {
    item.owner_note = plan::clean_owner_note(&item.owner_note);
    item.email = plan::normalize_email(&item.email);
    item.organization_name = plan::clean_organization_name(&item.organization_name);
    if item.organization_name.is_empty() {
        item.organization_name = plan::clean_organization_name(&item.organization_name_override);
    }
    item.organization_name_override = String::new();
    plan::normalize_account_plan_metadata(item);
    item.plan_limit = plan::clean_plan_limit(&item.plan_limit);
    item.plan_rank = plan::plan_rank(&plan::effective_plan_family(item));
    if !(0..=100).contains(&item.quota_protection_threshold) {
        // Invalid persisted policy must fail open rather than unexpectedly
        // removing a credential from routing after an upgrade.
        item.quota_protection_enabled = false;
        item.quota_protection_threshold = 0;
    }
    if item.label.trim().is_empty() {
        item.label = plan::management_credential_display_name(item);
    }
    if is_codex_device_auth(item) {
        item.codex_home = codex_home.to_string_lossy().into_owned();
        item.upstream_base_url = String::new();
        item.upstream_api_key = String::new();
    } else {
        // API-key providers are API-metered and do not have ChatGPT
        // subscription windows in this implementation.
        item.quota_protection_enabled = false;
        item.quota_protection_threshold = 0;
    }
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match std::fs::read(path) {
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Go writeJSONAtomic: indented JSON with a trailing newline, written to a
/// temporary file and renamed over the old one, mode 0600.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let mut data = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    data.push(b'\n');
    let temporary = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state")
    ));
    {
        use std::io::Write;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(&data)?;
    }
    std::fs::rename(&temporary, path)
}

/// Go quotaProtectionStatus.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaProtectionStatus {
    pub supported: bool,
    pub enabled: bool,
    pub threshold: i64,
    pub state: String,
    pub message: String,
    pub blocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_window: Option<QuotaWindow>,
    pub freshness: String,
    pub last_successful_update: Timestamp,
}

/// Use Pro last spends accounts in three tiers: ordinary accounts first, then
/// Pro 100 (Pro Lite), then Pro 200 and Pro 500. Pro Lite was once an ordinary
/// account here, and its 5x weight drew new sessions about as often as a
/// Business seat whose reset credit was about to expire, which is the opposite
/// of using the paid Pro allowance last; the owner asked for it to rank
/// between the two.
pub const PRO_TIER_ORDINARY: i32 = 0;
pub const PRO_TIER_LITE: i32 = 1;
pub const PRO_TIER_PRO: i32 = 2;

pub fn quota_error_blocks_routing(info: Option<&super::state::QuotaErrorInfo>) -> bool {
    let Some(info) = info else {
        return false;
    };
    // Quota polling is advisory and uses a different upstream path from
    // inference. A transient usage API 5xx, timeout, or decode failure must not
    // remove a healthy Pro fallback and turn a non-Pro-to-Pro transition into
    // a false 503. Only errors that explicitly prove the credential is unusable
    // may gate routing; the proxy path still quarantines a credential if
    // inference itself later returns 401/403.
    matches!(
        quota::sanitized_error_code(&info.code).as_str(),
        "account_auth_failed"
            | "invalid_token"
            | "token_invalidated"
            | "token_revoked"
            | "unauthorized"
            | "forbidden"
    )
}

pub fn manual_quota_available(item: &Account) -> Option<bool> {
    item.remaining_quota.map(|remaining| remaining > 0)
}

impl Inner {
    pub fn account_root(&self, id: &str) -> PathBuf {
        self.settings.data_dir.join("accounts").join(id)
    }

    pub fn account_codex_home(&self, id: &str) -> PathBuf {
        self.account_root(id).join(".codex")
    }

    pub fn account(&self, id: &str) -> Option<&Account> {
        self.config.accounts().iter().find(|item| item.id == id)
    }

    pub fn account_index(&self, id: &str) -> Option<usize> {
        self.config.accounts().iter().position(|item| item.id == id)
    }

    /// Go saveLocked: config.json first, then runtime.json.
    pub fn save(&mut self) -> std::io::Result<()> {
        let now = Timestamp::now();
        self.config.updated_at = now;
        self.state.updated_at = now;
        write_json_atomic(&self.settings.data_dir.join("config.json"), &self.config)?;
        write_json_atomic(
            &self.settings.data_dir.join("state").join("runtime.json"),
            &self.state,
        )
    }

    /// Go saveCompletedAuthVerificationLocked: the reverse order. Runtime
    /// identity and history changes must reach disk before config.json clears
    /// the durable routing gate; otherwise a config-only partial write could
    /// release a new identity alongside stale cache and affinity state after a
    /// restart.
    pub fn save_completed_auth_verification(&mut self) -> std::io::Result<()> {
        let now = Timestamp::now();
        self.config.updated_at = now;
        self.state.updated_at = now;
        write_json_atomic(
            &self.settings.data_dir.join("state").join("runtime.json"),
            &self.state,
        )?;
        write_json_atomic(&self.settings.data_dir.join("config.json"), &self.config)
    }

    /// Placeholder until the runtime pruning is ported with routing (4.2).
    pub fn prune_expired_runtime_state(&mut self, _now: Timestamp) -> bool {
        false
    }

    // ---- credentials ----

    pub fn read_codex_auth_file(&self, item: &Account) -> Result<CodexAuthFile, AuthError> {
        read_codex_auth_file(
            &self.account_codex_home(&item.id).join("auth.json"),
            &item.id,
        )
    }

    /// Go codexAuth.
    pub fn codex_auth(&self, item: &Account) -> Result<CodexAuthInfo, AuthError> {
        self.read_codex_auth_file(item)
            .map(|auth| codex_auth_info_from_file(&auth))
    }

    pub fn active_login_job_for_account(&self, account_id: &str) -> Option<&LoginJob> {
        self.jobs
            .values()
            .find(|job| job.account_id == account_id && login_job_active_status(&job.status))
    }

    pub fn account_login_in_progress(&self, account_id: &str) -> bool {
        self.active_login_job_for_account(account_id).is_some()
    }

    pub fn account_auth_verification_pending(&self, item: &Account) -> bool {
        item.pending_auth_verification || self.account_login_in_progress(&item.id)
    }

    pub fn has_usable_auth(&self, item: &Account) -> bool {
        if self.account_auth_verification_pending(item) {
            return false;
        }
        if is_codex_device_auth(item) {
            return self.codex_auth(item).is_ok();
        }
        !item.upstream_base_url.trim().is_empty()
    }

    // ---- identity ----

    /// Deliberately based on the upstream ChatGPT/Codex account identity, not
    /// the local slot ID. A single browser login can create several local
    /// device-auth slots from the same upstream account, especially when the
    /// host's own Codex session and the pool are authenticated from the same
    /// IP. Treating those slots as separate capacity causes noisy failover and
    /// can amplify refresh-token or team-workspace policy failures, so routing
    /// only lets one enabled slot per upstream identity become eligible.
    ///
    /// The identity is the member, not the workspace. The upstream account id
    /// of a Business or Team seat is the workspace's, shared by every member,
    /// while each member holds their own allowance. Keyed on the workspace
    /// alone, routing let one member of a workspace serve at a time, excluded
    /// every member after one member's failure, and gave them all the same
    /// placement draw: a pool of five Business seats in two workspaces routed
    /// as two accounts, and seats whose reset credit was about to expire sat
    /// behind a sibling that was nearly spent. Two slots from one login share
    /// an email and stay one identity; a slot whose email is unknown keeps the
    /// workspace key, because nothing proves it is a different member.
    pub fn upstream_identity_key(&self, item: &Account) -> String {
        let workspace = self.upstream_workspace_key(item);
        if !workspace.starts_with("codex-account:") {
            return workspace;
        }
        let mut email = plan::normalize_email(&item.email);
        if email.is_empty()
            && let Ok(auth) = self.codex_auth(item)
        {
            email = plan::normalize_email(&auth.email);
        }
        if email.is_empty() {
            return workspace;
        }
        format!("{workspace}|member:{email}")
    }

    /// The upstream account a slot authenticates against: for a Business or
    /// Team seat, the workspace every member shares.
    pub fn upstream_workspace_key(&self, item: &Account) -> String {
        if !is_codex_device_auth(item) {
            return String::new();
        }
        let account_id = item.account_id.trim();
        if !account_id.is_empty() {
            return format!("codex-account:{account_id}");
        }
        if let Ok(auth) = self.codex_auth(item) {
            let account_id = auth.account_id.trim();
            if !account_id.is_empty() {
                return format!("codex-account:{account_id}");
            }
            let email = plan::normalize_email(&auth.email);
            let organization = plan::clean_organization_name(&auth.organization_name);
            if !email.is_empty() || !organization.is_empty() {
                return format!("codex-profile:{email}|{organization}");
            }
        }
        let email = plan::normalize_email(&item.email);
        let organization = plan::clean_organization_name(&item.organization_name);
        if !email.is_empty() || !organization.is_empty() {
            return format!("codex-profile:{email}|{organization}");
        }
        String::new()
    }

    pub fn account_metadata_error(&self, account_id: &str) -> bool {
        quota_error_blocks_routing(
            self.state
                .quotas()
                .get(account_id)
                .and_then(|snapshot| snapshot.quota_error.as_ref()),
        )
    }

    pub fn account_cooling_down(&self, account_id: &str, model: &str, now: Timestamp) -> bool {
        self.state
            .cooldowns()
            .get(account_id)
            .is_some_and(|cooldowns| {
                cooldowns.iter().any(|cooldown| {
                    (model.is_empty() || cooldown.model_id == model)
                        && cooldown.next_retry_at.after(&now)
                })
            })
    }

    pub fn quota_snapshot(&self, account_id: &str) -> QuotaSnapshot {
        self.state
            .quotas()
            .get(account_id)
            .cloned()
            .unwrap_or_default()
    }

    /// The slot that represents an upstream identity right now, chosen from
    /// the local credential copies usable now, not merely the first slot with
    /// this upstream account id. Device-auth slots of the same visible account
    /// can carry different quota snapshots or session-scoped rate limits; if a
    /// stale, zero-quota, or cooling-down slot keeps owning the identity, the
    /// router falls through to Pro while a healthy Team/Plus copy sits idle.
    /// The duplicate guard still applies inside one failed request through
    /// excluded_upstream_identities, so a sibling is not an immediate retry
    /// target after the representative gets a 429/5xx.
    pub fn primary_upstream_account_id(
        &self,
        item: &Account,
        model: &str,
        now: Timestamp,
    ) -> String {
        let identity = self.upstream_identity_key(item);
        if identity.is_empty() {
            return String::new();
        }
        let mut candidates: Vec<&Account> = self
            .config
            .accounts()
            .iter()
            .filter(|candidate| {
                candidate.enabled
                    && candidate.in_pool
                    && (model.is_empty() || plan::allowed_model(candidate, model))
                    && self.has_usable_auth(candidate)
                    && !self.account_metadata_error(&candidate.id)
                    && self.upstream_identity_key(candidate) == identity
            })
            .collect();
        if candidates.is_empty() {
            return String::new();
        }
        slice_stable(&mut candidates, |left, right| {
            self.identity_representative_before(left, right, model, now)
        });
        candidates[0].id.clone()
    }

    fn identity_representative_before(
        &self,
        left: &Account,
        right: &Account,
        model: &str,
        now: Timestamp,
    ) -> bool {
        let left_cooldown = self.account_cooling_down(&left.id, model, now);
        let right_cooldown = self.account_cooling_down(&right.id, model, now);
        if left_cooldown != right_cooldown {
            return !left_cooldown;
        }
        let (left_class, left_remaining) =
            self.identity_representative_quota_class(left, model, now);
        let (right_class, right_remaining) =
            self.identity_representative_quota_class(right, model, now);
        if left_class != right_class {
            return left_class > right_class;
        }
        if left_class == 2 && left_remaining != right_remaining {
            return left_remaining > right_remaining;
        }
        self.preferred_before(left, right)
    }

    fn identity_representative_quota_class(
        &self,
        item: &Account,
        model: &str,
        now: Timestamp,
    ) -> (i32, i64) {
        let snapshot = self.quota_snapshot(&item.id);
        if quota_error_blocks_routing(snapshot.quota_error.as_ref()) {
            return (0, 0);
        }
        if let Some(quota) = &snapshot.quota {
            if quota::quota_explicitly_blocks_routing(
                quota,
                model,
                now,
                self.tuning.quota_floor_percent,
            ) || self.quota_protection_status(item, &snapshot, now).blocked
            {
                return (0, 0);
            }
            return (2, quota::remaining_quota_hint(quota));
        }
        match manual_quota_available(item) {
            Some(true) => (2, item.remaining_quota.unwrap_or_default()),
            Some(false) => (0, 0),
            None => (1, 0),
        }
    }

    /// Red outline first, then Use Pro last, then priority: the same order as
    /// balanced placement.
    pub fn preferred_before(&self, left: &Account, right: &Account) -> bool {
        let now = Timestamp::now();
        let (left_red, right_red) = (
            self.expiring_reset_credit(left, now),
            self.expiring_reset_credit(right, now),
        );
        if left_red != right_red {
            return left_red;
        }
        if self.preserve_pro_quota {
            let (left_tier, right_tier) = (self.pro_tier(left), self.pro_tier(right));
            if left_tier != right_tier {
                return left_tier < right_tier;
            }
        }
        if left.priority == right.priority {
            return left.id < right.id;
        }
        left.priority > right.priority
    }

    /// Placeholder until the red-outline rule is ported with placement (4.2).
    pub fn expiring_reset_credit(&self, _item: &Account, _now: Timestamp) -> bool {
        false
    }

    pub fn excluded_upstream_identities(
        &self,
        excluded: &HashMap<String, bool>,
    ) -> HashMap<String, bool> {
        self.config
            .accounts()
            .iter()
            .filter(|item| excluded.get(&item.id).copied().unwrap_or(false))
            .map(|item| self.upstream_identity_key(item))
            .filter(|identity| !identity.is_empty())
            .map(|identity| (identity, true))
            .collect()
    }

    pub fn duplicate_upstream_account_primary(&self, item: &Account, now: Timestamp) -> String {
        // The dashboard mirrors the routing rule. A duplicate slot is only
        // called duplicate when it is otherwise active and authenticated;
        // disabled, out-of-pool, or missing-auth slots keep their more direct
        // status.
        if !item.enabled || !item.in_pool || !self.has_usable_auth(item) {
            return String::new();
        }
        let primary = self.primary_upstream_account_id(item, "", now);
        if primary.is_empty() || primary == item.id {
            return String::new();
        }
        primary
    }

    // ---- eligibility ----

    pub fn usable(&self, item: &Account, model: &str, now: Timestamp) -> bool {
        if !item.enabled || !item.in_pool || !plan::allowed_model(item, model) {
            return false;
        }
        // Re-authentication rewrites auth.json in place. Keep the slot out of
        // selection for the whole login job without disabling it:
        // disable/pool-remove clears sticky routes, which would throw away the
        // KV cache locality this repair flow exists to preserve.
        if self.account_auth_verification_pending(item) {
            return false;
        }
        let primary = self.primary_upstream_account_id(item, model, now);
        if !primary.is_empty() && primary != item.id {
            return false;
        }
        if !self.quota_available(item, model, now) || !self.model_meter_allows(item, model) {
            return false;
        }
        if is_codex_device_auth(item) {
            if self.codex_auth(item).is_err() {
                return false;
            }
        } else if item.upstream_base_url.is_empty() {
            return false;
        }
        !self.account_cooling_down(&item.id, model, now)
    }

    pub fn quota_available(&self, item: &Account, model: &str, now: Timestamp) -> bool {
        let snapshot = self.quota_snapshot(&item.id);
        if quota_error_blocks_routing(snapshot.quota_error.as_ref()) {
            return false;
        }
        if let Some(quota) = &snapshot.quota {
            if quota::quota_explicitly_blocks_routing(
                quota,
                model,
                now,
                self.tuning.quota_floor_percent,
            ) {
                return self.same_identity_quota_hint_available(item, model, now);
            }
            return !self.quota_protection_status(item, &snapshot, now).blocked;
        }
        match manual_quota_available(item) {
            Some(true) | None => true,
            Some(false) => self.same_identity_quota_hint_available(item, model, now),
        }
    }

    /// Whether the snapshot decides availability, and how.
    pub fn quota_snapshot_available(
        &self,
        item: &Account,
        model: &str,
        now: Timestamp,
    ) -> Option<bool> {
        let snapshot = self.quota_snapshot(&item.id);
        if quota_error_blocks_routing(snapshot.quota_error.as_ref()) {
            return Some(false);
        }
        let quota = snapshot.quota.as_ref()?;
        Some(
            !(quota::quota_explicitly_blocks_routing(
                quota,
                model,
                now,
                self.tuning.quota_floor_percent,
            ) || self.quota_protection_status(item, &snapshot, now).blocked),
        )
    }

    pub fn quota_protection_status(
        &self,
        item: &Account,
        snapshot: &QuotaSnapshot,
        now: Timestamp,
    ) -> QuotaProtectionStatus {
        let mut status = QuotaProtectionStatus {
            supported: is_codex_device_auth(item),
            enabled: item.quota_protection_enabled,
            threshold: item.quota_protection_threshold.clamp(0, 100),
            state: "disabled".to_string(),
            message: "Protection disabled".to_string(),
            freshness: quota::quota_telemetry_state(snapshot, now).to_string(),
            last_successful_update: snapshot.last_successful_refresh_at,
            ..QuotaProtectionStatus::default()
        };
        if status.last_successful_update.is_zero() {
            status.last_successful_update = snapshot.usage_updated_at;
        }
        let set = |status: &mut QuotaProtectionStatus, state: &str, message: &str| {
            status.state = state.to_string();
            status.message = message.to_string();
        };
        if !status.supported {
            status.message =
                "Protection unavailable: API-key accounts do not use ChatGPT quota windows"
                    .to_string();
            return status;
        }
        if !status.enabled {
            return status;
        }
        let Some(snapshot_quota) = &snapshot.quota else {
            set(
                &mut status,
                "unavailable",
                "Protection unavailable: quota not reported; routing remains fail-open",
            );
            return status;
        };
        let windows = quota::quota_reported_windows(snapshot_quota);
        let effective = [
            quota::FIVE_HOUR_WINDOW_MINUTES,
            quota::WEEKLY_WINDOW_MINUTES,
        ]
        .into_iter()
        .find_map(|duration| {
            windows
                .iter()
                .find(|window| window.present && window.window_minutes == Some(duration))
                .cloned()
        });
        let Some(effective) = effective else {
            set(
                &mut status,
                "unavailable",
                "Protection unavailable: 5h/week quota not reported; routing remains fail-open",
            );
            return status;
        };
        let remaining = quota::quota_window_remaining(&effective);
        let reset_at = effective.reset_at;
        status.effective_window = Some(effective);
        #[allow(clippy::cast_precision_loss)]
        let threshold = status.threshold as f64;
        if remaining <= threshold {
            if reset_at.is_some_and(|reset_at| now.unix() >= reset_at) {
                set(
                    &mut status,
                    "unavailable",
                    "Protection unavailable: observed threshold window reset expired; routing remains fail-open",
                );
                return status;
            }
            if reset_at.is_none() && status.freshness != "fresh" {
                set(
                    &mut status,
                    "unavailable",
                    "Protection unavailable: quota telemetry is stale; routing remains fail-open",
                );
                return status;
            }
            // A positively observed violation remains evidence until the same
            // window resets, even if a later advisory refresh fails. This is
            // narrower than fail-closed polling: missing/failed telemetry never
            // creates a new block.
            set(&mut status, "blocked", "Protected: threshold reached");
            status.blocked = true;
            return status;
        }
        if status.freshness != "fresh" {
            set(
                &mut status,
                "unavailable",
                "Protection unavailable: quota not reported/stale; routing remains fail-open",
            );
            return status;
        }
        set(
            &mut status,
            "available",
            "Protection active: quota is above threshold",
        );
        status
    }

    /// Only a last-resort eligibility hint for the current representative.
    /// The representative chooser should normally move traffic to the local
    /// credential copy that has the positive quota snapshot. Keeping this
    /// fallback prevents a transiently incomplete snapshot from forcing Pro
    /// usage, while the primary-id check stops a zero-quota duplicate from
    /// becoming routable.
    pub fn same_identity_quota_hint_available(
        &self,
        item: &Account,
        model: &str,
        now: Timestamp,
    ) -> bool {
        let identity = self.upstream_identity_key(item);
        if identity.is_empty() {
            return false;
        }
        let primary = self.primary_upstream_account_id(item, model, now);
        if !primary.is_empty() && primary != item.id {
            return false;
        }
        for candidate in self.config.accounts() {
            if candidate.id == item.id
                || !candidate.enabled
                || !candidate.in_pool
                || !self.has_usable_auth(candidate)
            {
                continue;
            }
            if self.upstream_identity_key(candidate) != identity {
                continue;
            }
            // A duplicate slot with a persisted metadata/auth error is not
            // evidence that the shared upstream account has usable capacity.
            // Letting an errored sibling's stale manual quota keep a zero-quota
            // representative eligible made the router hammer the same Team
            // identity with 429s and return false 503s instead of moving on.
            if self.account_metadata_error(&candidate.id) {
                continue;
            }
            if self.quota_snapshot_available(candidate, model, now) == Some(true) {
                return true;
            }
            if manual_quota_available(candidate) == Some(true) {
                return true;
            }
        }
        false
    }

    /// Whether upstream currently tells this account it holds a meter for
    /// exactly this model.
    pub fn account_meters_model(&self, account_id: &str, model: &str) -> bool {
        self.state
            .quotas()
            .get(account_id)
            .and_then(|snapshot| snapshot.quota.as_ref())
            .is_some_and(|quota| {
                quota
                    .additional_limits
                    .iter()
                    .any(|limit| quota::additional_limit_names_model(limit, model))
            })
    }

    /// Keeps a model-scoped entitlement on the accounts that actually hold it.
    /// Some models are sold as a separate allowance on top of the
    /// subscription, and upstream advertises that allowance as a per-model
    /// meter on the accounts entitled to it. Routing such a model to an
    /// account without the meter spends an attempt on a request upstream will
    /// refuse, while the entitled account's allowance goes unused.
    ///
    /// The restriction only applies once some account reports the meter. A
    /// model no account meters, which is every ordinary subscription model, is
    /// unrestricted, and a pool whose quota refresh is failing everywhere
    /// reports no meters at all and therefore falls back to plain selection
    /// rather than failing closed.
    pub fn model_meter_allows(&self, item: &Account, model: &str) -> bool {
        if self.account_meters_model(&item.id, model) {
            return true;
        }
        !self.config.accounts().iter().any(|candidate| {
            candidate.id != item.id
                && candidate.enabled
                && candidate.in_pool
                && self.account_meters_model(&candidate.id, model)
        })
    }

    pub fn pro_tier(&self, item: &Account) -> i32 {
        let mut plan_type = item.plan_type.as_str();
        if let Some(snapshot) = self.state.quotas().get(&item.id)
            && !snapshot.plan_type.is_empty()
        {
            plan_type = &snapshot.plan_type;
        }
        match plan::normalize_plan_type(plan_type).as_str() {
            "pro" | "pro_max" => PRO_TIER_PRO,
            "pro_lite" => PRO_TIER_LITE,
            _ => PRO_TIER_ORDINARY,
        }
    }

    pub fn pro_account(&self, item: &Account) -> bool {
        self.pro_tier(item) == PRO_TIER_PRO
    }
}

/// Go readCodexAuthFile: invalid or incomplete content is retried as a
/// file-write race, but an absent file is not: empty onboarding slots are
/// expected and must classify quickly.
pub fn read_codex_auth_file(path: &Path, account_id: &str) -> Result<CodexAuthFile, AuthError> {
    let mut last = AuthError::Invalid(String::new());
    for attempt in 0..CODEX_AUTH_READ_ATTEMPTS {
        match read_codex_auth_file_once(path, account_id) {
            Ok(auth) => return Ok(auth),
            Err(error @ AuthError::Missing(_)) => return Err(error),
            Err(error) => last = error,
        }
        if attempt + 1 < CODEX_AUTH_READ_ATTEMPTS {
            std::thread::sleep(CODEX_AUTH_READ_RETRY_DELAY);
        }
    }
    Err(last)
}

fn read_codex_auth_file_once(path: &Path, account_id: &str) -> Result<CodexAuthFile, AuthError> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AuthError::Missing(format!(
                "codex auth is missing for account {account_id}"
            )));
        }
        Err(error) => {
            return Err(AuthError::Invalid(format!(
                "read codex auth for account {account_id}: {error}"
            )));
        }
    };
    let auth: CodexAuthFile = serde_json::from_slice(&data).map_err(|_| {
        AuthError::Invalid(format!("codex auth is invalid for account {account_id}"))
    })?;
    if auth
        .tokens
        .as_ref()
        .is_none_or(|tokens| tokens.access_token.trim().is_empty())
    {
        return Err(AuthError::Invalid(format!(
            "codex access token is missing for account {account_id}"
        )));
    }
    Ok(auth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::state::AccountQuota;
    use crate::pool::state::QuotaErrorInfo;
    use crate::pool::testing::test_app;
    use crate::pool::testing::write_codex_device_auth;
    use crate::pool::time::hours;
    use crate::pool::time::minutes;
    use crate::pool::time::seconds;

    fn window(
        role: &str,
        label: &str,
        remaining: f64,
        window_minutes: i64,
        reset_at: Option<i64>,
    ) -> QuotaWindow {
        QuotaWindow {
            role: role.into(),
            label: label.into(),
            present: true,
            observed: true,
            remaining_percent: Some(remaining),
            #[allow(clippy::cast_possible_truncation)]
            percentage: remaining as i64,
            window_minutes: Some(window_minutes),
            reset_at,
            ..QuotaWindow::default()
        }
    }

    // Go TestQuotaProtectionUsesFiveHourThenWeekAndFailsOpen.
    #[test]
    fn quota_protection_uses_five_hour_then_week_and_fails_open() {
        let now = Timestamp::from_unix(1_800_000_000);
        let future = now.add(hours(1)).unix();
        let expired = now.add(seconds(-1)).unix();
        let item = Account {
            id: "device".into(),
            auth_type: "codex_device_auth".into(),
            quota_protection_enabled: true,
            quota_protection_threshold: 20,
            ..Account::default()
        };
        let (app, _dir) = test_app(vec![item.clone()]);
        let inner = app.read();
        let mut snapshot = QuotaSnapshot {
            last_successful_refresh_at: now,
            quota: Some(AccountQuota {
                windows: vec![
                    window("secondary", "Week", 80.0, 10080, Some(future)),
                    window("primary", "5h", 20.0, 300, Some(future)),
                ],
                ..AccountQuota::default()
            }),
            ..QuotaSnapshot::default()
        };
        let status = inner.quota_protection_status(&item, &snapshot, now);
        assert!(status.blocked);
        assert_eq!(
            status.effective_window.and_then(|w| w.window_minutes),
            Some(300)
        );

        fn quota(snapshot: &mut QuotaSnapshot) -> &mut AccountQuota {
            snapshot.quota.get_or_insert_with(AccountQuota::default)
        }
        quota(&mut snapshot).windows.truncate(1);
        let status = inner.quota_protection_status(&item, &snapshot, now);
        assert!(!status.blocked);
        assert_eq!(
            status.effective_window.and_then(|w| w.window_minutes),
            Some(10080)
        );

        {
            let week = &mut quota(&mut snapshot).windows[0];
            week.remaining_percent = Some(20.0);
            week.percentage = 20;
            week.reset_at = Some(expired);
        }
        let status = inner.quota_protection_status(&item, &snapshot, now);
        assert!(!status.blocked);
        assert_eq!(status.state, "unavailable");

        quota(&mut snapshot).windows[0].reset_at = None;
        snapshot.last_successful_refresh_at =
            now.add(-minutes(quota::QUOTA_TELEMETRY_FRESHNESS_MINUTES) - seconds(1));
        let status = inner.quota_protection_status(&item, &snapshot, now);
        assert!(!status.blocked);
        assert_eq!(status.state, "unavailable");

        quota(&mut snapshot).windows[0].reset_at = Some(future);
        snapshot.quota_error = Some(QuotaErrorInfo {
            code: "upstream_status".into(),
            timestamp: now,
            ..QuotaErrorInfo::default()
        });
        assert!(inner.quota_protection_status(&item, &snapshot, now).blocked);

        let provider = Account {
            id: "provider".into(),
            auth_type: "provider_api_key".into(),
            quota_protection_enabled: true,
            quota_protection_threshold: 100,
            ..Account::default()
        };
        let status = inner.quota_protection_status(&provider, &QuotaSnapshot::default(), now);
        assert!(!status.supported && !status.blocked);
    }

    // The representative of a shared upstream identity is the usable copy.
    #[test]
    fn duplicate_slots_share_one_representative() {
        let mut first = crate::pool::testing::device_account("first", "acct-shared", 10);
        first.email = "member@example.test".into();
        let mut second = crate::pool::testing::device_account("second", "acct-shared", 20);
        second.email = "member@example.test".into();
        let (app, _dir) = test_app(vec![first.clone(), second.clone()]);
        write_codex_device_auth(&app, "first", "acct-shared", "member@example.test");
        write_codex_device_auth(&app, "second", "acct-shared", "member@example.test");
        let now = Timestamp::now();
        let inner = app.read();
        assert_eq!(
            inner.upstream_identity_key(&first),
            "codex-account:acct-shared|member:member@example.test"
        );
        // Higher priority wins among equals.
        assert_eq!(
            inner.primary_upstream_account_id(&first, "gpt-test", now),
            "second"
        );
        assert!(!inner.usable(&first, "gpt-test", now));
        assert!(inner.usable(&second, "gpt-test", now));
        drop(inner);
        // A cooling-down representative hands the identity to its sibling.
        app.write().state.cooldowns_mut().insert(
            "second".into(),
            vec![crate::pool::state::Cooldown {
                model_id: "gpt-test".into(),
                next_retry_at: now.add(minutes(5)),
                reason: "rate_limited".into(),
                ..Default::default()
            }],
        );
        let inner = app.read();
        assert_eq!(
            inner.primary_upstream_account_id(&first, "gpt-test", now),
            "first"
        );
        assert!(inner.usable(&first, "gpt-test", now));
    }

    // Go load(): accounts are normalized and the files round-trip.
    #[test]
    fn load_normalizes_accounts_and_saves() {
        let dir = crate::pool::testing::TempDir::new();
        let config = serde_json::json!({
            "defaultModel": "",
            "accounts": [{
                "id": "device", "authType": "codex_device_auth", "enabled": true, "inPool": true,
                "email": " Owner@Example.Test ", "rawPlanType": "prolite", "planLimit": "50x",
                "organizationNameOverride": "  Org\u{7} Name ", "quotaProtectionThreshold": 150,
                "quotaProtectionEnabled": true, "upstreamBaseUrl": "https://ignored.example.test"
            }]
        });
        if let Err(error) = std::fs::write(dir.path().join("config.json"), config.to_string()) {
            panic!("{error}");
        }
        let app = App::load(
            Settings::for_test(dir.path().to_path_buf()),
            Tuning::default(),
            |_| None,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let mut inner = app.write();
        assert_eq!(inner.config.default_model, DEFAULT_MODEL);
        let item = inner.config.accounts()[0].clone();
        assert_eq!(item.email, "owner@example.test");
        assert_eq!(item.organization_name, "Org Name");
        assert_eq!(item.plan_family, "pro_lite");
        assert_eq!(item.plan_rank, 250);
        assert_eq!(item.plan_limit, "");
        assert!(!item.quota_protection_enabled);
        assert_eq!(item.upstream_base_url, "");
        assert!(item.codex_home.ends_with("accounts/device/.codex"));
        assert_eq!(item.label, "ow***er@example.test");
        assert!(inner.save().is_ok());
        let written: Config = read_json(&dir.path().join("config.json"))
            .ok()
            .flatten()
            .unwrap_or_default();
        assert_eq!(written.accounts()[0].email, "owner@example.test");
        assert!(dir.path().join("state").join("runtime.json").exists());
    }
}
