//! Which account serves a request: the routing decision (sticky key,
//! lineage, balance key) and account selection (sticky sessions, strict
//! lineage binding, balanced placement, the red outline and Use Pro last).
//!
//! A port of routingDecision, selectAccount* and pickBalancedAccountLocked in
//! the Go pool's main.go. The comments that say why a rule exists come with
//! it: most record a session that was once split, stranded or moved wrongly.

use super::app::App;
use super::app::Inner;
use super::app::PRO_TIER_ORDINARY;
use super::app::PRO_TIER_PRO;
use super::gojson;
use super::gosort::slice_stable;
use super::hash;
use super::plan;
use super::quota;
use super::settings::ROUTING_STRATEGY_FAILOVER;
use super::state::Account;
use super::state::AccountQuota;
use super::state::ResponseBinding;
use super::state::StickySession;
use super::state::ThreadBinding;
use super::time::Timestamp;
use super::time::hours;
use super::time::minutes;
use chrono::TimeDelta;
use http::HeaderMap;
use serde_json::Map;
use serde_json::Value;
use std::collections::HashMap;

/// Longest identity value (thread, session, lineage id) read from a request.
pub const MAX_REQUEST_IDENTITY_VALUE: usize = 512;
/// How recently a session must have sent a request through an account to
/// count as a session that account is carrying.
pub const PLACEMENT_ACTIVE_WINDOW_MINUTES: i64 = 30;
/// Request-level routing diagnostics are a short rolling window, not a
/// durable ledger: the count bounds busy pools, the TTL removes stale events
/// after traffic stops.
pub const ROUTING_CACHE_EVENT_LIMIT: usize = 500;
pub const ROUTING_CACHE_EVENT_TTL_HOURS: i64 = 24;
/// The dashboard's red outline: an available reset credit expiring within
/// seven days.
pub const RESET_CREDIT_EXPIRY_WINDOW_HOURS: i64 = 7 * 24;

#[derive(Debug)]
pub enum SelectError {
    /// The session's account is completing sign-in repair; the caller should
    /// retry shortly rather than fail over and lose the KV cache.
    AuthRepairPending,
    NoEligibleAccount,
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthRepairPending => f.write_str("sticky account sign-in repair is in progress"),
            Self::NoEligibleAccount => {
                f.write_str("no eligible account is available for the requested model")
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestIdentity {
    pub session_id: String,
    pub thread_id: String,
    pub parent_thread_id: String,
    pub forked_from_id: String,
    pub lineage_root_id: String,
    pub subagent_kind: String,
    pub thread_source: String,
    pub is_subagent: bool,
}

/// Local diagnostic metadata about the sender, recorded in routing events
/// only. It must never be relayed or added upstream; the upstream request
/// carries exactly what the Codex client sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientFingerprint {
    pub originator: String,
    pub user_agent: String,
    pub request_kind: String,
    /// What the final attempt did with the client's X-Codex-Turn-State; set
    /// per attempt, not read from the request.
    pub turn_state: String,
    /// "native/http", "native/ws", "v1/http" or "v1/ws".
    pub entry: String,
}

/// The sticky key and the upstream prompt cache key are deliberately kept
/// separate. Codex main/child threads keep independent sticky keys (so response
/// chains, thread bindings and cache-key buckets stay per thread), but they
/// share one account through lineage_account_id; a bound previous_response_id
/// remains authoritative across client metadata skew.
#[derive(Clone, Debug, Default)]
pub struct RoutingDecision {
    pub sticky_key: String,
    pub upstream_prompt_cache_key: String,
    pub source: String,
    pub source_value: String,
    pub identity: RequestIdentity,
    pub client_prompt_cache_key: String,
    /// The account the request's Codex lineage (main agent plus every
    /// subagent) is pinned to. With lineage affinity, strict selection uses it
    /// instead of the thread's own sticky binding and refreshes it on every
    /// attempt, so it holds the binding this request last observed (which
    /// mark_success needs to reject stale moves). Without lineage affinity it
    /// is only the parent's account, a soft preference after the thread's own
    /// sticky route.
    pub lineage_account_id: String,
    pub lineage_affinity: bool,
    /// Seeds deterministic new-route assignment. With lineage affinity,
    /// lineage members share it so a main agent and its subagents that
    /// independently need a new or failover account still converge on one.
    pub balance_key: String,
    pub parent_affinity_attempted: bool,
    pub client: ClientFingerprint,
}

/// What the routing decision needs to know about the inbound request.
pub struct RequestFacts<'a> {
    pub headers: &'a HeaderMap,
    pub native: bool,
    pub websocket: bool,
}

#[derive(Clone, Debug)]
pub struct SessionPlacement {
    pub account_id: String,
    pub at: Timestamp,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

// ---- request identity ----

/// Accepts multiple Codex metadata generations. Canonical turn metadata wins
/// over flat client metadata, compatibility headers and top-level fallbacks;
/// malformed JSON is ignored so version skew cannot reject an otherwise valid
/// Responses request.
pub fn request_identity_from(headers: &HeaderMap, payload: &Map<String, Value>) -> RequestIdentity {
    let mut identity = RequestIdentity::default();
    let client_metadata = metadata_object(payload.get("client_metadata"));
    let turn_metadata = client_metadata.as_ref().and_then(|metadata| {
        first_metadata_value(
            metadata,
            &["x-codex-turn-metadata", "x_codex_turn_metadata"],
        )
    });
    merge_request_identity(&mut identity, metadata_object(turn_metadata).as_ref());
    merge_request_identity(&mut identity, client_metadata.as_ref());
    merge_request_identity(
        &mut identity,
        metadata_object(Some(&Value::String(
            header(headers, "x-codex-turn-metadata").to_string(),
        )))
        .as_ref(),
    );
    let compatibility: Map<String, Value> = [
        ("session_id", "x-codex-session-id"),
        ("thread_id", "x-codex-thread-id"),
        ("parent_thread_id", "x-codex-parent-thread-id"),
        ("forked_from_thread_id", "x-codex-forked-from-thread-id"),
        ("lineage_root_id", "x-codex-lineage-root-id"),
        ("subagent_kind", "x-openai-subagent"),
        ("thread_source", "x-codex-thread-source"),
    ]
    .into_iter()
    .map(|(key, name)| {
        (
            key.to_string(),
            Value::String(header(headers, name).to_string()),
        )
    })
    .collect();
    merge_request_identity(&mut identity, Some(&compatibility));
    merge_request_identity(&mut identity, Some(payload));
    if identity.parent_thread_id.is_empty() {
        identity
            .parent_thread_id
            .clone_from(&identity.forked_from_id);
    }
    identity.is_subagent = !identity.parent_thread_id.is_empty()
        || !identity.forked_from_id.is_empty()
        || !identity.subagent_kind.is_empty()
        || identity.thread_source.to_lowercase().contains("subagent");
    identity
}

fn merge_request_identity(identity: &mut RequestIdentity, values: Option<&Map<String, Value>>) {
    let Some(values) = values.filter(|values| !values.is_empty()) else {
        return;
    };
    let set = |target: &mut String, keys: &[&str]| {
        if target.is_empty() {
            *target = metadata_string(values, keys);
        }
    };
    set(&mut identity.session_id, &["session_id", "sessionId"]);
    set(&mut identity.thread_id, &["thread_id", "threadId"]);
    set(
        &mut identity.parent_thread_id,
        &[
            "parent_thread_id",
            "parentThreadId",
            "x-codex-parent-thread-id",
        ],
    );
    set(
        &mut identity.forked_from_id,
        &["forked_from_thread_id", "forkedFromThreadId"],
    );
    set(
        &mut identity.lineage_root_id,
        &["lineage_root_id", "lineageRootId"],
    );
    set(
        &mut identity.subagent_kind,
        &["subagent_kind", "subagentKind", "x-openai-subagent"],
    );
    set(
        &mut identity.thread_source,
        &["thread_source", "threadSource"],
    );
}

/// An object, or a string holding a JSON object.
pub fn metadata_object(value: Option<&Value>) -> Option<Map<String, Value>> {
    match value? {
        Value::Object(map) => Some(map.clone()),
        Value::String(text) => serde_json::from_str::<Map<String, Value>>(text).ok(),
        _ => None,
    }
}

pub fn first_metadata_value<'a>(
    values: &'a Map<String, Value>,
    keys: &[&str],
) -> Option<&'a Value> {
    keys.iter().find_map(|key| values.get(*key))
}

