//! The model catalog the pool advertises on /v1/models.
//!
//! A port of modelsLocked, codexModelCatalogLocked and the reasoning-level
//! rules in the Go pool's main.go. Codex reads the catalog with its
//! remote-model schema and treats it as authoritative for a ChatGPT-backed
//! provider, so every field it requires is present, and a model is advertised
//! only while upstream still serves it.

use super::app::Inner;
use super::gosort::slice_stable;
use super::proxy::parse_model;
use serde::Serialize;
use serde_json::Value;

/// The current Codex model lineup (September 2026), ordered the way the
/// picker should rank them. These are merged into the advertised catalog so a
/// stock Codex client can select any current model without its requested
/// model falling off this pool's catalog; a model missing here still works as
/// a request input, but the client then runs on bundled fallback metadata,
/// which prints a startup warning and can attach conflicting tools.
/// Advertising a model is not an access grant: per-account allowedModels and
/// excludedModels still gate routing, and upstream still enforces plan access.
///
/// Only models upstream still serves belong here: upstream's own catalog at
/// GET /backend-api/codex/models returns gpt-6-astra, gpt-5.6-sol,
/// gpt-5.6-terra, gpt-5.6-luna, gpt-5.5, codex-auto-review and gpt-reserve at
/// every client_version. gpt-6-sol and gpt-6-luna shipped on 2026-09-22 and
/// rank above their gpt-5.6 predecessors, which stay listed until upstream
/// drops them; there is no gpt-6-terra. gpt-6.1-sol shipped on 2026-09-29 and
/// ranks just under gpt-6-astra; there is no gpt-6.1-astra. gpt-6.1-terra
/// came with Codex 0.165, whose bundled list (models-manager/models.json)
/// ranks it between gpt-5.6-sol and gpt-5.6-terra; it is listed from that
/// file, not yet from upstream's own catalog. Listing a retired
/// model is worse than omitting it: a client picks it out of this catalog and
/// every request dies at the gateway, which answers a model it cannot route
/// with a 5xx that reads as an account failure.
pub const DEFAULT_CODEX_MODEL_SLUGS: &[&str] = &[
    "gpt-6-astra",
    "gpt-6.1-sol",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-6.1-terra",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
];

