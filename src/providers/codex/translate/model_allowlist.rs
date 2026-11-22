use std::{borrow::Cow, collections::BTreeSet};

use crate::config;

use super::model_catalog::{CatalogModel, catalog_model, catalog_models};
use super::request::ServiceTier;

pub const ALLOWED_MODELS: &[&str] = &[
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-6-sol",
    "gpt-6.1-sol",
    "gpt-6.1-terra",
];

const OPUS_TARGETS: &[&str] = &["gpt-6-sol", "gpt-6.1-sol", "gpt-6-astra", "gpt-5.6-sol"];
const SONNET_TARGETS: &[&str] = &["gpt-5.6-terra"];
const HAIKU_TARGETS: &[&str] = &["gpt-6-luna", "gpt-5.6-luna"];

pub const MODEL_ALIASES: &[(&str, &[&str])] = &[
    ("haiku", HAIKU_TARGETS),
    ("claude-haiku-4-5", HAIKU_TARGETS),
    ("claude-haiku-4-5-20251001", HAIKU_TARGETS),
    ("claude-haiku-5-5", HAIKU_TARGETS),
    ("sonnet", SONNET_TARGETS),
    ("claude-sonnet-4-6", SONNET_TARGETS),
    ("claude-sonnet-5", SONNET_TARGETS),
    ("claude-sonnet-5-5", SONNET_TARGETS),
    ("opus", OPUS_TARGETS),
    ("claude-opus-4-7", OPUS_TARGETS),
    ("claude-opus-4-8", OPUS_TARGETS),
    ("claude-opus-5", OPUS_TARGETS),
    ("claude-opus-5-5", OPUS_TARGETS),
    ("fable", OPUS_TARGETS),
    ("claude-fable-5", OPUS_TARGETS),
    ("claude-fable-5-1", OPUS_TARGETS),
];

pub const FAMILY_ALIASES: &[&str] = &["terra", "sol", "astra", "luna"];

pub fn is_family_alias(model: &str) -> bool {
    FAMILY_ALIASES.contains(&model.strip_prefix("anthropic-").unwrap_or(model))
}

pub(in crate::providers::codex) fn version(id: &str, family: &str) -> Option<[String; 3]> {
    let suffix = format!("-{family}");
    let numbers = id.strip_prefix("gpt-")?.strip_suffix(&suffix)?;
    let parts: Vec<_> = numbers.split('.').collect();
    if parts.is_empty()
        || parts.len() > 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let mut result = [String::from("0"), String::from("0"), String::from("0")];
    for (i, part) in parts.iter().enumerate() {
        let n = part.trim_start_matches('0');
        result[i] = if n.is_empty() { "0" } else { n }.to_string();
    }
    Some(result)
}

pub fn resolve_alias(model: &str) -> Option<Cow<'static, str>> {
    if !is_family_alias(model) {
        return alias_target(model).map(Cow::Borrowed);
    }
    resolve_family(model).map(|resolved| Cow::Owned(resolved.model))
}

pub(in crate::providers::codex) fn resolve_family(model: &str) -> Option<ResolvedModel> {
    if !is_family_alias(model) {
        return None;
    }
    resolve_family_in(model, &catalog_models())
}

pub(crate) fn resolve_family_in(model: &str, catalog: &[CatalogModel]) -> Option<ResolvedModel> {
    let family = model.strip_prefix("anthropic-").unwrap_or(model);
    let mut candidates: Vec<_> = catalog
        .iter()
        .filter(|entry| entry.listed && entry.supported_in_api)
        .filter_map(|entry| {
            version(&entry.slug, family).map(|v| (entry.slug.clone(), v, entry.use_responses_lite))
        })
        .collect();
    if candidates.is_empty() {
        candidates = ALLOWED_MODELS
            .iter()
            .filter_map(|id| version(id, family).map(|v| ((*id).to_string(), v, false)))
            .collect();
    }
    candidates.sort_by(|a, b| {
        for (x, y) in a.1.iter().zip(&b.1) {
            let order = y.len().cmp(&x.len()).then_with(|| y.cmp(x));
            if !order.is_eq() {
                return order;
            }
        }
        a.0.cmp(&b.0)
    });
    candidates.into_iter().next().map(|(model, _, lite)| {
        let family_catalog_lite = (!ALLOWED_MODELS.contains(&model.as_str())).then_some(lite);
        ResolvedModel {
            model,
            service_tier: None,
            family_catalog_lite,
        }
    })
}

