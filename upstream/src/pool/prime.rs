//! Starting a quota window that reset and has not started counting down.
//!
//! A port of primeUnanchoredQuotaWindow and its helpers (main.go) and of
//! codex_prime.go. A window that has been spent against reports a fixed reset
//! instant that draws closer on every refresh; one that reset and has seen no
//! usage reports a reset a full window ahead that slides with the clock, so
//! the quota never cycles. The pool first gives the owner a random 30 to 120
//! minutes to start it by using the account, then spends the smallest request
//! that registers as usage. A Codex account is primed by one turn of the
//! bundled Codex CLI, so upstream receives a native Codex request rather than
//! one the pool composed.

use super::app::App;
use super::app::Inner;
use super::plan;
use super::quota;
use super::state::Account;
use super::state::AccountQuota;
use super::state::QuotaSnapshot;
use super::state::QuotaWindow;
use super::time::Timestamp;
use super::time::hours;
use super::time::minutes;
use bytes::Bytes;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// How far from a full window ahead a reset may sit and still count as
/// untouched: the age of the snapshot that reported it, well below the
/// smallest anchored gap observed in practice.
const QUOTA_WINDOW_ANCHOR_SLACK_MINUTES: i64 = 10;
/// A successful prime anchors the window and stops the detector on its own;
/// this only bounds the cost when a prime fails or upstream does not anchor.
const QUOTA_PRIME_MIN_INTERVAL_HOURS: i64 = 1;
/// Deliberately trivial: the goal is to spend the smallest amount of quota
/// that still registers as usage, never to obtain an answer.
pub const QUOTA_PRIME_PROMPT: &str = "What day of the week is it today? Answer with one word.";
const QUOTA_PRIME_TIMEOUT: Duration = Duration::from_secs(90);
/// The owner's first chance to start the window by using the account. Long
/// on purpose: a prime is a request no person sent, so the owner gets up to
/// two hours to start the window with real work first.
const QUOTA_PRIME_GRACE_MIN: Duration = Duration::from_secs(30 * 60);
const QUOTA_PRIME_GRACE_MAX: Duration = Duration::from_secs(120 * 60);
/// Codex's installation id file in its home (core installation_id.rs).
const CODEX_INSTALLATION_ID_FILE: &str = "installation_id";

/// Runs one `codex exec` turn against a prepared home (tests replace it).
pub type PrimeRunner = dyn Fn(&Path, &Path, &str) -> futures::future::BoxFuture<'static, Result<(), String>>
    + Send
    + Sync;

/// The account's longest reported window when it has reset but not started
/// counting down. The longest window is the budget rather than the burst
/// limit, and a single request counts against every window at once, so
/// anchoring the budget window anchors the shorter ones too.
pub fn unanchored_budget_window(quota: &AccountQuota, now: Timestamp) -> Option<QuotaWindow> {
    let mut budget: Option<QuotaWindow> = None;
    for window in quota::quota_reported_windows(quota) {
        let Some(length) = window.window_minutes.filter(|length| *length > 0) else {
            continue;
        };
        if !window.present || window.reset_at.is_none() {
            continue;
        }
        if budget
            .as_ref()
            .is_none_or(|current| length > current.window_minutes.unwrap_or_default())
        {
            budget = Some(window);
        }
    }
    let budget = budget?;
    // Anything already spent has anchored the window by definition, so only
    // an untouched window is a candidate. This also keeps a window that
    // anchored moments ago, and so still sits close to a full window ahead,
    // from being primed a second time.
    if quota::quota_window_remaining(&budget) < 100.0 {
        return None;
    }
    let full = minutes(budget.window_minutes.unwrap_or_default());
    let remaining = Timestamp::from_unix(budget.reset_at.unwrap_or_default()).sub(&now);
    if remaining < full - minutes(QUOTA_WINDOW_ANCHOR_SLACK_MINUTES) {
        return None;
    }
    Some(budget)
}

/// A random grace in [30, 120] minutes.
pub fn random_quota_prime_grace() -> Duration {
    let unit = super::crypto::random_unit();
    QUOTA_PRIME_GRACE_MIN + (QUOTA_PRIME_GRACE_MAX - QUOTA_PRIME_GRACE_MIN).mul_f64(unit)
}

