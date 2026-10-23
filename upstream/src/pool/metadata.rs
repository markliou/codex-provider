//! Account metadata read from upstream: Codex's account check
//! (`/wham/accounts/check`) and the claims in an account's `auth.json`.
//!
//! A port of subscriptionMetadataFromValue, codexAuthInfoFromFile and the JWT
//! helpers in the Go pool's main.go. These answers are loose JSON whose shape
//! upstream has changed before, so they are read as values, field by field,
//! the way Go reads them as `map[string]any`.

use super::plan::claim_string;
use super::plan::clean_organization_name;
use super::plan::clean_plan_limit;
use super::plan::clean_raw_plan_type;
use super::plan::organization_name_from_map;
use super::plan::plan_family_from_raw;
use super::plan::plan_limit_from_map;
use super::time::Timestamp;
use base64::Engine;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionMetadata {
    pub account_id: String,
    pub organization_name: String,
    pub raw_plan_type: String,
    pub plan_family: String,
    pub plan_type: String,
    pub plan_limit: String,
}

const ACCOUNT_ID_KEYS: &[&str] = &[
    "account_id",
    "accountId",
    "id",
    "chatgpt_account_id",
    "workspace_id",
    "workspaceId",
];

/// Where a subscription record names its plan, in order of precedence. An
/// entitlement record is read with the subscription plan first instead.
const PLAN_KEYS: &[&str] = &[
    "plan_type",
    "planType",
    "subscription_plan",
    "subscriptionPlan",
    "plan_name",
    "planName",
    "sku",
    "sku_name",
    "product",
    "product_name",
];

pub fn subscription_metadata_from_map(values: &Map<String, Value>) -> SubscriptionMetadata {
    let raw_plan = clean_raw_plan_type(&first_metadata_string(Some(values), PLAN_KEYS));
    let family = plan_family_from_raw(&raw_plan);
    let mut metadata = SubscriptionMetadata {
        account_id: first_metadata_string(Some(values), ACCOUNT_ID_KEYS),
        organization_name: clean_organization_name(&organization_name_from_map(Some(values))),
        raw_plan_type: raw_plan,
        plan_family: family.to_string(),
        plan_type: family.to_string(),
        plan_limit: plan_limit_from_map(Some(values)),
    };
    if metadata.plan_type == "unknown" {
        metadata.plan_type = String::new();
        metadata.plan_family = String::new();
    }
    metadata
}

struct Record<'a> {
    key: String,
    node: &'a Map<String, Value>,
}

/// The metadata of the workspace this credential acts as, from an
/// accounts/check answer; None when the answer cannot say which.
pub fn subscription_metadata_from_value(
    value: &Value,
    preferred_account_id: &str,
) -> Option<SubscriptionMetadata> {
    let records = collect_subscription_account_records(value);
    if records.is_empty() {
        return value.as_object().map(subscription_metadata_from_map);
    }
    let preferred = preferred_account_id.trim();
    // accounts/check lists every workspace the login may act as. Picking an
    // arbitrary record labels this credential with a foreign workspace's
    // entitlement: a login that holds both a personal Pro subscription and a
    // Team workspace would show its Team slot as "Pro 20x", lose the
    // organization suffix, and then persist the foreign account ID as the
    // slot's upstream identity, which routing uses for duplicate detection.
    if !preferred.is_empty()
        && let Some(record) = records.iter().find(|record| {
            record.key.trim() == preferred
                || subscription_metadata_from_record(record.node).account_id == preferred
        })
    {
        return Some(subscription_metadata_from_selected_record(record));
    }
    // A sole accessible workspace stays unambiguous even when it is not the
    // one the credential last reported: the login can no longer act as the
    // previous workspace, and the caller deliberately treats that as an
    // upstream identity change that resets cache and sticky history. Several
    // workspaces with no match carry no evidence tying any of them to this
    // credential, so report nothing instead of guessing. account_ordering only
    // names the login default, which is not necessarily the workspace this
    // slot acts as.
    if records.len() != 1 {
        return None;
    }
    Some(subscription_metadata_from_selected_record(&records[0]))
}

fn subscription_metadata_from_selected_record(record: &Record<'_>) -> SubscriptionMetadata {
    let mut metadata = subscription_metadata_from_record(record.node);
    if metadata.account_id.is_empty() {
        metadata.account_id = record.key.trim().to_string();
    }
    metadata
}

