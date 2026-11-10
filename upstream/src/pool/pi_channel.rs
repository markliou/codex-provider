//! The Pi channel (SPEC.md 2.3.7): an app's request on /v1 that is not a
//! Codex request goes upstream the way an unmodified Pi signed in with
//! "Sign in with ChatGPT" sends it, through the public Responses API with the
//! account's own plan-usage token.
//!
//! The standard is Pi at the release in `PI_RELEASE`
//! (@earendil-works/pi-ai and pi-coding-agent), read from its source and
//! measured by `scripts/conformance/pi-capture.sh`; the measured request is
//! recorded in `upstream/testdata/pi-reference.json`. OpenAI's own rules for
//! this route are its "ChatGPT plan usage" docs
//! (developers.openai.com/siwc/token-sharing-open-source/...). What the pool
//! needs differently from Pi is said where it is done.
//!
//! The Codex channel and its credential (`auth.json`) are untouched: an
//! account signs in for this channel separately, and a failure here never
//! quarantines the account's Codex credential.
use super::app::App;
use super::proxy::ForwardError;
use super::state::Account;
use super::time::Timestamp;
use crate::transport::UpstreamResponse;
use bytes::Bytes;
use http::HeaderMap;
use http::Method;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::value::RawValue;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// The Pi release this channel matches (pi-ai and pi-coding-agent share it).
pub const PI_RELEASE: &str = "1.5.0";

// pi-ai dist/auth/oauth/openai-chatgpt.js, verbatim values.
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const AGENT_NAME_HINT: &str = "Pi";
pub const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
pub const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Pi refreshes this long before the real expiry "so a request never starts
/// with a token about to expire" (EXPIRY_MARGIN_MS), and stores the expiry
/// with the margin already taken off.
const EXPIRY_MARGIN_MS: i64 = 3 * 60 * 1000;
/// Not Pi's: Pi leaves its token call unbounded. The pool holds the
/// account's lock across a refresh, so a refresh that never answers would
/// stall every request of the account; the bound is Codex 0.167's own.
const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
/// A sign-in the owner started and has not finished is kept this long.
const PENDING_LOGIN_TTL_SECONDS: i64 = 15 * 60;

pub const CREDENTIAL_FILE: &str = "pi-auth.json";
pub const HOST_ID_FILE: &str = "pi-host-id";

/// The headers a Pi of `PI_RELEASE` sends with a Responses request, in its
/// order, as measured (pi-reference.json `responses.headers`). The OpenAI SDK
/// (openai 7.19.0) adds the x-stainless set; Pi's identity is the
/// user-agent. `authorization`, `session_id` and `x-client-request-id` are
/// filled per request. The body step adds content-type. Not sent: the
/// connection and accept-encoding headers Node's HTTP client adds itself
/// (the pool's client negotiates its own; a forced accept-encoding would
/// hand it a body it does not decode).
const PI_REQUEST_HEADERS: &[(&str, &str)] = &[
    ("accept", "application/json"),
    ("x-stainless-retry-count", "0"),
    ("x-stainless-timeout", "300"),
    ("x-stainless-lang", "js"),
    ("x-stainless-package-version", "7.19.0"),
    ("x-stainless-os", "Linux"),
    ("x-stainless-arch", "x64"),
    ("x-stainless-runtime", "node"),
    ("x-stainless-runtime-version", "v22.23.2"),
    ("authorization", ""),
    ("user-agent", "pi/1.5.0 (linux; node/v22.23.2; x64)"),
    ("session_id", ""),
    ("x-client-request-id", ""),
    ("accept-language", "*"),
    ("sec-fetch-mode", "cors"),
];

/// The headers of Pi's token call (pi-reference.json `token.headers`), sent
/// by Node's fetch with no user agent of Pi's own.
const PI_TOKEN_HEADERS: &[(&str, &str)] = &[
    ("accept", "application/json"),
    ("content-type", "application/x-www-form-urlencoded"),
    ("accept-language", "*"),
    ("sec-fetch-mode", "cors"),
    ("user-agent", "undici"),
];

/// Fields Pi leaves out on this route (pi-ai api/openai-responses.js
/// buildParams, `omitUnsupportedFields` when "Sign in with ChatGPT rejects
/// these request fields"). A Pi pointed at the pool does not know it is
/// signed in and sends them (max_output_tokens 128000 was measured), so the
/// pool removes exactly these, as Pi itself does. Any other field that
/// changes the answer is refused by the request gate, never removed.
pub const PI_OMITTED_FIELDS: &[&str] = &[
    "prompt_cache_retention",
    "prompt_cache_options",
    "max_output_tokens",
    "temperature",
];

/// Pi's order of the Responses request fields (buildParams): fields Pi
/// sends go first in this order, anything else after them as the client
/// wrote it.
const PI_FIELD_ORDER: &[&str] = &[
    "model",
    "input",
    "stream",
    "prompt_cache_key",
    "store",
    "service_tier",
    "tools",
    "tool_choice",
    "reasoning",
    "include",
];

/// Where the channel sends; tests point it at a fake upstream.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub responses: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize: AUTHORIZE_URL.to_string(),
            token: TOKEN_URL.to_string(),
            responses: RESPONSES_URL.to_string(),
        }
    }
}

/// The account's plan-usage credential, stored as Pi stores its own
/// (pi-coding-agent auth.json `openai` entry, without `type`): `expires` is
/// milliseconds since the epoch with Pi's margin already taken off.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Credential {
    pub client_id: String,
    pub access: String,
    pub refresh: String,
    pub expires: i64,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Why the channel cannot use an account right now. Kept in the account's
/// directory as well (STATE_FILE), so a restart does not try a refused or
/// resting account again only to be refused: each such try is a request
/// upstream answers for nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "camelCase")]
pub enum Unavailable {
    /// No credential, or upstream refused it for good: the owner signs in.
    SignIn(String),
    /// Upstream said this account cannot use the route
    /// (subscription_sharing_user_not_eligible and the like).
    Refused(String),
    /// The plan's or this app's usage limit; tried again after `until`.
    Limited { code: String, until: Timestamp },
}

/// A sign-in the owner started: what the pasted callback is checked
/// against.
#[derive(Clone, Debug)]
struct Pending {
    verifier: String,
    state: String,
    started: Timestamp,
}

tokio::task_local! {
    /// Set while the Pi channel's attempt loop picks accounts: an account then
    /// needs its Pi sign-in, not its Codex one (Inner::usable). Every other
    /// selection (Codex requests, sockets, the native endpoint, the pool's
    /// own calls) runs outside it and keeps needing the Codex credential, so
    /// an account signed in for Pi alone is never given a Codex request.
    static PI_SELECTION: bool;
}

/// Whether the current task is picking an account for the Pi channel.
pub fn pi_selection() -> bool {
    PI_SELECTION.try_with(|value| *value).unwrap_or(false)
}