/// What a Codex child process may see: where to find itself and its home,
/// locale, and the CA and proxy settings the container gives every outbound
/// client. Nothing else from the pool's environment.
pub fn codex_child_environment(
    home: &Path,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("CODEX_HOME".to_string(), home.display().to_string()),
        (
            "HOME".to_string(),
            home.parent().unwrap_or(home).display().to_string(),
        ),
    ];
    for name in [
        "PATH",
        "LANG",
        "LC_ALL",
        "TZ",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CODEX_CA_CERTIFICATE",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "no_proxy",
    ] {
        if let Some(value) = lookup(name) {
            env.push((name.to_string(), value));
        }
    }
    env
}

/// Runs one ephemeral, read-only `codex exec` turn. Its output is the model's
/// answer and Codex's own diagnostics; neither is kept, and an error reports
/// only the exit status. The whole process group is killed on timeout.
pub async fn run_codex_exec_prime(
    cli: &str,
    home: &Path,
    workdir: &Path,
    model: &str,
) -> Result<(), String> {
    let mut command = tokio::process::Command::new(cli);
    command
        .args([
            "exec",
            "--skip-git-repo-check",
            "--ephemeral",
            "--sandbox",
            "read-only",
            "--color",
            "never",
            "-m",
            model,
            QUOTA_PRIME_PROMPT,
        ])
        .current_dir(workdir)
        .env_clear()
        .envs(codex_child_environment(home, |name| {
            std::env::var(name).ok()
        }))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("codex exec: {error}"))?;
    let status = child
        .wait()
        .await
        .map_err(|error| format!("codex exec: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("codex exec: {status}"))
    }
}

/// Whether the CLI exists: a path as given, or a name on PATH (Go
/// exec.LookPath).
fn codex_cli_available(cli: &str) -> bool {
    if cli.contains('/') {
        return Path::new(cli).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(cli).is_file()))
}

/// The prime-grace bookkeeping: when each account's wait ends. In memory:
/// after a restart the window is seen again and gets a new wait.
#[derive(Default)]
pub struct PrimeGrace {
    until: Mutex<HashMap<String, Timestamp>>,
}

impl PrimeGrace {
    fn until(&self) -> std::sync::MutexGuard<'_, HashMap<String, Timestamp>> {
        self.until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub fn set(&self, account_id: &str, at: Timestamp) {
        self.until().insert(account_id.to_string(), at);
    }
}

impl Inner {
    /// The single slot allowed to prime a shared upstream identity. The
    /// routing duplicate guard cannot be reused here: it ignores out-of-pool
    /// slots, while priming covers every enabled slot, so two copies of one
    /// workspace would each spend a prime for the same window. The lowest
    /// slot id wins so the choice is stable across refreshes and restarts,
    /// and a slot whose identity cannot be determined owns itself.
    pub fn quota_prime_identity_owner(&self, item: &Account) -> String {
        let identity = self.upstream_identity_key(item);
        if identity.is_empty() {
            return item.id.clone();
        }
        self.config
            .accounts()
            .iter()
            .filter(|candidate| {
                candidate.enabled
                    && self.has_usable_auth(candidate)
                    && self.upstream_identity_key(candidate) == identity
            })
            .map(|candidate| candidate.id.clone())
            .min()
            .unwrap_or_else(|| item.id.clone())
    }

    /// The model a prime names. The account's own filters decide it: a prime
    /// must never reach for a model this slot is not allowed to route.
    pub fn quota_prime_model(&self, item: &Account) -> String {
        item.allowed_models
            .iter()
            .chain(std::iter::once(&self.config.default_model))
            .map(|candidate| super::proxy::parse_model(candidate).0)
            .find(|base| !base.is_empty() && plan::allowed_model(item, base))
            .unwrap_or_default()
    }
}

impl App {
    /// Starts the account's grace the first time its window is seen
    /// unanchored and reports whether it has run out.
    pub fn prime_grace_elapsed(&self, account_id: &str, now: Timestamp) -> bool {
        let mut until = self.prime_grace.until();
        let at = *until.entry(account_id.to_string()).or_insert_with(|| {
            let grace = self
                .settings
                .quota_prime_grace
                .unwrap_or_else(random_quota_prime_grace);
            now.add(chrono::TimeDelta::from_std(grace).unwrap_or_default())
        });
        if now.before(&at) {
            return false;
        }
        until.remove(account_id);
        true
    }

