//! The pool's startup settings, read from the environment once.
//!
//! A port of newAppFromEnv's environment handling in the Go pool's main.go.
//! Every invalid value is a startup error, never a silent fallback, and the
//! defaults are the product contract SPEC.md records (balanced routing,
//! lineage affinity on, priming on, the public dashboard on).

use super::crypto;
use super::time::millis;
use chrono::TimeDelta;
use std::path::PathBuf;
use std::time::Duration;

pub const LISTEN_ADDRESS_DEFAULT: &str = ":8317";
pub const CODEX_BASE_URL_DEFAULT: &str = "https://chatgpt.com/backend-api";
pub const CODEX_REFRESH_URL_DEFAULT: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_OAUTH_CLIENT_ID_DEFAULT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const DEFAULT_MODEL: &str = "gpt-6.1-sol(xhigh)";
pub const ROUTING_STRATEGY_BALANCED: &str = "sticky_balanced";
pub const ROUTING_STRATEGY_FAILOVER: &str = "sticky_failover";
pub const SESSION_AFFINITY_TTL_DEFAULT_HOURS: i64 = 24;
/// Codex turns routinely stream for one to two minutes, so anything shorter
/// cuts ordinary work. compose.yaml's stop_grace_period must stay above it, or
/// Docker kills the container before the drain ends.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(120);
/// Spreads a coarse (project/user) prompt cache key across a few buckets so a
/// hot scope stays under OpenAI's ~15 RPM per (prefix + prompt_cache_key)
/// limit while still sharing the static prefix across conversations. 4 covers
/// a heavy single user (~60 RPM) before any overflow.
pub const PROMPT_CACHE_BUCKETS_DEFAULT: i64 = 4;

#[derive(Clone, Debug)]
pub struct Settings {
    pub data_dir: PathBuf,
    pub api_keys: Vec<String>,
    pub admin_user: String,
    pub admin_hash: String,
    pub session_key: Vec<u8>,
    pub session_affinity_ttl: TimeDelta,
    pub max_retry_accounts: usize,
    pub routing_strategy: String,
    pub prompt_cache_key_mode: String,
    pub prompt_cache_key_scope: String,
    pub prompt_cache_key_policy: String,
    pub prompt_cache_buckets: i64,
    pub prompt_cache_retention: String,
    pub listen_address: String,
    pub public_dashboard: bool,
    pub codex_base_url: String,
    pub default_model_override: Option<String>,
    pub shutdown_grace: Duration,
    /// The starting values of the Console switches; config.json overrides
    /// them at load (Go app.preserveProQuota and friends).
    pub preserve_pro_quota: bool,
    pub lineage_affinity: bool,
    pub quota_prime: bool,
    /// CODEX_POOL_QUOTA_PRIME_GRACE: a fixed grace for tests; None draws one
    /// at random (codex_prime.go, quota prime grace).
    pub quota_prime_grace: Option<Duration>,
    /// Codex's token endpoint and OAuth client (CODEX_REFRESH_TOKEN_URL_OVERRIDE
    /// and CODEX_APP_SERVER_LOGIN_CLIENT_ID, the variables Codex itself reads).
    pub refresh_url: String,
    pub oauth_client_id: String,
    /// Endpoint overrides for the pool's own calls (tests, wire-capture);
    /// empty means the Codex backend's path under codex_base_url.
    pub usage_url: String,
    pub reset_credit_consume_url: String,
    pub reset_credits_url: String,
    pub accounts_check_url: String,
    /// The bundled Codex CLI the pool signs slots in and primes windows with
    /// (`codex` on PATH; tests point it at a stand-in script, since PATH is
    /// one per process and tests run in parallel).
    pub codex_cli: String,
    /// CODEX_POOL_COOKIE_SECURE=true marks the owner's cookies Secure even
    /// without a forwarded https.
    pub cookie_secure: bool,
    /// CODEX_POOL_TRUSTED_PROXIES: the networks of reverse proxies whose
    /// X-Forwarded-For names the real client. Behind a proxy every connection
    /// comes from the proxy, so a lockout keyed on it would make everyone
    /// share one failure count; a forwarded address from anywhere else is the
    /// client's own claim and is ignored.
    pub trusted_proxies: Vec<(std::net::IpAddr, u8)>,
}

