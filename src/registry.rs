use crate::{config::AliasProvider, provider::Provider};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const ANTHROPIC_STYLE_ALIASES: &[&str] = &[
    "haiku",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "claude-haiku-5-5",
    "sonnet",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "claude-sonnet-5-5",
    "opus",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-opus-5-5",
    "fable",
    "claude-fable-5",
    "claude-fable-5-1",
];

pub struct Registry {
    alias_provider: AliasProvider,
    source_startup: Option<Arc<crate::providers::codex::source_startup::SourceStartup>>,
    dynamic_codex: bool,
    codex_provider: Option<Arc<crate::providers::codex::CodexProvider>>,
    models: BTreeMap<String, Vec<String>>,
    handlers: BTreeMap<String, Arc<dyn Provider>>,
}

impl Registry {
    pub async fn for_serve(alias_provider: AliasProvider) -> anyhow::Result<Self> {
        Self::for_serve_with_input(
            alias_provider,
            crate::providers::codex::source_startup::StartupInput::from_environment(),
        )
        .await
    }

    pub async fn for_serve_with_input(
        alias_provider: AliasProvider,
        input: crate::providers::codex::source_startup::StartupInput,
    ) -> anyhow::Result<Self> {
        let startup = crate::providers::codex::source_startup::SourceStartup::build(input).await?;
        let mut registry = tokio::task::spawn_blocking(move || Self::new(alias_provider))
            .await
            .map_err(|_| anyhow::anyhow!("registry startup failed"))?;
        registry.source_startup = Some(Arc::new(startup));
        Ok(registry)
    }

    pub(crate) fn codex_provider(&self) -> Option<&Arc<crate::providers::codex::CodexProvider>> {
        self.codex_provider.as_ref()
    }

    pub fn source_startup(
        &self,
    ) -> Option<&Arc<crate::providers::codex::source_startup::SourceStartup>> {
        self.source_startup.as_ref()
    }

    pub fn resolve_api_model(
        &self,
        requested: &str,
    ) -> Option<crate::providers::codex::translate::model_api::ApiResolvedModel> {
        let startup = self.source_startup.as_ref()?;
        let api = startup.api_client()?;
        let catalog = api.catalog();
        crate::providers::codex::translate::model_api::resolve_api_model(
            requested,
            catalog.as_deref(),
        )
    }

    pub fn source_models(&self, chatgpt: Option<&[String]>) -> Option<Vec<String>> {
        let startup = self.source_startup.as_ref()?;
        let catalog = startup.api_client().and_then(|api| api.catalog());
        let api = startup.api_client().map(|_| catalog.as_deref());
        Some(
            crate::providers::codex::translate::model_api::source_model_union(
                startup.sources().order().sources(),
                chatgpt,
                api,
            ),
        )
    }

    pub fn new(alias_provider: AliasProvider) -> Self {
        let mut models: BTreeMap<String, Vec<String>> = BTreeMap::new();
        models.insert(
            "anthropic".into(),
            ANTHROPIC_STYLE_ALIASES
                .iter()
                .map(|alias| (*alias).to_string())
                .collect(),
        );
        models.insert(
            "codex".into(),
            crate::providers::codex::translate::model_allowlist::listed_models(),
        );
        let mut handlers: BTreeMap<String, Arc<dyn Provider>> = BTreeMap::new();
        handlers.insert(
            "anthropic".into(),
            Arc::new(crate::providers::anthropic::AnthropicProvider::new()),
        );
        let codex_provider = Arc::new(crate::providers::codex::CodexProvider::new());
        handlers.insert("codex".into(), codex_provider.clone());

        Self {
            alias_provider,
            source_startup: None,
            dynamic_codex: true,
            codex_provider: Some(codex_provider),
            models,
            handlers,
        }
    }

    pub fn with_default_alias() -> Self {
        Self::new(crate::config::alias_provider())
    }

    pub fn from_providers(
        alias_provider: AliasProvider,
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
    ) -> Self {
        let mut models = BTreeMap::new();
        let mut handlers = BTreeMap::new();
        for provider in providers {
            let name = provider.name().to_string();
            models.insert(name.clone(), provider.supported_models());
            handlers.insert(name, provider);
        }
        Self {
            alias_provider,
            source_startup: None,
            dynamic_codex: false,
            codex_provider: None,
            models,
            handlers,
        }
    }