    pub fn clear_prime_grace(&self, account_id: &str) {
        self.prime_grace.until().remove(account_id);
    }

    /// When the account must be polled again to decide its prime.
    pub fn prime_grace_due(&self, account_id: &str) -> Option<Timestamp> {
        self.prime_grace.until().get(account_id).copied()
    }

    /// Spends a minimal amount of an account's quota so a window that has
    /// reset actually starts counting down.
    ///
    /// This skips the proxy handler, so it creates no sticky or thread
    /// affinity, response bindings, prompt-cache statistics, throughput
    /// results or routing events, and does not touch account health: it is
    /// pool bookkeeping rather than a client request. It does not skip the
    /// account's transport: the request goes out over the path that slot
    /// normally uses, with the account's own credential, because charging
    /// that identity is the point.
    pub async fn prime_unanchored_quota_window(&self, account_id: &str, snapshot: &QuotaSnapshot) {
        let now = Timestamp::now();
        let (candidate, model, label) = {
            let mut inner = self.write();
            let Some(item) = inner.account(account_id).cloned() else {
                return;
            };
            let Some(quota) = snapshot
                .quota
                .as_ref()
                .filter(|_| snapshot.quota_error.is_none())
            else {
                return;
            };
            // Nothing when the owner turned priming off.
            if !inner.quota_prime {
                return;
            }
            // Pool membership is not a condition. A slot kept out of the pool
            // still owns a quota window that has to start counting down, or
            // the window is still pinned whenever the slot is put back.
            if !item.enabled || inner.account_auth_verification_pending(&item) {
                return;
            }
            // Several slots can hold credentials for one upstream workspace,
            // and priming spends that workspace's quota, so only one slot may
            // spend it per window.
            if inner.quota_prime_identity_owner(&item) != item.id {
                return;
            }
            let Some(window) = unanchored_budget_window(quota, now) else {
                self.clear_prime_grace(account_id);
                return;
            };
            // The user gets the first chance to start the window by using the
            // account; only a window still untouched after the grace is primed.
            if !self.prime_grace_elapsed(account_id, now) {
                return;
            }
            let model = inner.quota_prime_model(&item);
            if model.is_empty() || inner.account_cooling_down(&item.id, &model, now) {
                return;
            }
            if let Some(last) = inner.state.quota_window_primes().get(account_id)
                && now.sub(last) < hours(QUOTA_PRIME_MIN_INTERVAL_HOURS)
            {
                return;
            }
            // Record the attempt before making it. A prime that fails midway
            // must still consume its interval, or a persistently failing
            // account would be retried on every refresh sweep.
            inner
                .state
                .quota_window_primes_mut()
                .insert(account_id.to_string(), now);
            if let Err(error) = inner.save() {
                self.logger.printf(&format!(
                    "quota window prime state save failed for {account_id}: {error}"
                ));
            }
            let label = if window.label.is_empty() {
                "budget".to_string()
            } else {
                window.label
            };
            (item, model, label)
        };
        match self.send_quota_prime_request(&candidate, &model).await {
            Ok(()) => self.logger.printf(&format!("quota window primed for {account_id} ({label} window, model {model})")),
            Err(error) => self.logger.printf(&format!("quota window prime failed for {account_id} ({label} window, model {model}): {error}")),
        }
    }

    pub async fn send_quota_prime_request(
        &self,
        candidate: &Account,
        model: &str,
    ) -> Result<(), String> {
        // A Codex account is primed by Codex itself, so upstream sees a native
        // Codex request rather than one the pool composed. The composed
        // request below remains only for API-key providers, which are not
        // Codex backend clients.
        if Self::uses_codex_backend_directly(candidate) {
            return self.prime_with_codex_cli(candidate, model).await;
        }
        let body = super::gojson::marshal(
            &serde_json::json!({"model": model, "input": QUOTA_PRIME_PROMPT, "stream": false}),
        );
        let (endpoint, outbound, _) =
            self.prepare_upstream_request(candidate, &Bytes::from(body), false)?;
        let sent = tokio::time::timeout(QUOTA_PRIME_TIMEOUT, async {
            let response = self
                .forward(
                    candidate,
                    &endpoint,
                    outbound,
                    &http::HeaderMap::new(),
                    super::proxy::Shaping::Adapt,
                )
                .await
                .map_err(|error| error.to_string())?;
            // The answer is worthless; only the usage it recorded matters.
            // Drain a bounded prefix so the connection can be reused.
            let status = response.status;
            let _ = super::stream::SseReader::new(response.body)
                .read_all(1 << 20)
                .await;
            if status.is_success() {
                Ok(())
            } else {
                Err(format!("upstream returned {}", status.as_u16()))
            }
        })
        .await;
        sent.unwrap_or_else(|_| Err("quota prime timed out".to_string()))
    }