/// Codex treats remote model metadata as authoritative for ChatGPT-backed
/// providers. This must stay non-empty or a schema-only fix would silently
/// remove the coding-agent instructions after a successful refresh.
pub const CODEX_MODEL_BASE_INSTRUCTIONS: &str = "You are Codex, a coding agent running in a terminal-based coding assistant. Inspect the workspace before acting, follow repository instructions such as AGENTS.md, use the available tools to implement and verify changes, keep the user informed, and continue until the task is genuinely handled.";

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ReasoningLevel {
    pub effort: &'static str,
    pub description: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct TruncationPolicy {
    pub mode: &'static str,
    pub limit: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub default_reasoning_level: String,
    pub supported_reasoning_levels: Vec<ReasoningLevel>,
    pub shell_type: &'static str,
    pub visibility: &'static str,
    pub supported_in_api: bool,
    pub priority: i64,
    pub additional_speed_tiers: Vec<String>,
    pub service_tiers: Vec<Value>,
    pub default_service_tier: Option<Value>,
    pub availability_nux: Option<Value>,
    pub upgrade: Option<Value>,
    pub base_instructions: &'static str,
    pub model_messages: Option<Value>,
    pub supports_reasoning_summaries: bool,
    pub supports_reasoning_summary_parameter: bool,
    pub default_reasoning_summary: &'static str,
    pub support_verbosity: bool,
    pub default_verbosity: &'static str,
    pub apply_patch_tool_type: &'static str,
    pub web_search_tool_type: &'static str,
    pub truncation_policy: TruncationPolicy,
    pub supports_parallel_tool_calls: bool,
    pub supports_image_detail_original: bool,
    pub context_window: i64,
    pub context_length: i64,
    pub max_context_window: i64,
    pub auto_compact_token_limit: Option<Value>,
    pub comp_hash: Option<Value>,
    pub effective_context_window_percent: i64,
    pub experimental_supported_tools: Vec<String>,
    pub input_modalities: Vec<&'static str>,
    pub include_skills_usage_instructions: bool,
    pub supports_search_tool: bool,
    pub use_responses_lite: bool,
    pub auto_review_model_override: Option<Value>,
    pub tool_mode: Option<Value>,
    pub multi_agent_version: Option<Value>,
}

pub fn codex_reasoning_levels() -> Vec<ReasoningLevel> {
    vec![
        ReasoningLevel {
            effort: "low",
            description: "Fast responses with lighter reasoning",
        },
        ReasoningLevel {
            effort: "medium",
            description: "Balances speed and reasoning depth for everyday tasks",
        },
        ReasoningLevel {
            effort: "high",
            description: "Greater reasoning depth for complex problems",
        },
        ReasoningLevel {
            effort: "xhigh",
            description: "Extra high reasoning depth for complex problems",
        },
    ]
}

/// Whether a slug is the family itself or one of its hyphen-separated
/// members. The boundary matters: a bare prefix test would pull unrelated
/// slugs such as gpt-60-legacy or gpt-5.61 into a documented family and
/// advertise capabilities their upstream never promised.
pub fn codex_model_in_family(model: &str, family: &str) -> bool {
    model == family || model.starts_with(&format!("{family}-"))
}

/// Whether a model family documents the extended `max` tier: the gpt-6,
/// gpt-6.1 and gpt-5.6 families; older families stop at xhigh. gpt-6.1 is its
/// own family on purpose: a point release is not a hyphenated member of gpt-6,
/// so gpt-6.1-sol would otherwise silently fall back to the low-xhigh range.
pub fn codex_extended_reasoning_model(model: &str) -> bool {
    codex_model_in_family(model, "gpt-6")
        || codex_model_in_family(model, "gpt-6.1")
        || codex_model_in_family(model, "gpt-5.6")
}

/// Whether an extended-tier model also documents `ultra`. Advertising ultra
/// where it is not documented would let the client submit an effort upstream
/// rejects, so two cases stop at `max`: gpt-6-luna (exact slug on purpose; the
/// rest of gpt-6, and gpt-5.6-luna, keep ultra), and the gpt-6.1 family, whose
/// ultra support OpenAI lists as "coming later" (2026-09-29).
/// TODO(upstream): drop the gpt-6.1 exclusion once the Codex models page
/// documents ultra for gpt-6.1-sol.
pub fn codex_ultra_reasoning_model(model: &str) -> bool {
    if model == "gpt-6-luna" || codex_model_in_family(model, "gpt-6.1") {
        return false;
    }
    codex_extended_reasoning_model(model)
}

pub fn codex_reasoning_levels_for_model(model: &str) -> Vec<ReasoningLevel> {
    let mut levels = codex_reasoning_levels();
    if codex_extended_reasoning_model(model) {
        levels.push(ReasoningLevel {
            effort: "max",
            description: "Maximum reasoning depth for the hardest problems",
        });
    }
    if codex_ultra_reasoning_model(model) {
        levels.push(ReasoningLevel {
            effort: "ultra",
            description: "Deepest reasoning for ambiguous, high-value work",
        });
    }
    levels
}

fn catalog_reasoning_default(tier: &str, levels: &[ReasoningLevel]) -> String {
    if levels.iter().any(|level| level.effort == tier) {
        tier.to_string()
    } else {
        "medium".to_string()
    }
}

/// The configured default model first, then the built-in lineup in order,
/// then any operator-configured extras.
pub fn codex_catalog_priority(model: &str, default_model: &str) -> i64 {
    if model == default_model {
        return 0;
    }
    DEFAULT_CODEX_MODEL_SLUGS
        .iter()
        .position(|slug| *slug == model)
        .map_or(1000, |index| i64::try_from(index).unwrap_or(999) + 1)
}

impl Inner {
    /// The advertised model names: the compiled-in lineup, the configured
    /// default model with its reasoning tiers, per-account allowedModels and
    /// aliases, sorted.
    pub fn models(&self) -> Vec<String> {
        let mut set = std::collections::BTreeSet::new();
        set.insert(self.config.default_model.clone());
        let (base, _) = parse_model(&self.config.default_model);
        if !base.is_empty() {
            for level in codex_reasoning_levels_for_model(&base) {
                set.insert(format!("{base}({})", level.effort));
            }
            set.insert(base);
        }
        set.extend(
            DEFAULT_CODEX_MODEL_SLUGS
                .iter()
                .map(|slug| (*slug).to_string()),
        );
        for account in self.config.accounts() {
            set.extend(account.allowed_models.iter().cloned());
        }
        set.extend(self.config.model_aliases().keys().cloned());
        set.into_iter().filter(|model| !model.is_empty()).collect()
    }

    pub fn codex_model_catalog(&self, models: &[String]) -> Vec<ModelInfo> {
        let (default_model, default_tier) = parse_model(&self.config.default_model);
        let mut seen = std::collections::HashSet::new();
        let mut items = Vec::new();
        for model in models {
            let (canonical, tier) = parse_model(model);
            let model = if tier.is_empty() {
                model.clone()
            } else {
                canonical
            };
            if model.is_empty() || !seen.insert(model.clone()) {
                continue;
            }
            let levels = codex_reasoning_levels_for_model(&model);
            let reasoning_default = if model == default_model {
                catalog_reasoning_default(&default_tier, &levels)
            } else {
                "medium".to_string()
            };
            items.push(ModelInfo {
                id: model.clone(),
                slug: model.clone(),
                display_name: model.clone(),
                description: model.clone(),
                default_reasoning_level: reasoning_default,
                supported_reasoning_levels: levels,
                shell_type: "shell_command",
                visibility: "list",
                supported_in_api: true,
                priority: codex_catalog_priority(&model, &default_model),
                additional_speed_tiers: Vec::new(),
                service_tiers: Vec::new(),
                default_service_tier: None,
                availability_nux: None,
                upgrade: None,
                base_instructions: CODEX_MODEL_BASE_INSTRUCTIONS,
                model_messages: None,
                supports_reasoning_summaries: true,
                supports_reasoning_summary_parameter: true,
                default_reasoning_summary: "none",
                support_verbosity: true,
                default_verbosity: "low",
                apply_patch_tool_type: "freeform",
                web_search_tool_type: "text_and_image",
                truncation_policy: TruncationPolicy {
                    mode: "tokens",
                    limit: 10000,
                },
                supports_parallel_tool_calls: true,
                supports_image_detail_original: true,
                context_window: 272_000,
                context_length: 272_000,
                max_context_window: 272_000,
                auto_compact_token_limit: None,
                comp_hash: None,
                effective_context_window_percent: 95,
                experimental_supported_tools: Vec::new(),
                input_modalities: vec!["text", "image"],
                include_skills_usage_instructions: false,
                supports_search_tool: false,
                use_responses_lite: false,
                auto_review_model_override: None,
                tool_mode: None,
                multi_agent_version: None,
            });
        }
        slice_stable(&mut items, |left, right| left.priority < right.priority);
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::testing::test_app;

    fn efforts(model: &str) -> String {
        codex_reasoning_levels_for_model(model)
            .iter()
            .map(|level| level.effort)
            .collect::<Vec<_>>()
            .join(",")
    }

    // Go TestDefaultModelCatalogIncludesThinkingTiers.
    #[test]
    fn default_model_catalog_includes_thinking_tiers() {
        let (app, _dir) = test_app(vec![]);
        app.write().config.default_model = "gpt-5.5(xhigh)".into();
        let models = app.read().models();
        for expected in [
            "gpt-5.5",
            "gpt-5.5(low)",
            "gpt-5.5(medium)",
            "gpt-5.5(high)",
            "gpt-5.5(xhigh)",
        ] {
            assert!(
                models.iter().any(|model| model == expected),
                "{expected} missing from {models:?}"
            );
        }
    }

    // Go TestCodexModelCatalogNormalizesUnsupportedDefaultReasoningTier.
    #[test]
    fn catalog_normalizes_unsupported_default_reasoning_tier() {
        let (app, _dir) = test_app(vec![]);
        app.write().config.default_model = "gpt-5.5(max)".into();
        let inner = app.read();
        let catalog = inner.codex_model_catalog(&inner.models());
        let model = catalog.iter().find(|model| model.slug == "gpt-5.5");
        assert_eq!(
            model.map(|model| model.default_reasoning_level.as_str()),
            Some("medium")
        );
    }

    // Go TestCodexModelCatalogIncludesCurrentCodexLineup.
    #[test]
    fn catalog_includes_current_codex_lineup() {
        let (app, _dir) = test_app(vec![]);
        app.write().config.default_model = "gpt-5.5(xhigh)".into();
        let inner = app.read();
        let catalog = inner.codex_model_catalog(&inner.models());
        let by_slug: std::collections::HashMap<&str, &ModelInfo> = catalog
            .iter()
            .map(|model| (model.slug.as_str(), model))
            .collect();
        for slug in DEFAULT_CODEX_MODEL_SLUGS {
            assert!(by_slug.contains_key(slug), "{slug}");
        }
        assert_eq!(
            (catalog[0].slug.as_str(), catalog[0].priority),
            ("gpt-5.5", 0)
        );
        assert!(by_slug["gpt-6-astra"].priority < by_slug["gpt-5.6-luna"].priority);
        assert_eq!(efforts("gpt-5.6-sol"), "low,medium,high,xhigh,max,ultra");
        assert_eq!(by_slug["gpt-5.5"].supported_reasoning_levels.len(), 4);
    }

    // Go TestGPT6LunaStopsAtMax and TestExtendedReasoningRequiresFamilyBoundary.
    #[test]
    fn reasoning_tiers_follow_the_documented_families() {
        for model in ["gpt-6-luna", "gpt-6.1-sol"] {
            assert_eq!(efforts(model), "low,medium,high,xhigh,max", "{model}");
        }
        for model in ["gpt-6-sol", "gpt-6-astra", "gpt-5.6-luna"] {
            assert_eq!(efforts(model), "low,medium,high,xhigh,max,ultra", "{model}");
        }
        for model in [
            "gpt-6",
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-6.1",
            "gpt-6.1-sol",
            "gpt-5.6",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
        ] {
            assert!(codex_extended_reasoning_model(model), "{model}");
        }
        for model in [
            "gpt-60-legacy",
            "gpt-6preview",
            "gpt-5.61",
            "gpt-5.6x",
            "gpt-5.5",
            "gpt-5.2-codex",
            "gpt-61",
            "gpt-6.10-sol",
            "gpt-6.1x",
        ] {
            assert!(!codex_extended_reasoning_model(model), "{model}");
        }
    }

    // Go TestDefaultLineupMatchesWhatUpstreamStillServes and the lineup half
    // of TestRetiredModelMetersAreDropped.
    #[test]
    fn default_lineup_matches_what_upstream_still_serves() {
        let want = [
            "gpt-6-astra",
            "gpt-6.1-sol",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-6.1-terra",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
        ];
        assert_eq!(DEFAULT_CODEX_MODEL_SLUGS, want);
        for slug in crate::pool::quota::RETIRED_CODEX_MODEL_SLUGS {
            assert!(
                !DEFAULT_CODEX_MODEL_SLUGS.contains(slug),
                "{slug} is both retired and advertised"
            );
        }
    }

    // Go TestCodexModelCatalogAdvertisesGPT6.
    #[test]
    fn catalog_advertises_gpt6() {
        let (app, _dir) = test_app(Vec::new());
        app.write().config.default_model = "gpt-5.5(xhigh)".into();
        let inner = app.read();
        let models = inner.codex_model_catalog(&inner.models());
        let by_slug = |slug: &str| models.iter().find(|model| model.slug == slug).cloned();
        let astra = by_slug("gpt-6-astra").unwrap_or_else(|| panic!("catalog missing gpt-6-astra"));
        let efforts: Vec<&str> = astra
            .supported_reasoning_levels
            .iter()
            .map(|level| level.effort)
            .collect();
        assert_eq!(
            efforts.join(","),
            "low,medium,high,xhigh,max,ultra",
            "gpt-6 reasoning levels"
        );
        let sol = by_slug("gpt-5.6-sol").unwrap_or_else(|| panic!("catalog missing gpt-5.6-sol"));
        assert!(
            astra.priority < sol.priority,
            "gpt-6 must outrank the gpt-5.6 family"
        );
    }

    // Go TestCodexModelCatalogKeepsExtendedDefaultReasoningTierOn56.
    #[test]
    fn catalog_keeps_extended_default_reasoning_tier_on_56() {
        let (app, _dir) = test_app(Vec::new());
        app.write().config.default_model = "gpt-5.6-sol(ultra)".into();
        let inner = app.read();
        let models = inner.codex_model_catalog(&inner.models());
        assert!(
            models[0].slug == "gpt-5.6-sol" && models[0].default_reasoning_level == "ultra",
            "gpt-5.6 default reasoning tier was not preserved"
        );
        let aliases = inner.models().join("\n");
        for expected in ["gpt-5.6-sol(max)", "gpt-5.6-sol(ultra)"] {
            assert!(
                aliases.contains(expected),
                "generic model list missing {expected}"
            );
        }
    }
}