/// The test fixture's admin password hash: "admin-password" with a fixed
/// salt at the minimum iteration count (Go testApp's
/// newPasswordHash("admin-password")). Computed once per process: a hash
/// literal in the source would be one more password hash in the repository,
/// which the pre-commit audit rightly refuses.
fn test_admin_password_hash() -> String {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        use base64::Engine;
        let engine = base64::engine::general_purpose::STANDARD_NO_PAD;
        let salt = [3u8; 16];
        let derived = crypto::pbkdf2_sha256(b"admin-password", &salt, 100_000, 32);
        format!(
            "pbkdf2-sha256:100000:{}:{}",
            engine.encode(salt),
            engine.encode(derived)
        )
    })
    .clone()
}

impl Settings {
    /// The Go tests' testApp fixture: failover routing (most routing tests
    /// assert strict account order; balanced ones opt in), lineage affinity,
    /// no prime grace, fixed keys.
    pub fn for_test(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            api_keys: vec!["client-key".to_string()],
            admin_user: "admin".to_string(),
            admin_hash: test_admin_password_hash(),
            session_key: b"01234567890123456789012345678901".to_vec(),
            session_affinity_ttl: TimeDelta::hours(SESSION_AFFINITY_TTL_DEFAULT_HOURS),
            max_retry_accounts: 0,
            routing_strategy: ROUTING_STRATEGY_FAILOVER.to_string(),
            prompt_cache_key_mode: "auto".to_string(),
            prompt_cache_key_scope: "auto".to_string(),
            prompt_cache_key_policy: "preserve".to_string(),
            prompt_cache_buckets: PROMPT_CACHE_BUCKETS_DEFAULT,
            prompt_cache_retention: String::new(),
            listen_address: "127.0.0.1:0".to_string(),
            public_dashboard: true,
            codex_base_url: "https://chatgpt.example.test/backend-api".to_string(),
            default_model_override: None,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
            preserve_pro_quota: false,
            lineage_affinity: true,
            quota_prime: false,
            quota_prime_grace: Some(Duration::ZERO),
            refresh_url: CODEX_REFRESH_URL_DEFAULT.to_string(),
            oauth_client_id: CODEX_OAUTH_CLIENT_ID_DEFAULT.to_string(),
            usage_url: String::new(),
            reset_credit_consume_url: String::new(),
            reset_credits_url: String::new(),
            accounts_check_url: String::new(),
            codex_cli: "codex".to_string(),
            cookie_secure: false,
            trusted_proxies: Vec::new(),
        }
    }

    pub fn codex_url(&self, override_url: &str, path: &str) -> String {
        if override_url.trim().is_empty() {
            format!("{}{path}", self.codex_base_url.trim_end_matches('/'))
        } else {
            override_url.trim().to_string()
        }
    }

    pub fn codex_usage_url(&self) -> String {
        self.codex_url(&self.usage_url, "/wham/usage")
    }

    /// The upstream endpoint that spends one earned reset credit and clears
    /// the account's eligible rate-limit windows.
    pub fn codex_reset_credit_consume_url(&self) -> String {
        self.codex_url(
            &self.reset_credit_consume_url,
            "/wham/rate-limit-reset-credits/consume",
        )
    }

    pub fn codex_reset_credits_url(&self) -> String {
        self.codex_url(&self.reset_credits_url, "/wham/rate-limit-reset-credits")
    }

    pub fn codex_accounts_check_url(&self) -> String {
        self.codex_url(&self.accounts_check_url, "/wham/accounts/check")
    }

    /// Where a native Codex CLI signed in with ChatGPT sends Responses
    /// requests: the Codex backend under /codex. The bare backend-api root is
    /// only for quota, account, and subscription endpoints; sending inference
    /// there instead reaches a different API.
    pub fn codex_responses_base_url(&self) -> String {
        format!("{}/codex", self.codex_base_url.trim_end_matches('/'))
    }

    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let get = |name: &str| env(name).unwrap_or_default();
        let api_keys = load_api_keys(&get("CODEX_POOL_API_KEYS"), &get("CODEX_POOL_API_KEY"))?;
        let admin_hash = get("CODEX_POOL_ADMIN_PASSWORD_HASH");
        if admin_hash.is_empty() {
            return Err("CODEX_POOL_ADMIN_PASSWORD_HASH is required".to_string());
        }
        if !crypto::valid_password_hash(&admin_hash) {
            return Err(
                "CODEX_POOL_ADMIN_PASSWORD_HASH must be a valid pbkdf2-sha256 hash generated by hash-password"
                    .to_string(),
            );
        }
        let listen_address =
            combined_listen_address(&get("CODEX_POOL_ADDR"), &get("CODEX_POOL_PUBLIC_ADDR"));
        let shutdown_grace = shutdown_grace(&get("CODEX_POOL_SHUTDOWN_GRACE"))?;
        // The provider API and management UI intentionally share one listener.
        // Keep this explicit opt-in for non-loopback binds: merging the ports
        // must not silently expose password login and public repair controls
        // wherever /v1 is reachable.
        let allow_remote = get("CODEX_POOL_ALLOW_REMOTE_ADMIN") == "true";
        if !allow_remote && !is_loopback_address(&listen_address) {
            return Err(
                "combined HTTP address must be loopback unless CODEX_POOL_ALLOW_REMOTE_ADMIN=true"
                    .to_string(),
            );
        }
        let session_affinity_ttl =
            session_affinity_ttl(&get("CODEX_POOL_SESSION_AFFINITY_TTL_MS"))?;
        let max_retry_accounts = max_retry_accounts(&get("CODEX_POOL_MAX_RETRY_ACCOUNTS"))?;
        let routing_strategy = routing_strategy(&get("CODEX_POOL_ROUTING_STRATEGY"))?;
        reject_retired_gateway_mode(&get("CODEX_POOL_CODEX_GATEWAY_MODE"))?;
        let prompt_cache_key_mode =
            prompt_cache_key_mode(&get("CODEX_POOL_PROMPT_CACHE_KEY_MODE"))?;
        let prompt_cache_key_scope =
            prompt_cache_key_scope(&get("CODEX_POOL_PROMPT_CACHE_KEY_SCOPE"))?;
        let prompt_cache_key_policy =
            prompt_cache_key_policy(&get("CODEX_POOL_PROMPT_CACHE_KEY_POLICY"))?;
        let prompt_cache_buckets = prompt_cache_buckets(&get("CODEX_POOL_PROMPT_CACHE_BUCKETS"))?;
        let prompt_cache_retention =
            prompt_cache_retention(&get("CODEX_POOL_PROMPT_CACHE_RETENTION"))?;
        let preserve_pro_quota = bool_from_env(
            &get("CODEX_POOL_PRESERVE_PRO_QUOTA"),
            "CODEX_POOL_PRESERVE_PRO_QUOTA",
            false,
        )?;
        // Lineage affinity defaults on: splitting a main agent and its
        // subagents across accounts was the reported bug, so only an explicit
        // opt-out may restore the old first-request-only parent preference.
        let lineage_affinity = bool_from_env(
            &get("CODEX_POOL_LINEAGE_AFFINITY"),
            "CODEX_POOL_LINEAGE_AFFINITY",
            true,
        )?;
        // Quota priming defaults on, the owner's choice: an idle account's
        // window should start counting so its quota cycles back. A prime is a
        // request no native Codex sends, so it waits a random 30 to 120 minutes
        // for a real request to start the window first, and the Console switch
        // turns it off (SPEC.md, quota window priming).
        let quota_prime = bool_from_env(
            &get("CODEX_POOL_QUOTA_PRIME"),
            "CODEX_POOL_QUOTA_PRIME",
            true,
        )?;
        let quota_prime_grace = quota_prime_grace(&get("CODEX_POOL_QUOTA_PRIME_GRACE"))?;
        // Product contract: the combined-port root is a public control page,
        // while management actions require password auth. A previous hardening
        // pass flipped this default and broke the expected landing page, so
        // keep the default true unless the operator explicitly hides it.
        let public_dashboard = bool_from_env(
            &get("CODEX_POOL_PUBLIC_DASHBOARD"),
            "CODEX_POOL_PUBLIC_DASHBOARD",
            true,
        )?;
        let default_model_override = Some(get("CODEX_POOL_DEFAULT_MODEL").trim().to_string())
            .filter(|model| !model.is_empty());
        let data_dir = env_or(&get("CODEX_POOL_DATA_DIR"), "/data");
        let admin_user = env_or(&get("CODEX_POOL_ADMIN_USERNAME"), "admin");
        let codex_base_url = env_or(&get("CODEX_POOL_CODEX_BASE_URL"), CODEX_BASE_URL_DEFAULT)
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            data_dir: PathBuf::from(data_dir),
            api_keys,
            admin_user,
            admin_hash,
            session_key: crypto::random_bytes::<32>().to_vec(),
            session_affinity_ttl,
            max_retry_accounts,
            routing_strategy,
            prompt_cache_key_mode,
            prompt_cache_key_scope,
            prompt_cache_key_policy,
            prompt_cache_buckets,
            prompt_cache_retention,
            listen_address,
            public_dashboard,
            codex_base_url,
            default_model_override,
            shutdown_grace,
            preserve_pro_quota,
            lineage_affinity,
            quota_prime,
            quota_prime_grace,
            refresh_url: env_or(
                &get("CODEX_REFRESH_TOKEN_URL_OVERRIDE"),
                CODEX_REFRESH_URL_DEFAULT,
            ),
            oauth_client_id: env_or(
                &get("CODEX_APP_SERVER_LOGIN_CLIENT_ID"),
                CODEX_OAUTH_CLIENT_ID_DEFAULT,
            ),
            usage_url: get("CODEX_POOL_CODEX_USAGE_URL").trim().to_string(),
            reset_credit_consume_url: get("CODEX_POOL_CODEX_RESET_CREDIT_CONSUME_URL")
                .trim()
                .to_string(),
            reset_credits_url: get("CODEX_POOL_CODEX_RESET_CREDITS_URL").trim().to_string(),
            accounts_check_url: get("CODEX_POOL_CODEX_ACCOUNTS_CHECK_URL")
                .trim()
                .to_string(),
            codex_cli: "codex".to_string(),
            cookie_secure: get("CODEX_POOL_COOKIE_SECURE") == "true",
            trusted_proxies: trusted_proxies(&get("CODEX_POOL_TRUSTED_PROXIES"))?,
        })
    }
}