/// Runs `future` as the Pi channel's account selection.
pub async fn as_pi_selection<F: std::future::Future>(future: F) -> F::Output {
    PI_SELECTION.scope(true, future).await
}

impl super::app::Inner {
    /// Whether the account holds a Pi sign-in.
    pub fn has_pi_credential(&self, item: &Account) -> bool {
        self.account_codex_home(&item.id)
            .join(CREDENTIAL_FILE)
            .is_file()
    }

    /// An account signed in for Pi and not for Codex: it serves the Pi
    /// channel only, and nothing of the Codex side (the quota poll, the
    /// missing-sign-in status) applies to it.
    pub fn pi_only(&self, item: &Account) -> bool {
        super::plan::is_codex_device_auth(item)
            && self.has_pi_credential(item)
            && !self
                .account_codex_home(&item.id)
                .join("auth.json")
                .is_file()
    }
}

pub const STATE_FILE: &str = "pi-state.json";

/// The channel's in-memory state; each account's `Unavailable` is read from
/// its STATE_FILE the first time it is asked for (App::pi_unavailable).
#[derive(Default)]
pub struct PiChannel {
    endpoints: std::sync::RwLock<Endpoints>,
    pending: std::sync::Mutex<HashMap<String, Pending>>,
    unavailable: std::sync::Mutex<HashMap<String, Unavailable>>,
    loaded: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Serializes the usage files' read-modify-write.
    usage_lock: std::sync::Mutex<()>,
}

impl PiChannel {
    pub fn endpoints(&self) -> Endpoints {
        self.endpoints
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_endpoints(&self, endpoints: Endpoints) {
        *self
            .endpoints
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = endpoints;
    }

    /// The account's state in memory; `None` for a rest that has ended
    /// (`expired` then says so, for the caller to forget the file too).
    fn unavailable(&self, account_id: &str, now: Timestamp) -> (Option<Unavailable>, bool) {
        let mut all = self
            .unavailable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match all.get(account_id) {
            Some(Unavailable::Limited { until, .. }) if !now.before(until) => {
                all.remove(account_id);
                (None, true)
            }
            other => (other.cloned(), false),
        }
    }

    fn set(&self, account_id: &str, reason: Option<Unavailable>) {
        let mut all = self
            .unavailable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match reason {
            Some(reason) => all.insert(account_id.to_string(), reason),
            None => all.remove(account_id),
        };
    }

    /// Whether the account's file still has to be read (once per process).
    fn first_look(&self, account_id: &str) -> bool {
        self.loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(account_id.to_string())
    }
}

pub const USAGE_FILE: &str = "pi-usage.json";
/// Kept a little past the week the Console shows.
const USAGE_HOURS_KEPT: i64 = 8 * 24;

/// One hour of the channel's use of an account, from each answer's own
/// usage. OpenAI offers no way for an app to read its plan usage (the
/// route's docs send the user to ChatGPT's Settings > Usage page), so this
/// is the pool's own count; the plan's windows are the Codex channel's quota
/// poll, which both channels share.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageHour {
    pub hour: i64,
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
}