pub fn metadata_string(values: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| values.get(*key))
        .map(request_identity_string)
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

pub fn string_value(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn request_identity_string(value: &Value) -> String {
    let mut text = string_value(Some(value));
    if text.len() > MAX_REQUEST_IDENTITY_VALUE {
        let mut cut = MAX_REQUEST_IDENTITY_VALUE;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

pub fn identity_parent_id(identity: &RequestIdentity) -> &str {
    if identity.parent_thread_id.is_empty() {
        &identity.forked_from_id
    } else {
        &identity.parent_thread_id
    }
}

pub fn thread_binding_state_key(model: &str, thread_id: &str) -> String {
    format!("{model}:{thread_id}")
}

// ---- client fingerprint ----

/// Keeps only short, low-cardinality labels: the originator, the
/// user-agent's product token (the OS/terminal comment after it is dropped),
/// and the turn metadata request_kind. Each value is bounded and reduced to
/// token characters so a client cannot write arbitrary text into persisted
/// telemetry.
pub fn client_fingerprint_from(
    facts: &RequestFacts<'_>,
    payload: &Map<String, Value>,
) -> ClientFingerprint {
    let user_agent = header(facts.headers, "user-agent").trim();
    let user_agent = user_agent
        .split_once(' ')
        .map_or(user_agent, |(product, _)| product);
    let mut request_kind = metadata_object(Some(&Value::String(
        header(facts.headers, "x-codex-turn-metadata").to_string(),
    )))
    .map(|metadata| metadata_string(&metadata, &["request_kind"]))
    .unwrap_or_default();
    if request_kind.is_empty() {
        let client_metadata = metadata_object(payload.get("client_metadata"));
        let turn = client_metadata.as_ref().and_then(|metadata| {
            first_metadata_value(
                metadata,
                &["x-codex-turn-metadata", "x_codex_turn_metadata"],
            )
        });
        request_kind = metadata_object(turn)
            .map(|metadata| metadata_string(&metadata, &["request_kind"]))
            .unwrap_or_default();
    }
    let mut entry = if facts.native { "native" } else { "v1" }.to_string();
    entry.push_str(if facts.websocket { "/ws" } else { "/http" });
    ClientFingerprint {
        originator: telemetry_label(header(facts.headers, "originator")),
        user_agent: telemetry_label(user_agent),
        request_kind: telemetry_label(&request_kind),
        turn_state: String::new(),
        entry,
    }
}

pub fn telemetry_label(value: &str) -> String {
    const MAX_LEN: usize = 64;
    let mut label = String::new();
    for character in value.trim().chars() {
        if label.len() >= MAX_LEN {
            break;
        }
        if character.is_ascii_alphanumeric() || "._-/:+".contains(character) {
            label.push(character);
        } else {
            label.push('_');
        }
    }
    label
}

// ---- prompt cache keys ----

pub fn conversation_id(payload: &Map<String, Value>) -> String {
    match payload.get("conversation") {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Object(object)) => ["id", "conversation_id", "conversationId"]
            .iter()
            .filter_map(|key| object.get(*key).and_then(Value::as_str))
            .map(str::trim)
            .find(|id| !id.is_empty())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

pub fn normalized_prompt_prefix(payload: &Map<String, Value>) -> Vec<u8> {
    let mut prefix = Map::new();
    if let Some(value) = payload.get("input") {
        prefix.insert("input".to_string(), value.clone());
    } else if let Some(value) = payload.get("messages") {
        prefix.insert("messages".to_string(), value.clone());
    }
    for name in ["tools", "text", "response_format"] {
        if let Some(value) = payload.get(name) {
            prefix.insert(name.to_string(), value.clone());
        }
    }
    let mut encoded = gojson::marshal(&Value::Object(prefix));
    encoded.truncate(8192);
    encoded
}

impl Inner {
    pub fn auto_prompt_cache_key_enabled(&self) -> bool {
        let mode = &self.settings.prompt_cache_key_mode;
        mode.is_empty() || mode == "auto"
    }

    /// The coarse grouping ("project" or "user") and its value, or None to
    /// fall back to per-conversation keys.
    fn prompt_cache_scope(
        &self,
        headers: &HeaderMap,
        api_key: &str,
    ) -> Option<(&'static str, String)> {
        let project = header(headers, "x-codex-pool-project").trim().to_string();
        let fingerprint = hash::api_key_fingerprint(api_key);
        let has_user = fingerprint != "anonymous";
        match self.settings.prompt_cache_key_scope.as_str() {
            "project" | "auto" if !project.is_empty() => Some(("project", project)),
            "project" | "user" | "auto" if has_user => Some(("user", fingerprint)),
            _ => None,
        }
    }

    /// The prompt_cache_key. The default "auto" (and the explicit
    /// "project"/"user") scopes group conversations that share the same
    /// static prefix (system prompt + tools) under one key so they reuse each
    /// other's prompt cache on the account the router already concentrates
    /// them on. The sticky key is hashed into a small number of buckets so each
    /// (prefix + key) combination stays under OpenAI's ~15 RPM cache-routing
    /// limit. Without a coarse signal the key falls back to the historical
    /// per-conversation format, so behaviour never gets worse.
    fn scoped_prompt_cache_key(
        &self,
        headers: &HeaderMap,
        model: &str,
        api_key: &str,
        source: &str,
        value: &str,
        sticky_key: &str,
    ) -> String {
        match self.prompt_cache_scope(headers, api_key) {
            None => hash::prompt_cache_key_hash(model, source, value),
            Some((scope, coarse)) => {
                let bucket = self.prompt_cache_bucket(sticky_key);
                hash::prompt_cache_key_hash(model, scope, &format!("{coarse}#{bucket}"))
            }
        }
    }

    fn prompt_cache_bucket(&self, sticky_key: &str) -> i32 {
        hash::prompt_cache_bucket_index(
            sticky_key,
            i32::try_from(self.settings.prompt_cache_buckets).unwrap_or(1),
        )
    }

    pub fn upstream_prompt_cache_key(
        &self,
        headers: &HeaderMap,
        model: &str,
        api_key: &str,
        route: &RoutingDecision,
    ) -> String {
        let bucket = self.prompt_cache_bucket(&route.sticky_key);
        match self.settings.prompt_cache_key_policy.as_str() {
            "lineage" => {
                let root = [
                    &route.identity.lineage_root_id,
                    &route.identity.thread_id,
                    &route.sticky_key,
                ]
                .into_iter()
                .find(|value| !value.is_empty())
                .cloned()
                .unwrap_or_default();
                hash::prompt_cache_key_hash(model, "lineage", &format!("{root}#{bucket}"))
            }
            "project" => {
                let mut project = header(headers, "x-codex-pool-project").trim().to_string();
                if project.is_empty() {
                    project = format!("user:{}", hash::api_key_fingerprint(api_key));
                }
                hash::prompt_cache_key_hash(model, "project", &format!("{project}#{bucket}"))
            }
            "user" => hash::prompt_cache_key_hash(
                model,
                "user",
                &format!("{}#{bucket}", hash::api_key_fingerprint(api_key)),
            ),
            _ => {
                if !route.client_prompt_cache_key.is_empty() {
                    return route.client_prompt_cache_key.clone();
                }
                if !self.auto_prompt_cache_key_enabled() {
                    return String::new();
                }
                self.scoped_prompt_cache_key(
                    headers,
                    model,
                    api_key,
                    &route.source,
                    &route.source_value,
                    &route.sticky_key,
                )
            }
        }
    }

    /// Preserve is the compatibility default: an existing Codex thread key
    /// must not be rewritten. Only an explicit lineage/project/user policy may
    /// replace a client key, independently of the legacy missing-key
    /// auto-injection mode.
    pub fn apply_prompt_cache_controls(
        &self,
        payload: &mut Map<String, Value>,
        route: &RoutingDecision,
    ) {
        let retention = &self.settings.prompt_cache_retention;
        if !retention.is_empty() && !payload.contains_key("prompt_cache_retention") {
            payload.insert(
                "prompt_cache_retention".to_string(),
                Value::String(retention.clone()),
            );
        }
        let policy = self.settings.prompt_cache_key_policy.as_str();
        if (policy.is_empty() || policy == "preserve")
            && (!self.auto_prompt_cache_key_enabled() || payload.contains_key("prompt_cache_key"))
        {
            return;
        }
        if !route.upstream_prompt_cache_key.is_empty() {
            payload.insert(
                "prompt_cache_key".to_string(),
                Value::String(route.upstream_prompt_cache_key.clone()),
            );
        }
    }

    // ---- bindings ----

    pub fn sticky_ttl(&self) -> TimeDelta {
        if self.settings.session_affinity_ttl > TimeDelta::zero() {
            self.settings.session_affinity_ttl
        } else {
            hours(24)
        }
    }

    pub fn sticky_expires_at(&self, item: &StickySession) -> Timestamp {
        if !item.expires_at.is_zero() {
            return item.expires_at;
        }
        let base = if item.last_success_at.is_zero() {
            item.created_at
        } else {
            item.last_success_at
        };
        if base.is_zero() {
            return Timestamp::zero();
        }
        base.add(self.sticky_ttl())
    }

    pub fn sticky_session_expired(&self, item: &StickySession, now: Timestamp) -> bool {
        let expires_at = self.sticky_expires_at(item);
        expires_at.is_zero() || !expires_at.after(&now)
    }

    pub fn prune_expired_sticky_sessions(&mut self, now: Timestamp) -> bool {
        let expired: Vec<String> = self
            .state
            .sticky_sessions()
            .iter()
            .filter(|(_, item)| self.sticky_session_expired(item, now))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &expired {
            self.state.sticky_sessions_mut().remove(key);
        }
        !expired.is_empty()
    }

    pub fn prune_expired_runtime_state(&mut self, now: Timestamp) -> bool {
        let mut changed = self.prune_expired_sticky_sessions(now);
        let before = self.state.response_bindings().len()
            + self.state.thread_bindings().len()
            + self.state.lineage_bindings().len();
        self.state
            .response_bindings_mut()
            .retain(|_, binding| binding.expires_at.after(&now));
        self.state
            .thread_bindings_mut()
            .retain(|_, binding| binding.expires_at.after(&now));
        if self.state.lineage_bindings.is_some() {
            self.state
                .lineage_bindings_mut()
                .retain(|_, binding| binding.expires_at.after(&now));
        }
        let after = self.state.response_bindings().len()
            + self.state.thread_bindings().len()
            + self.state.lineage_bindings().len();
        changed |= before != after;
        changed |= self.prune_routing_cache_events(now);
        changed
    }

    pub fn prune_routing_cache_events(&mut self, now: Timestamp) -> bool {
        let events = self.state.routing_cache_events();
        if events.is_empty() {
            return false;
        }
        let cutoff = now.add(-hours(ROUTING_CACHE_EVENT_TTL_HOURS));
        let mut first = events
            .iter()
            .take_while(|event| event.timestamp.before(&cutoff))
            .count();
        let remaining = events.len() - first;
        if remaining > ROUTING_CACHE_EVENT_LIMIT {
            first += remaining - ROUTING_CACHE_EVENT_LIMIT;
        }
        if first == 0 {
            return false;
        }
        self.state.routing_cache_events_mut().drain(..first);
        true
    }

    pub fn latest_sticky_session(&self, model: &str, now: Timestamp) -> Option<StickySession> {
        let mut best: Option<&StickySession> = None;
        for item in self.state.sticky_sessions().values() {
            if item.model_id != model || self.sticky_session_expired(item, now) {
                continue;
            }
            if best.is_none_or(|best| sticky_activity_time(item).after(&sticky_activity_time(best)))
            {
                best = Some(item);
            }
        }
        best.cloned()
    }

    pub fn clear_sticky_for_account(&mut self, id: &str) {
        self.state
            .sticky_sessions_mut()
            .retain(|_, item| item.account_id != id);
        self.state
            .response_bindings_mut()
            .retain(|_, item| item.account_id != id);
        self.state
            .thread_bindings_mut()
            .retain(|_, item| item.account_id != id);
        if self.state.lineage_bindings.is_some() {
            self.state
                .lineage_bindings_mut()
                .retain(|_, item| item.account_id != id);
        }
        if !self.state.routing_cache_events().is_empty() {
            self.state
                .routing_cache_events_mut()
                .retain(|event| event.account_id != id && event.failover_from_account_id != id);
        }
    }

    /// A thread's TTL-live binding, preferring the requested model. Subagents
    /// frequently run a different model than the main agent, so a same-model
    /// lookup alone would miss the parent and split the lineage onto another
    /// account; fall back to the most recent binding for the thread under any
    /// model.
    pub fn live_thread_binding(
        &self,
        model: &str,
        thread_id: &str,
        now: Timestamp,
    ) -> Option<ThreadBinding> {
        if thread_id.is_empty() {
            return None;
        }
        if let Some(binding) = self
            .state
            .thread_bindings()
            .get(&thread_binding_state_key(model, thread_id))
            && binding.expires_at.after(&now)
        {
            return Some(binding.clone());
        }
        let mut best: Option<&ThreadBinding> = None;
        for binding in self.state.thread_bindings().values() {
            if binding.thread_id != thread_id || !binding.expires_at.after(&now) {
                continue;
            }
            if best.is_none_or(|best| binding.last_success_at.after(&best.last_success_at)) {
                best = Some(binding);
            }
        }
        best.cloned()
    }

    /// The lineage's pinned account. Without a live lineage binding (a lineage
    /// first seen before bindings existed, or switched back on), it seeds from
    /// the member thread that most recently succeeded, any model, so existing
    /// sessions join their current account instead of a new one.
    pub fn lineage_account_id(&self, lineage_root_id: &str, now: Timestamp) -> Option<String> {
        if lineage_root_id.is_empty() {
            return None;
        }
        if let Some(binding) = self.state.lineage_bindings().get(lineage_root_id)
            && !binding.account_id.is_empty()
            && binding.expires_at.after(&now)
        {
            return Some(binding.account_id.clone());
        }
        let mut best: Option<&ThreadBinding> = None;
        for binding in self.state.thread_bindings().values() {
            if binding.lineage_root_id != lineage_root_id
                || binding.account_id.is_empty()
                || !binding.expires_at.after(&now)
            {
                continue;
            }
            let better = best.is_none_or(|best| {
                binding.last_success_at.after(&best.last_success_at)
                    || (binding.last_success_at == best.last_success_at
                        && binding.account_id < best.account_id)
            });
            if better {
                best = Some(binding);
            }
        }
        best.map(|binding| binding.account_id.clone())
    }

    pub fn request_identity_for_sticky(
        &self,
        sticky_key: &str,
        model: &str,
        now: Timestamp,
    ) -> Option<RequestIdentity> {
        self.state
            .thread_bindings()
            .values()
            .find(|binding| {
                binding.model_id == model
                    && binding.sticky_key == sticky_key
                    && binding.expires_at.after(&now)
            })
            .map(|binding| {
                let mut identity = RequestIdentity {
                    session_id: binding.session_id.clone(),
                    thread_id: binding.thread_id.clone(),
                    parent_thread_id: binding.parent_thread_id.clone(),
                    lineage_root_id: binding.lineage_root_id.clone(),
                    subagent_kind: binding.subagent_kind.clone(),
                    ..RequestIdentity::default()
                };
                identity.is_subagent =
                    !identity.parent_thread_id.is_empty() || !identity.subagent_kind.is_empty();
                identity
            })
    }

    pub fn live_sticky_session(&self, sticky_key: &str, now: Timestamp) -> bool {
        self.state
            .sticky_sessions()
            .get(sticky_key)
            .is_some_and(|binding| !self.sticky_session_expired(binding, now))
    }

    /// Fills the lineage root from the parent thread, and reports the parent's
    /// account when the parent is bound.
    pub fn resolve_request_identity(
        &self,
        mut identity: RequestIdentity,
        model: &str,
        now: Timestamp,
    ) -> (RequestIdentity, Option<String>) {
        let parent = self.live_thread_binding(model, identity_parent_id(&identity), now);
        if identity.lineage_root_id.is_empty() {
            let parent_id = identity_parent_id(&identity).to_string();
            identity.lineage_root_id = match &parent {
                Some(parent) if !parent.lineage_root_id.is_empty() => {
                    parent.lineage_root_id.clone()
                }
                Some(parent) => parent.thread_id.clone(),
                None if !parent_id.is_empty() => parent_id,
                None => identity.thread_id.clone(),
            };
        }
        let parent_account = parent.map(|parent| parent.account_id);
        (identity, parent_account)
    }
}

fn sticky_activity_time(item: &StickySession) -> Timestamp {
    if item.last_success_at.is_zero() {
        item.created_at
    } else {
        item.last_success_at
    }
}

impl App {
    /// A previous_response_id binding for the model, dropping it once expired
    /// or bound to another model.
    pub fn response_binding(&self, response_id: &str, model: &str) -> Option<ResponseBinding> {
        let mut inner = self.write();
        let now = Timestamp::now();
        let binding = inner.state.response_bindings().get(response_id).cloned()?;
        if binding.model_id != model || !binding.expires_at.after(&now) {
            inner.state.response_bindings_mut().remove(response_id);
            let _ = inner.save();
            return None;
        }
        Some(binding)
    }

    pub fn routing_decision(
        &self,
        facts: &RequestFacts<'_>,
        payload: &Map<String, Value>,
        model: &str,
        api_key: &str,
    ) -> RoutingDecision {
        let now = Timestamp::now();
        let identity = request_identity_from(facts.headers, payload);
        let (mut identity, mut parent_account) =
            self.read().resolve_request_identity(identity, model, now);
        let client_prompt_cache_key = string_value(payload.get("prompt_cache_key"));
        let lineage_affinity = self.read().lineage_affinity;
        let mut route = None;
        let previous_response_id = string_value(payload.get("previous_response_id"));
        if !previous_response_id.is_empty()
            && let Some(binding) = self.response_binding(&previous_response_id, model)
        {
            // The response chain is stronger evidence than version-skewed
            // turn metadata. Recover the bound thread identity so success does
            // not create a second, incorrect thread binding for the same
            // continuation.
            let bound = self
                .read()
                .request_identity_for_sticky(&binding.sticky_key, model, now);
            if let Some(bound) = bound {
                (identity, parent_account) =
                    self.read().resolve_request_identity(bound, model, now);
            }
            route = Some(decision(
                &binding.sticky_key,
                "previous_response_id",
                &binding.sticky_key,
            ));
        }
        let route = route
            .or_else(|| {
                (!identity.thread_id.is_empty()).then(|| {
                    decision(
                        &format!("{model}:thread:{}", identity.thread_id),
                        "thread_id",
                        &identity.thread_id,
                    )
                })
            })
            .or_else(|| {
                [
                    ("x-codex-pool-session", "session"),
                    ("x-codex-pool-project", "project"),
                ]
                .into_iter()
                .find_map(|(name, source)| {
                    let value = header(facts.headers, name).trim();
                    (!value.is_empty())
                        .then(|| decision(&format!("{model}:{value}"), source, value))
                })
            })
            .or_else(|| {
                (!client_prompt_cache_key.is_empty()).then(|| {
                    decision(
                        &format!("{model}:{client_prompt_cache_key}"),
                        "prompt_cache_key",
                        &client_prompt_cache_key,
                    )
                })
            })
            .or_else(|| {
                let value = conversation_id(payload);
                (!value.is_empty())
                    .then(|| decision(&format!("{model}:{value}"), "conversation", &value))
            })
            .or_else(|| {
                ["session_id", "conversation_id"]
                    .into_iter()
                    .find_map(|name| {
                        let value = string_value(payload.get(name));
                        (!value.is_empty())
                            .then(|| decision(&format!("{model}:{value}"), name, &value))
                    })
            })
            .or_else(|| {
                (!previous_response_id.is_empty()).then(|| {
                    let value = format!(
                        "previous:{}",
                        hash::short_hash(previous_response_id.as_bytes())
                    );
                    decision(&format!("{model}:{value}"), "previous_response_id", &value)
                })
            })
            .unwrap_or_else(|| fallback_routing_decision(payload, model, api_key));
        self.finish_route(
            route,
            facts,
            payload,
            model,
            api_key,
            identity,
            parent_account,
            client_prompt_cache_key,
            lineage_affinity,
            now,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_route(
        &self,
        mut route: RoutingDecision,
        facts: &RequestFacts<'_>,
        payload: &Map<String, Value>,
        model: &str,
        api_key: &str,
        identity: RequestIdentity,
        parent_account: Option<String>,
        client_prompt_cache_key: String,
        lineage_affinity: bool,
        now: Timestamp,
    ) -> RoutingDecision {
        let inner = self.read();
        route.identity = identity;
        route.client_prompt_cache_key = client_prompt_cache_key;
        route.lineage_affinity = lineage_affinity;
        route.balance_key.clone_from(&route.sticky_key);
        if lineage_affinity && !route.identity.lineage_root_id.is_empty() {
            route.balance_key = format!("lineage:{}", route.identity.lineage_root_id);
            if let Some(account_id) = inner.lineage_account_id(&route.identity.lineage_root_id, now)
            {
                route.lineage_account_id = account_id;
            }
        }
        if route.lineage_account_id.is_empty()
            && let Some(parent) = parent_account
        {
            route.lineage_account_id = parent;
        }
        route.parent_affinity_attempted = route.identity.is_subagent
            && !identity_parent_id(&route.identity).is_empty()
            && !inner.live_sticky_session(&route.sticky_key, now);
        route.upstream_prompt_cache_key =
            inner.upstream_prompt_cache_key(facts.headers, model, api_key, &route);
        route.client = client_fingerprint_from(facts, payload);
        route
    }
}

fn decision(sticky_key: &str, source: &str, source_value: &str) -> RoutingDecision {
    RoutingDecision {
        sticky_key: sticky_key.to_string(),
        source: source.to_string(),
        source_value: source_value.to_string(),
        ..RoutingDecision::default()
    }
}

pub fn fallback_routing_decision(
    payload: &Map<String, Value>,
    model: &str,
    api_key: &str,
) -> RoutingDecision {
    let mut material = format!("{}:{model}:", hash::api_key_fingerprint(api_key)).into_bytes();
    material.extend_from_slice(&normalized_prompt_prefix(payload));
    let value = hash::short_hash(&material);
    decision(&format!("{model}:prompt:{value}"), "prompt", &value)
}

// ---- selection ----

type Excluded = HashMap<String, bool>;

fn is_excluded(excluded: &Excluded, id: &str) -> bool {
    excluded.get(id).copied().unwrap_or(false)
}

impl Inner {
    pub fn effective_routing_strategy(&self) -> &'static str {
        if self.settings.routing_strategy == ROUTING_STRATEGY_FAILOVER {
            ROUTING_STRATEGY_FAILOVER
        } else {
            super::settings::ROUTING_STRATEGY_BALANCED
        }
    }

    /// Every model the lineage has a live thread binding for, plus the
    /// current request's model (first).
    pub fn lineage_models(
        &self,
        lineage_root_id: &str,
        model: &str,
        now: Timestamp,
    ) -> Vec<String> {
        let mut models = vec![model.to_string()];
        for binding in self.state.thread_bindings().values() {
            if binding.lineage_root_id != lineage_root_id
                || !binding.expires_at.after(&now)
                || binding.model_id.is_empty()
                || models.contains(&binding.model_id)
            {
                continue;
            }
            models.push(binding.model_id.clone());
        }
        models[1..].sort();
        models
    }

    /// The lineage's new account, among accounts usable for every lineage
    /// model, so a subagent on another model cannot move the lineage somewhere
    /// the main agent cannot follow. Only when no account serves every model
    /// does it fall back to the request's model alone; refusing the request
    /// outright would be worse than the unavoidable split.
    pub fn preferred_lineage_account(
        &self,
        balance_key: &str,
        model: &str,
        models: &[String],
        excluded: &Excluded,
        now: Timestamp,
    ) -> Option<Account> {
        let mut eligible = self.eligible_accounts(model, excluded, now);
        if models.len() > 1 {
            let joint: Vec<Account> = eligible
                .iter()
                .filter(|item| {
                    models
                        .iter()
                        .all(|other| other == model || self.usable(item, other, now))
                })
                .cloned()
                .collect();
            if !joint.is_empty() {
                eligible = joint;
            }
        }
        self.pick_balanced_account(balance_key, eligible)
    }

    /// The lineage (or, with lineage affinity off, parent) account preference.
    /// It is never a hard pin: the account must pass every normal eligibility,
    /// duplicate-identity, cooldown and Pro-preservation gate, otherwise an
    /// unhealthy lineage account would block failover. A Pro account under Pro
    /// preservation, or an account without a red outline while a red-outlined
    /// one can serve, yields to the balance-key replacement directly rather
    /// than to this thread's own sticky account, which may differ per member
    /// and would split the lineage; `models` (strict mode) keeps that
    /// replacement usable for the whole lineage. `hold_for_repair` keeps a
    /// lineage on an account completing sign-in repair, the same rule as a
    /// live sticky session.
    #[allow(clippy::too_many_arguments)]
    pub fn lineage_preference(
        &self,
        lineage_account_id: &str,
        balance_key: &str,
        model: &str,
        models: Option<&[String]>,
        excluded: &Excluded,
        hold_for_repair: bool,
        now: Timestamp,
    ) -> Result<Option<Account>, SelectError> {
        let Some(lineage) = self.account(lineage_account_id).cloned() else {
            return Ok(None);
        };
        if self.selectable_account(&lineage, model, excluded, now) {
            if !self.bound_session_may_move(&lineage, now) {
                return Ok(Some(lineage));
            }
            let replacement = match models {
                Some(models) => {
                    self.preferred_lineage_account(balance_key, model, models, excluded, now)
                }
                None => self.preferred_account_for_sticky(balance_key, model, excluded, now),
            };
            if let Some(replacement) = replacement
                && self.bound_session_moves_to(&lineage, &replacement, now)
            {
                return Ok(Some(replacement));
            }
            return Ok(Some(lineage));
        }
        if hold_for_repair
            && !is_excluded(excluded, &lineage.id)
            && self.account_auth_verification_pending(&lineage)
        {
            return Err(SelectError::AuthRepairPending);
        }
        Ok(None)
    }

    /// A bound session normally stays on its account while that account
    /// serves (6.4, failback = new_session_only). Two exceptions move it on its
    /// next request, rewriting the binding on success; under strict lineage
    /// binding the whole lineage follows, at the cost of one prefill on the new
    /// account.
    ///
    ///   - Red outline: a session on an account without an expiring reset
    ///     credit moves to a red-outlined account, a Pro one included. A reset
    ///     credit not spent before it expires is lost, so the owner treats
    ///     those accounts as a special case, ranked above Use Pro last because
    ///     an unspent credit on a Pro account is lost just the same. Placing
    ///     only new sessions there was not enough: a transient overload or a
    ///     one-minute 429 moved running sessions off them for good while the
    ///     red-outlined accounts sat idle.
    ///   - Use Pro last: a session on a higher plan tier moves to an eligible
    ///     lower-tier account, so Pro quota stays the last resort, but never
    ///     off a red outline onto an account without one.
    ///
    /// Each move only goes towards a red outline or down a plan tier, so a
    /// session cannot oscillate, and spending the credit stays the owner's
    /// decision. This is the cheap pre-check that spares computing a
    /// replacement for a session no exception can move.
    pub fn bound_session_may_move(&self, current: &Account, now: Timestamp) -> bool {
        if self.preserve_pro_quota && self.pro_tier(current) > PRO_TIER_ORDINARY {
            return true;
        }
        !self.expiring_reset_credit(current, now)
    }

    pub fn bound_session_moves_to(
        &self,
        current: &Account,
        replacement: &Account,
        now: Timestamp,
    ) -> bool {
        if replacement.id == current.id {
            return false;
        }
        let replacement_red = self.expiring_reset_credit(replacement, now);
        let current_red = self.expiring_reset_credit(current, now);
        if replacement_red && !current_red {
            self.logger.printf(&format!(
                "bound session moves from {} to red-outlined {}",
                current.id, replacement.id
            ));
            return true;
        }
        if current_red && !replacement_red {
            // Use Pro last never pulls a session off a red outline.
            return false;
        }
        self.preserve_pro_quota && self.pro_tier(replacement) < self.pro_tier(current)
    }

    /// The soonest expiry when every active cooldown for this account/model is
    /// a transient upstream overload. A single rate-limit or auth cooldown
    /// disqualifies the account: those carry an upstream instruction to wait,
    /// and ignoring them can deepen the limit.
    pub fn overloaded_only_cooldown(
        &self,
        account_id: &str,
        model: &str,
        now: Timestamp,
    ) -> Option<Timestamp> {
        let mut soonest: Option<Timestamp> = None;
        for cooldown in self.state.cooldowns().get(account_id).into_iter().flatten() {
            if (!model.is_empty() && cooldown.model_id != model)
                || !cooldown.next_retry_at.after(&now)
            {
                continue;
            }
            if !matches!(
                cooldown.reason.as_str(),
                "server_is_overloaded" | "slow_down" | "upstream_5xx"
            ) {
                return None;
            }
            if soonest.is_none_or(|soonest| cooldown.next_retry_at.before(&soonest)) {
                soonest = Some(cooldown.next_retry_at);
            }
        }
        soonest
    }

    /// The account closest to leaving a transient overload cooldown. Every
    /// other eligibility gate still applies: usability is judged at a time past
    /// the overload cooldown, so only that cooldown is neutralised.
    pub fn overloaded_fallback_account(
        &self,
        model: &str,
        excluded: &Excluded,
        now: Timestamp,
    ) -> Option<Account> {
        let excluded_identities = self.excluded_upstream_identities(excluded);
        let mut best: Option<(Account, Timestamp)> = None;
        for item in self.config.accounts() {
            if is_excluded(excluded, &item.id) {
                continue;
            }
            let Some(retry_at) = self.overloaded_only_cooldown(&item.id, model, now) else {
                continue;
            };
            if !self.usable(item, model, retry_at) {
                continue;
            }
            let identity = self.upstream_identity_key(item);
            if !identity.is_empty() && is_excluded(&excluded_identities, &identity) {
                continue;
            }
            let better = match &best {
                None => true,
                Some((best, best_at)) => {
                    retry_at.before(best_at)
                        || (retry_at == *best_at && self.preferred_before(item, best))
                }
            };
            if better {
                best = Some((item.clone(), retry_at));
            }
        }
        best.map(|(item, _)| item)
    }

    pub fn selectable_account(
        &self,
        item: &Account,
        model: &str,
        excluded: &Excluded,
        now: Timestamp,
    ) -> bool {
        if is_excluded(excluded, &item.id) || !self.usable(item, model, now) {
            return false;
        }
        let identity = self.upstream_identity_key(item);
        if !identity.is_empty()
            && is_excluded(&self.excluded_upstream_identities(excluded), &identity)
        {
            return false;
        }
        let primary = self.primary_upstream_account_id(item, model, now);
        primary.is_empty() || primary == item.id
    }

    pub fn preferred_account(
        &self,
        model: &str,
        excluded: &Excluded,
        now: Timestamp,
    ) -> Option<Account> {
        let mut eligible = self.eligible_accounts(model, excluded, now);
        slice_stable(&mut eligible, |left, right| {
            self.preferred_before(left, right)
        });
        eligible.into_iter().next()
    }

    pub fn eligible_accounts(
        &self,
        model: &str,
        excluded: &Excluded,
        now: Timestamp,
    ) -> Vec<Account> {
        // `excluded` is per user request. If one local slot failed, any other
        // slot with the same upstream identity must be excluded too; otherwise
        // a single upstream ChatGPT account can masquerade as multiple
        // failover accounts.
        let excluded_identities = self.excluded_upstream_identities(excluded);
        self.config
            .accounts()
            .iter()
            .filter(|item| {
                if is_excluded(excluded, &item.id) || !self.usable(item, model, now) {
                    return false;
                }
                let identity = self.upstream_identity_key(item);
                if !identity.is_empty() && is_excluded(&excluded_identities, &identity) {
                    return false;
                }
                let primary = self.primary_upstream_account_id(item, model, now);
                primary.is_empty() || primary == item.id
            })
            .cloned()
            .collect()
    }

    /// Assigns only an unbound route; existing sticky routes and parent
    /// affinity are resolved before this. In the balanced strategy the route
    /// key, not arrival order or completed-route counts, chooses the account.
    /// That deterministic choice prevents simultaneous cold starts from
    /// stampeding one "least loaded" account and guarantees concurrent first
    /// turns for the same session select the same credential before a success
    /// has had time to persist the sticky binding.
    pub fn preferred_account_for_sticky(
        &self,
        sticky_key: &str,
        model: &str,
        excluded: &Excluded,
        now: Timestamp,
    ) -> Option<Account> {
        self.pick_balanced_account(sticky_key, self.eligible_accounts(model, excluded, now))
    }

    pub fn pick_balanced_account(
        &self,
        sticky_key: &str,
        mut eligible: Vec<Account>,
    ) -> Option<Account> {
        if eligible.is_empty() {
            return None;
        }
        if self.effective_routing_strategy() == ROUTING_STRATEGY_FAILOVER {
            slice_stable(&mut eligible, |left, right| {
                self.preferred_before(left, right)
            });
            return eligible.into_iter().next();
        }
        let now = Timestamp::now();
        // Accounts holding a reset credit that expires within seven days (the
        // dashboard's red outline) form the first tier: while one can take the
        // session, the session goes to one of them, even a busy one over an
        // idle ordinary account, and even a Pro account over an ordinary one.
        // The owner asked for these to be spent first because the credit is
        // lost at expiry; as a weight alone they still lost about one new
        // session in six to ordinary accounts, which strict binding then kept
        // there. It sits above Use Pro last because an unspent credit on a Pro
        // account is lost just the same; Use Pro last still orders the
        // red-outlined accounts among themselves. A bound session on an account
        // without a red outline also moves here (bound_session_moves_to),
        // because a transient failure once moved running sessions off these
        // accounts for good. Spending the credit stays the owner's decision.
        let expiring: Vec<Account> = eligible
            .iter()
            .filter(|item| self.expiring_reset_credit(item, now))
            .cloned()
            .collect();
        if !expiring.is_empty() {
            eligible = expiring;
        }
        // Preserve the existing priority contract as capacity tiers, then
        // balance sessions only across the best eligible tier. Newly-created
        // accounts all use the same priority, so normal pools distribute
        // evenly; operators who intentionally configured a lower priority still
        // retain a standby tier.
        if self.preserve_pro_quota {
            let lowest = eligible
                .iter()
                .map(|item| self.pro_tier(item))
                .min()
                .unwrap_or(PRO_TIER_PRO);
            eligible.retain(|item| self.pro_tier(item) == lowest);
        }
        let highest = eligible
            .iter()
            .map(|item| item.priority)
            .max()
            .unwrap_or_default();
        let mut tier: Vec<Account> = eligible
            .into_iter()
            .filter(|item| item.priority == highest)
            .collect();

        // Session placement. Separate sessions spread their consumption so no
        // single allowance drains early and every account's remote KV cache
        // stays in use, but an account already carrying sessions keeps its
        // remaining allowance for them rather than taking new ones. So a new
        // session (or a lineage that must move) goes to an idle account when
        // there is one, chosen by remaining allowance times plan multiplier;
        // only when every account is busy does it share one, with the weight
        // falling with the square of the sessions already there. "Busy" counts
        // a session from the moment it is placed, not from its first success,
        // so an account whose window just reset takes one new session and the
        // next goes elsewhere instead of the whole queue sticking to it. The
        // hash keeps the choice deterministic for one balance key, so lineage
        // members placed together converge.
        let active = self.active_session_counts(now, sticky_key);
        let idle: Vec<Account> = tier
            .iter()
            .filter(|item| active.get(&item.id).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();
        if !idle.is_empty() {
            tier = idle;
        }
        let mut selected = tier[0].clone();
        let mut selected_score = self.placement_score(sticky_key, &selected, &active);
        for item in &tier[1..] {
            let score = self.placement_score(sticky_key, item, &active);
            if score > selected_score || (score == selected_score && item.id < selected.id) {
                selected = item.clone();
                selected_score = score;
            }
        }
        Some(selected)
    }

    /// The sessions each account is carrying, leaving out the session being
    /// placed so it never counts against itself.
    pub fn active_session_counts(&self, now: Timestamp, own_key: &str) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for (key, placement) in &self.session_placements {
            if key != own_key && now.sub(&placement.at) <= minutes(PLACEMENT_ACTIVE_WINDOW_MINUTES)
            {
                *counts.entry(placement.account_id.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    /// An account's share of new sessions: remaining allowance (the tighter of
    /// its windows, never below 1 so an eligible account stays reachable)
    /// times its weekly plan multiplier, divided by the square of one plus the
    /// sessions it already carries.
    pub fn placement_weight(&self, item: &Account, active: &HashMap<String, usize>) -> f64 {
        let snapshot = self.quota_snapshot(&item.id);
        #[allow(clippy::cast_precision_loss)]
        let remaining = snapshot
            .quota
            .as_ref()
            .map_or(100.0, |quota| quota::remaining_quota_hint(quota) as f64)
            .max(1.0);
        let choose = |value: &str, fallback: &str| {
            if value.trim().is_empty() {
                fallback.to_string()
            } else {
                value.to_string()
            }
        };
        let (mut multiplier, _) = plan::account_weekly_weight(
            &choose(&snapshot.plan_family, &item.plan_family),
            &choose(&snapshot.plan_limit, &item.plan_limit),
            &choose(&snapshot.seat_type, &item.seat_type),
            self.tuning.premium_seat_weight,
        );
        if multiplier <= 0.0 {
            multiplier = plan::BASE_WEEKLY_WEIGHT;
        }
        #[allow(clippy::cast_precision_loss)]
        let carried = (1 + active.get(&item.id).copied().unwrap_or(0)) as f64;
        remaining
            * multiplier
            * reset_credit_expiry_boost(snapshot.quota.as_ref(), Timestamp::now())
            / (carried * carried)
    }

    /// The weighted rendezvous score weight / -ln(u), with u in (0, 1) drawn
    /// from the balance key and the account's upstream identity.
    pub fn placement_score(
        &self,
        sticky_key: &str,
        item: &Account,
        active: &HashMap<String, usize>,
    ) -> f64 {
        let identity = hash::sticky_identity_or_slot(&self.upstream_identity_key(item), &item.id);
        hash::placement_score(sticky_key, &identity, self.placement_weight(item, active))
    }

    /// The dashboard's red outline: an available reset credit that expires
    /// within seven days.
    pub fn expiring_reset_credit(&self, item: &Account, now: Timestamp) -> bool {
        reset_credit_expiry_boost(self.quota_snapshot(&item.id).quota.as_ref(), now) > 1.0
    }
}

/// Weighs the expiring-credit tier by urgency, from x2 at seven days out to
/// x7 at expiry, so among several red-outlined accounts the one whose credit is
/// lost soonest draws the most new sessions. A credit only resets a window that
/// has been spent, so drawing on the account first gives its owner the chance
/// to spend the credit before it is lost; spending it stays the owner's
/// decision.
pub fn reset_credit_expiry_boost(quota: Option<&AccountQuota>, now: Timestamp) -> f64 {
    let Some(credits) = quota.and_then(|quota| quota.reset_credits.as_ref()) else {
        return 1.0;
    };
    let (Some(count), Some(expires_at)) = (credits.available_count, credits.expires_at) else {
        return 1.0;
    };
    if count <= 0 {
        return 1.0;
    }
    let left = Timestamp::from_unix(expires_at).sub(&now);
    let window = hours(RESET_CREDIT_EXPIRY_WINDOW_HOURS);
    if left <= TimeDelta::zero() || left > window {
        return 1.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio =
        left.num_nanoseconds().unwrap_or(0) as f64 / window.num_nanoseconds().unwrap_or(1) as f64;
    2.0 + 5.0 * (1.0 - ratio)
}

impl App {
    /// Records that the session behind balance_key (a whole lineage, or one
    /// routed conversation) is now being served by account_id. Called for
    /// every attempt, so a session counts against its account from the first
    /// request, before any success, and moves with it on failover.
    pub fn note_session_placement(&self, balance_key: &str, account_id: &str) {
        if balance_key.is_empty() || account_id.is_empty() {
            return;
        }
        let now = Timestamp::now();
        let mut inner = self.write();
        inner.session_placements.insert(
            balance_key.to_string(),
            SessionPlacement {
                account_id: account_id.to_string(),
                at: now,
            },
        );
        inner.session_placements.retain(|_, placement| {
            now.sub(&placement.at) <= minutes(PLACEMENT_ACTIVE_WINDOW_MINUTES)
        });
    }

    pub fn has_preferred_account(&self, model: &str, excluded: &Excluded) -> bool {
        self.read()
            .preferred_account(model, excluded, Timestamp::now())
            .is_some()
    }

    #[cfg(test)]
    pub fn select_account(
        &self,
        sticky_key: &str,
        model: &str,
        excluded: &Excluded,
    ) -> Result<Account, SelectError> {
        self.select_account_with_preference(sticky_key, sticky_key, model, "", excluded)
    }

    /// Takes the route mutably because strict lineage selection records the
    /// lineage account it observed on this attempt; mark_success needs exactly
    /// that value to tell a deliberate lineage move from a stale in-flight
    /// success.
    pub fn select_account_for_route(
        &self,
        route: &mut RoutingDecision,
        model: &str,
        excluded: &Excluded,
    ) -> Result<Account, SelectError> {
        let balance_key = if route.balance_key.is_empty() {
            route.sticky_key.clone()
        } else {
            route.balance_key.clone()
        };
        if route.lineage_affinity && !route.identity.lineage_root_id.is_empty() {
            return self.select_lineage_account(route, &balance_key, model, excluded);
        }
        let parent = route.lineage_account_id.clone();
        self.select_account_with_preference(
            &route.sticky_key,
            &balance_key,
            model,
            &parent,
            excluded,
        )
    }

    /// Strict lineage affinity: every thread of a lineage uses the lineage
    /// account, and the thread's own sticky route is never a fallback, because
    /// falling back to it is exactly how a main agent and its subagents ended
    /// up split. The operator explicitly accepts the cost: moving the lineage
    /// re-prefills the main agent on the new account.
    ///
    /// The lineage account is re-read on every attempt so a retry joins a move
    /// a sibling already made instead of hashing to its own replacement. When
    /// it is unusable for this request, the replacement is chosen by the
    /// lineage-shared balance key among accounts usable for every model the
    /// lineage has used, so all members that need to move converge on one
    /// account; that success becomes the new lineage binding and the rest
    /// follow on their next request. Eligibility, duplicate-identity, cooldown,
    /// quota and Pro-preservation gates still apply: strict binding must never
    /// turn into sending traffic to an unusable account.
    fn select_lineage_account(
        &self,
        route: &mut RoutingDecision,
        balance_key: &str,
        model: &str,
        excluded: &Excluded,
    ) -> Result<Account, SelectError> {
        let inner = self.write();
        let now = Timestamp::now();
        let root = route.identity.lineage_root_id.clone();
        let lineage_account_id = inner.lineage_account_id(&root, now).unwrap_or_default();
        route.lineage_account_id.clone_from(&lineage_account_id);
        let models = inner.lineage_models(&root, model, now);
        if !lineage_account_id.is_empty()
            && let Some(selected) = inner.lineage_preference(
                &lineage_account_id,
                balance_key,
                model,
                Some(&models),
                excluded,
                true,
                now,
            )?
        {
            return Ok(selected);
        }
        if let Some(selected) =
            inner.preferred_lineage_account(balance_key, model, &models, excluded, now)
        {
            return Ok(selected);
        }
        inner
            .overloaded_fallback_account(model, excluded, now)
            .ok_or(SelectError::NoEligibleAccount)
    }

    /// Serves routes without strict lineage affinity: non-Codex routes, and
    /// Codex threads while the Console switch is off.
    pub fn select_account_with_preference(
        &self,
        sticky_key: &str,
        balance_key: &str,
        model: &str,
        parent_account_id: &str,
        excluded: &Excluded,
    ) -> Result<Account, SelectError> {
        let mut inner = self.write();
        let now = Timestamp::now();
        let mut sticky_changed = false;
        if let Some(existing) = inner.state.sticky_sessions().get(sticky_key).cloned()
            && !is_excluded(excluded, &existing.account_id)
        {
            let item = inner.account(&existing.account_id).cloned();
            if inner.sticky_session_expired(&existing, now) {
                inner.state.sticky_sessions_mut().remove(sticky_key);
                sticky_changed = true;
            } else if let Some(item) = item.as_ref().filter(|item| inner.usable(item, model, now)) {
                if inner.bound_session_may_move(item, now)
                    && let Some(replacement) =
                        inner.preferred_account_for_sticky(balance_key, model, excluded, now)
                    && inner.bound_session_moves_to(item, &replacement, now)
                {
                    return Ok(replacement);
                }
                return Ok(item.clone());
            } else if let Some(item) = &item {
                if inner.account_auth_verification_pending(item) {
                    // Do not turn a short credential repair into a cold
                    // failover for a live session. Preserve the binding and ask
                    // the client to retry; unbound sessions may still use
                    // another healthy account.
                    return Err(SelectError::AuthRepairPending);
                }
            } else {
                inner.state.sticky_sessions_mut().remove(sticky_key);
                sticky_changed = true;
            }
        }
        if sticky_changed {
            let _ = inner.save();
        }
        // With lineage affinity switched off, keep the legacy contract: the
        // parent account is only a soft preference for a child that has no
        // usable sticky route of its own, so children may stay on other
        // accounts afterwards.
        if !parent_account_id.is_empty()
            && let Ok(Some(selected)) = inner.lineage_preference(
                parent_account_id,
                balance_key,
                model,
                None,
                excluded,
                false,
                now,
            )
        {
            return Ok(selected);
        }
        if let Some(selected) =
            inner.preferred_account_for_sticky(balance_key, model, excluded, now)
        {
            return Ok(selected);
        }
        // Every candidate is blocked. A transient upstream-overload cooldown
        // is a probabilistic hint, not proof the account will refuse again, so
        // failing the request outright wastes a real chance of success. Retry
        // the account whose overload cooldown expires soonest instead. This
        // must stay a last resort reached only after normal selection found
        // nothing, and it must never bypass quota, auth, duplicate-identity,
        // pool membership, or a rate-limit cooldown, where retrying early is
        // harmful rather than merely unlucky.
        inner
            .overloaded_fallback_account(model, excluded, now)
            .ok_or(SelectError::NoEligibleAccount)
    }
}