/// Parses CODEX_POOL_TRUSTED_PROXIES: addresses or CIDR networks, comma
/// separated (Go applyTrustedProxiesEnv). A bare IPv4 address is a /32, any
/// other bare address a /128.
pub fn trusted_proxies(raw: &str) -> Result<Vec<(std::net::IpAddr, u8)>, String> {
    let mut networks = Vec::new();
    for part in raw
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let invalid = || format!("CODEX_POOL_TRUSTED_PROXIES: {part:?} is not an address or CIDR");
        let (address, bits) = match part.split_once('/') {
            Some((address, bits)) => (address, Some(bits.parse::<u8>().map_err(|_| invalid())?)),
            None => (part, None),
        };
        let address: std::net::IpAddr = address.parse().map_err(|_| invalid())?;
        let max = if address.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        if bits > max {
            return Err(invalid());
        }
        networks.push((address, bits));
    }
    Ok(networks)
}

fn prefix_matches(network: &[u8], address: &[u8], bits: u8) -> bool {
    let (whole, rest) = (usize::from(bits / 8), bits % 8);
    if network[..whole] != address[..whole] {
        return false;
    }
    rest == 0 || (network[whole] ^ address[whole]) & (0xff_u8 << (8 - rest)) == 0
}

impl Settings {
    /// Whether the host is a configured reverse proxy (Go trustedProxy). An
    /// IPv4 address in IPv6 form counts as the IPv4 address, as in Go.
    pub fn trusted_proxy(&self, host: &str) -> bool {
        let Ok(address) = host.parse::<std::net::IpAddr>() else {
            return false;
        };
        let address = address.to_canonical();
        self.trusted_proxies
            .iter()
            .any(|(network, bits)| match (network.to_canonical(), address) {
                (std::net::IpAddr::V4(network), std::net::IpAddr::V4(address)) => {
                    prefix_matches(&network.octets(), &address.octets(), *bits)
                }
                (std::net::IpAddr::V6(network), std::net::IpAddr::V6(address)) => {
                    prefix_matches(&network.octets(), &address.octets(), *bits)
                }
                _ => false,
            })
    }
}

