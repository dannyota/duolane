use std::{ffi::OsString, path::PathBuf, sync::Arc};

use super::{api::ApiClient, auth::api_key::KeyStore, sources::CodexSources};
use crate::config::codex_auth::{self, AuthSource};

pub struct StartupInput {
    pub config_dir: PathBuf,
    pub order: Option<OsString>,
    pub key: Option<OsString>,
    pub base_url: Option<OsString>,
}

impl StartupInput {
    pub fn from_environment() -> Self {
        Self {
            config_dir: crate::paths::config_dir(),
            order: std::env::var_os("DUOLANE_CODEX_AUTH_ORDER"),
            key: std::env::var_os("DUOLANE_OPENAI_API_KEY"),
            base_url: std::env::var_os("DUOLANE_OPENAI_BASE_URL"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupWarning {
    ApiKeyInactive,
    ApiKeyUnavailable,
}

pub struct SourceStartup {
    sources: CodexSources,
    api_client: Option<Arc<ApiClient>>,
    warnings: Vec<StartupWarning>,
}

impl SourceStartup {
    pub async fn build(input: StartupInput) -> anyhow::Result<Self> {
        let (order, api_client, warnings) = tokio::task::spawn_blocking(move || {
            let mut snapshot = if input.order.is_none() {
                Some(codex_auth::read_config_snapshot(&input.config_dir)?)
            } else {
                None
            };
            let order = codex_auth::order_from_snapshot(snapshot.as_ref(), input.order.as_deref())?;
            let enabled = order.sources().contains(&AuthSource::ApiKey);
            let key = KeyStore::new(input.config_dir.clone()).load(input.key.as_deref());
            let mut warnings = Vec::new();
            let key = match key {
                Ok((Some(key), _)) if enabled => Some(key),
                Ok((Some(_), _)) => {
                    warnings.push(StartupWarning::ApiKeyInactive);
                    None
                }
                Ok((None, _)) if !enabled => None,
                _ => {
                    warnings.push(StartupWarning::ApiKeyUnavailable);
                    None
                }
            };
            let api_client = if let Some(key) = key {
                if snapshot.is_none() && input.base_url.is_none() {
                    snapshot = Some(codex_auth::read_config_snapshot(&input.config_dir)?);
                }
                let base = codex_auth::openai_base_url_from_snapshot(
                    snapshot.as_ref(),
                    input.base_url.as_deref(),
                )?;
                match ApiClient::new(base, key) {
                    Ok(client) => Some(Arc::new(client)),
                    Err(_) => {
                        warnings.push(StartupWarning::ApiKeyUnavailable);
                        None
                    }
                }
            } else {
                None
            };
            Ok::<_, anyhow::Error>((order, api_client, warnings))
        })
        .await
        .map_err(|_| anyhow::anyhow!("source startup failed"))??;
        if let Some(client) = &api_client {
            client.refresh_models().await;
        }
        Ok(Self {
            sources: CodexSources::new(order),
            api_client,
            warnings,
        })
    }

    pub fn sources(&self) -> &CodexSources {
        &self.sources
    }
    pub fn api_client(&self) -> Option<&Arc<ApiClient>> {
        self.api_client.as_ref()
    }
    pub fn warnings(&self) -> &[StartupWarning] {
        &self.warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn startup_respects_opt_in_and_validates_before_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let app = Router::new().route(
            "/v1/models",
            get(move || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({"data": []}))
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let input = |order: Option<&str>, key: Option<&str>| StartupInput {
            config_dir: dir.path().to_owned(),
            order: order.map(Into::into),
            key: key.map(Into::into),
            base_url: Some(base.clone().into()),
        };
        let err = match SourceStartup::build(input(Some("bogus"), Some("toy-key"))).await {
            Ok(_) => panic!("invalid order accepted"),
            Err(err) => err,
        };
        assert_eq!(err.to_string(), "invalid DUOLANE_CODEX_AUTH_ORDER");
        let inactive = SourceStartup::build(input(None, Some("toy-key")))
            .await
            .unwrap();
        assert!(inactive.api_client().is_none());
        assert!(
            inactive
                .warnings()
                .contains(&StartupWarning::ApiKeyInactive)
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let active = SourceStartup::build(input(Some("api-key"), Some("toy-key")))
            .await
            .unwrap();
        assert!(active.api_client().is_some());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let missing = SourceStartup::build(input(Some("api-key"), None))
            .await
            .unwrap();
        assert!(missing.api_client().is_none());
        assert!(
            missing
                .warnings()
                .contains(&StartupWarning::ApiKeyUnavailable)
        );
        let invalid = SourceStartup::build(input(Some("api-key"), Some("\n")))
            .await
            .unwrap();
        assert!(invalid.api_client().is_none());
        assert!(
            invalid
                .warnings()
                .contains(&StartupWarning::ApiKeyUnavailable)
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        server.abort();
    }
    #[tokio::test]
    async fn startup_rejects_invalid_selected_url_and_ignores_inactive_url() {
        let dir = tempfile::tempdir().unwrap();
        let input = |order: Option<&str>| StartupInput {
            config_dir: dir.path().to_owned(),
            order: order.map(Into::into),
            key: Some("toy-key".into()),
            base_url: Some("http://example.com/secret".into()),
        };
        let err = match SourceStartup::build(input(Some("api-key"))).await {
            Ok(_) => panic!("invalid url accepted"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("DUOLANE_OPENAI_BASE_URL"));
        assert!(!err.to_string().contains("example.com"));
        assert!(SourceStartup::build(input(None)).await.is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_skips_unsafe_stored_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("credentials");
        std::fs::create_dir(&credentials).unwrap();
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o777)).unwrap();
        let startup = SourceStartup::build(StartupInput {
            config_dir: dir.path().to_owned(),
            order: Some("api-key".into()),
            key: None,
            base_url: None,
        })
        .await
        .unwrap();
        assert!(startup.api_client().is_none());
        assert!(
            startup
                .warnings()
                .contains(&StartupWarning::ApiKeyUnavailable)
        );
    }
    #[tokio::test]
    async fn registry_keeps_constructed_source_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::registry::Registry::for_serve_with_input(
            crate::config::AliasProvider::Anthropic,
            StartupInput {
                config_dir: dir.path().to_owned(),
                order: Some("api-key,chatgpt".into()),
                key: None,
                base_url: None,
            },
        )
        .await
        .unwrap();
        let state = registry.source_startup().unwrap();
        assert_eq!(
            state.sources().order().sources(),
            &[AuthSource::ApiKey, AuthSource::ChatGpt]
        );
        assert!(state.api_client().is_none());
        assert!(
            state
                .warnings()
                .contains(&StartupWarning::ApiKeyUnavailable)
        );
    }
    #[tokio::test]
    async fn startup_ignores_malformed_file_when_all_needed_settings_are_in_environment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), b"{").unwrap();
        let state = SourceStartup::build(StartupInput {
            config_dir: dir.path().to_owned(),
            order: Some("chatgpt".into()),
            key: Some("toy-key".into()),
            base_url: Some("invalid".into()),
        })
        .await
        .unwrap();
        assert!(state.warnings().contains(&StartupWarning::ApiKeyInactive));
        let input = |base_url: Option<&str>| StartupInput {
            config_dir: dir.path().to_owned(),
            order: Some("api-key".into()),
            key: Some("toy-key".into()),
            base_url: base_url.map(Into::into),
        };
        let err = match SourceStartup::build(input(Some("invalid"))).await {
            Ok(_) => panic!("invalid url accepted"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("DUOLANE_OPENAI_BASE_URL"));
        let err = match SourceStartup::build(input(None)).await {
            Ok(_) => panic!("invalid config accepted"),
            Err(err) => err,
        };
        assert_eq!(err.to_string(), "invalid config.json");
    }

    #[test]
    fn snapshot_keeps_order_and_url_together_after_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"codex":{"authOrder":["api-key"],"apiBaseUrl":"http://127.0.0.1:1111/v1"}}"#,
        )
        .unwrap();
        let snapshot = codex_auth::read_config_snapshot(dir.path()).unwrap();
        std::fs::write(
            &path,
            r#"{"codex":{"authOrder":["chatgpt"],"apiBaseUrl":"http://127.0.0.1:2222/v1"}}"#,
        )
        .unwrap();
        let order = codex_auth::order_from_snapshot(Some(&snapshot), None).unwrap();
        let url = codex_auth::openai_base_url_from_snapshot(Some(&snapshot), None).unwrap();
        assert_eq!(order.sources(), &[AuthSource::ApiKey]);
        assert_eq!(url.port(), Some(1111));
    }
}