pub fn alias_target(model: &str) -> Option<&'static str> {
    alias_target_in(model, listed_catalog_models)
}

fn alias_target_in(model: &str, catalog: impl FnOnce() -> Vec<String>) -> Option<&'static str> {
    let targets = MODEL_ALIASES.iter().find(|(alias, _)| *alias == model)?.1;
    let catalog = catalog();
    Some(
        targets
            .iter()
            .copied()
            .find(|target| catalog.iter().any(|m| m == target))
            .unwrap_or(targets[0]),
    )
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub service_tier: Option<ServiceTier>,
    family_catalog_lite: Option<bool>,
}

impl ResolvedModel {
    pub(in crate::providers::codex) fn assert_allowed(&self) -> Result<(), ModelNotAllowedError> {
        if self.family_catalog_lite.is_some() {
            Ok(())
        } else {
            assert_allowed_model(&self.model)
        }
    }

    pub(in crate::providers::codex) fn uses_responses_lite(&self, full_lane: bool) -> bool {
        self.family_catalog_lite
            .unwrap_or_else(|| uses_responses_lite_with_full_lane(&self.model, full_lane))
    }
}

pub fn is_allowed_model(model: &str) -> bool {
    ALLOWED_MODELS.contains(&model)
        || catalog_model(model).is_some_and(|entry| entry.supported_in_api)
}

pub fn allowed_models() -> Vec<String> {
    let mut set: BTreeSet<String> = ALLOWED_MODELS.iter().map(|m| (*m).to_string()).collect();
    for entry in catalog_models() {
        if entry.supported_in_api && entry.listed {
            set.insert(entry.slug);
        }
    }
    set.into_iter().collect()
}

pub fn listed_catalog_models() -> Vec<String> {
    catalog_models()
        .into_iter()
        .filter(|entry| entry.listed && entry.supported_in_api)
        .map(|entry| entry.slug)
        .collect()
}

pub fn allowed_models_display() -> String {
    allowed_models().join(", ")
}

pub fn listed_models() -> Vec<String> {
    let mut models: Vec<String> = allowed_models()
        .into_iter()
        .flat_map(|m| {
            let variants = tier_model_variants(&m);
            std::iter::once(m).chain(variants)
        })
        .collect();
    for family in FAMILY_ALIASES {
        models.push((*family).to_string());
        models.push(format!("anthropic-{family}"));
    }
    models.sort_unstable();
    models
}

pub const ULTRAFAST_MODELS: &[&str] = &["gpt-6-astra", "gpt-6.1-sol"];

const FAST_SUFFIX: &str = "-fast";
const ULTRAFAST_SUFFIX: &str = "-ultrafast";

pub fn split_tier_suffix(model: &str) -> Option<(&str, ServiceTier)> {
    split_tier_suffix_with(model, is_allowed_model)
}

pub(crate) fn split_tier_suffix_with(
    model: &str,
    is_allowed: impl FnOnce(&str) -> bool,
) -> Option<(&str, ServiceTier)> {
    if let Some(base) = model.strip_suffix(ULTRAFAST_SUFFIX) {
        return ULTRAFAST_MODELS
            .contains(&base)
            .then_some((base, ServiceTier::Ultrafast));
    }
    let base = model.strip_suffix(FAST_SUFFIX)?;
    is_allowed(base).then_some((base, ServiceTier::Priority))
}

pub fn tier_model_variants(model: &str) -> Vec<String> {
    let mut variants = vec![format!("{model}{FAST_SUFFIX}")];
    if ULTRAFAST_MODELS.contains(&model) {
        variants.push(format!("{model}{ULTRAFAST_SUFFIX}"));
    }
    variants
}

