//! An account's credential: the Codex agent the pool's own calls carry, the
//! token refresh Codex would make, and the memory of credentials upstream
//! refused for good.
//!
//! A port of codex_own.go (clientAgent, sendCodexOwnCall),
//! refreshCodexAuth* in main.go and refused_credential.go. The pool's own
//! calls are composed by own_call.rs and sent in process with Codex's client.

use super::app::App;
use super::app::AuthError;
use super::app::read_codex_auth_file;
use super::app::write_json_atomic;
use super::metadata::CodexAuthFile;
use super::metadata::CodexAuthInfo;
use super::metadata::codex_auth_info_from_file;
use super::metadata::jwt_expiry;
use super::own_call::BackendCall;
use super::own_call::OwnCall;
use super::own_call::RefreshCall;
use super::quota::sanitized_error_code;
use super::state::Account;
use super::state::ClientAgent;
use super::stream::SseReader;
use super::time::Timestamp;
use super::time::minutes;
use crate::transport::SendFailure;
use crate::transport::UpstreamResponse;
use http::HeaderMap;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

/// The Codex release the pool is held to. It must equal the tag in
/// upstream/Cargo.toml and the bundled CLI in the Dockerfile; a test
/// (bundled_codex_matches_the_pinned_release) keeps the three together.
pub const PINNED_CODEX_VERSION: &str = "0.166.0";
/// Why a token refresh is made, as Codex names it upstream (login
/// oauth/client.rs RefreshReason::as_str, 0.164.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshReason {
    ProactiveExpiry,
    UnauthorizedRecovery,
}

impl RefreshReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProactiveExpiry => "proactive_expiry",
            Self::UnauthorizedRecovery => "unauthorized_recovery",
        }
    }
}

/// A token is refreshed this long before it expires.
pub const CODEX_TOKEN_REFRESH_WINDOW_MINUTES: i64 = 5;
/// The reason recorded for a refused token refresh. Only that refusal makes
/// later refreshes fail without network; a revoked access token alone still
/// leaves the refresh Codex would try after a 401.
pub const REFUSED_REASON_REFRESH: &str = "token refresh refused";

/// Why a credential could not be used. `auth_failed` separates credential
/// failures from transport failures: the proxy loop quarantines the account
/// and tries another eligible one instead of turning one revoked token into a
/// request outage. `refused` is upstream's permanent refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialError {
    pub auth_failed: bool,
    pub refused: bool,
    pub missing: bool,
    pub message: String,
}

impl CredentialError {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            auth_failed: false,
            refused: false,
            missing: false,
            message: message.into(),
        }
    }

    fn auth(message: impl Into<String>) -> Self {
        Self {
            auth_failed: true,
            ..Self::transient(message)
        }
    }
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<AuthError> for CredentialError {
    fn from(error: AuthError) -> Self {
        let missing = matches!(error, AuthError::Missing(_));
        Self {
            missing,
            ..Self::auth(error.to_string())
        }
    }
}

#[derive(Clone, Debug)]
struct RefusedCredential {
    tokens: [u8; 32],
    reason: String,
}

/// A credential upstream has refused for good is not asked again.
///
/// A native Codex caches a permanent refresh failure for the auth snapshot it
/// happened on, so later refreshes of the same credentials fail fast without
/// network (login auth/manager.rs CachedAuth.permanent_refresh_failure); it
/// asks again only once the user logs in and the credentials change. The pool
/// once polled such an account about every five minutes regardless: an account
/// OpenAI deactivated answered 401 token_revoked about 280 times a day, each a
/// request a native Codex would never send. The refusal is remembered against
/// the account's current tokens, and background work (quota polling,
/// refresh-all, priming) skips the account until auth.json holds different
/// tokens. A single-account refresh from the management page still asks, once,
/// because an operator asked for it. In memory only, holding a digest of the
/// tokens, never the tokens; after a restart the first poll asks once and
/// records it again.
#[derive(Default)]
pub struct RefusedCredentials {
    by_account: Mutex<HashMap<String, RefusedCredential>>,
}

impl RefusedCredentials {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, RefusedCredential>> {
        self.by_account
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record(&self, account_id: &str, tokens: [u8; 32], reason: &str) {
        self.lock().insert(
            account_id.to_string(),
            RefusedCredential {
                tokens,
                reason: reason.to_string(),
            },
        );
    }

    /// The refusal of the account's current tokens; different tokens (a new
    /// login) clear the record.
    fn refused(&self, account_id: &str, tokens: [u8; 32]) -> Option<String> {
        let mut records = self.lock();
        let record = records.get(account_id)?;
        if record.tokens != tokens {
            records.remove(account_id);
            return None;
        }
        Some(record.reason.clone())
    }