fn read_usage(home: &Path) -> Vec<UsageHour> {
    std::fs::read_to_string(home.join(USAGE_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Sums the hours from `since` (an hour number) on.
fn usage_total(hours: &[UsageHour], since: i64) -> Value {
    let mut total = UsageHour::default();
    for hour in hours.iter().filter(|hour| hour.hour >= since) {
        total.requests += hour.requests;
        total.input_tokens += hour.input_tokens;
        total.cached_tokens += hour.cached_tokens;
        total.output_tokens += hour.output_tokens;
    }
    serde_json::json!({
        "requests": total.requests,
        "inputTokens": total.input_tokens,
        "cachedTokens": total.cached_tokens,
        "outputTokens": total.output_tokens,
    })
}

impl App {
    /// Adds one answer's usage to the account's current hour.
    pub fn pi_record_usage(
        &self,
        account_id: &str,
        usage: &super::response_info::PromptCacheUsage,
    ) {
        let home = self.read().account_codex_home(account_id);
        let hour = Timestamp::now().unix() / 3600;
        let _guard = self
            .pi_channel
            .usage_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hours = read_usage(&home);
        hours.retain(|kept| kept.hour > hour - USAGE_HOURS_KEPT);
        if hours.last().is_none_or(|last| last.hour != hour) {
            hours.push(UsageHour {
                hour,
                ..UsageHour::default()
            });
        }
        if let Some(current) = hours.last_mut() {
            current.requests += 1;
            current.input_tokens += usage.input_tokens;
            current.cached_tokens += usage.cached_tokens;
            current.output_tokens += usage.output_tokens;
        }
        if let Err(error) = std::fs::create_dir_all(&home)
            .and_then(|()| super::app::write_json_atomic(&home.join(USAGE_FILE), &hours))
        {
            self.logger.printf(&format!(
                "pi channel usage for {account_id} not saved: {error}"
            ));
        }
    }

    /// The account's use of the channel over the last five hours and the
    /// last seven days, in whole hours (the current hour included).
    pub fn pi_usage(&self, account_id: &str) -> Value {
        let home = self.read().account_codex_home(account_id);
        let hours = read_usage(&home);
        let now = Timestamp::now().unix() / 3600;
        serde_json::json!({
            "fiveHours": usage_total(&hours, now - 4),
            "week": usage_total(&hours, now - 7 * 24 + 1),
            "lastHour": hours.last().map(|last| last.hour * 3600),
        })
    }

    /// One line per distinct answer shape on this channel: its terminal
    /// event and whether it carried usage the pool read, never content.
    pub fn note_pi_answer_shape(&self, info: &super::response_info::ProxyResponseInfo) {
        let shape = format!(
            "terminal={} usage={}",
            if info.terminal_event.is_empty() {
                "-"
            } else {
                info.terminal_event.as_str()
            },
            if info.usage.present { "read" } else { "absent" }
        );
        let first = {
            let mut seen = self
                .unrelayed_header_sets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            seen.len() < 256 && seen.insert(format!("pi-answer:{shape}"))
        };
        if first {
            self.logger
                .printf(&format!("pi channel answer shape: {shape}"));
        }
    }

    /// One line per distinct set of response header names on this channel,
    /// names only: what upstream sends back here is not documented, and a
    /// usage or limit header would show up in this list first.
    pub fn note_pi_response_headers(&self, headers: &HeaderMap) {
        let mut names: Vec<&str> = headers.keys().map(http::HeaderName::as_str).collect();
        names.sort_unstable();
        names.dedup();
        let list = names.join(",");
        let first = {
            let mut seen = self
                .unrelayed_header_sets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            seen.len() < 256 && seen.insert(format!("pi-response:{list}"))
        };
        if first {
            self.logger
                .printf(&format!("pi channel response header names: {list}"));
        }
    }

    /// The account's state on the channel, read from its file the first
    /// time this process asks.
    pub fn pi_unavailable(&self, account_id: &str, now: Timestamp) -> Option<Unavailable> {
        let home = self.read().account_codex_home(account_id);
        if self.pi_channel.first_look(account_id)
            && let Ok(text) = std::fs::read_to_string(home.join(STATE_FILE))
            && let Ok(reason) = serde_json::from_str::<Unavailable>(&text)
        {
            self.pi_channel.set(account_id, Some(reason));
        }
        let (reason, expired) = self.pi_channel.unavailable(account_id, now);
        if expired {
            let _ = std::fs::remove_file(home.join(STATE_FILE));
        }
        reason
    }

    /// Marks the account, in memory and in its file.
    pub fn pi_mark(&self, account_id: &str, reason: Unavailable) {
        self.pi_channel.first_look(account_id);
        let home = self.read().account_codex_home(account_id);
        if let Err(error) = std::fs::create_dir_all(&home)
            .and_then(|()| super::app::write_json_atomic(&home.join(STATE_FILE), &reason))
        {
            self.logger.printf(&format!(
                "pi channel state for {account_id} not saved: {error}"
            ));
        }
        self.pi_channel.set(account_id, Some(reason));
    }

    /// Clears the account's mark, in memory and in its file.
    pub fn pi_clear(&self, account_id: &str) {
        self.pi_channel.first_look(account_id);
        let home = self.read().account_codex_home(account_id);
        let _ = std::fs::remove_file(home.join(STATE_FILE));
        self.pi_channel.set(account_id, None);
    }
}

// ---- files ----

fn read_credential(home: &Path) -> Option<Credential> {
    let text = std::fs::read_to_string(home.join(CREDENTIAL_FILE)).ok()?;
    serde_json::from_str::<Credential>(&text)
        .ok()
        .filter(|credential| !credential.access.is_empty() && !credential.client_id.is_empty())
}

fn write_credential(home: &Path, credential: &Credential) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    super::app::write_json_atomic(&home.join(CREDENTIAL_FILE), credential)
}

/// The account's host id, made once and kept: "Choose and persist it before
/// the first sign-in, and reuse it afterward"; a `urn:uuid:` of a v4 UUID,
/// lowercase (openai-chatgpt.js agentHostId). One per account, not one per
/// pool, as each account presents its own Codex installation id
/// (installation_id.rs): no id ties the pool's accounts together.
pub fn resolve_host_id(home: &Path, new_v4: impl FnOnce() -> String) -> std::io::Result<String> {
    let path = home.join(HOST_ID_FILE);
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            if let Some(id) = contents
                .trim()
                .strip_prefix("urn:uuid:")
                .and_then(super::installation_id::canonical_uuid)
            {
                return Ok(format!("urn:uuid:{id}"));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let id = format!("urn:uuid:{}", new_v4().to_ascii_lowercase());
    std::fs::create_dir_all(home)?;
    let temporary = home.join(format!("{HOST_ID_FILE}.tmp"));
    std::fs::write(&temporary, &id)?;
    std::fs::rename(&temporary, &path)?;
    Ok(id)
}

// ---- the request ----

/// Top-level fields in the order the client wrote them, each value kept as
/// its raw bytes: re-encoding would sort the keys of every nested object
/// (tool schemas included), a layout Pi never sends.
struct Ordered(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Ordered;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Ordered, A::Error> {
                let mut fields = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    fields.push((key, value));
                }
                Ok(Ordered(fields))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

fn literal(text: String) -> Result<Box<RawValue>, String> {
    RawValue::from_string(text).map_err(|error| error.to_string())
}

fn quoted(text: &str) -> Result<String, String> {
    serde_json::to_string(text).map_err(|error| error.to_string())
}

/// An object written in the given key order, values as raw JSON.
fn object(fields: &[(String, Box<RawValue>)]) -> Result<String, String> {
    let mut out = String::from("{");
    for (index, (name, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&quoted(name)?);
        out.push(':');
        out.push_str(value.get());
    }
    out.push('}');
    Ok(out)
}

/// Pi's developer message (convertResponsesMessages): role first, string
/// content.
fn developer_message(text: &str) -> Result<Box<RawValue>, String> {
    literal(format!(
        r#"{{"role":"developer","content":{}}}"#,
        quoted(text)?
    ))
}

/// Pi's user message: role first, one input_text part.
fn user_message(text: &str) -> Result<Box<RawValue>, String> {
    literal(format!(
        r#"{{"role":"user","content":[{{"type":"input_text","text":{}}}]}}"#,
        quoted(text)?
    ))
}

/// Pi's `configuration_update` input item.
#[derive(Deserialize)]
struct ConfigurationUpdate {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    reasoning: Option<ConfiguredReasoning>,
}

#[derive(Deserialize)]
struct ConfiguredReasoning {
    #[serde(default)]
    effort: Option<String>,
}

/// A `reasoning` object with its effort set, its other keys in place.
fn with_effort(reasoning: &RawValue, effort: &str) -> Result<Box<RawValue>, String> {
    let Ok(Ordered(mut fields)) = serde_json::from_str::<Ordered>(reasoning.get()) else {
        return literal(format!(r#"{{"effort":{}}}"#, quoted(effort)?));
    };
    let value = literal(quoted(effort)?)?;
    match fields.iter_mut().find(|(name, _)| name == "effort") {
        Some((_, slot)) => *slot = value,
        None => fields.insert(0, ("effort".to_string(), value)),
    }
    literal(object(&fields)?)
}

/// An input item with a system role written as a developer one, every other
/// byte as the client wrote it.
fn without_system_role(item: Box<RawValue>) -> Result<Box<RawValue>, String> {
    let Ok(Ordered(mut fields)) = serde_json::from_str::<Ordered>(item.get()) else {
        return Ok(item);
    };
    let mut changed = false;
    for (name, value) in &mut fields {
        if name == "role" && value.get() == r#""system""# {
            *value = literal(r#""developer""#.to_string())?;
            changed = true;
        }
    }
    if changed {
        literal(object(&fields)?)
    } else {
        Ok(item)
    }
}

/// The request as a Pi of `PI_RELEASE` sends it on this route, and the
/// fields removed on the way. What Pi always sends is made so (`stream`
/// true, `store` false, the system prompt as the first developer message,
/// list input); the fields Pi omits here are removed (PI_OMITTED_FIELDS);
/// the conversation itself is the client's and is not edited.
///
/// `model` is the model the pool resolved (its aliases are its own catalog),
/// and `tier` a reasoning effort named in the model's suffix; both are
/// written in place, so every other byte stays the client's.
pub fn pi_request_body(
    body: &[u8],
    model: &str,
    tier: Option<&str>,
) -> Result<(Bytes, Vec<String>), String> {
    let Ordered(fields) = serde_json::from_slice::<Ordered>(body)
        .map_err(|error| format!("request body: {error}"))?;
    let mut removed = Vec::new();
    let mut instructions: Option<String> = None;
    let mut kept: Vec<(String, Box<RawValue>)> = Vec::new();
    let mut fields = fields;
    let set =
        |fields: &mut Vec<(String, Box<RawValue>)>, name: &str, value: Box<RawValue>| match fields
            .iter_mut()
            .find(|(field, _)| field == name)
        {
            Some((_, slot)) => *slot = value,
            None => fields.push((name.to_string(), value)),
        };
    if !model.is_empty() {
        set(&mut fields, "model", literal(quoted(model)?)?);
    }
    if let Some(tier) = tier {
        set(
            &mut fields,
            "reasoning",
            literal(format!(r#"{{"effort":{}}}"#, quoted(tier)?))?,
        );
    }
    for (name, value) in fields {
        if PI_OMITTED_FIELDS.contains(&name.as_str()) {
            removed.push(name);
            continue;
        }
        if name == "instructions" {
            // Pi sends its system prompt as the first input item, a
            // developer message with string content
            // (convertResponsesMessages), never as `instructions`.
            match serde_json::from_str::<Value>(value.get()) {
                Ok(Value::String(text)) => instructions = Some(text),
                Ok(Value::Null) => {}
                _ => return Err("instructions must be a string".to_string()),
            }
            continue;
        }
        kept.push((name, value));
    }
    let mut input_items: Vec<Box<RawValue>> = Vec::new();
    let mut had_input = false;
    let mut configured_effort: Option<String> = None;
    let mut removed_updates = false;
    if let Some(position) = kept.iter().position(|(name, _)| name == "input") {
        had_input = true;
        let (_, value) = kept.remove(position);
        if let Ok(text) = serde_json::from_str::<String>(value.get()) {
            // A string input is one user message, as Pi writes a user turn.
            input_items.push(user_message(&text)?);
        } else if let Ok(items) = serde_json::from_str::<Vec<Box<RawValue>>>(value.get()) {
            // The route rejects system items ("Explicit system message items
            // are rejected. Use instructions or developer messages
            // instead."); Pi writes a system prompt as a developer message.
            for item in items {
                // "Sign in with ChatGPT rejects configuration_update items"
                // (pi-ai api/openai-responses.js getConfigurationUpdateEffort):
                // signed in, Pi sends none and puts the thinking level in
                // reasoning.effort. A Pi pointed at the pool does not know it
                // is signed in, so it sends the items with reasoning.effort
                // fixed at "medium" (CONFIGURATION_UPDATE_REQUEST_EFFORT); the
                // pool writes the request as Pi signed in writes it. The last
                // item carries the current level (pushConfigurationUpdate).
                if let Ok(update) = serde_json::from_str::<ConfigurationUpdate>(item.get())
                    && update.kind == "configuration_update"
                {
                    if let Some(effort) = update.reasoning.and_then(|reasoning| reasoning.effort) {
                        configured_effort = Some(effort);
                    }
                    removed_updates = true;
                    continue;
                }
                input_items.push(without_system_role(item)?);
            }
        } else {
            return Err("input must be a string or a list".to_string());
        }
    }
    if removed_updates {
        removed.push("configuration_update".to_string());
    }
    if tier.is_none()
        && let Some(effort) = configured_effort
    {
        let reasoning = match kept.iter().position(|(name, _)| name == "reasoning") {
            Some(position) => {
                let (_, value) = kept.remove(position);
                with_effort(&value, &effort)?
            }
            None => literal(format!(r#"{{"effort":{}}}"#, quoted(&effort)?))?,
        };
        kept.push(("reasoning".to_string(), reasoning));
    }
    if let Some(text) = instructions {
        input_items.insert(0, developer_message(&text)?);
        had_input = true;
    }
    // Pi always streams and never stores on this route ("Set store to false
    // and stream to true on each HTTP inference request"); the client's own
    // values give way to them. A client that asked for no stream still gets
    // one answer: the attempt loop assembles it from the stream.
    kept.retain(|(name, _)| name != "stream" && name != "store");
    let mut ordered: Vec<(String, Box<RawValue>)> = Vec::new();
    for name in PI_FIELD_ORDER {
        match *name {
            "input" if had_input => {
                let items: Vec<&str> = input_items.iter().map(|item| item.get()).collect();
                ordered.push((
                    "input".to_string(),
                    literal(format!("[{}]", items.join(",")))?,
                ));
            }
            "stream" => ordered.push(("stream".to_string(), literal("true".to_string())?)),
            "store" => ordered.push(("store".to_string(), literal("false".to_string())?)),
            other => {
                if let Some(position) = kept.iter().position(|(name, _)| name == other) {
                    ordered.push(kept.remove(position));
                }
            }
        }
    }
    ordered.extend(kept);
    Ok((Bytes::from(object(&ordered)?), removed))
}

/// The client's own name for itself, kept for the log only: a Pi a
/// colleague changed names itself in its user agent, and the pool records
/// that name before presenting Pi's.
pub fn client_name(inbound: &HeaderMap) -> String {
    let agent = super::http_io::header_value(inbound, "user-agent").trim();
    let name = agent.split(['/', ' ']).next().unwrap_or_default();
    super::proxy::safe_header_value(name)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(*c, '-' | '_' | '.'))
        .take(64)
        .collect()
}

fn header_map(pairs: &[(String, String)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    headers
}

/// Pi's headers for one request: its fixed set, the account's token, and
/// the client's session id where Pi sends its own (`session_id` and
/// `x-client-request-id` carry the same value, createClient). A request
/// without one goes without both, as Pi's does when it has no session.
pub fn pi_request_headers(access_token: &str, inbound: &HeaderMap) -> (HeaderMap, String) {
    let session = ["session_id", "session-id", "x-client-request-id"]
        .iter()
        .map(|name| super::proxy::safe_header_value(super::http_io::header_value(inbound, name)))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    let mut pairs = Vec::new();
    let mut order = Vec::new();
    for (name, value) in PI_REQUEST_HEADERS {
        let value = match *name {
            "authorization" => format!("Bearer {access_token}"),
            "session_id" | "x-client-request-id" => session.clone(),
            _ => (*value).to_string(),
        };
        if value.is_empty() {
            continue;
        }
        pairs.push(((*name).to_string(), value));
        order.push(*name);
    }
    (header_map(&pairs), format!("{}|", order.join(",")))
}

// ---- token calls ----

#[derive(Deserialize)]
struct TokenReply {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    expires_in: Option<f64>,
}

/// Token errors after which only a new sign-in helps ("Errors and
/// recovery": invalid_grant, invalid_refresh_token, token_expired,
/// refresh_token_expired, refresh_token_invalidated, refresh_token_reused
/// → re-sign-in; invalid_client → fix the client).
pub fn token_error_is_permanent(code: &str) -> bool {
    matches!(
        code,
        "invalid_grant"
            | "invalid_refresh_token"
            | "token_expired"
            | "refresh_token_expired"
            | "refresh_token_invalidated"
            | "refresh_token_reused"
            | "invalid_client"
    )
}

fn token_error_code(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return String::new();
    };
    let code = match value.get("error") {
        Some(Value::String(code)) => code.clone(),
        Some(Value::Object(error)) => error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    };
    super::quota::sanitized_error_code(&code)
}

/// A failed token call: permanent ones need the owner to sign in again.
#[derive(Debug)]
pub struct TokenError {
    pub permanent: bool,
    pub message: String,
}

fn credential_from_reply(
    reply: &TokenReply,
    client_id: &str,
    now_ms: i64,
) -> Result<Credential, TokenError> {
    let fail = |message: &str| TokenError {
        permanent: true,
        message: message.to_string(),
    };
    if reply.access_token.trim().is_empty() {
        return Err(fail("token response has no access_token"));
    }
    if reply.refresh_token.trim().is_empty() {
        return Err(fail("token response has no refresh_token"));
    }
    let expires_in = reply
        .expires_in
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| fail("token response has invalid expires_in"))?;
    let scopes: Vec<String> = reply.scope.split_whitespace().map(str::to_string).collect();
    // "If the OAuth result lacks chatgpt.tokens.use.direct, keep the
    // sign-in, mark plan usage as disabled": without it the token cannot be
    // used on this route at all, so the pool does not keep it.
    if !scopes.iter().any(|scope| scope == DIRECT_TOKEN_SCOPE) {
        return Err(fail(&format!(
            "the grant did not include {DIRECT_TOKEN_SCOPE}"
        )));
    }
    #[allow(clippy::cast_possible_truncation)]
    let lifetime_ms = (expires_in * 1000.0) as i64;
    Ok(Credential {
        client_id: client_id.to_string(),
        access: reply.access_token.clone(),
        refresh: reply.refresh_token.clone(),
        expires: now_ms + lifetime_ms - EXPIRY_MARGIN_MS,
        scopes,
    })
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// One token call, as Pi makes it (requestToken): a form POST with Node's
/// headers.
async fn token_call(
    transports: &crate::transport::Transports,
    url: &str,
    body: String,
) -> Result<TokenReply, TokenError> {
    let pairs: Vec<(String, String)> = PI_TOKEN_HEADERS
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect();
    let order = format!(
        "{}|",
        PI_TOKEN_HEADERS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(",")
    );
    let transient = |message: String| TokenError {
        permanent: false,
        message,
    };
    let (mut request, after) = crate::transport::build_request(
        Method::POST,
        url.to_string(),
        &header_map(&pairs),
        Bytes::from(body),
        codex_http_client::RequestCompression::None,
        Some(&order),
    )
    .map_err(transient)?;
    // The transport leaves content-type to the body step, which writes one
    // only for a JSON body; this body is a form, so Pi's headers are set
    // whole, in its order, content-type second.
    request.headers = header_map(&pairs);
    let response = crate::transport::send_request(transports, request, &after)
        .await
        .map_err(|failure| transient(format!("token call: {failure}")))?;
    let status = response.status;
    let body = super::credentials::read_body(response, 1 << 20)
        .await
        .map_err(|error| transient(format!("token call: {error}")))?;
    if !status.is_success() {
        let code = token_error_code(&body);
        return Err(TokenError {
            permanent: token_error_is_permanent(&code),
            message: format!(
                "token call failed ({status}){}",
                if code.is_empty() {
                    String::new()
                } else {
                    format!(": {code}")
                }
            ),
        });
    }
    serde_json::from_slice::<TokenReply>(&body).map_err(|_| TokenError {
        permanent: false,
        message: "token response must be a JSON object".to_string(),
    })
}

/// PKCE S256 (pi-ai auth/oauth/pkce.js): a 32-byte verifier and its
/// SHA-256 challenge, both unpadded base64url.
fn pkce() -> (String, String) {
    use base64::Engine;
    use sha2::Digest;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let verifier = engine.encode(super::crypto::random_bytes::<32>());
    let challenge = engine.encode(sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

impl App {
    fn pi_home(&self, account_id: &str) -> std::path::PathBuf {
        self.read().account_codex_home(account_id)
    }

    /// Whether the account holds a plan-usage credential for this channel.
    pub fn pi_signed_in(&self, account_id: &str) -> bool {
        read_credential(&self.pi_home(account_id)).is_some()
    }

    /// Starts the owner's sign-in for an account: the URL to open in a
    /// browser. Pi's authorize request (loginOpenAIChatGPT): the dynamic
    /// client, Pi's agent name, the account's host id, a loopback redirect,
    /// PKCE, state and nonce. A later sign-in of the same account repeats it
    /// with its saved host id.
    pub fn pi_login_start(&self, account_id: &str) -> Result<String, String> {
        let home = self.pi_home(account_id);
        let host = resolve_host_id(&home, super::crypto::random_uuid)
            .map_err(|error| format!("host id: {error}"))?;
        let (verifier, challenge) = pkce();
        let state = super::crypto::random_id();
        let nonce = super::crypto::random_id();
        let mut url = url::Url::parse(&self.pi_channel.endpoints().authorize)
            .map_err(|error| error.to_string())?;
        url.query_pairs_mut()
            .append_pair("client_id", DYNAMIC_CLIENT_ID)
            .append_pair("agent_name_hint", AGENT_NAME_HINT)
            .append_pair("ext_agent_host_id", &host)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("resource", RESOURCE)
            .append_pair("scope", SCOPE)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("nonce", &nonce);
        self.pi_channel
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                account_id.to_string(),
                Pending {
                    verifier,
                    state,
                    started: Timestamp::now(),
                },
            );
        Ok(url.to_string())
    }

    /// Finishes the owner's sign-in from the address the browser landed on
    /// (a loopback callback the pool cannot receive, so it is pasted, as
    /// Pi's manual path takes it: authorizationResultFromManualInput), then
    /// exchanges the code as Pi does (exchangeAuthorizationCode).
    pub async fn pi_login_complete(&self, account_id: &str, pasted: &str) -> Result<(), String> {
        let pending = self
            .pi_channel
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(account_id)
            .cloned()
            .ok_or("no sign-in was started for this account")?;
        if Timestamp::now().sub(&pending.started).num_seconds() > PENDING_LOGIN_TTL_SECONDS {
            return Err("the sign-in expired; start it again".to_string());
        }
        let url = url::Url::parse(pasted.trim())
            .map_err(|_| "paste the full address the browser landed on".to_string())?;
        let expected = url::Url::parse(REDIRECT_URI).map_err(|error| error.to_string())?;
        if url.origin() != expected.origin() || url.path() != expected.path() {
            return Err(format!("the pasted address must start with {REDIRECT_URI}"));
        }
        let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
        if let Some(error) = query.get("error") {
            return Err(format!("ChatGPT authorization failed: {error}"));
        }
        let code = query.get("code").ok_or("the address has no code")?;
        if query.get("state") != Some(&pending.state) {
            return Err("the address does not belong to this sign-in".to_string());
        }
        let client_id = query
            .get("client_id")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or("the address has no issued client id")?;
        let body = form(&[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("code", code),
            ("code_verifier", &pending.verifier),
            ("redirect_uri", REDIRECT_URI),
            ("resource", RESOURCE),
        ]);
        let reply = token_call(&self.transports, &self.pi_channel.endpoints().token, body)
            .await
            .map_err(|error| error.message)?;
        // Pi checks the ID token is there as part of the token-response
        // contract and does not read it.
        if reply.id_token.trim().is_empty() {
            return Err("the token response has no ID token".to_string());
        }
        let credential =
            credential_from_reply(&reply, &client_id, now_ms()).map_err(|error| error.message)?;
        let home = self.pi_home(account_id);
        // A new sign-in replaces whatever was stored, under the account's
        // lock so a refresh in flight cannot write over it afterwards (its
        // compare-before-write sees the new tokens and discards its own).
        let _permit = self.pi_lock(account_id).await;
        write_credential(&home, &credential).map_err(|error| error.to_string())?;
        self.pi_channel
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(account_id);
        self.pi_clear(account_id);
        self.activate_pi_slot(account_id, &reply.id_token);
        self.logger
            .printf(&format!("pi channel signed in for {account_id}"));
        Ok(())
    }

    /// A slot added for Pi (Add account, Pi) waits out of the pool until its
    /// sign-in finishes, as a Codex slot waits for its device sign-in
    /// (login.rs, pending_pool_activation); then it joins the pool. The email
    /// and plan in the ID token are kept for recognition only, as a Codex
    /// sign-in's are: the pool never sends them anywhere.
    fn activate_pi_slot(&self, account_id: &str, id_token: &str) {
        let claims = id_token
            .split('.')
            .nth(1)
            .and_then(|payload| {
                use base64::Engine;
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(payload.trim_end_matches('='))
                    .ok()
            })
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .unwrap_or(Value::Null);
        let email = claims
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let plan = claims
            .get("https://api.openai.com/auth")
            .and_then(|auth| auth.get("chatgpt_plan_type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut inner = self.write();
        let Some(index) = inner.account_index(account_id) else {
            return;
        };
        let pi_only = {
            let item = inner.config.accounts()[index].clone();
            inner.pi_only(&item)
        };
        let slot = &mut inner.config.accounts_mut()[index];
        if !pi_only {
            return;
        }
        if slot.email.is_empty() && !email.is_empty() {
            slot.email = super::plan::normalize_email(email);
        }
        if slot.raw_plan_type.is_empty() && !plan.is_empty() {
            slot.raw_plan_type = super::plan::clean_raw_plan_type(plan);
            slot.plan_family = super::plan::plan_family_from_raw(&slot.raw_plan_type).to_string();
            slot.plan_type.clone_from(&slot.plan_family);
            slot.plan_rank = super::plan::plan_rank(&slot.plan_family);
        }
        if slot.pending_pool_activation {
            slot.enabled = true;
            slot.in_pool = true;
            slot.pending_pool_activation = false;
        }
        slot.updated_at = Timestamp::now();
        // A quota error recorded while the slot had no sign-in at all is the
        // Codex poll's, not this account's state (Inner::credential_error_blocks).
        if let Some(snapshot) = inner.state.quotas_mut().get_mut(account_id) {
            snapshot.quota_error = None;
        }
        if inner.save().is_err() {
            drop(inner);
            self.logger.printf(&format!(
                "pi channel sign-in for {account_id}: the account could not be saved"
            ));
        }
    }

    /// Forgets the account's credential for this channel. Upstream's
    /// revocation endpoint is not in Pi or the docs this follows, so nothing
    /// is sent; the owner can disconnect the app in ChatGPT settings.
    pub async fn pi_sign_out(&self, account_id: &str) -> std::io::Result<()> {
        let _permit = self.pi_lock(account_id).await;
        match std::fs::remove_file(self.pi_home(account_id).join(CREDENTIAL_FILE)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        self.pi_mark(
            account_id,
            Unavailable::SignIn("signed out by the owner".to_string()),
        );
        Ok(())
    }

    async fn pi_lock(&self, account_id: &str) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let semaphore = {
            let mut locks = self
                .auth_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            locks
                .entry(format!("pi:{account_id}"))
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
                .clone()
        };
        semaphore.acquire_owned().await.ok()
    }

    /// The account's credential, refreshed first when Pi would refresh it
    /// (its stored expiry, margin included, has passed). A refresh, once
    /// sent, runs to the end in a task of its own holding the account's
    /// lock, and writes only over the tokens it spent: refresh tokens rotate
    /// ("refresh_token_reused" is an error), so a refresh abandoned midway
    /// would leave the stored one spent (credentials.rs, the same rule for
    /// the Codex credential).
    pub async fn pi_credential(&self, account_id: &str) -> Result<Credential, TokenError> {
        let home = self.pi_home(account_id);
        let missing = || TokenError {
            permanent: true,
            message: "not signed in for the pi channel".to_string(),
        };
        let current = read_credential(&home).ok_or_else(missing)?;
        if current.expires > now_ms() {
            return Ok(current);
        }
        let permit = self.pi_lock(account_id).await;
        let spent = read_credential(&home).ok_or_else(missing)?;
        if spent.expires > now_ms() {
            // Another request refreshed it while this one waited.
            return Ok(spent);
        }
        let transports = self.transports.clone();
        let logger = self.logger.clone();
        let token_url = self.pi_channel.endpoints().token;
        let settling = super::credentials::SettlingRefresh::start(self.refreshes_settling.clone());
        let account = account_id.to_string();
        let task = tokio::spawn(async move {
            let _permit = permit;
            let _settling = settling;
            let body = form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &spent.client_id),
                ("refresh_token", &spent.refresh),
                ("resource", RESOURCE),
            ]);
            let reply = match tokio::time::timeout(
                REFRESH_TIMEOUT,
                token_call(&transports, &token_url, body),
            )
            .await
            {
                Ok(reply) => reply?,
                Err(_) => {
                    return Err(TokenError {
                        permanent: false,
                        message: "pi token refresh timed out".to_string(),
                    });
                }
            };
            let refreshed = credential_from_reply(&reply, &spent.client_id, now_ms())?;
            match read_credential(&home) {
                Some(stored) if stored.refresh == spent.refresh => {
                    write_credential(&home, &refreshed).map_err(|error| TokenError {
                        permanent: false,
                        message: format!("store refreshed pi token: {error}"),
                    })?;
                    Ok(refreshed)
                }
                stored => {
                    logger.printf(&format!(
                        "refreshed pi token for {account} discarded: the account's credential changed during the refresh"
                    ));
                    stored.ok_or_else(|| TokenError {
                        permanent: true,
                        message: "not signed in for the pi channel".to_string(),
                    })
                }
            }
        });
        task.await.unwrap_or_else(|error| {
            Err(TokenError {
                permanent: false,
                message: format!("pi token refresh: {error}"),
            })
        })
    }

    /// One send on this channel: the account's plan-usage token, Pi's
    /// headers, the request already in Pi's form. No 401 retry: Pi does not
    /// refresh on a 401, and the docs say a 401 is diagnosed, not answered
    /// with another sign-in.
    pub async fn pi_forward(
        &self,
        candidate: &Account,
        body: Bytes,
        inbound: &HeaderMap,
    ) -> Result<UpstreamResponse, ForwardError> {
        let credential = match self.pi_credential(&candidate.id).await {
            Ok(credential) => credential,
            Err(error) if error.permanent => {
                self.pi_mark(&candidate.id, Unavailable::SignIn(error.message.clone()));
                return Err(ForwardError::Auth(error.message));
            }
            Err(error) => return Err(ForwardError::Transport(error.message)),
        };
        let (headers, order) = pi_request_headers(&credential.access, inbound);
        let (request, after) = crate::transport::build_request(
            Method::POST,
            self.pi_channel.endpoints().responses,
            &headers,
            body,
            codex_http_client::RequestCompression::None,
            Some(&order),
        )
        .map_err(ForwardError::Transport)?;
        let mut response = crate::transport::send_request(&self.transports, request, &after)
            .await
            .map_err(|failure| ForwardError::Transport(format!("pi upstream: {failure}")))?;
        // This route answers its stream with no Content-Type at all, as the
        // Codex backend does (measured: the first live answer's header names
        // had none). Every request here streams, so a success without one is
        // that stream; read as a plain body, the pool saw no terminal event
        // and no usage, and the pre-commit window never ran (App::forward
        // does the same for the Codex channel).
        if response.status.is_success()
            && super::http_io::header_value(&response.headers, "content-type")
                .trim()
                .is_empty()
        {
            response.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/event-stream"),
            );
        }
        Ok(response)
    }

    /// The accounts this channel cannot use now, for the attempt loop's
    /// exclusions: no credential, refused, or resting after a usage limit.
    pub fn pi_unusable_accounts(&self) -> Vec<String> {
        let now = Timestamp::now();
        // Only an account the pool signs in to ChatGPT with can carry plan
        // usage; an API-key slot never goes on this channel.
        let ids: Vec<(String, std::path::PathBuf, bool)> = {
            let inner = self.read();
            inner
                .config
                .accounts()
                .iter()
                .map(|item| {
                    (
                        item.id.clone(),
                        inner.account_codex_home(&item.id),
                        App::uses_codex_backend_directly(item),
                    )
                })
                .collect()
        };
        ids.into_iter()
            .filter(|(id, home, chatgpt)| {
                !chatgpt
                    || read_credential(home).is_none()
                    || self.pi_unavailable(id, now).is_some()
            })
            .map(|(id, _, _)| id)
            .collect()
    }

    /// The channel's state of each account, for the Console.
    pub fn pi_account_states(&self) -> Vec<Value> {
        let now = Timestamp::now();
        let ids: Vec<(String, std::path::PathBuf)> = {
            let inner = self.read();
            inner
                .config
                .accounts()
                .iter()
                .filter(|item| super::plan::is_codex_device_auth(item))
                .map(|item| (item.id.clone(), inner.account_codex_home(&item.id)))
                .collect()
        };
        ids.into_iter()
            .map(|(id, home)| {
                let signed_in = read_credential(&home).is_some();
                let (state, detail) = match self.pi_unavailable(&id, now) {
                    Some(Unavailable::SignIn(detail)) => ("sign_in", detail),
                    Some(Unavailable::Refused(code)) => ("refused", code),
                    Some(Unavailable::Limited { code, until }) => {
                        ("limited", format!("{code} until {}", until.unix()))
                    }
                    None if signed_in => ("ready", String::new()),
                    None => ("sign_in", "not signed in".to_string()),
                };
                serde_json::json!({
                    "accountId": id,
                    "state": state,
                    "detail": detail,
                    "usage": self.pi_usage(&id),
                    // Whether the account holds a Codex sign-in at all, for
                    // the Codex badge: an account signed in for Pi alone
                    // shows none.
                    "codexSignedIn": home.join("auth.json").is_file(),
                })
            })
            .collect()
    }
}

/// What an upstream answer on this channel means for the account, from
/// OpenAI's "Errors and recovery" table. `None` leaves the answer to the
/// caller as it came.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The account's credential: the owner signs in again; try another.
    SignIn(String),
    /// This account cannot use the route; try another.
    Refused(String),
    /// The plan's or this app's usage limit: rest the account, try another.
    Limited(String),
    /// Upstream's condition, not the account's: retry this account after a
    /// bounded backoff ("Retry with bounded backoff. Keep credentials.").
    Retry(String),
    /// The request or the integration is wrong; no account can serve it:
    /// answer the caller as upstream answered.
    Caller(String),
}

/// Reads a non-success answer (status and body) the way the docs say an app
/// must act on it.
pub fn verdict(status: http::StatusCode, body: &[u8]) -> Option<Verdict> {
    let value = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    let code = value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .map(super::quota::sanitized_error_code)
        .unwrap_or_default();
    // Pre-stream admission errors carry only {"detail": "..."}.
    let detail_only = code.is_empty() && value.get("detail").is_some();
    match (status.as_u16(), code.as_str()) {
        (_, "subscription_sharing_usage_limit_exceeded") => Some(Verdict::Limited(code)),
        (_, "subscription_sharing_user_not_eligible") => Some(Verdict::Refused(code)),
        (_, "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable") => {
            Some(Verdict::Retry(code))
        }
        (
            _,
            "subscription_sharing_unsupported_capability"
            | "subscription_sharing_route_not_supported"
            | "chatpass_v2_scope_not_authorized"
            | "chatpass_v2_invalid_authorization_context",
        ) => Some(Verdict::Caller(code)),
        (_, "subscription_sharing_invalid_user") => Some(Verdict::SignIn(code)),
        (401, _) if detail_only || code.is_empty() => {
            Some(Verdict::SignIn("unauthorized".to_string()))
        }
        // "Policy or permission check failed (e.g., serving region)": the
        // account's, as far as the pool can tell; another may pass.
        (403, _) if detail_only => Some(Verdict::Refused("forbidden".to_string())),
        (503, _) if detail_only => Some(Verdict::Retry("direct_routing_unavailable".to_string())),
        _ => None,
    }
}

/// The terminal failure code of a stream that failed before any output,
/// when it is one of this route's usage codes ("Usage-limit failures can
/// arrive mid-stream as response.failed").
pub fn stream_failure_verdict(code: &str) -> Option<Verdict> {
    let code = super::quota::sanitized_error_code(code);
    match code.as_str() {
        "subscription_sharing_usage_limit_exceeded" => Some(Verdict::Limited(code)),
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            Some(Verdict::Retry(code))
        }
        _ => None,
    }
}

/// How long an account rests after a usage limit. The docs give no reset
/// time ("Don't infer a reset time, since an app-specific limit may also
/// apply"), so the pool waits a fixed while and lets the next request find
/// out; the quota poll still shows the plan's own windows.
pub const LIMIT_REST: chrono::TimeDelta = chrono::TimeDelta::minutes(30);

/// The request gate's part: fields Pi leaves out on this route are taken out
/// of an app's request before it is checked, so a Pi pointed at the pool
/// (which sends max_output_tokens) is not refused for what Pi itself drops.
pub fn strip_pi_omitted(payload: &mut Map<String, Value>) -> Vec<String> {
    let mut removed = Vec::new();
    for name in PI_OMITTED_FIELDS {
        if payload.remove(*name).is_some() {
            removed.push((*name).to_string());
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pi_request_reaches_upstream_in_pis_form() {
        // What a Pi pointed at the pool sent (pi-reference.json
        // `viaPool.body`, trimmed): max_output_tokens is dropped as Pi drops
        // it when signed in; the rest keeps Pi's order and bytes.
        let body = br#"{"model":"gpt-5.5","input":[{"role":"developer","content":"sys"},{"role":"user","content":[{"type":"input_text","text":"say hi"}]}],"stream":true,"prompt_cache_key":"s1","store":false,"max_output_tokens":128000,"tools":[{"type":"function","name":"read","parameters":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}},"strict":false}],"reasoning":{"effort":"medium","summary":"auto"},"include":["reasoning.encrypted_content"]}"#;
        let (out, removed) = pi_request_body(body, "gpt-5.5", None).unwrap();
        assert_eq!(removed, vec!["max_output_tokens".to_string()]);
        assert_eq!(
            String::from_utf8(out.to_vec()).unwrap(),
            r#"{"model":"gpt-5.5","input":[{"role":"developer","content":"sys"},{"role":"user","content":[{"type":"input_text","text":"say hi"}]}],"stream":true,"prompt_cache_key":"s1","store":false,"tools":[{"type":"function","name":"read","parameters":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}},"strict":false}],"reasoning":{"effort":"medium","summary":"auto"},"include":["reasoning.encrypted_content"]}"#
        );
    }

    #[test]
    fn an_apps_request_is_written_as_pi_writes_one() {
        let body = br#"{"input":"hello","model":"gpt-5.5","instructions":"be brief","stream":false,"temperature":0.2}"#;
        let (out, removed) = pi_request_body(body, "gpt-5.5", None).unwrap();
        assert_eq!(removed, vec!["temperature".to_string()]);
        assert_eq!(
            String::from_utf8(out.to_vec()).unwrap(),
            r#"{"model":"gpt-5.5","input":[{"role":"developer","content":"be brief"},{"role":"user","content":[{"type":"input_text","text":"hello"}]}],"stream":true,"store":false}"#
        );
    }

    #[test]
    fn a_system_item_becomes_a_developer_message() {
        let (out, _) = pi_request_body(
            br#"{"model":"m","input":[{"role":"system","content":"x"}]}"#,
            "m",
            None,
        )
        .unwrap();
        assert!(
            String::from_utf8(out.to_vec())
                .unwrap()
                .contains(r#""role":"developer""#)
        );
    }

    #[test]
    fn headers_are_pis_with_the_clients_session() {
        let mut inbound = HeaderMap::new();
        inbound.insert("session_id", http::HeaderValue::from_static("abc"));
        inbound.insert(
            "user-agent",
            http::HeaderValue::from_static("pi-team/1.5.0 (linux; node/v22; x64)"),
        );
        let (headers, order) = pi_request_headers("tok", &inbound);
        assert_eq!(headers.get("authorization").unwrap(), "Bearer tok");
        assert_eq!(
            headers.get("user-agent").unwrap(),
            "pi/1.5.0 (linux; node/v22.23.2; x64)"
        );
        assert_eq!(headers.get("session_id").unwrap(), "abc");
        assert_eq!(headers.get("x-client-request-id").unwrap(), "abc");
        assert!(order.starts_with("accept,x-stainless-retry-count,"));
        assert_eq!(client_name(&inbound), "pi-team");
    }

    #[test]
    fn a_request_without_a_session_carries_none() {
        let (headers, order) = pi_request_headers("tok", &HeaderMap::new());
        assert!(headers.get("session_id").is_none());
        assert!(!order.contains("session_id"));
    }

    #[test]
    fn upstream_answers_are_read_as_the_docs_say() {
        let body = |code: &str| format!(r#"{{"error":{{"code":"{code}","message":"m"}}}}"#);
        let check = |status: u16, body: &str| {
            verdict(http::StatusCode::from_u16(status).unwrap(), body.as_bytes())
        };
        assert_eq!(
            check(429, &body("subscription_sharing_usage_limit_exceeded")),
            Some(Verdict::Limited(
                "subscription_sharing_usage_limit_exceeded".into()
            ))
        );
        assert_eq!(
            check(403, &body("subscription_sharing_user_not_eligible")),
            Some(Verdict::Refused(
                "subscription_sharing_user_not_eligible".into()
            ))
        );
        assert_eq!(
            check(400, &body("subscription_sharing_unsupported_capability")),
            Some(Verdict::Caller(
                "subscription_sharing_unsupported_capability".into()
            ))
        );
        assert_eq!(
            check(503, &body("subscription_sharing_usage_unavailable")),
            Some(Verdict::Retry(
                "subscription_sharing_usage_unavailable".into()
            ))
        );
        assert_eq!(
            check(401, r#"{"detail":"x"}"#),
            Some(Verdict::SignIn("unauthorized".into()))
        );
        assert_eq!(
            check(503, r#"{"detail":"x"}"#),
            Some(Verdict::Retry("direct_routing_unavailable".into()))
        );
        assert_eq!(check(400, &body("invalid_value")), None);
    }

    #[test]
    fn a_host_id_is_made_once() {
        let dir = super::super::testing::TempDir::new();
        let first =
            resolve_host_id(dir.path(), || "0F8FAD5B-D9CB-469F-A165-70867728950E".into()).unwrap();
        assert_eq!(first, "urn:uuid:0f8fad5b-d9cb-469f-a165-70867728950e");
        let again = resolve_host_id(dir.path(), || unreachable!()).unwrap();
        assert_eq!(again, first);
    }
}