fn env_or(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

pub fn load_api_keys(list: &str, single: &str) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    for value in list.split(',').chain(std::iter::once(single)) {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if value.contains("replace_with") || value.contains("replace-with") {
            return Err("public API key cannot use an example value".to_string());
        }
        keys.push(value.to_string());
    }
    if keys.is_empty() {
        return Err("CODEX_POOL_API_KEY or CODEX_POOL_API_KEYS is required".to_string());
    }
    Ok(keys)
}

/// CODEX_POOL_PUBLIC_ADDR was the provider-only listener before the HTTP
/// surfaces were merged; it stays an alias so an existing deployment does not
/// move ports.
pub fn combined_listen_address(address: &str, legacy: &str) -> String {
    let address = address.trim();
    if !address.is_empty() {
        return address.to_string();
    }
    env_or(legacy, LISTEN_ADDRESS_DEFAULT)
}

pub fn is_loopback_address(address: &str) -> bool {
    let Some((host, _port)) = address.rsplit_once(':') else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host == "localhost" {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

pub fn shutdown_grace(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(DEFAULT_SHUTDOWN_GRACE);
    }
    // Zero is accepted and means the default (Go serveUntilStopped).
    parse_go_duration(raw).ok_or_else(|| {
        "CODEX_POOL_SHUTDOWN_GRACE must be a non-negative Go duration such as 120s or 5m"
            .to_string()
    })
}

pub fn session_affinity_ttl(raw: &str) -> Result<TimeDelta, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(TimeDelta::hours(SESSION_AFFINITY_TTL_DEFAULT_HOURS));
    }
    match raw.parse::<i64>() {
        Ok(value) if value > 0 => Ok(millis(value)),
        _ => Err(
            "CODEX_POOL_SESSION_AFFINITY_TTL_MS must be a positive integer number of milliseconds"
                .to_string(),
        ),
    }
}