    pub fn clear(&self, account_id: &str) {
        self.lock().remove(account_id);
    }
}

pub fn codex_tokens_digest_of(auth: &CodexAuthFile) -> [u8; 32] {
    let Some(tokens) = &auth.tokens else {
        return [0; 32];
    };
    let mut hasher = Sha256::new();
    hasher.update(tokens.access_token.as_bytes());
    hasher.update([0]);
    hasher.update(tokens.refresh_token.as_bytes());
    hasher.finalize().into()
}

/// A usage or backend answer that revokes the access token itself, which no
/// retry with the same tokens can get past.
pub fn usage_refuses_credential(status: u16, code: &str) -> bool {
    (status == 401 || status == 403)
        && matches!(
            sanitized_error_code(code).as_str(),
            "token_revoked" | "token_invalidated"
        )
}

/// Codex's verdict on a failed token refresh (login auth/manager.rs
/// request_chatgpt_token_refresh and classify_refresh_token_failure):
/// permanent on a 401, on one of the three refresh_token_* codes, or on a 400
/// invalid_grant; anything else is transient.
pub fn codex_refresh_failure_is_permanent(status: u16, body: &[u8]) -> bool {
    let code = codex_token_error_code(body);
    if matches!(
        code.to_lowercase().as_str(),
        "refresh_token_expired" | "refresh_token_reused" | "refresh_token_invalidated"
    ) {
        return true;
    }
    status == 401 || (status == 400 && code.eq_ignore_ascii_case("invalid_grant"))
}

/// The code as login oauth/error.rs TokenErrorDetail::parse reads it: "error"
/// as a string, else "error.code", else a top-level "code".
pub fn codex_token_error_code(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let Ok(parsed) =
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(text.trim())
    else {
        return String::new();
    };
    let non_empty = |value: Option<&serde_json::Value>| {
        value
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
    };
    non_empty(parsed.get("error"))
        .or_else(|| non_empty(parsed.get("error").and_then(|error| error.get("code"))))
        .or_else(|| non_empty(parsed.get("code")))
        .unwrap_or_default()
}

/// The User-Agent of Codex's default HTTP client, which a Codex builds before
/// its user-agent suffix is set: the token refresh of a native Codex carries
/// "codex_exec/0.163.0 (OS; arch) terminal" while its conversation requests
/// add " (codex_exec; 0.163.0)". The agent the pool records comes from
/// conversation requests, so the suffix is removed here.
pub fn codex_default_client_user_agent(user_agent: &str) -> String {
    let (Some(open), Some(last)) = (user_agent.find('('), user_agent.rfind(" (")) else {
        return user_agent.to_string();
    };
    if last <= open || !user_agent.ends_with(')') {
        return user_agent.to_string();
    }
    user_agent[..last].to_string()
}

/// The "version" header a native Codex sends to the Codex backend (its own
/// release, from the built-in provider). A custom-provider client omits it, so
/// it is taken from the client's own User-Agent product token
/// (codex_exec/0.163.0, codex-tui/0.163.0, ...). Other clients get none.
pub fn codex_client_version(headers: &HeaderMap) -> String {
    let user_agent = super::http_io::header_value(headers, "user-agent").trim();
    let product = user_agent
        .split_once(' ')
        .map_or(user_agent, |(product, _)| product);
    let Some((name, version)) = product.split_once('/') else {
        return String::new();
    };
    if !name.to_lowercase().starts_with("codex") || version.is_empty() {
        return String::new();
    }
    if version
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c.is_ascii_lowercase())
    {
        version.to_string()
    } else {
        String::new()
    }
}

pub fn codex_originator_from_user_agent(user_agent: &str) -> String {
    let product = user_agent
        .split_once(' ')
        .map_or(user_agent, |(product, _)| product);
    product
        .split_once('/')
        .map_or(product, |(name, _)| name)
        .to_string()
}