    /// One turn of the bundled Codex CLI against a temporary CODEX_HOME that
    /// holds the account's current access and ID tokens, no refresh token,
    /// and the account's installation id. Refresh tokens rotate on use: a
    /// CLI that refreshed would revoke the pool's own copy and sign the
    /// account out, so it is given nothing to refresh with, and last_refresh
    /// is now so Codex sees no reason to try. Nothing is written back; the
    /// directory is removed after. Without the CLI the prime is skipped, not
    /// replaced by a composed request.
    pub async fn prime_with_codex_cli(
        &self,
        candidate: &Account,
        model: &str,
    ) -> Result<(), String> {
        let runner = self
            .prime_runner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if runner.is_none() && !codex_cli_available(&self.settings.codex_cli) {
            return Err("bundled Codex CLI is unavailable; quota prime skipped".to_string());
        }
        let auth = self
            .refreshed_codex_auth(candidate)
            .await
            .map_err(|error| format!("account credential: {}", error.message))?;
        let file = self
            .read()
            .read_codex_auth_file(candidate)
            .map_err(|_| "account credential file is unreadable".to_string())?;
        let tokens = file
            .tokens
            .ok_or_else(|| "account credential file is unreadable".to_string())?;
        let root = TempHome::new()?;
        let home = root.path().join("home");
        let workdir = root.path().join("work");
        for dir in [&home, &workdir] {
            std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        let account_id = if auth.account_id.is_empty() {
            candidate.account_id.clone()
        } else {
            auth.account_id.clone()
        };
        let prime_auth = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": tokens.id_token,
                "access_token": auth.access_token,
                "refresh_token": "",
                "account_id": account_id,
            },
            "last_refresh": Timestamp::now(),
        });
        write_private(
            &home.join("auth.json"),
            &super::gojson::marshal(&prime_auth),
        )?;
        // The CLI sends the installation id in its home; a temporary home
        // would make a new one for every prime. It gets the account's own,
        // the one the account presents on every request.
        let installation = self
            .presented_installation_id(candidate)
            .ok_or_else(|| "account installation id is unavailable".to_string())?;
        std::fs::write(home.join(CODEX_INSTALLATION_ID_FILE), &installation)
            .map_err(|error| error.to_string())?;
        let config = format!(
            "cli_auth_credentials_store = \"file\"\nmodel = {}\n",
            serde_json::Value::String(model.to_string())
        );
        write_private(&home.join("config.toml"), config.as_bytes())?;
        let run = match runner {
            Some(runner) => runner(&home, &workdir, model),
            None => {
                let (cli, home, workdir, model) = (
                    self.settings.codex_cli.clone(),
                    home.clone(),
                    workdir.clone(),
                    model.to_string(),
                );
                Box::pin(async move { run_codex_exec_prime(&cli, &home, &workdir, &model).await })
            }
        };
        tokio::time::timeout(QUOTA_PRIME_TIMEOUT, run)
            .await
            .unwrap_or_else(|_| Err("codex exec: timed out".to_string()))
    }

    /// Sets the prime runner (tests stand in for the bundled CLI).
    #[cfg(test)]
    pub fn set_prime_runner(&self, runner: std::sync::Arc<PrimeRunner>) {
        *self
            .prime_runner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(runner);
    }
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| error.to_string())?;
    std::io::Write::write_all(&mut file, data).map_err(|error| error.to_string())
}

/// A private temporary directory, removed when dropped (Go os.MkdirTemp plus
/// the deferred RemoveAll).
struct TempHome(std::path::PathBuf);

impl TempHome {
    fn new() -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::env::temp_dir().join(format!("codex-prime-{}", super::crypto::random_id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|error| error.to_string())?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