pub fn service_tier_for_model(model: &str, tier: ServiceTier) -> ServiceTier {
    match tier {
        ServiceTier::Ultrafast if !ULTRAFAST_MODELS.contains(&model) => ServiceTier::Priority,
        tier => tier,
    }
}

pub(in crate::providers::codex) fn resolve_tier_model_alias(model: &str) -> ResolvedModel {
    match split_tier_suffix(model) {
        Some((base, tier)) => ResolvedModel {
            model: base.to_string(),
            service_tier: Some(tier),
            family_catalog_lite: None,
        },
        None => ResolvedModel {
            model: model.to_string(),
            service_tier: None,
            family_catalog_lite: None,
        },
    }
}

pub fn resolve_model_request(model: &str) -> ResolvedModel {
    resolve_model_request_with_config_override(model, true)
}

pub fn resolve_model_request_with_config_override(
    model: &str,
    apply_config_override: bool,
) -> ResolvedModel {
    let override_model = apply_config_override.then(config::codex_model).flatten();
    resolve_with_model_override(model, override_model.as_deref())
}

fn resolve_with_model_override(model: &str, override_model: Option<&str>) -> ResolvedModel {
    let requested = resolve_family(model)
        .unwrap_or_else(|| resolve_tier_model_alias(alias_target(model).unwrap_or(model)));
    let requested_tier = requested.service_tier.clone();
    let mut resolved = match override_model {
        Some(val) if !val.is_empty() => resolve_tier_model_alias(val),
        _ => requested,
    };

    resolved.service_tier = resolved
        .service_tier
        .or(requested_tier)
        .map(|tier| service_tier_for_model(&resolved.model, tier));
    resolved
}

pub fn resolve_model(model: &str) -> String {
    resolve_model_request(model).model
}

#[derive(Debug, Clone)]
pub struct ModelNotAllowedError {
    pub model: String,
}

impl std::fmt::Display for ModelNotAllowedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model not allowed: {}", self.model)
    }
}

pub fn assert_allowed_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if is_allowed_model(model) {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

pub fn uses_responses_lite(model: &str) -> bool {
    matches!(
        model,
        "gpt-5.6-luna"
            | "gpt-5.6-sol"
            | "gpt-5.6-terra"
            | "gpt-6-astra"
            | "gpt-6-luna"
            | "gpt-6-sol"
            | "gpt-6.1-sol"
            | "gpt-6.1-terra"
    ) || (!ALLOWED_MODELS.contains(&model)
        && catalog_model(model)
            .is_some_and(|entry| entry.supported_in_api && entry.use_responses_lite))
}

pub fn uses_responses_lite_with_full_lane(model: &str, full_lane: bool) -> bool {
    if full_lane
        && matches!(
            model,
            "gpt-5.6-sol"
                | "gpt-5.6-terra"
                | "gpt-6-astra"
                | "gpt-6-sol"
                | "gpt-6.1-sol"
                | "gpt-6.1-terra"
        )
    {
        return false;
    }
    uses_responses_lite(model)
}

pub fn full_lane_web_search_model(resolved: &ResolvedModel) -> Cow<'_, str> {
    full_lane_web_search_model_in(resolved, catalog_models)
}

fn full_lane_web_search_model_in(
    resolved: &ResolvedModel,
    catalog: impl FnOnce() -> Vec<CatalogModel>,
) -> Cow<'_, str> {
    let model = resolved.model.as_str();
    match model {
        "gpt-5.6-luna" => Cow::Borrowed("gpt-5.6-sol"),
        "gpt-6-luna" => Cow::Borrowed("gpt-6-sol"),
        _ if version(model, "luna").is_some() && resolved.uses_responses_lite(false) => {
            let sibling = format!("{}-sol", model.strip_suffix("-luna").unwrap_or(model));
            let catalog: Vec<_> = catalog()
                .into_iter()
                .filter(|entry| {
                    !entry.use_responses_lite || ALLOWED_MODELS.contains(&entry.slug.as_str())
                })
                .collect();
            if ALLOWED_MODELS.contains(&sibling.as_str())
                || catalog
                    .iter()
                    .any(|entry| entry.slug == sibling && entry.listed && entry.supported_in_api)
            {
                Cow::Owned(sibling)
            } else {
                resolve_family_in("sol", &catalog)
                    .map(|resolved| Cow::Owned(resolved.model))
                    .unwrap_or(Cow::Borrowed(model))
            }
        }
        _ => Cow::Borrowed(model),
    }
}