/// The agent the pool presents before any Codex has ever connected, in the
/// shape the bundled Codex CLI would send; SPEC.md 2.3.2 records that
/// exception.
pub fn fallback_client_agent() -> ClientAgent {
    let mut os_name = "Linux".to_string();
    let mut os_version = String::new();
    if let Ok(release) = std::fs::read_to_string("/etc/os-release") {
        for line in release.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim_matches('"').to_string();
            match key {
                "NAME" => os_name = value,
                "VERSION_ID" => os_version = value,
                _ => {}
            }
        }
    }
    let platform = format!("{os_name} {os_version}").trim().to_string();
    // Rust names the architectures as Codex does (Go maps amd64 to x86_64).
    let arch = std::env::consts::ARCH;
    ClientAgent {
        user_agent: format!(
            "codex_exec/{PINNED_CODEX_VERSION} ({platform}; {arch}) unknown (codex_exec; {PINNED_CODEX_VERSION})"
        ),
        originator: "codex_exec".to_string(),
        ..ClientAgent::default()
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RefreshResponse {
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

pub async fn read_body(response: UpstreamResponse, limit: usize) -> Result<Vec<u8>, String> {
    SseReader::new(response.body).read_all(limit).await
}

/// send_own_call without the App, for work that must outlive its caller.
async fn send_own_call_with(
    transports: &crate::transport::Transports,
    call: &OwnCall,
) -> Result<UpstreamResponse, SendFailure> {
    let composed = super::own_call::compose(call).map_err(SendFailure::Prepare)?;
    let (request, after) =
        crate::transport::build_own_call(composed).map_err(SendFailure::Prepare)?;
    crate::transport::send_request(transports, request, &after).await
}

/// How long a token refresh exchange may take before it fails as transient.
/// Codex 0.166 sets none; 0.168 bounds its own at 45 seconds (login
/// auth/manager.rs request_chatgpt_token_refresh, `timeout:
/// Some(Duration::from_secs(45))`, rust-v0.168.0-alpha.3). The pool takes it
/// ahead of the upgrade: a refresh now runs to the end detached from its
/// caller and holds the account's refresh lock, so without a bound a stalled
/// exchange would stall every request to the account and the shutdown drain.
pub const CODEX_TOKEN_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// The token refresh exchange (login auth/manager.rs
/// request_chatgpt_token_refresh), with Codex's verdict on a failure.
async fn exchange_codex_token_refresh(
    transports: &crate::transport::Transports,
    call: &OwnCall,
) -> Result<RefreshResponse, CredentialError> {
    let exchange = async {
        let response = send_own_call_with(transports, call)
            .await
            .map_err(|error| {
                CredentialError::transient(format!("refresh codex token: {error:?}"))
            })?;
        let status = response.status.as_u16();
        let body = read_body(response, super::stream::MAX_REQUEST_BODY)
            .await
            .map_err(|error| CredentialError::transient(format!("refresh codex token: {error}")))?;
        if !(200..300).contains(&status) {
            let message = format!("refresh codex token failed with status {status}");
            let refused = codex_refresh_failure_is_permanent(status, &body);
            return Err(CredentialError {
                auth_failed: matches!(status, 400 | 401 | 403),
                refused,
                missing: false,
                message,
            });
        }
        serde_json::from_slice(&body).map_err(|error| {
            CredentialError::transient(format!("decode refreshed codex token: {error}"))
        })
    };
    tokio::time::timeout(CODEX_TOKEN_REFRESH_TIMEOUT, exchange)
        .await
        .unwrap_or_else(|_| Err(CredentialError::transient("refresh codex token: timed out")))
}

/// Everything a refresh needs once it has started, owned so it can finish
/// after its caller is gone (App::refresh_codex_auth_holding).
struct RefreshSettle {
    transports: Arc<crate::transport::Transports>,
    refused: Arc<RefusedCredentials>,
    logger: Arc<super::log::Logger>,
    account_id: String,
    path: std::path::PathBuf,
    /// The tokens this refresh spends, as read from auth.json.
    spent: super::metadata::CodexAuthTokens,
    digest: [u8; 32],
}

impl RefreshSettle {
    async fn run(self, call: &OwnCall) -> Result<(), CredentialError> {
        let refreshed = match exchange_codex_token_refresh(&self.transports, call).await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                if error.refused {
                    note_refused_tokens(
                        &self.refused,
                        &self.logger,
                        &self.account_id,
                        self.digest,
                        REFUSED_REASON_REFRESH,
                    );
                }
                return Err(error);
            }
        };
        self.persist(refreshed)
    }

    /// Writes the refreshed tokens only over the tokens this refresh spent
    /// (login auth/manager.rs persist_tokens, which compares the stored
    /// refresh and access tokens with the ones it refreshed and saves with
    /// compare_and_save, rust-v0.168.0-alpha.3). A sign-in finished from the
    /// management page while the exchange was out wrote a new login; the
    /// rotated tokens of the old one must not overwrite it. Unlike Codex,
    /// which fails the refresh, the pool keeps going: the caller reads
    /// auth.json next and gets the new login.
    fn persist(self, refreshed: RefreshResponse) -> Result<(), CredentialError> {
        let mut auth = read_codex_auth_file(&self.path, &self.account_id)?;
        let Some(tokens) = auth.tokens.as_mut().filter(|tokens| {
            tokens.refresh_token == self.spent.refresh_token
                && tokens.access_token == self.spent.access_token
        }) else {
            self.logger.printf(&format!(
                "refreshed codex token for {} discarded: the account's credentials changed during the refresh",
                self.account_id
            ));
            return Ok(());
        };
        if let Some(id_token) = refreshed.id_token.filter(|token| !token.is_empty()) {
            tokens.id_token = id_token;
        }
        if let Some(access) = refreshed.access_token.filter(|token| !token.is_empty()) {
            tokens.access_token = access;
        }
        if let Some(refresh) = refreshed.refresh_token.filter(|token| !token.is_empty()) {
            tokens.refresh_token = refresh;
        }
        auth.last_refresh = Some(Timestamp::now());
        write_json_atomic(&self.path, &auth).map_err(|error| {
            CredentialError::transient(format!("persist refreshed codex auth: {error}"))
        })
    }
}

