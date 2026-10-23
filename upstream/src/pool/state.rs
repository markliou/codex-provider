//! Serde models for the pool's persistent `config.json` and `runtime.json`.
//!
//! Every known field keeps Go's JSON name and omission rule.  Each object also
//! has a flattened `extra` map: upgrades must not discard fields written by a
//! newer pool before this Rust port knows their meaning.  The fixture tests
//! intentionally exercise both top-level and nested unknown fields.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

fn option_map_empty<K, V>(value: &Option<BTreeMap<K, V>>) -> bool {
    value.as_ref().is_none_or(BTreeMap::is_empty)
}

fn option_vec_empty<T>(value: &Option<Vec<T>>) -> bool {
    value.as_ref().is_none_or(Vec::is_empty)
}

/// Fields this port does not know, kept so a newer pool's state survives a
/// read and a write here. In tests every key it receives is also recorded:
/// the Go fixtures set every known field, so a key landing here that is not
/// one of the fixtures' deliberate `future*` keys is a field the Rust types
/// are missing, which serde's flatten would otherwise carry along silently.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Unknown(pub BTreeMap<String, Value>);

impl std::ops::Deref for Unknown {
    type Target = BTreeMap<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Unknown {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fields = BTreeMap::<String, Value>::deserialize(deserializer)?;
        #[cfg(test)]
        tests::record_unknown(fields.keys());
        Ok(Self(fields))
    }
}