fn collect_subscription_account_records(value: &Value) -> Vec<Record<'_>> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_object)
            .map(|node| Record {
                key: String::new(),
                node,
            })
            .collect(),
        Value::Object(object) => {
            for key in ["accounts", "account_items", "items", "data"] {
                let records = collect_records_from_field(object.get(key));
                if !records.is_empty() {
                    return records;
                }
            }
            if object.get("account").is_some_and(Value::is_object)
                || !first_metadata_string(Some(object), ACCOUNT_ID_KEYS).is_empty()
            {
                return vec![Record {
                    key: String::new(),
                    node: object,
                }];
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn collect_records_from_field(value: Option<&Value>) -> Vec<Record<'_>> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_object)
            .map(|node| Record {
                key: String::new(),
                node,
            })
            .collect(),
        Some(Value::Object(object)) => object
            .iter()
            .filter_map(|(key, item)| {
                item.as_object().map(|node| Record {
                    key: key.clone(),
                    node,
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn subscription_metadata_from_record(record: &Map<String, Value>) -> SubscriptionMetadata {
    let account_record = record
        .get("account")
        .and_then(Value::as_object)
        .unwrap_or(record);
    let entitlement = record.get("entitlement").and_then(Value::as_object);
    let mut raw_plan = String::new();
    if entitlement.is_some() {
        raw_plan = first_metadata_string(
            entitlement,
            &[
                "subscription_plan",
                "subscriptionPlan",
                "plan_type",
                "planType",
                "plan_name",
                "planName",
                "sku",
                "sku_name",
                "product",
                "product_name",
            ],
        );
    }
    if raw_plan.is_empty() {
        raw_plan = first_metadata_string(Some(account_record), PLAN_KEYS);
    }
    let raw_plan = clean_raw_plan_type(&raw_plan);
    let family = plan_family_from_raw(&raw_plan);
    let mut organization_name =
        clean_organization_name(&organization_name_from_map(Some(account_record)));
    if organization_name.is_empty() {
        organization_name = clean_organization_name(&organization_name_from_map(Some(record)));
    }
    let mut metadata = SubscriptionMetadata {
        account_id: first_metadata_string(Some(account_record), ACCOUNT_ID_KEYS),
        organization_name,
        raw_plan_type: raw_plan,
        plan_family: family.to_string(),
        plan_type: family.to_string(),
        plan_limit: String::new(),
    };
    if entitlement.is_some() {
        metadata.plan_limit = plan_limit_from_map(entitlement);
    }
    if metadata.plan_limit.is_empty() {
        metadata.plan_limit = plan_limit_from_map(Some(account_record));
    }
    if metadata.plan_type == "unknown" {
        metadata.plan_type = String::new();
        metadata.plan_family = String::new();
    }
    metadata
}

pub fn first_metadata_string(values: Option<&Map<String, Value>>, keys: &[&str]) -> String {
    let Some(values) = values else {
        return String::new();
    };
    for key in keys {
        let value = claim_string(values, key);
        if !value.is_empty() {
            return value.to_string();
        }
    }
    for key in [
        "plan",
        "subscription",
        "subscriptions",
        "entitlement",
        "billing",
        "account",
        "accounts",
        "items",
        "data",
        "quota",
        "rate_limit",
        "limits",
        "codex",
    ] {
        let value = first_metadata_string(values.get(key).and_then(Value::as_object), keys);
        if !value.is_empty() {
            return value;
        }
        if let Some(items) = values.get(key).and_then(Value::as_array) {
            for item in items {
                let value = first_metadata_string(item.as_object(), keys);
                if !value.is_empty() {
                    return value;
                }
            }
        }
    }
    String::new()
}

// ---- auth.json ----

/// An account's `auth.json`, as Codex writes it. Go's codexAuthFile knows only
/// these fields and dropped every other one Codex writes when it saved a
/// refresh; they are carried here instead (`extra`), so a refresh leaves the
/// rest of Codex's file as Codex wrote it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CodexAuthFile {
    #[serde(default)]
    pub auth_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<Timestamp>,
    #[serde(default)]
    pub tokens: Option<CodexAuthTokens>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CodexAuthTokens {
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// What the pool reads from an account's credential.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexAuthInfo {
    pub access_token: String,
    pub account_id: String,
    pub email: String,
    pub organization_name: String,
    pub plan_type: String,
    pub plan_limit: String,
    pub fedramp: bool,
}

pub fn codex_auth_info_from_file(auth: &CodexAuthFile) -> CodexAuthInfo {
    let tokens = auth.tokens.clone().unwrap_or_default();
    let mut info = CodexAuthInfo {
        access_token: tokens.access_token.trim().to_string(),
        account_id: tokens
            .account_id
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string(),
        ..CodexAuthInfo::default()
    };
    let Some(claims) = jwt_payload(&tokens.id_token) else {
        return info;
    };
    info.email = claim_string(&claims, "email").to_string();
    info.organization_name = organization_name_from_map(Some(&claims));
    info.plan_limit = plan_limit_from_map(Some(&claims));
    if let Some(profile) = claims
        .get("https://api.openai.com/profile")
        .and_then(Value::as_object)
    {
        if info.email.is_empty() {
            info.email = claim_string(profile, "email").to_string();
        }
        if info.plan_limit.is_empty() {
            info.plan_limit = plan_limit_from_map(Some(profile));
        }
    }
    if let Some(auth_claims) = claims
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
    {
        if info.account_id.is_empty() {
            info.account_id = claim_string(auth_claims, "chatgpt_account_id").to_string();
        }
        let organization_name = organization_name_from_map(Some(auth_claims));
        if !organization_name.is_empty() {
            info.organization_name = organization_name;
        }
        info.plan_type = claim_string(auth_claims, "chatgpt_plan_type").to_string();
        if info.plan_limit.is_empty() {
            info.plan_limit = clean_plan_limit(&info.plan_type);
        }
        if info.plan_limit.is_empty() {
            info.plan_limit = plan_limit_from_map(Some(auth_claims));
        }
        if let Some(fedramp) = auth_claims
            .get("chatgpt_account_is_fedramp")
            .and_then(Value::as_bool)
        {
            info.fedramp = fedramp;
        }
    }
    info
}

/// The claims of a JWT, unverified: the pool reads its own credential's
/// metadata, it does not authenticate anyone with it.
pub fn jwt_payload(token: &str) -> Option<Map<String, Value>> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 3 || parts[1].is_empty() {
        return None;
    }
    let data = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    serde_json::from_slice(&data).ok()
}

/// Orders two credentials for the same account: a later iat (or, without one,
/// a later exp) is the token a more recent refresh produced.
pub fn jwt_issued_at(token: &str) -> Option<Timestamp> {
    let claims = jwt_payload(token)?;
    if let Some(iat) = claims.get("iat").and_then(Value::as_f64)
        && iat > 0.0
    {
        #[allow(clippy::cast_possible_truncation)]
        return Some(Timestamp::from_unix(iat as i64));
    }
    jwt_expiry(token)
}

pub fn jwt_expiry(token: &str) -> Option<Timestamp> {
    let claims = jwt_payload(token)?;
    #[allow(clippy::cast_possible_truncation)]
    claims
        .get("exp")
        .and_then(Value::as_f64)
        .map(|exp| Timestamp::from_unix(exp as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    pub(crate) fn fake_jwt(claims: &Value) -> String {
        let encode = |value: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
        };
        format!(
            "{}.{}.signature",
            encode(&json!({"alg": "none"})),
            encode(claims)
        )
    }

    // Go TestPlanMetadataKeepsRawFamilySeatAndMultiplierSeparate (the
    // accounts/check half).
    #[test]
    fn metadata_keeps_raw_plan_and_family_separate() {
        let record = json!({"plan_type": "team", "seat_type": "premium", "workspace_name": "Premium Workspace"});
        let metadata = subscription_metadata_from_map(record.as_object().unwrap_or(&Map::new()));
        assert_eq!(metadata.raw_plan_type, "team");
        assert_eq!(metadata.plan_family, "business");
        assert_eq!(metadata.plan_type, "business");
        assert_eq!(metadata.organization_name, "Premium Workspace");
    }

    #[test]
    fn picks_the_credentials_own_workspace() {
        let answer = json!({"accounts": {
            "acct-personal": {"account": {"plan_type": "pro"}, "entitlement": {"subscription_plan": "pro", "plan_limit": "20x"}},
            "acct-team": {"account": {"plan_type": "team", "name": "Team Space", "structure": "workspace"}}
        }});
        let team = subscription_metadata_from_value(&answer, "acct-team").unwrap_or_default();
        assert_eq!(team.account_id, "acct-team");
        assert_eq!(team.plan_family, "business");
        assert_eq!(team.organization_name, "Team Space");
        let personal =
            subscription_metadata_from_value(&answer, "acct-personal").unwrap_or_default();
        assert_eq!(personal.plan_family, "pro");
        assert_eq!(personal.plan_limit, "20x");
        // Several workspaces and no match: nothing, rather than a guess.
        assert_eq!(
            subscription_metadata_from_value(&answer, "acct-other"),
            None
        );
        // A sole workspace is unambiguous.
        let sole = json!({"accounts": [{"id": "acct-x", "plan_type": "plus"}]});
        assert_eq!(
            subscription_metadata_from_value(&sole, "acct-old").map(|m| m.account_id),
            Some("acct-x".to_string())
        );
    }

    fn team(account: Value) -> Value {
        json!({"account": account, "entitlement": {"subscription_plan": "chatgptteamplan"}})
    }

    fn pro() -> Value {
        json!({"account": {"account_id": "acct-pro", "plan_type": "pro"}, "entitlement": {"subscription_plan": "chatgptpro"}})
    }

    // Go TestSubscriptionMetadataFromAccountCheckUsesExplicitWorkspaceName.
    #[test]
    fn account_check_uses_explicit_workspace_name() {
        let answer = json!({
            "accounts": {
                "acct-team": team(json!({"account_id": "acct-team", "name": "Example Person", "workspace_name": "Example Workspace", "plan_type": "team"})),
                "acct-pro": pro(),
            },
            "account_ordering": ["acct-team"],
        });
        let metadata = subscription_metadata_from_value(&answer, "acct-team").unwrap_or_default();
        assert_eq!(
            (
                metadata.account_id.as_str(),
                metadata.organization_name.as_str(),
                metadata.raw_plan_type.as_str(),
                metadata.plan_type.as_str()
            ),
            (
                "acct-team",
                "Example Workspace",
                "chatgptteamplan",
                "business"
            ),
            "unexpected team metadata"
        );
        // A personal account name is not a workspace name.
        let answer = json!({
            "accounts": {"acct-team": team(json!({"account_id": "acct-team", "name": "Example Person", "plan_type": "team"}))},
            "account_ordering": ["acct-team"],
        });
        let metadata = subscription_metadata_from_value(&answer, "acct-team").unwrap_or_default();
        assert_eq!(
            (
                metadata.account_id.as_str(),
                metadata.organization_name.as_str(),
                metadata.plan_type.as_str()
            ),
            ("acct-team", "", "business"),
            "personal account name was used as team organization metadata"
        );
        let answer = json!({
            "accounts": {"acct-team": team(json!({"account_id": "acct-team", "name": "Example Workspace", "structure": "workspace", "plan_type": "team"}))},
            "account_ordering": ["acct-team"],
        });
        let metadata = subscription_metadata_from_value(&answer, "acct-team").unwrap_or_default();
        assert_eq!(
            (
                metadata.account_id.as_str(),
                metadata.organization_name.as_str(),
                metadata.plan_type.as_str()
            ),
            ("acct-team", "Example Workspace", "business"),
            "workspace account name was not used as team organization metadata"
        );
        let answer = json!({"accounts": {
            "acct-team": team(json!({"account_id": "acct-team", "name": "Example Workspace", "plan_type": "team"})),
            "acct-pro": pro(),
        }});
        let metadata = subscription_metadata_from_value(&answer, "acct-pro").unwrap_or_default();
        assert_eq!(
            (
                metadata.account_id.as_str(),
                metadata.plan_type.as_str(),
                metadata.plan_limit.as_str()
            ),
            ("acct-pro", "pro", ""),
            "unexpected Pro metadata"
        );
    }

    // Go TestSubscriptionMetadataRejectsUnmatchedWorkspace.
    #[test]
    fn rejects_unmatched_workspace() {
        let answer = json!({
            "accounts": {
                "acct-team": team(json!({"account_id": "acct-team", "name": "Example Workspace", "structure": "workspace", "plan_type": "team"})),
                "acct-pro": pro(),
            },
            "account_ordering": ["acct-pro", "acct-team"],
        });
        assert_eq!(
            subscription_metadata_from_value(&answer, "acct-missing"),
            None,
            "unmatched workspace reported foreign metadata"
        );
        // account_ordering names only the login default, not this slot.
        assert_eq!(
            subscription_metadata_from_value(&answer, ""),
            None,
            "ambiguous multi-workspace login reported metadata"
        );
        let metadata = subscription_metadata_from_value(&answer, "acct-team").unwrap_or_default();
        assert_eq!(
            (
                metadata.account_id.as_str(),
                metadata.plan_type.as_str(),
                metadata.organization_name.as_str()
            ),
            ("acct-team", "business", "Example Workspace"),
            "preferred team workspace resolved incorrectly"
        );
        // An unmatched preference against a sole accessible workspace still
        // resolves; the caller reports it as an upstream identity change.
        let moved = json!({
            "accounts": {"acct-moved": team(json!({"account_id": "acct-moved", "workspace_name": "Different Workspace", "plan_type": "team"}))},
            "account_ordering": ["acct-moved"],
        });
        let metadata = subscription_metadata_from_value(&moved, "acct-team").unwrap_or_default();
        assert_eq!(
            (metadata.account_id.as_str(), metadata.plan_type.as_str()),
            ("acct-moved", "business"),
            "sole remaining workspace"
        );
    }

    // Go TestSubscriptionMetadataResolvesSoleWorkspaceWithoutPreference.
    #[test]
    fn resolves_sole_workspace_without_preference() {
        let answer = json!({"accounts": {
            "acct-team": team(json!({"account_id": "acct-team", "name": "Example Workspace", "structure": "workspace", "plan_type": "team"})),
        }});
        let metadata = subscription_metadata_from_value(&answer, "").unwrap_or_default();
        assert_eq!(
            (
                metadata.account_id.as_str(),
                metadata.plan_type.as_str(),
                metadata.organization_name.as_str()
            ),
            ("acct-team", "business", "Example Workspace"),
            "sole workspace resolved incorrectly"
        );
    }

    #[test]
    fn reads_the_credentials_claims() {
        let id_token = fake_jwt(&json!({
            "email": "owner@example.test",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-from-claims",
                "chatgpt_plan_type": "pro",
                "organization_name": "Org",
                "chatgpt_account_is_fedramp": true
            },
            "iat": 1_800_000_000,
            "exp": 1_800_003_600
        }));
        let auth = CodexAuthFile {
            auth_mode: "chatgpt".into(),
            last_refresh: None,
            tokens: Some(CodexAuthTokens {
                id_token: id_token.clone(),
                access_token: " access ".into(),
                refresh_token: "refresh".into(),
                account_id: None,
                extra: Map::new(),
            }),
            extra: Map::new(),
        };
        let info = codex_auth_info_from_file(&auth);
        assert_eq!(info.access_token, "access");
        assert_eq!(info.account_id, "acct-from-claims");
        assert_eq!(info.email, "owner@example.test");
        assert_eq!(info.organization_name, "Org");
        assert_eq!(info.plan_type, "pro");
        assert!(info.fedramp);
        assert_eq!(
            jwt_issued_at(&id_token).map(|t| t.unix()),
            Some(1_800_000_000)
        );
        assert_eq!(jwt_expiry(&id_token).map(|t| t.unix()), Some(1_800_003_600));
        assert_eq!(jwt_payload("not.a"), None);
    }

    // What Codex writes beside the fields the pool reads survives a rewrite.
    #[test]
    fn auth_file_keeps_what_codex_wrote() {
        let written = json!({
            "OPENAI_API_KEY": null,
            "auth_mode": "chatgpt",
            "tokens": {"id_token": "<id>", "access_token": "<access>", "refresh_token": "<refresh>", "account_id": "acct", "future": 1},
            "last_refresh": "2026-10-22T06:00:20.123456Z"
        });
        let auth: CodexAuthFile = serde_json::from_value(written.clone()).unwrap_or_default();
        let rewritten = serde_json::to_value(&auth).unwrap_or_default();
        assert_eq!(rewritten, written);
    }
}