    pub fn list_provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    pub fn provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.handlers.get(name).cloned()
    }

    pub fn supported_models_for(&self, provider: &str) -> Vec<String> {
        let mut models = if self.dynamic_codex && provider == "codex" {
            crate::providers::codex::translate::model_allowlist::listed_models()
        } else {
            self.models.get(provider).cloned().unwrap_or_default()
        };
        if provider == self.alias_provider.as_str() {
            for alias in ANTHROPIC_STYLE_ALIASES {
                if !models.iter().any(|value| value == alias) {
                    models.push((*alias).to_string());
                }
            }
        }
        models.sort_unstable();
        models
    }

    pub fn signed_out(&self) -> BTreeMap<&str, &'static str> {
        self.handlers
            .iter()
            .filter_map(|(name, provider)| Some((name.as_str(), provider.missing_login_hint()?)))
            .collect()
    }

    pub async fn discovery_models(&self) -> Vec<(String, String)> {
        use crate::config::codex_auth::AuthSource;
        use crate::providers::codex::translate::{model_allowlist::MODEL_ALIASES, model_api};

        let Some(startup) = &self.source_startup else {
            return self.all_supported_models();
        };
        if !startup
            .sources()
            .order()
            .sources()
            .contains(&AuthSource::ApiKey)
        {
            return self.all_supported_models();
        }
        if let Some(api) = startup.api_client() {
            api.refresh_models().await;
        }
        let chat_models = if startup
            .sources()
            .order()
            .sources()
            .contains(&AuthSource::ChatGpt)
            && let Some(provider) = &self.codex_provider
            && provider
                .current_credential_snapshot()
                .await
                .ok()
                .flatten()
                .is_some()
        {
            tokio::task::spawn_blocking(
                crate::providers::codex::translate::model_allowlist::listed_models,
            )
            .await
            .ok()
        } else {
            None
        };
        let catalog = startup.api_client().and_then(|api| api.catalog());
        let api = startup.api_client().map(|_| catalog.as_deref());
        let codex = model_api::source_model_union(
            startup.sources().order().sources(),
            chat_models.as_deref(),
            api,
        );
        let mut out = Vec::new();
        for provider in self.handlers.keys() {
            let models = if provider == "codex" {
                let mut models = codex.clone();
                if self.alias_provider == AliasProvider::Codex {
                    models.extend(
                        ANTHROPIC_STYLE_ALIASES
                            .iter()
                            .filter(|alias| {
                                let chat_available = chat_models.as_ref().is_some_and(|chat| {
                                    MODEL_ALIASES
                                        .iter()
                                        .find(|(name, _)| name == *alias)
                                        .is_some_and(|(_, targets)| {
                                            targets
                                                .iter()
                                                .any(|target| chat.iter().any(|id| id == target))
                                        })
                                });
                                chat_available
                                    || api.is_some_and(|catalog| {
                                        model_api::resolve_api_model(alias, catalog).is_some()
                                    })
                            })
                            .map(|alias| (*alias).to_string()),
                    );
                    models.sort_unstable();
                    models.dedup();
                }
                models
            } else {
                self.supported_models_for(provider)
            };
            out.extend(models.into_iter().map(|model| (model, provider.clone())));
        }
        out
    }

    pub fn translated_provider_for_model(
        &self,
        raw_model: &str,
        session_affinity: Option<&AliasProvider>,
    ) -> Option<Arc<dyn Provider>> {
        self.provider_for_model(raw_model, session_affinity)
            .or_else(|| {
                let startup = self.source_startup.as_ref()?;
                if !startup
                    .sources()
                    .order()
                    .sources()
                    .contains(&crate::config::codex_auth::AuthSource::ApiKey)
                {
                    return None;
                }
                crate::providers::codex::translate::model_api::resolve_api_model(raw_model, None)?;
                self.handlers.get("codex").cloned()
            })
    }

    pub fn all_supported_models(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for provider in self.handlers.keys() {
            for model in self.supported_models_for(provider) {
                out.push((model, provider.clone()));
            }
        }
        out
    }

    pub fn grouped_models(&self) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for provider in self.handlers.keys() {
            out.insert(provider.clone(), self.supported_models_for(provider));
        }
        out
    }

    pub fn provider_for_model(
        &self,
        raw_model: &str,
        session_affinity: Option<&AliasProvider>,
    ) -> Option<Arc<dyn Provider>> {
        let normalized = normalize_incoming_model(raw_model);
        let _ = session_affinity;
        if is_anthropic_alias(&normalized) || normalized.starts_with("claude-") {
            return self.handlers.get(self.alias_provider.as_str()).cloned();
        }
        for (name, models) in &self.models {
            if !(self.dynamic_codex && name == "codex")
                && models.iter().any(|candidate| candidate == &normalized)
            {
                return self.handlers.get(name).cloned();
            }
        }

        if self.dynamic_codex
            && crate::providers::codex::translate::model_allowlist::is_valid_model_for_codex(
                &normalized,
            )
        {
            return self.handlers.get("codex").cloned();
        }

        None
    }

    pub async fn unknown_model_message(self: &Arc<Self>) -> String {
        self.unknown_model_message_off_thread(false).await
    }

    pub async fn unknown_codex_model_message(self: &Arc<Self>) -> String {
        self.unknown_model_message_off_thread(true).await
    }

    async fn unknown_model_message_off_thread(self: &Arc<Self>, codex_only: bool) -> String {
        let registry = self.clone();
        tokio::task::spawn_blocking(move || {
            registry.unknown_model_message_for(|provider| !codex_only || provider == "codex")
        })
        .await
        .unwrap_or_default()
    }

    fn unknown_model_message_for(&self, include: impl Fn(&str) -> bool) -> String {
        let signed_out = self.signed_out();
        let mut parts = Vec::new();
        for (provider, models) in self.grouped_models() {
            if !include(&provider) || signed_out.contains_key(provider.as_str()) {
                continue;
            }
            let mut models = models;
            models.sort_unstable();
            parts.push(format!("{}: {}", provider, models.join(", ")));
        }
        let hints: Vec<String> = signed_out
            .iter()
            .filter(|(provider, _)| include(provider))
            .map(|(provider, hint)| format!("{provider} ({hint})"))
            .collect();
        let mut sentences = Vec::new();
        if !parts.is_empty() {
            sentences.push(format!("Supported: {}.", parts.join("; ")));
        }
        if !hints.is_empty() {
            sentences.push(format!("Not signed in: {}.", hints.join("; ")));
        }
        sentences.join(" ")
    }
}

