use std::collections::BTreeSet;

use super::model_allowlist::{
    ALLOWED_MODELS, FAMILY_ALIASES, MODEL_ALIASES, compare_family_versions, is_family_alias,
    version,
};
use super::model_catalog::valid_slug;
use super::request::ServiceTier;
use crate::config::codex_auth::AuthSource;

#[derive(Debug, Clone)]
pub struct ApiResolvedModel {
    pub model: String,
    pub service_tier: Option<ServiceTier>,
}

impl ApiResolvedModel {
    pub fn uses_responses_lite(&self) -> bool {
        false
    }
}

fn valid_id(id: &str) -> bool {
    valid_slug(id)
        && (ALLOWED_MODELS.contains(&id)
            || FAMILY_ALIASES
                .iter()
                .any(|family| version(id, family).is_some()))
}

pub fn resolve_api_model(requested: &str, catalog: Option<&[String]>) -> Option<ApiResolvedModel> {
    let normalized = crate::registry::normalize_incoming_model(requested);
    let (base, service_tier) = if let Some(base) = normalized.strip_suffix("-ultrafast") {
        (base, Some(ServiceTier::Priority))
    } else if let Some(base) = normalized.strip_suffix("-fast") {
        (base, Some(ServiceTier::Priority))
    } else {
        (normalized.as_str(), None)
    };
    if service_tier.is_some() && !valid_id(base) {
        return None;
    }
    let fallback: Vec<String> = ALLOWED_MODELS.iter().map(|id| (*id).to_string()).collect();
    let candidates = catalog.unwrap_or(&fallback);
    let model = if is_family_alias(base) {
        let family = base.strip_prefix("anthropic-").unwrap_or(base);
        let mut models: Vec<_> = candidates
            .iter()
            .filter(|id| valid_id(id))
            .filter_map(|id| version(id, family).map(|v| (id, v)))
            .collect();
        models.sort_by(|a, b| compare_family_versions((a.0, &a.1), (b.0, &b.1)));
        models.first()?.0.to_string()
    } else if let Some((_, targets)) = MODEL_ALIASES.iter().find(|(alias, _)| *alias == base) {
        targets
            .iter()
            .find(|target| candidates.iter().any(|id| id == **target))?
            .to_string()
    } else {
        if !valid_id(base) || catalog.is_some_and(|ids| !ids.iter().any(|id| id == base)) {
            return None;
        }
        base.to_string()
    };
    Some(ApiResolvedModel {
        model,
        service_tier,
    })
}