/// Counts a refresh from its start until it has settled, for the shutdown
/// drain (serve.rs): a refresh cut by the process exit is the one gap the
/// detached task cannot close.
pub struct SettlingRefresh(Arc<std::sync::atomic::AtomicI64>);

impl SettlingRefresh {
    pub(super) fn start(count: Arc<std::sync::atomic::AtomicI64>) -> Self {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for SettlingRefresh {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn note_refused_tokens(
    refused: &RefusedCredentials,
    logger: &super::log::Logger,
    account_id: &str,
    tokens: [u8; 32],
    reason: &str,
) {
    refused.record(account_id, tokens, reason);
    logger.printf(&format!(
        "credential refused for {account_id}: {reason}; automatic quota refresh stops until the account logs in again"
    ));
}

impl App {
    /// Records the agent of a Codex request routed to account_id, and as the
    /// pool-wide latest. Only a Codex product token is recorded, so a foreign
    /// client never becomes the identity of the pool's own calls.
    pub fn note_client_agent(&self, account_id: &str, inbound: &HeaderMap) {
        if codex_client_version(inbound).is_empty() {
            return;
        }
        let mut agent = ClientAgent {
            user_agent: super::http_io::header_value(inbound, "user-agent")
                .trim()
                .to_string(),
            originator: super::http_io::header_value(inbound, "originator")
                .trim()
                .to_string(),
            seen_at: Timestamp::now(),
            beta_features: super::http_io::header_value(inbound, "x-codex-beta-features")
                .trim()
                .to_string(),
            turn_profile: super::codex_request::profile_from_turn_metadata(
                super::http_io::header_value(inbound, "x-codex-turn-metadata"),
            ),
            ..ClientAgent::default()
        };
        let mut inner = self.write();
        // A request without turn metadata (a compaction) says nothing about
        // the profile; keep the one this same Codex sent with its turns.
        if agent.turn_profile.is_empty()
            && let Some(previous) = inner
                .state
                .client_agents()
                .get(account_id)
                .filter(|previous| previous.user_agent == agent.user_agent)
        {
            agent.turn_profile.clone_from(&previous.turn_profile);
            if agent.beta_features.is_empty() {
                agent.beta_features.clone_from(&previous.beta_features);
            }
        }
        if !account_id.is_empty() {
            inner
                .state
                .client_agents_mut()
                .insert(account_id.to_string(), agent.clone());
        }
        inner.state.client_agents_mut().insert(String::new(), agent);
    }

    /// The agent for one of the pool's own calls: the Codex that last used
    /// the account, else the last Codex seen at all (persisted, so a restart
    /// keeps it), else one of the pool's own making.
    pub fn client_agent_for(&self, account_id: &str) -> ClientAgent {
        let inner = self.read();
        let agents = inner.state.client_agents();
        let mut agent = agents
            .get(account_id)
            .filter(|agent| !agent.user_agent.is_empty())
            .or_else(|| agents.get(""))
            .cloned();
        drop(inner);
        match agent.as_mut() {
            Some(agent) if !agent.user_agent.is_empty() => {
                if agent.originator.is_empty() {
                    agent.originator = codex_originator_from_user_agent(&agent.user_agent);
                }
                agent.clone()
            }
            _ => fallback_client_agent(),
        }
    }

    /// A backend-client call for the account: the agent of the Codex that
    /// last used it, and its credential.
    pub fn codex_backend_call(
        &self,
        account_id: &str,
        url: String,
        auth: &CodexAuthInfo,
    ) -> BackendCall {
        BackendCall {
            url,
            user_agent: self.client_agent_for(account_id).user_agent,
            access_token: auth.access_token.clone(),
            account_id: Some(auth.account_id.clone()).filter(|id| !id.is_empty()),
            fedramp: auth.fedramp,
        }
    }

    /// Sends one of the pool's own calls, composed as Codex composes it
    /// (own_call.rs), with Codex's client.
    pub async fn send_own_call(&self, call: &OwnCall) -> Result<UpstreamResponse, SendFailure> {
        send_own_call_with(&self.transports, call).await
    }

    /// The account's refresh lock: one permit, held across the refresh's
    /// network call so two requests never spend the same rotating refresh
    /// token (a semaphore permit, not a mutex guard, may be held across an
    /// await under Codex's lint rules).
    async fn auth_lock(&self, account_id: &str) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let semaphore = {
            let mut locks = self
                .auth_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            locks
                .entry(account_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
                .clone()
        };
        semaphore.acquire_owned().await.ok()
    }

    fn auth_path(&self, account_id: &str) -> std::path::PathBuf {
        self.read().account_codex_home(account_id).join("auth.json")
    }

    pub fn codex_auth(&self, item: &Account) -> Result<CodexAuthInfo, CredentialError> {
        read_codex_auth_file(&self.auth_path(&item.id), &item.id)
            .map(|auth| codex_auth_info_from_file(&auth))
            .map_err(CredentialError::from)
    }

    /// Codex's ChatGPT token refresh request (login auth/manager.rs
    /// request_chatgpt_token_refresh), carrying the account's Codex agent.
    fn token_refresh_call(
        &self,
        account_id: &str,
        refresh_token: &str,
        reason: RefreshReason,
    ) -> OwnCall {
        let agent = self.client_agent_for(account_id);
        OwnCall::TokenRefresh(RefreshCall {
            url: self.settings.refresh_url.clone(),
            user_agent: codex_default_client_user_agent(&agent.user_agent),
            originator: agent.originator,
            client_id: self.settings.oauth_client_id.clone(),
            refresh_token: refresh_token.to_string(),
            refresh_reason: Some(reason.as_str().to_string()),
        })
    }

    /// Codex's ChatGPT token refresh (login auth/manager.rs
    /// request_chatgpt_token_refresh), with Codex's verdict on a failure.
    pub async fn request_codex_token_refresh(
        &self,
        account_id: &str,
        refresh_token: &str,
        reason: RefreshReason,
    ) -> Result<RefreshResponse, CredentialError> {
        let call = self.token_refresh_call(account_id, refresh_token, reason);
        exchange_codex_token_refresh(&self.transports, &call).await
    }

    /// Refreshes the account's token when it expires within the refresh
    /// window, or now when `force`, under the account's refresh lock.
    pub async fn refresh_codex_auth(
        &self,
        item: &Account,
        force: bool,
    ) -> Result<(), CredentialError> {
        let permit = self.auth_lock(&item.id).await;
        self.refresh_codex_auth_holding(item, force, permit).await
    }

    /// The refresh itself; the caller already holds the account's refresh
    /// lock and hands its permit over. A refresh upstream already refused for
    /// these tokens fails without network until they change, as Codex's
    /// cached permanent_refresh_failure does.
    ///
    /// Once the exchange starts it runs to the end in a task of its own,
    /// holding the permit, whatever happens to the caller. Refresh tokens
    /// rotate on use: when a client disconnected (or a caller's timeout fired)
    /// after upstream had issued new tokens but before they reached auth.json,
    /// the account was left holding a refresh token upstream had already
    /// spent, and its next refresh presented it again. Codex 0.168 guards the
    /// same gap by remembering an exchange that was dropped mid-way
    /// (login auth/manager.rs unsettled_chatgpt_refresh,
    /// rust-v0.168.0-alpha.3); the pool never drops one. Do not move the
    /// exchange back into the caller's future, and keep the permit inside the
    /// task, or a second request can spend the same token while the first is
    /// still settling.
    async fn refresh_codex_auth_holding(
        &self,
        item: &Account,
        force: bool,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<(), CredentialError> {
        let path = self.auth_path(&item.id);
        let auth = read_codex_auth_file(&path, &item.id)?;
        let Some(spent) = auth
            .tokens
            .as_ref()
            .filter(|tokens| !tokens.refresh_token.is_empty())
            .cloned()
        else {
            return Ok(());
        };
        let digest = codex_tokens_digest_of(&auth);
        if self.refused.refused(&item.id, digest).as_deref() == Some(REFUSED_REASON_REFRESH) {
            return Err(CredentialError {
                refused: true,
                ..CredentialError::auth("token refresh refused earlier")
            });
        }
        if !force {
            match jwt_expiry(&spent.access_token) {
                Some(expiry)
                    if expiry.sub(&Timestamp::now())
                        <= minutes(CODEX_TOKEN_REFRESH_WINDOW_MINUTES) => {}
                _ => return Ok(()),
            }
        }
        // Codex names the refresh it makes before expiry proactive_expiry and
        // the one after a 401 unauthorized_recovery (login auth/manager.rs
        // proactive_refresh_reason and UnauthorizedRecovery). Its scheduled
        // refresh is for tokens without an expiry; ChatGPT access tokens
        // always carry one, so the pool never makes it.
        let reason = if force {
            RefreshReason::UnauthorizedRecovery
        } else {
            RefreshReason::ProactiveExpiry
        };
        let call = self.token_refresh_call(&item.id, &spent.refresh_token, reason);
        let settle = RefreshSettle {
            transports: self.transports.clone(),
            refused: self.refused.clone(),
            logger: self.logger.clone(),
            account_id: item.id.clone(),
            path,
            spent,
            digest,
        };
        let settling = SettlingRefresh::start(self.refreshes_settling.clone());
        let task = tokio::spawn(async move {
            let _permit = permit;
            let _settling = settling;
            settle.run(&call).await
        });
        task.await.unwrap_or_else(|error| {
            Err(CredentialError::transient(format!(
                "refresh codex token: {error}"
            )))
        })
    }

    /// Go refreshedCodexAuthContext: the account's credential, refreshed
    /// first if it is about to expire, under the account's auth lock.
    pub async fn refreshed_codex_auth(
        &self,
        item: &Account,
    ) -> Result<CodexAuthInfo, CredentialError> {
        let permit = self.auth_lock(&item.id).await;
        self.refresh_codex_auth_holding(item, false, permit).await?;
        self.codex_auth(item)
    }

    /// Refreshes the credential upstream rejected, unless a concurrent request
    /// already replaced it. Refresh tokens rotate on use, so refreshing again
    /// with a token another request just rotated would spend it twice; the
    /// caller retries with whatever is current instead.
    pub async fn refresh_codex_auth_after_rejection(
        &self,
        item: &Account,
        rejected_access_token: &str,
    ) -> Result<CodexAuthInfo, CredentialError> {
        let permit = self.auth_lock(&item.id).await;
        let current = self.codex_auth(item)?;
        if current.access_token != rejected_access_token {
            return Ok(current);
        }
        self.refresh_codex_auth_holding(item, true, permit).await?;
        self.codex_auth(item)
    }

    /// Records a refusal of tokens the caller already holds.
    pub fn record_refused_tokens(&self, account_id: &str, tokens: [u8; 32], reason: &str) {
        note_refused_tokens(&self.refused, &self.logger, account_id, tokens, reason);
    }

    /// Remembers that upstream refused the account's current tokens for good.
    pub fn record_refused_credential(&self, account_id: &str, reason: &str) {
        let Some(item) = self.read().account(account_id).cloned() else {
            return;
        };
        if let Some(tokens) = self.codex_tokens_digest(&item) {
            self.record_refused_tokens(account_id, tokens, reason);
        }
    }

    pub fn codex_tokens_digest(&self, item: &Account) -> Option<[u8; 32]> {
        let auth = read_codex_auth_file(&self.auth_path(&item.id), &item.id).ok()?;
        auth.tokens.as_ref()?;
        Some(codex_tokens_digest_of(&auth))
    }

    /// Whether background work must leave the account alone because upstream
    /// refused its current tokens.
    pub fn credential_refused(&self, item: &Account) -> bool {
        self.codex_tokens_digest(item)
            .is_some_and(|tokens| self.refused.refused(&item.id, tokens).is_some())
    }

    pub fn clear_refused_credential(&self, account_id: &str) {
        self.refused.clear(account_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::testing::FakeResponse;
    use crate::pool::testing::FakeServer;
    use crate::pool::testing::device_account;
    use crate::pool::testing::fake_jwt;
    use crate::pool::testing::test_app_with;
    use crate::pool::testing::write_test_codex_auth_tokens;

    // Go TestCodexRefreshFailureIsPermanentAsCodexDecides.
    #[test]
    fn codex_refresh_failure_is_permanent_as_codex_decides() {
        for (status, body, want) in [
            (400, r#"{"code":"refresh_token_expired"}"#, true),
            (
                401,
                r#"{"error": { "code": "refresh_token_invalidated" }}"#,
                true,
            ),
            (
                400,
                r#"{"error":"invalid_grant","error_description":"refresh token expired"}"#,
                true,
            ),
            (401, "service unavailable", true),
            (400, r#"{"error":"temporarily_unavailable"}"#, false),
            (
                400,
                r#"{"error":{"code":"proxy_auth_required","message":"proxy authentication required"}}"#,
                false,
            ),
            (503, "service unavailable", false),
            (500, "  ", false),
        ] {
            assert_eq!(
                codex_refresh_failure_is_permanent(status, body.as_bytes()),
                want,
                "{status} {body}"
            );
        }
    }

    // Go TestRefusedTokenRefreshFailsFastUntilTokensChange.
    #[tokio::test]
    async fn refused_token_refresh_fails_fast_until_tokens_change() {
        let refresh = FakeServer::start(|_| {
            // app-server tests/suite/v2/thread_start.rs, verbatim.
            FakeResponse::json(401, r#"{"error": { "code": "refresh_token_invalidated" }}"#)
        })
        .await;
        let url = refresh.url.clone();
        let (app, _dir) = test_app_with(vec![device_account("stale", "", 100)], |settings| {
            settings.refresh_url = url
        });
        write_test_codex_auth_tokens(
            &app,
            "stale",
            &fake_jwt(Timestamp::now().add(minutes(1))),
            "<refresh-token>",
        );
        let item = app.read().config.accounts()[0].clone();
        for attempt in 0..3 {
            let error = app
                .refresh_codex_auth(&item, false)
                .await
                .err()
                .unwrap_or_else(|| panic!("refresh {attempt} succeeded"));
            assert!(
                error.auth_failed && error.refused,
                "refresh {attempt}: {error:?}"
            );
        }
        assert_eq!(refresh.count(), 1, "a refused refresh was sent again");
        assert!(app.credential_refused(&item));
        write_test_codex_auth_tokens(
            &app,
            "stale",
            &fake_jwt(Timestamp::now().add(minutes(2))),
            "<new-refresh-token>",
        );
        let _ = app.refresh_codex_auth(&item, false).await;
        assert_eq!(refresh.count(), 2, "new tokens were not refreshed");
    }

    fn rotated_refresh_reply() -> FakeResponse {
        FakeResponse::json(
            200,
            &format!(
                r#"{{"access_token":"{}","refresh_token":"<rotated>"}}"#,
                fake_jwt(Timestamp::now().add(super::super::time::hours(1)))
            ),
        )
    }

    // A request whose client went away while upstream was answering the
    // refresh used to drop the exchange with it: upstream had rotated the
    // refresh token, auth.json still held the spent one.
    #[tokio::test]
    async fn a_refresh_settles_after_its_caller_is_dropped() {
        let refresh = FakeServer::start(|_| {
            let mut reply = rotated_refresh_reply();
            reply.chunks[0].0 = std::time::Duration::from_millis(300);
            reply
        })
        .await;
        let url = refresh.url.clone();
        let (app, _dir) = test_app_with(
            vec![device_account("device", "acct-device", 100)],
            |settings| settings.refresh_url = url,
        );
        write_test_codex_auth_tokens(
            &app,
            "device",
            &fake_jwt(Timestamp::now().add(minutes(1))),
            "<refresh>",
        );
        let item = app.read().config.accounts()[0].clone();
        let dropped = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            app.refreshed_codex_auth(&item),
        )
        .await;
        assert!(dropped.is_err(), "the caller was not dropped mid-refresh");
        // The next request waits for the settling refresh rather than
        // spending the same refresh token again.
        let auth = app
            .refreshed_codex_auth(&item)
            .await
            .unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(refresh.count(), 1, "the spent refresh token was sent again");
        let file = app
            .read()
            .read_codex_auth_file(&item)
            .unwrap_or_else(|error| panic!("{error:?}"));
        let tokens = file.tokens.unwrap_or_default();
        assert_eq!(tokens.refresh_token, "<rotated>");
        assert_eq!(auth.access_token, tokens.access_token);
        assert_eq!(
            app.refreshes_settling
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    // A sign-in finished while a refresh was out wins over the old login's
    // rotated tokens (Codex 0.168 persist_tokens).
    #[tokio::test]
    async fn a_refresh_never_overwrites_a_new_login() {
        let new_login = fake_jwt(Timestamp::now().add(super::super::time::hours(2)));
        let auth_file: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let signed_in = (auth_file.clone(), new_login.clone());
        let refresh = FakeServer::start(move |_| {
            let (path, access) = &signed_in;
            if let Some(path) = path.lock().ok().and_then(|path| path.clone()) {
                let login = serde_json::json!({"auth_mode": "chatgpt", "tokens": {"id_token": "", "access_token": access, "refresh_token": "<new-login>"}});
                if let Err(error) = std::fs::write(path, login.to_string()) {
                    panic!("{error}");
                }
            }
            rotated_refresh_reply()
        })
        .await;
        let url = refresh.url.clone();
        let (app, _dir) = test_app_with(
            vec![device_account("device", "acct-device", 100)],
            |settings| settings.refresh_url = url,
        );
        write_test_codex_auth_tokens(
            &app,
            "device",
            &fake_jwt(Timestamp::now().add(minutes(1))),
            "<refresh>",
        );
        if let Ok(mut path) = auth_file.lock() {
            *path = Some(app.auth_path("device"));
        }
        let item = app.read().config.accounts()[0].clone();
        let auth = app
            .refreshed_codex_auth(&item)
            .await
            .unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(refresh.count(), 1);
        assert_eq!(auth.access_token, new_login);
        let file = app
            .read()
            .read_codex_auth_file(&item)
            .unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(file.tokens.unwrap_or_default().refresh_token, "<new-login>");
    }

    // Go TestPoolOwnCallsPresentTheAccountsCodex (the refresh half; the usage
    // half is with the quota refresh).
    #[tokio::test]
    async fn refresh_carries_the_accounts_codex_default_client_agent() {
        let refresh = FakeServer::start(|_| {
            FakeResponse::json(
                200,
                &format!(
                    r#"{{"access_token":"{}","refresh_token":"<rotated>"}}"#,
                    fake_jwt(Timestamp::now().add(super::super::time::hours(1)))
                ),
            )
        })
        .await;
        let url = refresh.url.clone();
        let (app, _dir) = test_app_with(
            vec![device_account("device", "acct-device", 100)],
            |settings| settings.refresh_url = url,
        );
        let mut client = HeaderMap::new();
        client.insert(
            "user-agent",
            http::HeaderValue::from_static(
                "codex-tui/0.163.0 (Mac OS 26.0.0; arm64) iTerm.app/3.6 (Codex_Desktop; 0.163.0)",
            ),
        );
        client.insert(
            "originator",
            http::HeaderValue::from_static("Codex_Desktop"),
        );
        app.note_client_agent("device", &client);
        let refreshed = app
            .request_codex_token_refresh("device", "<refresh>", RefreshReason::ProactiveExpiry)
            .await;
        assert!(refreshed.is_ok(), "{refreshed:?}");
        let request = refresh.requests().remove(0);
        // A native Codex's refresh carries its default client's agent, without
        // the suffix its conversation requests add.
        assert_eq!(
            request.header("user-agent"),
            "codex-tui/0.163.0 (Mac OS 26.0.0; arm64) iTerm.app/3.6"
        );
        assert_eq!(request.header("originator"), "Codex_Desktop");
        assert_eq!(request.header("content-type"), "application/json");
        assert_eq!(
            request.body_string(),
            r#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann","grant_type":"refresh_token","refresh_reason":"proactive_expiry","refresh_token":"<refresh>"}"#
        );
        // A foreign client never becomes the identity of the pool's own calls.
        let mut foreign = HeaderMap::new();
        foreign.insert(
            "user-agent",
            http::HeaderValue::from_static("Python-urllib/3.12"),
        );
        app.note_client_agent("device", &foreign);
        assert_eq!(
            app.client_agent_for("device").user_agent,
            super::super::http_io::header_value(&client, "user-agent")
        );
        // An account no Codex has used takes the last Codex seen.
        assert_eq!(
            app.client_agent_for("unused").user_agent,
            super::super::http_io::header_value(&client, "user-agent")
        );
    }

    // Go TestCodexJSONMatchesSerde (the agent half).
    #[test]
    fn default_client_agent_drops_only_the_suffix() {
        assert_eq!(
            codex_default_client_user_agent(
                "codex_exec/0.163.0 (Alpine Linux 3.21.7; x86_64) unknown (codex_exec; 0.163.0)"
            ),
            "codex_exec/0.163.0 (Alpine Linux 3.21.7; x86_64) unknown"
        );
        assert_eq!(
            codex_default_client_user_agent(
                "codex_exec/0.163.0 (Alpine Linux 3.21.7; x86_64) unknown"
            ),
            "codex_exec/0.163.0 (Alpine Linux 3.21.7; x86_64) unknown"
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "user-agent",
            http::HeaderValue::from_static("codex_exec/0.163.0 (Linux; x86_64)"),
        );
        assert_eq!(codex_client_version(&headers), "0.163.0");
        headers.insert("user-agent", http::HeaderValue::from_static("curl/8.0"));
        assert_eq!(codex_client_version(&headers), "");
    }

    /// A repository file, read at test time (never compiled in).
    fn repository_file(path: &str) -> String {
        let full = format!("{}/../{path}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&full).unwrap_or_else(|error| panic!("{full}: {error}"))
    }

    fn fixture_release() -> String {
        let fixture: serde_json::Value = serde_json::from_str(&repository_file(
            "upstream/testdata/codex-upstream-failures.json",
        ))
        .unwrap_or_default();
        fixture["codexRelease"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    // Go TestBundledCodexMatchesThePinnedRelease: one Codex release, named in
    // three places: the crates the pool is built from, the CLI bundled
    // for sign-in and priming, and the version the pool's own calls present.
    // The bundled CLI once stayed at 0.141.0 while the rest moved to 0.163.0,
    // so sign-in traffic came from an older Codex than everything else.
    #[test]
    fn bundled_codex_matches_the_pinned_release() {
        assert_eq!(
            fixture_release(),
            format!("rust-v{PINNED_CODEX_VERSION}"),
            "PINNED_CODEX_VERSION does not match the pinned Codex tag"
        );
        let dockerfile = repository_file("Dockerfile");
        let bundled = dockerfile
            .lines()
            .find_map(|line| line.trim().strip_prefix("ARG CODEX_CLI_VERSION="))
            .map(str::trim);
        assert_eq!(
            bundled,
            Some(PINNED_CODEX_VERSION),
            "the Dockerfile bundles another Codex"
        );
    }

    // Go TestCodexShapeFixtureTracksThePinnedCodexRelease: bumping the Codex
    // tag without revisiting the shapes that release parses is exactly how
    // the pool drifts from Codex.
    #[test]
    fn codex_shape_fixture_tracks_the_pinned_codex_release() {
        let release = fixture_release();
        let cargo = repository_file("upstream/Cargo.toml");
        let tags: Vec<&str> = cargo
            .split(r#"openai/codex", tag = ""#)
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .collect();
        assert!(!tags.is_empty(), "upstream/Cargo.toml names no Codex tag");
        for tag in tags {
            assert_eq!(
                tag, release,
                "upstream/Cargo.toml pins Codex {tag} but the shape fixture describes {release}; re-run scripts/conformance/codex-conformance.sh and update the fixture"
            );
        }
    }
}