pub fn max_retry_accounts(raw: &str) -> Result<usize, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(0);
    }
    raw.parse::<usize>()
        .map_err(|_| "CODEX_POOL_MAX_RETRY_ACCOUNTS must be zero or a positive integer".to_string())
}

pub fn routing_strategy(raw: &str) -> Result<String, String> {
    let value = env_or(raw.trim(), ROUTING_STRATEGY_BALANCED)
        .trim()
        .to_lowercase();
    match value.as_str() {
        ROUTING_STRATEGY_BALANCED | ROUTING_STRATEGY_FAILOVER => Ok(value),
        _ => Err(
            "CODEX_POOL_ROUTING_STRATEGY must be sticky_balanced or sticky_failover".to_string(),
        ),
    }
}

pub fn reject_retired_gateway_mode(raw: &str) -> Result<(), String> {
    let mode = raw.trim().to_lowercase();
    if mode.is_empty() || mode == "direct" {
        return Ok(());
    }
    Err(format!(
        "CODEX_POOL_CODEX_GATEWAY_MODE={mode} is no longer supported: the CLIProxyAPI sidecar was removed and the pool always talks to the Codex backend directly; remove the setting"
    ))
}

fn one_of(raw: &str, fallback: &str, allowed: &[&str], message: &str) -> Result<String, String> {
    let value = env_or(raw.trim(), fallback).trim().to_lowercase();
    if allowed.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(message.to_string())
    }
}

pub fn prompt_cache_key_mode(raw: &str) -> Result<String, String> {
    one_of(
        raw,
        "auto",
        &["auto", "off", "passthrough"],
        "CODEX_POOL_PROMPT_CACHE_KEY_MODE must be auto, off, or passthrough",
    )
}

pub fn prompt_cache_key_scope(raw: &str) -> Result<String, String> {
    one_of(
        raw,
        "auto",
        &["auto", "conversation", "project", "user"],
        "CODEX_POOL_PROMPT_CACHE_KEY_SCOPE must be auto, conversation, project, or user",
    )
}

pub fn prompt_cache_key_policy(raw: &str) -> Result<String, String> {
    one_of(
        raw,
        "preserve",
        &["preserve", "lineage", "project", "user"],
        "CODEX_POOL_PROMPT_CACHE_KEY_POLICY must be preserve, lineage, project, or user",
    )
}

