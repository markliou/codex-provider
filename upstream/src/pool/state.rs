//! Serde models for the Go pool's persistent `config.json` and `runtime.json`.
//!
//! Every known field keeps Go's JSON name and omission rule.  Each object also
//! has a flattened `extra` map: upgrades must not discard fields written by a
//! newer pool before this Rust port knows their meaning.  The fixture tests
//! intentionally exercise both top-level and nested unknown fields.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_i32(value: &i32) -> bool {
    *value == 0
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

/// RFC 3339 timestamp as emitted by Go's `time.Time.MarshalJSON`.
///
/// Keeping the source text preserves nanosecond precision and the original
/// offset during a read/write migration.  Values are validated on read so a
/// malformed state file fails closed just as Go's `time.Time` decoder does.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Timestamp(pub String);

impl Timestamp {
    pub fn new(value: impl Into<String>) -> Result<Self, chrono::ParseError> {
        let value = value.into();
        chrono::DateTime::parse_from_rfc3339(&value)?;
        Ok(Self(value))
    }
}

impl Default for Timestamp {
    fn default() -> Self {
        Self("0001-01-01T00:00:00Z".to_owned())
    }
}

impl Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        chrono::DateTime::parse_from_rfc3339(&value)
            .map_err(serde::de::Error::custom)
            .map(|_| Self(value))
    }
}

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
    #[serde(default)]
    pub created_at: Timestamp,
    #[serde(default)]
    pub updated_at: Timestamp,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
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
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub plan_rank: i32,
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
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_quota: Option<i32>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub quota_protection_enabled: bool,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub quota_protection_threshold: i32,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    pub consecutive_failure: i32,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default)]
    pub percentage: i32,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    pub remaining_percent: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaResetCredits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub precommit_blocks: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub precommit_bytes: i32,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
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
    pub extra: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(source: &str) -> Value {
        serde_json::from_str(source).expect("Go state fixture must be valid JSON")
    }

    #[test]
    fn config_round_trip_preserves_known_and_unknown_fields() {
        let source = fixture(include_str!("testdata/state-vectors-config.json"));
        let config: Config = serde_json::from_value(source.clone()).expect("config should decode");
        assert_eq!(config.default_model, "gpt-test");
        assert_eq!(config.extra["futureConfigField"]["kept"], true);
        let encoded = serde_json::to_value(config).expect("config should encode");
        assert_eq!(encoded, source);
    }

    #[test]
    fn runtime_round_trip_preserves_known_and_unknown_fields() {
        let source = fixture(include_str!("testdata/state-vectors-runtime.json"));
        let runtime: RuntimeState =
            serde_json::from_value(source.clone()).expect("runtime should decode");
        assert_eq!(runtime.extra["futureRuntimeField"], "preserve-me");
        assert_eq!(
            runtime.sticky_sessions.as_ref().expect("sticky sessions")["gpt-test:thread:one"].extra
                ["futureStickyField"],
            serde_json::json!(["kept", 7])
        );
        let encoded = serde_json::to_value(runtime).expect("runtime should encode");
        assert_eq!(encoded, source);
    }

    #[test]
    fn round_trip_writes_artifacts_when_requested() {
        let Ok(directory) = std::env::var("POOL_STATE_ROUNDTRIP_DIR") else {
            return;
        };
        let config = fixture(include_str!("testdata/state-vectors-config.json"));
        let runtime = fixture(include_str!("testdata/state-vectors-runtime.json"));
        let config_model: Config = serde_json::from_value(config).expect("config should decode");
        let runtime_model: RuntimeState =
            serde_json::from_value(runtime).expect("runtime should decode");
        let config_output = serde_json::to_vec_pretty(
            &serde_json::to_value(config_model).expect("config should encode"),
        )
        .expect("config output should encode");
        let runtime_output = serde_json::to_vec_pretty(
            &serde_json::to_value(runtime_model).expect("runtime should encode"),
        )
        .expect("runtime output should encode");
        std::fs::write(
            std::path::Path::new(&directory).join("rust-roundtrip-config.json"),
            [config_output.as_slice(), b"\n"].concat(),
        )
        .expect("config artifact should write");
        std::fs::write(
            std::path::Path::new(&directory).join("rust-roundtrip-runtime.json"),
            [runtime_output.as_slice(), b"\n"].concat(),
        )
        .expect("runtime artifact should write");
    }

    #[test]
    fn zero_and_omitted_fields_match_go_json_rules() {
        let config = serde_json::to_value(Config::default()).expect("default config encodes");
        assert_eq!(
            config,
            serde_json::json!({
                "accounts": null,
                "createdAt": "0001-01-01T00:00:00Z",
                "defaultModel": "",
                "modelAliases": null,
                "updatedAt": "0001-01-01T00:00:00Z"
            })
        );

        let account = serde_json::to_value(Account::default()).expect("default account encodes");
        assert_eq!(account["authType"], "");
        assert_eq!(account["createdAt"], "0001-01-01T00:00:00Z");
        assert_eq!(account["lastLoginAt"], "0001-01-01T00:00:00Z");
        assert!(
            !account
                .as_object()
                .expect("account object")
                .contains_key("email")
        );

        let runtime = serde_json::to_value(RuntimeState {
            sticky_sessions: None,
            cooldowns: None,
            health: None,
            response_bindings: Some(BTreeMap::new()),
            routing_cache_events: Some(Vec::new()),
            ..RuntimeState::default()
        })
        .expect("default runtime encodes");
        assert_eq!(runtime["stickySessions"], serde_json::Value::Null);
        assert_eq!(runtime["cooldowns"], serde_json::Value::Null);
        assert_eq!(runtime["health"], serde_json::Value::Null);
        assert!(
            !runtime
                .as_object()
                .expect("runtime object")
                .contains_key("responseBindings")
        );
        assert_eq!(runtime["updatedAt"], "0001-01-01T00:00:00Z");
    }
}