pub fn normalize_incoming_model(model: &str) -> String {
    let suffix = "[1m]";
    if model.len() >= suffix.len() && model.to_ascii_lowercase().ends_with(suffix) {
        return model[..model.len() - suffix.len()].to_string();
    }
    model.to_string()
}

pub fn is_anthropic_alias(model: &str) -> bool {
    ANTHROPIC_STYLE_ALIASES.contains(&model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        anthropic::schema::MessagesRequest,
        provider::{CliHandlers, RequestContext},
    };
    use async_trait::async_trait;
    use axum::{http::StatusCode, response::IntoResponse, response::Response};

    struct NoCli;

    impl CliHandlers for NoCli {
        fn login(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn device(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn status(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn logout(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    static NO_CLI: NoCli = NoCli;

    struct ToyProvider {
        name: &'static str,
        model: &'static str,
        login_hint: Option<&'static str>,
    }

    #[async_trait]
    impl Provider for ToyProvider {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supported_models(&self) -> Vec<String> {
            vec![self.model.to_string()]
        }

        fn cli(&self) -> &'static dyn CliHandlers {
            &NO_CLI
        }

        fn missing_login_hint(&self) -> Option<&'static str> {
            self.login_hint
        }

        async fn handle_messages(&self, _body: MessagesRequest, _ctx: RequestContext) -> Response {
            StatusCode::NOT_IMPLEMENTED.into_response()
        }

        async fn handle_count_tokens(
            &self,
            _body: MessagesRequest,
            _ctx: RequestContext,
        ) -> Response {
            StatusCode::NOT_IMPLEMENTED.into_response()
        }
    }

    #[test]
    fn registry_has_exactly_the_anthropic_and_codex_backends() {
        let registry = Registry::new(AliasProvider::Anthropic);
        assert_eq!(registry.list_provider_names(), ["anthropic", "codex"]);
    }

    #[tokio::test]
    async fn unknown_codex_model_message_lists_only_codex() {
        let provider = |login_hint| {
            Arc::new(ToyProvider {
                name: "codex",
                model: "gpt-toy",
                login_hint,
            }) as Arc<dyn Provider>
        };
        let anthropic = || {
            Arc::new(ToyProvider {
                name: "anthropic",
                model: "claude-toy",
                login_hint: None,
            }) as Arc<dyn Provider>
        };

        let signed_in = Arc::new(Registry::from_providers(
            AliasProvider::Anthropic,
            [anthropic(), provider(None)],
        ));
        assert_eq!(
            signed_in.unknown_codex_model_message().await,
            "Supported: codex: gpt-toy."
        );

        let signed_out = Arc::new(Registry::from_providers(
            AliasProvider::Anthropic,
            [anthropic(), provider(Some("run `codex login`"))],
        ));
        assert_eq!(
            signed_out.unknown_codex_model_message().await,
            "Not signed in: codex (run `codex login`)."
        );
    }

    #[tokio::test]
    async fn signed_out_backends_are_named_instead_of_listed() {
        let registry = Arc::new(Registry::from_providers(
            AliasProvider::Anthropic,
            [
                Arc::new(ToyProvider {
                    name: "anthropic",
                    model: "claude-toy",
                    login_hint: None,
                }) as Arc<dyn Provider>,
                Arc::new(ToyProvider {
                    name: "codex",
                    model: "gpt-toy",
                    login_hint: Some("run `codex login`"),
                }),
            ],
        ));

        assert_eq!(
            registry.signed_out(),
            BTreeMap::from([("codex", "run `codex login`")])
        );
        assert_eq!(
            registry.provider_for_model("gpt-toy", None).unwrap().name(),
            "codex"
        );
        let message = registry.unknown_model_message().await;
        assert!(message.contains("claude-toy") && !message.contains("gpt-toy"));
        assert!(message.ends_with("Not signed in: codex (run `codex login`)."));
    }

    #[test]
    fn normalize_model_trims_hint() {
        assert_eq!(normalize_incoming_model("gpt-5.4-fast[1m]"), "gpt-5.4-fast");
        assert_eq!(normalize_incoming_model("gpt-5.4-fast"), "gpt-5.4-fast");
        assert_eq!(
            normalize_incoming_model("gpt-6-astra-ultrafast[1m]"),
            "gpt-6-astra-ultrafast"
        );
    }

    #[test]
    fn ultrafast_variant_routes_to_codex_only_where_offered() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "gpt-6-astra-ultrafast",
            "gpt-6-astra-ultrafast[1m]",
            "gpt-6.1-sol-ultrafast",
        ] {
            assert_eq!(
                registry.provider_for_model(model, None).unwrap().name(),
                "codex"
            );
        }
        for model in ["gpt-6-sol-ultrafast", "gpt-6-luna-ultrafast"] {
            assert!(
                registry.provider_for_model(model, None).is_none(),
                "{model}"
            );
        }
    }

    #[test]
    fn alias_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        let p = registry.provider_for_model("haiku", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn opus_4_8_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        let p = registry.provider_for_model("claude-opus-4-8", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn claude_5_aliases_route_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "claude-sonnet-5",
            "claude-sonnet-5-5",
            "claude-opus-5",
            "claude-opus-5-5",
            "fable",
            "claude-fable-5",
            "claude-fable-5-1",
        ] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route to a provider");
            assert_eq!(p.expect("provider").name(), "codex");
        }
    }

    #[test]
    fn claude_models_route_to_anthropic_passthrough_by_default() {
        let registry = Registry::new(AliasProvider::Anthropic);
        for model in [
            "opus",
            "claude-opus-4-8",
            "sonnet",
            "haiku",
            "claude-3-5-haiku-20241022",
        ] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route");
            assert_eq!(p.expect("provider").name(), "anthropic", "{model}");
        }
    }

    #[test]
    fn explicit_codex_model_routes_to_codex_while_default_is_anthropic() {
        let registry = Registry::new(AliasProvider::Anthropic);
        let p = registry.provider_for_model("gpt-5.6-terra", None);
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn session_affinity_cannot_hijack_claude_slot() {
        let registry = Registry::new(AliasProvider::Anthropic);
        let p = registry.provider_for_model("claude-opus-4-8", Some(&AliasProvider::Codex));
        assert_eq!(p.expect("provider").name(), "anthropic");
    }

    #[test]
    fn gpt_6_models_route_to_codex() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "gpt-6-sol",
            "gpt-6-sol-fast",
            "gpt-6-luna",
            "gpt-6.1-sol",
            "gpt-6.1-sol-fast",
            "gpt-6.1-terra",
            "gpt-6.1-terra-fast",
        ] {
            assert_eq!(
                registry.provider_for_model(model, None).unwrap().name(),
                "codex"
            );
        }
    }
}