pub fn prompt_cache_buckets(raw: &str) -> Result<i64, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(PROMPT_CACHE_BUCKETS_DEFAULT);
    }
    match raw.parse::<i64>() {
        Ok(value) if (1..=256).contains(&value) => Ok(value),
        _ => {
            Err("CODEX_POOL_PROMPT_CACHE_BUCKETS must be an integer between 1 and 256".to_string())
        }
    }
}

pub fn prompt_cache_retention(raw: &str) -> Result<String, String> {
    match raw.trim().to_lowercase().as_str() {
        // Extended retention by default, so prompt (KV) caches survive the
        // idle gaps between conversation turns, the single biggest lever for
        // cache hit rate. "passthrough" leaves requests untouched.
        "" => Ok("24h".to_string()),
        "passthrough" => Ok(String::new()),
        value @ ("24h" | "in_memory") => Ok(value.to_string()),
        _ => Err(
            "CODEX_POOL_PROMPT_CACHE_RETENTION must be empty, passthrough, 24h, or in_memory"
                .to_string(),
        ),
    }
}

pub fn bool_from_env(raw: &str, name: &str, fallback: bool) -> Result<bool, String> {
    match raw.trim().to_lowercase().as_str() {
        "" => Ok(fallback),
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("{name} must be true or false")),
    }
}

/// CODEX_POOL_QUOTA_PRIME_GRACE fixes the grace (tests); empty draws at random.
pub fn quota_prime_grace(raw: &str) -> Result<Option<Duration>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    parse_go_duration(raw).map(Some).ok_or_else(|| {
        format!("CODEX_POOL_QUOTA_PRIME_GRACE must be a duration such as 0s or 10m, got {raw:?}")
    })
}