pub use super::time::Timestamp;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub model_aliases: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub accounts: Option<Vec<Account>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_pro_quota: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage_affinity: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_prime: Option<bool>,
    #[serde(default)]
    pub created_at: Timestamp,
    #[serde(default)]
    pub updated_at: Timestamp,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner_note: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub organization_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub organization_name_override: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan_limit: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub plan_rank: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw_plan_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan_family: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub seat_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub seat_type_raw: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub seat_type_inferred: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota_policy: Vec<String>,
    #[serde(default)]
    pub auth_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub codex_home: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub in_pool: bool,
    #[serde(default)]
    pub priority: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_quota: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub quota_protection_enabled: bool,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub quota_protection_threshold: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upstream_base_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub upstream_api_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wire_api: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pending_pool_activation: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub pending_auth_verification: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pending_auth_expected_account_id: String,
    #[serde(default)]
    pub created_at: Timestamp,
    #[serde(default)]
    pub updated_at: Timestamp,
    // Go's time.Time is a struct, so `omitempty` does not suppress a zero time.
    #[serde(default)]
    pub last_login_at: Timestamp,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cooldown {
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub next_retry_at: Timestamp,
    #[serde(default)]
    pub reason: String,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountHealth {
    #[serde(default)]
    pub last_success_at: Timestamp,
    #[serde(default)]
    pub last_failure_at: Timestamp,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_failure_reason: String,
    #[serde(default)]
    pub consecutive_failure: i64,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StickySession {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub created_at: Timestamp,
    #[serde(default)]
    pub last_success_at: Timestamp,
    #[serde(default)]
    pub expires_at: Timestamp,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failover_from: String,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseBinding {
    #[serde(default)]
    pub response_id: String,
    #[serde(default)]
    pub sticky_key: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub created_at: Timestamp,
    #[serde(default)]
    pub expires_at: Timestamp,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBinding {
    #[serde(default)]
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent_thread_id: String,
    #[serde(default)]
    pub lineage_root_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subagent_kind: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub sticky_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_cache_key: String,
    #[serde(default)]
    pub created_at: Timestamp,
    #[serde(default)]
    pub last_success_at: Timestamp,
    #[serde(default)]
    pub expires_at: Timestamp,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineageBinding {
    #[serde(default)]
    pub lineage_root_id: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failover_from: String,
    #[serde(default)]
    pub created_at: Timestamp,
    #[serde(default)]
    pub moved_at: Timestamp,
    #[serde(default)]
    pub last_success_at: Timestamp,
    #[serde(default)]
    pub expires_at: Timestamp,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default)]
    pub percentage: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub observed: bool,
    #[serde(default)]
    pub present: bool,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaCredits {
    #[serde(default)]
    pub has_credits: bool,
    #[serde(default)]
    pub unlimited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSpendControl {
    #[serde(default)]
    pub reached: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub limit: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub used: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub remaining: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaLimit {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub limit_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub limit_name: String,
    #[serde(default)]
    pub primary: QuotaWindow,
    #[serde(default)]
    pub secondary: QuotaWindow,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<QuotaWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reached: Option<bool>,
    #[serde(default)]
    pub exhausted: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exhaustion_reason: String,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaResetCredits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_count: Option<i64>,
    /// The earliest expiry among currently available reset credits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Process-local fetch throttling (Go `json:"-"`): it must not add
    /// diagnostic noise to the dashboard or the persisted runtime JSON.
    #[serde(skip)]
    pub details_observed_at: super::time::Timestamp,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuota {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub limit_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub limit_name: String,
    #[serde(default)]
    pub primary: QuotaWindow,
    #[serde(default)]
    pub secondary: QuotaWindow,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<QuotaWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<QuotaCredits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub individual_limit: Option<QuotaSpendControl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reached: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rate_limit_reached_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_limits: Vec<QuotaLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<QuotaResetCredits>,
    #[serde(default)]
    pub exhausted: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exhaustion_reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provenance: String,
    #[serde(default)]
    pub observed_at: Timestamp,
    #[serde(default)]
    pub hourly: QuotaWindow,
    #[serde(default)]
    pub weekly: QuotaWindow,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaErrorInfo {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub timestamp: Timestamp,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSnapshot {
    #[serde(default)]
    pub account_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub organization_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw_plan_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan_family: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan_limit: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub seat_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub seat_type_raw: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub seat_type_inferred: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota_policy: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<AccountQuota>,
    #[serde(default)]
    pub observed_at: Timestamp,
    #[serde(default)]
    pub last_successful_refresh_at: Timestamp,
    #[serde(default)]
    pub usage_updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub freshness: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provenance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_error: Option<QuotaErrorInfo>,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCacheStat {
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_kind: String,
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub usage_observed_request_count: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    #[serde(default)]
    pub cache_write_observed_request_count: u64,
    #[serde(default)]
    pub cache_hit_request_count: u64,
    #[serde(default)]
    pub cache_eligible_request_count: u64,
    #[serde(default)]
    pub cold_request_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub parent_affinity_hit_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub parent_affinity_fallback_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub lineage_failover_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub routing_failover_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub unrecognized_failure_count: u64,
    #[serde(default)]
    pub updated_at: Timestamp,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingCacheEvent {
    #[serde(default)]
    pub timestamp: Timestamp,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_id_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub response_id_hash: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub agent_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thread_id_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lineage_root_id_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sticky_key_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_cache_key_hash: String,
    #[serde(default)]
    pub routing_outcome: String,
    #[serde(default)]
    pub routing_source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_event: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_failure_class: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_error_code: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub precommit_committed: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub precommit_close_reason: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub precommit_blocks: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub precommit_bytes: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub precommit_hold_ms: i64,
    #[serde(default)]
    pub parent_affinity: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failover_from_account_id: String,
    #[serde(default)]
    pub usage_observed: bool,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default)]
    pub uncached_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_reuse_balance: Option<i64>,
    #[serde(default)]
    pub cache_hit: bool,
    #[serde(default)]
    pub cold_cache_eligible: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_originator: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_user_agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_request_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_turn_state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_entry: String,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThroughputBucket {
    #[serde(default)]
    pub bucket_at: Timestamp,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_id: String,
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub success_count: u64,
    #[serde(default)]
    pub failure_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cancelled_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub streaming_request_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub usage_observed_request_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub output_observed_request_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cached_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub total_duration_millis: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub duration_histogram: Vec<u64>,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientAgent {
    #[serde(default)]
    pub user_agent: String,
    #[serde(default)]
    pub originator: String,
    #[serde(default)]
    pub seen_at: Timestamp,
    #[serde(flatten)]
    pub extra: Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeState {
    #[serde(default)]
    pub sticky_sessions: Option<BTreeMap<String, StickySession>>,
    #[serde(default, skip_serializing_if = "option_map_empty")]
    pub response_bindings: Option<BTreeMap<String, ResponseBinding>>,
    #[serde(default, skip_serializing_if = "option_map_empty")]
    pub thread_bindings: Option<BTreeMap<String, ThreadBinding>>,
    #[serde(default, skip_serializing_if = "option_map_empty")]
    pub lineage_bindings: Option<BTreeMap<String, LineageBinding>>,
    #[serde(default)]
    pub cooldowns: Option<BTreeMap<String, Vec<Cooldown>>>,
    #[serde(default)]
    pub health: Option<BTreeMap<String, AccountHealth>>,
    #[serde(default, skip_serializing_if = "option_map_empty")]
    pub quotas: Option<BTreeMap<String, QuotaSnapshot>>,
    #[serde(default, skip_serializing_if = "option_map_empty")]
    pub prompt_cache: Option<BTreeMap<String, PromptCacheStat>>,
    #[serde(default, skip_serializing_if = "option_map_empty")]
    pub prompt_cache_baseline: Option<BTreeMap<String, PromptCacheStat>>,
    // Go's time.Time is a struct, so its `omitempty` tag still serializes the
    // zero timestamp. Keep this non-optional for byte-compatible upgrades.
    #[serde(default)]
    pub prompt_cache_reset_at: Timestamp,
    #[serde(default, skip_serializing_if = "option_map_empty")]
    pub prompt_cache_reset_at_by_account: Option<BTreeMap<String, Timestamp>>,
    #[serde(default, skip_serializing_if = "option_map_empty")]
    pub quota_window_primes: Option<BTreeMap<String, Timestamp>>,
    #[serde(default, skip_serializing_if = "option_vec_empty")]
    pub routing_cache_events: Option<Vec<RoutingCacheEvent>>,
    #[serde(default, skip_serializing_if = "option_map_empty")]
    pub client_agents: Option<BTreeMap<String, ClientAgent>>,
    #[serde(default, skip_serializing_if = "option_vec_empty")]
    pub throughput_buckets: Option<Vec<ThroughputBucket>>,
    #[serde(default)]
    pub request_count: u64,
    #[serde(default)]
    pub success_count: u64,
    #[serde(default)]
    pub failure_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub upstream_response_failed_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub stream_incomplete_count: u64,
    #[serde(default)]
    pub updated_at: Timestamp,
    #[serde(flatten)]
    pub extra: Unknown,
}

/// Go's maps and slices are nil until set, and the state files carry that as
/// `null`; the pool's code reads a nil map as empty and Go's load() makes the
/// main ones non-nil. These accessors give the same reading.
macro_rules! map_access {
    ($owner:ty, $field:ident, $field_mut:ident, $value:ty) => {
        impl $owner {
            pub fn $field(&self) -> &BTreeMap<String, $value> {
                static EMPTY: BTreeMap<String, $value> = BTreeMap::new();
                self.$field.as_ref().unwrap_or(&EMPTY)
            }

            pub fn $field_mut(&mut self) -> &mut BTreeMap<String, $value> {
                self.$field.get_or_insert_with(BTreeMap::new)
            }
        }
    };
}

map_access!(
    RuntimeState,
    sticky_sessions,
    sticky_sessions_mut,
    StickySession
);
map_access!(
    RuntimeState,
    response_bindings,
    response_bindings_mut,
    ResponseBinding
);
map_access!(
    RuntimeState,
    thread_bindings,
    thread_bindings_mut,
    ThreadBinding
);
map_access!(
    RuntimeState,
    lineage_bindings,
    lineage_bindings_mut,
    LineageBinding
);
map_access!(RuntimeState, cooldowns, cooldowns_mut, Vec<Cooldown>);
map_access!(RuntimeState, health, health_mut, AccountHealth);
map_access!(RuntimeState, quotas, quotas_mut, QuotaSnapshot);
map_access!(
    RuntimeState,
    prompt_cache,
    prompt_cache_mut,
    PromptCacheStat
);
map_access!(
    RuntimeState,
    prompt_cache_baseline,
    prompt_cache_baseline_mut,
    PromptCacheStat
);
map_access!(
    RuntimeState,
    prompt_cache_reset_at_by_account,
    prompt_cache_reset_at_by_account_mut,
    Timestamp
);
map_access!(
    RuntimeState,
    quota_window_primes,
    quota_window_primes_mut,
    Timestamp
);
map_access!(RuntimeState, client_agents, client_agents_mut, ClientAgent);
map_access!(Config, model_aliases, model_aliases_mut, String);

impl RuntimeState {
    pub fn routing_cache_events(&self) -> &[RoutingCacheEvent] {
        self.routing_cache_events.as_deref().unwrap_or_default()
    }

    pub fn routing_cache_events_mut(&mut self) -> &mut Vec<RoutingCacheEvent> {
        self.routing_cache_events.get_or_insert_with(Vec::new)
    }
}

impl Config {
    pub fn accounts(&self) -> &[Account] {
        self.accounts.as_deref().unwrap_or_default()
    }

    pub fn accounts_mut(&mut self) -> &mut Vec<Account> {
        self.accounts.get_or_insert_with(Vec::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use std::cell::RefCell;

    thread_local! {
        static UNKNOWN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record_unknown<'a>(keys: impl Iterator<Item = &'a String>) {
        UNKNOWN.with(|seen| seen.borrow_mut().extend(keys.cloned()));
    }

    fn take_unknown() -> Vec<String> {
        UNKNOWN.with(|seen| std::mem::take(&mut *seen.borrow_mut()))
    }

    // Written from the Go pool's types before it was removed; the state files
    // on disk still have that shape.
    fn documents() -> Value {
        serde_json::from_str(include_str!("../../testdata/state-vectors.json"))
            .expect("Go state vectors must be valid JSON")
    }

    // JSON equality with numbers compared by value: Go writes a whole float
    // as 5 and serde_json as 5.0, which both read back as the same number.
    fn same_json(left: &Value, right: &Value) -> bool {
        match (left, right) {
            (Value::Number(a), Value::Number(b)) => a == b || a.as_f64() == b.as_f64(),
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_json(x, y))
            }
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(k, v)| b.get(k).is_some_and(|w| same_json(v, w)))
            }
            _ => left == right,
        }
    }

    /// Reads a Go document, checks that every key it does not know is a
    /// deliberate future field, and writes it back unchanged.
    fn round_trip<T: DeserializeOwned + Serialize>(name: &str, document: &Value) -> Vec<String> {
        take_unknown();
        let decoded: T = serde_json::from_value(document.clone())
            .unwrap_or_else(|error| panic!("{name}: Go document does not decode: {error}"));
        let unknown = take_unknown();
        let missing: Vec<_> = unknown
            .iter()
            .filter(|key| !key.starts_with("future"))
            .collect();
        assert!(
            missing.is_empty(),
            "{name}: fields the Rust types lack: {missing:?}"
        );
        let encoded = serde_json::to_value(decoded).expect("encode");
        assert!(
            same_json(&encoded, document),
            "{name}: written back differently\nGo:   {document}\nRust: {encoded}"
        );
        unknown
    }

    #[test]
    fn go_state_documents_round_trip() {
        let documents = documents();
        for variant in ["full", "zero"] {
            let config = round_trip::<Config>(variant, &documents["config"][variant]);
            let runtime = round_trip::<RuntimeState>(variant, &documents["runtime"][variant]);
            if variant == "full" {
                // The future fields sit at the top, in an account, a sticky
                // session and a quota window; all of them must be carried.
                assert_eq!(config.len(), 3, "{config:?}");
                assert_eq!(runtime.len(), 3, "{runtime:?}");
            } else {
                assert!(config.is_empty() && runtime.is_empty());
            }
        }
    }

    #[test]
    fn timestamps_keep_their_text() {
        let documents = documents();
        let runtime: RuntimeState =
            serde_json::from_value(documents["runtime"]["full"].clone()).expect("decode");
        // Nanoseconds and the +08:00 offset survive as written, not as UTC.
        let text = runtime.updated_at.to_string();
        assert!(text.ends_with("+08:00"), "{text}");
        assert!(text.contains('.'), "{text}");
        assert!(serde_json::from_value::<Timestamp>(Value::from("yesterday")).is_err());
    }
}