pub fn source_model_union(
    order: &[AuthSource],
    chatgpt: Option<&[String]>,
    api: Option<Option<&[String]>>,
) -> Vec<String> {
    let mut ids = BTreeSet::new();
    if order.contains(&AuthSource::ChatGpt)
        && let Some(chatgpt) = chatgpt
    {
        ids.extend(chatgpt.iter().cloned());
    }
    if order.contains(&AuthSource::ApiKey)
        && let Some(api) = api
    {
        let fallback: Vec<String> = ALLOWED_MODELS.iter().map(|id| (*id).to_string()).collect();
        let catalog = api.unwrap_or(&fallback);
        for id in catalog.iter().filter(|id| valid_id(id)) {
            ids.insert(id.clone());
            ids.insert(format!("{id}-fast"));
            ids.insert(format!("{id}-ultrafast"));
        }
        for family in FAMILY_ALIASES {
            if resolve_api_model(family, Some(catalog)).is_some() {
                ids.insert((*family).to_string());
                ids.insert(format!("anthropic-{family}"));
            }
        }
    }
    ids.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::translate::request::ServiceTier;

    #[test]
    fn aliases_resolve_against_each_source_and_numeric_versions() {
        let first = vec!["gpt-9-sol".into(), "gpt-10-sol".into()];
        let second = vec!["gpt-11-sol".into()];
        assert_eq!(
            resolve_api_model("sol[1m]", Some(&first)).unwrap().model,
            "gpt-10-sol"
        );
        assert_eq!(
            resolve_api_model("sol[1m]", Some(&second)).unwrap().model,
            "gpt-11-sol"
        );
        assert_eq!(
            resolve_api_model("opus", Some(&["gpt-6-astra".into()]))
                .unwrap()
                .model,
            "gpt-6-astra"
        );
    }

    #[test]
    fn known_catalog_is_authoritative_and_exact_remains_exact() {
        assert!(resolve_api_model("sol", Some(&[])).is_none());
        assert!(resolve_api_model("gpt-5.5", Some(&[])).is_none());
        assert_eq!(
            resolve_api_model("gpt-99-sol", None).unwrap().model,
            "gpt-99-sol"
        );
        assert!(resolve_api_model("gpt-99-sol", Some(&["gpt-100-sol".into()])).is_none());
        assert!(resolve_api_model("sol", None).is_some());
        assert!(resolve_api_model("unrelated", None).is_none());
    }

    #[test]
    fn api_tier_is_priority_and_never_lite() {
        let resolved = resolve_api_model("gpt-6-astra-ultrafast", None).unwrap();
        assert_eq!(resolved.service_tier, Some(ServiceTier::Priority));
        assert!(!resolved.uses_responses_lite());
        assert!(resolve_api_model("gpt-6-astra-fast-fast", None).is_none());
    }

    #[test]
    fn tier_suffixes_apply_only_to_exact_ids() {
        let catalog = vec!["gpt-6-astra".to_string(), "gpt-99-sol".to_string()];
        for alias in [
            "sol-fast",
            "anthropic-sol-ultrafast",
            "opus-fast",
            "claude-opus-5-ultrafast",
        ] {
            assert!(resolve_api_model(alias, Some(&catalog)).is_none());
        }
        assert!(resolve_api_model("sol", Some(&catalog)).is_some());
        for suffix in ["fast", "ultrafast"] {
            let resolved =
                resolve_api_model(&format!("gpt-99-sol-{suffix}"), Some(&catalog)).unwrap();
            assert_eq!(resolved.service_tier, Some(ServiceTier::Priority));
        }
        let ids = source_model_union(&[AuthSource::ApiKey], None, Some(Some(&catalog)));
        assert!(ids.contains(&"sol".to_string()));
        assert!(ids.contains(&"gpt-99-sol-fast".to_string()));
        assert!(!ids.contains(&"sol-fast".to_string()));
        assert!(!ids.contains(&"anthropic-sol-ultrafast".to_string()));
    }

    #[test]
    fn discovery_uses_only_enabled_credential_catalogs() {
        let api = vec!["gpt-99-sol".into()];
        let chat = vec!["gpt-88-luna".into()];
        let ids = source_model_union(&[AuthSource::ApiKey], Some(&chat), Some(Some(&api)));
        assert!(ids.contains(&"gpt-99-sol".to_string()));
        assert!(ids.contains(&"sol".to_string()));
        assert!(!ids.contains(&"gpt-88-luna".to_string()));
        assert!(source_model_union(&[AuthSource::ApiKey], Some(&chat), None).is_empty());
        assert!(source_model_union(&[AuthSource::ApiKey], None, Some(Some(&[]))).is_empty());
        assert!(
            source_model_union(&[AuthSource::ApiKey], None, Some(None))
                .contains(&"gpt-5.5".to_string())
        );
    }
    #[tokio::test]
    async fn serve_registry_uses_live_api_catalog_and_caller_chatgpt_readiness() {
        use crate::{
            config::AliasProvider, providers::codex::source_startup::StartupInput,
            registry::Registry,
        };
        use axum::{Json, Router, routing::get};
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/v1/models",
            get(|| async { Json(serde_json::json!({"data": [{"id": "gpt-99-sol"}]})) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let registry = Registry::for_serve_with_input(
            AliasProvider::Anthropic,
            StartupInput {
                config_dir: dir.path().to_owned(),
                order: Some("chatgpt,api-key".into()),
                key: Some("toy-key".into()),
                base_url: Some(base.into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            registry.resolve_api_model("sol").unwrap().model,
            "gpt-99-sol"
        );
        assert!(registry.resolve_api_model("gpt-5.5").is_none());
        assert!(
            registry
                .source_models(None)
                .unwrap()
                .contains(&"gpt-99-sol".to_string())
        );
        let chat = vec!["gpt-88-luna".to_string()];
        assert!(
            registry
                .source_models(Some(&chat))
                .unwrap()
                .contains(&chat[0])
        );
        assert!(!registry.source_models(None).unwrap().contains(&chat[0]));
        assert!(
            Registry::new(AliasProvider::Anthropic)
                .source_models(Some(&chat))
                .is_none()
        );
        server.abort();
    }
}