/// Go's time.ParseDuration for non-negative values ("120s", "1m30s", "1.5h",
/// "500ms", "0"). A negative or malformed value is None.
pub fn parse_go_duration(raw: &str) -> Option<Duration> {
    let mut rest = raw.trim();
    if rest == "0" {
        return Some(Duration::ZERO);
    }
    if rest.is_empty() || rest.starts_with('-') {
        return None;
    }
    rest = rest.strip_prefix('+').unwrap_or(rest);
    let mut total = 0f64;
    while !rest.is_empty() {
        let number_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number: f64 = rest[..number_end].parse().ok()?;
        rest = &rest[number_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_end] {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        rest = &rest[unit_end..];
        total += number * scale;
    }
    Duration::try_from_secs_f64(total).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn admin_hash() -> String {
        // A valid hash at the minimum iteration count, to keep tests quick.
        use base64::Engine;
        let engine = base64::engine::general_purpose::STANDARD_NO_PAD;
        let salt = [1u8; 16];
        format!(
            "pbkdf2-sha256:100000:{}:{}",
            engine.encode(salt),
            engine.encode(crypto::pbkdf2_sha256(b"admin-password", &salt, 100_000, 32))
        )
    }

    // Go TestPromptCacheKeyScopeFromEnv, ...PolicyFromEnv, ...BucketsFromEnv,
    // ...RetentionFromEnv, TestRoutingStrategyFromEnv.
    #[test]
    fn prompt_cache_and_routing_settings() {
        for value in ["", "auto", "conversation", "project", "user", "  PROJECT "] {
            assert!(prompt_cache_key_scope(value).is_ok(), "{value}");
        }
        assert_eq!(prompt_cache_key_scope(""), Ok("auto".into()));
        assert!(prompt_cache_key_scope("bogus").is_err());
        for value in ["", "preserve", "lineage", "project", "user", "  LINEAGE "] {
            assert!(prompt_cache_key_policy(value).is_ok(), "{value}");
        }
        assert_eq!(prompt_cache_key_policy(""), Ok("preserve".into()));
        assert!(prompt_cache_key_policy("bogus").is_err());
        assert_eq!(prompt_cache_buckets(""), Ok(PROMPT_CACHE_BUCKETS_DEFAULT));
        assert_eq!(prompt_cache_buckets("8"), Ok(8));
        for bad in ["0", "-1", "300", "abc"] {
            assert!(prompt_cache_buckets(bad).is_err(), "{bad}");
        }
        for (value, want) in [
            ("", "24h"),
            ("passthrough", ""),
            ("24h", "24h"),
            ("in_memory", "in_memory"),
            ("  24H ", "24h"),
        ] {
            assert_eq!(
                prompt_cache_retention(value),
                Ok(want.to_string()),
                "{value}"
            );
        }
        assert!(prompt_cache_retention("forever").is_err());
        for (value, want) in [
            ("", ROUTING_STRATEGY_BALANCED),
            ("sticky_balanced", ROUTING_STRATEGY_BALANCED),
            ("  STICKY_BALANCED ", ROUTING_STRATEGY_BALANCED),
            ("sticky_failover", ROUTING_STRATEGY_FAILOVER),
        ] {
            assert_eq!(routing_strategy(value), Ok(want.to_string()), "{value}");
        }
        assert!(routing_strategy("round_robin").is_err());
    }

    // Go TestLineageAffinityDefaultsOnAndEnvCanDisable, TestQuotaPrimeDefaultsOn.
    #[test]
    fn switches_default_on() {
        let settings = Settings::from_env(env(&[
            ("CODEX_POOL_API_KEY", "client-key"),
            ("CODEX_POOL_ADMIN_PASSWORD_HASH", &admin_hash()),
            ("CODEX_POOL_ADDR", "127.0.0.1:8317"),
        ]));
        let settings = settings.unwrap_or_else(|error| panic!("{error}"));
        assert!(settings.lineage_affinity && settings.quota_prime && settings.public_dashboard);
        assert!(!settings.preserve_pro_quota);
        assert_eq!(settings.routing_strategy, ROUTING_STRATEGY_BALANCED);
        assert_eq!(
            bool_from_env("false", "CODEX_POOL_LINEAGE_AFFINITY", true),
            Ok(false)
        );
        assert!(bool_from_env("maybe", "X", true).is_err());
    }

    // Go TestIsLoopbackAddress, TestCombinedListenAddressPrefersCanonicalEnvAndSupportsLegacyAlias,
    // TestCombinedListenerRequiresRemoteAdminOptIn.
    #[test]
    fn listener_rules() {
        for (address, want) in [
            ("127.0.0.1:8317", true),
            ("localhost:8317", true),
            ("0.0.0.0:8317", false),
            (":8317", false),
            ("[::1]:8317", true),
        ] {
            assert_eq!(is_loopback_address(address), want, "{address}");
        }
        assert_eq!(
            combined_listen_address("127.0.0.1:9000", "127.0.0.1:9001"),
            "127.0.0.1:9000"
        );
        assert_eq!(
            combined_listen_address("", "127.0.0.1:9001"),
            "127.0.0.1:9001"
        );
        let hash = admin_hash();
        let remote = |allow: &str| {
            Settings::from_env(env(&[
                ("CODEX_POOL_API_KEY", "client-key"),
                ("CODEX_POOL_ADMIN_PASSWORD_HASH", &hash),
                ("CODEX_POOL_ADDR", "0.0.0.0:8317"),
                ("CODEX_POOL_ALLOW_REMOTE_ADMIN", allow),
            ]))
        };
        assert!(
            remote("false")
                .is_err_and(|error| error.contains("combined HTTP address must be loopback"))
        );
        assert_eq!(
            remote("true").map(|s| s.listen_address),
            Ok("0.0.0.0:8317".to_string())
        );
    }

    // Go TestRetiredSidecarGatewayModeStopsStartup.
    #[test]
    fn retired_sidecar_gateway_mode_stops_startup() {
        for mode in ["", "direct", "DIRECT"] {
            assert!(reject_retired_gateway_mode(mode).is_ok(), "{mode}");
        }
        assert!(
            reject_retired_gateway_mode("sidecar")
                .is_err_and(|error| error.contains("no longer supported"))
        );
    }

    // Go TestShutdownGraceFromEnv.
    #[test]
    fn shutdown_grace_from_env() {
        assert_eq!(shutdown_grace(""), Ok(DEFAULT_SHUTDOWN_GRACE));
        assert_eq!(shutdown_grace("5m"), Ok(Duration::from_secs(300)));
        for bad in ["soon", "-1s", "120"] {
            assert!(shutdown_grace(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_go_duration("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_go_duration("0s"), Some(Duration::ZERO));
        assert!(load_api_keys("replace_with_key", "").is_err());
        assert!(load_api_keys("", " ").is_err());
        assert_eq!(
            load_api_keys("a, b", "c"),
            Ok(vec!["a".into(), "b".into(), "c".into()])
        );
    }
}