pub fn is_valid_model_for_codex(model: &str) -> bool {
    if is_family_alias(model) || is_allowed_model(model) {
        return true;
    }
    if split_tier_suffix(model).is_some() {
        return true;
    }
    MODEL_ALIASES.iter().any(|(alias, _)| *alias == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_family_model_remains_valid_after_catalog_removal() {
        let id = "gpt-99.1-sol";
        assert!(!is_allowed_model(id));
        for lite in [true, false] {
            let resolved = resolve_family_in(
                "anthropic-sol",
                &[CatalogModel {
                    slug: id.to_string(),
                    use_responses_lite: lite,
                    supported_in_api: true,
                    listed: true,
                    priority: None,
                    display_name: None,
                }],
            )
            .unwrap();
            assert_eq!(resolved.model, id);
            assert!(resolved.assert_allowed().is_ok());
            assert_eq!(resolved.uses_responses_lite(true), lite);
        }
    }

    #[test]
    fn lite_luna_maps_to_sol_after_catalog_removal() {
        let resolved = resolve_family_in(
            "luna",
            &[CatalogModel {
                slug: "gpt-99-luna".to_string(),
                use_responses_lite: true,
                supported_in_api: true,
                listed: true,
                priority: None,
                display_name: None,
            }],
        )
        .unwrap();
        assert_eq!(
            full_lane_web_search_model_in(&resolved, Vec::new),
            "gpt-6.1-sol"
        );
    }

    #[test]
    fn haiku_resolves_to_luna() {
        let r = resolve_model_request("haiku");
        assert_eq!(r.model, "gpt-6-luna");
    }

    #[test]
    fn full_lane_covers_all_lite_models_except_luna() {
        for model in [
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6.1-sol",
            "gpt-6.1-terra",
        ] {
            assert!(uses_responses_lite_with_full_lane(model, false), "{model}");
            assert!(!uses_responses_lite_with_full_lane(model, true), "{model}");
        }
        for model in ["gpt-5.6-luna", "gpt-6-luna"] {
            assert!(uses_responses_lite_with_full_lane(model, false), "{model}");
            assert!(uses_responses_lite_with_full_lane(model, true), "{model}");
        }
        for model in ["gpt-5.2", "gpt-5.3-codex", "gpt-5.4", "gpt-5.5"] {
            assert!(!uses_responses_lite_with_full_lane(model, false), "{model}");
            assert!(!uses_responses_lite_with_full_lane(model, true), "{model}");
        }
    }

    #[test]
    fn web_search_upgrades_luna_to_full_lane_sibling() {
        for (model, expected) in [
            ("gpt-5.6-luna", "gpt-5.6-sol"),
            ("gpt-5.6-sol", "gpt-5.6-sol"),
            ("gpt-5.6-terra", "gpt-5.6-terra"),
            ("gpt-5.4", "gpt-5.4"),
            ("gpt-6-luna", "gpt-6-sol"),
            ("gpt-6-sol", "gpt-6-sol"),
        ] {
            let resolved = resolve_tier_model_alias(model);
            assert_eq!(full_lane_web_search_model(&resolved), expected);
        }
    }

    #[test]
    fn sonnet_resolves_to_terra() {
        let r = resolve_model_request("sonnet");
        assert_eq!(r.model, "gpt-5.6-terra");
    }

    #[test]
    fn sonnet_5_resolves_to_terra() {
        let r = resolve_model_request("claude-sonnet-5");
        assert_eq!(r.model, "gpt-5.6-terra");
    }

    #[test]
    fn opus_resolves_to_sol() {
        let r = resolve_model_request("opus");
        assert_eq!(r.model, "gpt-6-sol");
    }

    #[test]
    fn opus_aliases_resolve_to_sol() {
        for model in ["claude-opus-4-8", "claude-opus-5", "claude-opus-5-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-6-sol");
        }
    }

    #[test]
    fn fable_5_resolves_to_sol() {
        for model in ["fable", "claude-fable-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-6-sol");
        }
    }

    #[test]
    fn gpt_6_sol_fast_adds_priority() {
        for model in ["gpt-6-sol", "gpt-6.1-sol", "gpt-6.1-terra"] {
            let r = resolve_model_request(&format!("{model}-fast"));
            assert_eq!(r.model, model);
            assert_eq!(r.service_tier, Some(ServiceTier::Priority));
        }
    }

    #[test]
    fn gpt_6_models_use_responses_lite() {
        assert!(uses_responses_lite("gpt-6-sol"));
        assert!(uses_responses_lite("gpt-6-luna"));
        assert!(uses_responses_lite("gpt-6.1-sol"));
        assert!(uses_responses_lite("gpt-6.1-terra"));
    }

    #[test]
    fn alias_prefers_the_first_target_the_catalog_lists() {
        let catalog = ["gpt-6-astra".to_string(), "gpt-5.6-terra".to_string()];
        assert_eq!(
            alias_target_in("opus", || catalog.to_vec()),
            Some("gpt-6-astra")
        );
        assert_eq!(
            alias_target_in("claude-fable-5-1", || catalog.to_vec()),
            Some("gpt-6-astra")
        );
        assert_eq!(
            alias_target_in("haiku", || catalog.to_vec()),
            Some("gpt-6-luna")
        );
        assert_eq!(alias_target_in("opus", Vec::new), Some("gpt-6-sol"));
        assert_eq!(alias_target_in("gpt-6-astra", || catalog.to_vec()), None);
    }

    #[test]
    fn listed_models_pair_every_model_with_fast() {
        let listed = listed_models();
        for model in ALLOWED_MODELS {
            assert!(listed.iter().any(|m| m == model));
            assert!(listed.contains(&format!("{model}-fast")));
        }
    }

    #[test]
    fn fast_suffix_adds_priority() {
        let r = resolve_model_request("gpt-5.6-sol-fast");
        assert_eq!(r.model, "gpt-5.6-sol");
        assert_eq!(r.service_tier, Some(ServiceTier::Priority));
    }

    #[test]
    fn ultrafast_suffix_adds_ultrafast_tier() {
        for model in ULTRAFAST_MODELS {
            let name = format!("{model}-ultrafast");
            assert!(is_valid_model_for_codex(&name), "{name}");
            let r = resolve_with_model_override(&name, None);
            assert_eq!(r.model, *model);
            assert_eq!(r.service_tier, Some(ServiceTier::Ultrafast));
            assert!(assert_allowed_model(&r.model).is_ok());
        }
    }

    #[test]
    fn ultrafast_suffix_is_rejected_for_models_without_the_tier() {
        for model in ALLOWED_MODELS {
            if ULTRAFAST_MODELS.contains(model) {
                continue;
            }
            let name = format!("{model}-ultrafast");
            assert!(!is_valid_model_for_codex(&name), "{name}");
            assert_eq!(split_tier_suffix(&name), None, "{name}");
            let r = resolve_with_model_override(&name, None);
            assert_eq!(r.service_tier, None, "{name}");
            assert!(assert_allowed_model(&r.model).is_err(), "{name}");
        }
    }

    #[test]
    fn ultrafast_models_are_registered() {
        for model in ULTRAFAST_MODELS {
            assert!(ALLOWED_MODELS.contains(model), "{model}");
        }
    }

    #[test]
    fn service_tier_narrows_to_what_the_model_offers() {
        for (model, tier, expected) in [
            (
                "gpt-6-astra",
                ServiceTier::Ultrafast,
                ServiceTier::Ultrafast,
            ),
            ("gpt-6-astra", ServiceTier::Priority, ServiceTier::Priority),
            ("gpt-6-astra", ServiceTier::Flex, ServiceTier::Flex),
            (
                "gpt-6.1-sol",
                ServiceTier::Ultrafast,
                ServiceTier::Ultrafast,
            ),
            ("gpt-6-sol", ServiceTier::Ultrafast, ServiceTier::Priority),
            ("gpt-6-luna", ServiceTier::Ultrafast, ServiceTier::Priority),
            ("gpt-6-sol", ServiceTier::Priority, ServiceTier::Priority),
            ("gpt-6-sol", ServiceTier::Flex, ServiceTier::Flex),
        ] {
            assert_eq!(
                service_tier_for_model(model, tier.clone()),
                expected,
                "{model} {tier:?}"
            );
        }
    }

    #[test]
    fn tier_suffix_is_stripped_once() {
        assert_eq!(
            split_tier_suffix("gpt-6-astra-fast"),
            Some(("gpt-6-astra", ServiceTier::Priority))
        );
        assert_eq!(split_tier_suffix("gpt-6-astra-fast-fast"), None);
        assert_eq!(split_tier_suffix("gpt-6-astra-ultrafast-fast"), None);
        assert_eq!(split_tier_suffix("gpt-6-astra"), None);
        assert_eq!(split_tier_suffix("-fast"), None);
    }

    #[test]
    fn tier_model_variants_lists_supported_suffixes() {
        assert_eq!(
            tier_model_variants("gpt-6-astra"),
            ["gpt-6-astra-fast", "gpt-6-astra-ultrafast"]
        );
        assert_eq!(tier_model_variants("gpt-6-sol"), ["gpt-6-sol-fast"]);
    }

    #[test]
    fn model_override_merges_tier_suffixes() {
        let cases: &[(&str, Option<&str>, &str, Option<ServiceTier>)] = &[
            ("gpt-6-sol", None, "gpt-6-sol", None),
            ("gpt-6-sol", Some(""), "gpt-6-sol", None),
            (
                "gpt-6-sol-fast",
                None,
                "gpt-6-sol",
                Some(ServiceTier::Priority),
            ),
            (
                "gpt-6-sol",
                Some("gpt-6-astra-fast"),
                "gpt-6-astra",
                Some(ServiceTier::Priority),
            ),
            (
                "gpt-6-sol-fast",
                Some("gpt-6-astra"),
                "gpt-6-astra",
                Some(ServiceTier::Priority),
            ),
            (
                "gpt-6-sol",
                Some("gpt-6-astra-ultrafast"),
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                "gpt-6-sol-fast",
                Some("gpt-6-astra-ultrafast"),
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                "gpt-6-astra-ultrafast",
                Some("gpt-6-astra-fast"),
                "gpt-6-astra",
                Some(ServiceTier::Priority),
            ),
            (
                "gpt-6-astra-ultrafast",
                Some("gpt-6-astra"),
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                "gpt-6-astra-ultrafast",
                Some("gpt-6-sol"),
                "gpt-6-sol",
                Some(ServiceTier::Priority),
            ),
            ("opus", Some("gpt-6-astra"), "gpt-6-astra", None),
        ];
        for (requested, override_model, model, tier) in cases {
            let r = resolve_with_model_override(requested, *override_model);
            assert_eq!(r.model, *model, "{requested} + {override_model:?}");
            assert_eq!(&r.service_tier, tier, "{requested} + {override_model:?}");
        }
    }

    #[test]
    fn allowed_models_accept_base() {
        assert!(assert_allowed_model("gpt-5.4").is_ok());
        assert!(assert_allowed_model("gpt-5.6-sol").is_ok());
        assert!(assert_allowed_model("gpt-5.6-terra").is_ok());
        assert!(assert_allowed_model("gpt-6-astra").is_ok());
        assert!(assert_allowed_model("gpt-5.6-luna").is_ok());
        assert!(assert_allowed_model("gpt-6.1-sol").is_ok());
        assert!(assert_allowed_model("gpt-6.1-terra").is_ok());
    }

    #[test]
    fn not_allowed_rejected() {
        assert!(assert_allowed_model("gpt-7").is_err());
        assert!(assert_allowed_model("gpt-7-fast").is_err());
    }

    #[test]
    fn allowed_models_listing_contains_baseline() {
        let listing = allowed_models();
        for baseline in ALLOWED_MODELS {
            assert!(listing.iter().any(|m| m == baseline), "{baseline} missing");
        }
        assert!(allowed_models_display().contains("gpt-6-astra"));
    }
}
