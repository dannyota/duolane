use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::Response,
};
use tokio::sync::Semaphore;
use tower::ServiceExt;

use super::{
    AppFeatures, app_for_profile,
    openai::native_session_id,
    request_monitor::{monitor_failed, with_request_id},
};
use crate::{
    anthropic::json_error,
    monitor::{EndpointKind, MonitorHandle},
    providers::codex::{
        auth::token_store::file_store_at,
        client::CodexHttpClient,
        native::openai_error,
        profiles::{Profiles, SELECTION_ERROR},
        translate::model_catalog::{CacheState, REQUEST_CATALOG, RequestCatalog},
    },
    registry::Registry,
};

struct ProfileApp {
    router: Router,
    catalog_path: PathBuf,
    catalog_cache: Mutex<CacheState>,
}

struct ProfileApps {
    profiles: anyhow::Result<Profiles>,
    apps: BTreeMap<String, Arc<ProfileApp>>,
    fallback: Router,
    monitor: Option<MonitorHandle>,
    features: AppFeatures,
}

pub(super) fn app(
    registry: Arc<Registry>,
    monitor: Option<MonitorHandle>,
    features: AppFeatures,
) -> Router {
    let image_limiter = Arc::new(Semaphore::new(2));
    let transcription_limiter = Arc::new(Semaphore::new(4));
    let fallback = app_for_profile(
        registry.clone(),
        monitor.clone(),
        features,
        Arc::new(CodexHttpClient::new()),
        image_limiter.clone(),
        transcription_limiter.clone(),
    );
    if !registry.uses_default_codex() {
        return fallback;
    }

    let profiles = Profiles::load();
    let mut apps = BTreeMap::new();
    if let Ok(profiles) = &profiles {
        for name in profiles.names() {
            if let Some(auth_path) = profiles.auth_path(name) {
                let catalog_path = auth_path.with_file_name("models_cache.json");
                let scope = Profiles::account_scope(name, &auth_path);
                let client = Arc::new(CodexHttpClient::with_store(file_store_at(auth_path), scope));
                let router = app_for_profile(
                    Arc::new(registry.with_codex_client(client.clone())),
                    monitor.clone(),
                    features,
                    client,
                    image_limiter.clone(),
                    transcription_limiter.clone(),
                );
                apps.insert(
                    name.to_owned(),
                    Arc::new(ProfileApp {
                        router,
                        catalog_path,
                        catalog_cache: Mutex::new(CacheState::default()),
                    }),
                );
            }
        }
    }
    let state = Arc::new(ProfileApps {
        profiles,
        apps,
        fallback,
        monitor,
        features,
    });
    Router::new().fallback(dispatch).with_state(state)
}

impl ProfileApps {
    fn snapshot(&self) -> Option<(Router, RequestCatalog)> {
        let profiles = self.profiles.as_ref().ok()?;
        let selected = profiles.active().ok()?;
        let app = self.apps.get(&selected)?;
        let catalog = RequestCatalog::load(
            &app.catalog_path,
            &app.catalog_cache,
            profiles.has_named_profiles(),
        );
        Some((app.router.clone(), catalog))
    }

    fn endpoint(&self, req: &Request) -> Endpoint {
        match (req.method(), req.uri().path()) {
            (&Method::GET | &Method::HEAD, "/v1/models") => Endpoint::Models,
            (&Method::POST, "/v1/messages" | "/v1/messages/count_tokens") => Endpoint::Messages,
            (&Method::POST, "/v1/responses") if self.features.responses_api => {
                Endpoint::OpenAi(EndpointKind::Responses)
            }
            (&Method::POST, "/v1/chat/completions") if self.features.responses_api => {
                Endpoint::OpenAi(EndpointKind::ChatCompletions)
            }
            (&Method::POST, "/v1/images/generations" | "/v1/images/edits")
                if self.features.images_api =>
            {
                Endpoint::OpenAi(EndpointKind::Images)
            }
            (&Method::POST, "/v1/audio/transcriptions") if self.features.transcriptions_api => {
                Endpoint::OpenAi(EndpointKind::Transcriptions)
            }
            _ => Endpoint::Other,
        }
    }
}

enum Endpoint {
    Messages,
    Models,
    OpenAi(EndpointKind),
    Other,
}

async fn dispatch(State(apps): State<Arc<ProfileApps>>, req: Request) -> Response {
    let endpoint = apps.endpoint(&req);
    if matches!(endpoint, Endpoint::Other) {
        return run(apps.fallback.clone(), req).await;
    }
    let snapshot = {
        let apps = apps.clone();
        tokio::task::spawn_blocking(move || apps.snapshot())
            .await
            .ok()
            .flatten()
    };
    match (snapshot, endpoint) {
        (Some((router, catalog)), _) => REQUEST_CATALOG.scope(catalog, run(router, req)).await,
        (None, Endpoint::OpenAi(kind)) => {
            invalid_openai_selection(apps.monitor.as_ref(), kind, &req)
        }
        (None, Endpoint::Models) => invalid_selection_response(),
        (None, Endpoint::Messages | Endpoint::Other) => {
            REQUEST_CATALOG
                .scope(
                    RequestCatalog::InvalidSelection,
                    run(apps.fallback.clone(), req),
                )
                .await
        }
    }
}

fn invalid_openai_selection(
    monitor: Option<&MonitorHandle>,
    kind: EndpointKind,
    req: &Request,
) -> Response {
    let req_id = uuid::Uuid::new_v4().to_string();
    if let Some(monitor) = monitor {
        monitor.request_started(&req_id, native_session_id(req.headers()), None, kind);
    }
    let status = StatusCode::SERVICE_UNAVAILABLE;
    monitor_failed(monitor, &req_id, Some(status), SELECTION_ERROR);
    let response = openai_error(
        status,
        "api_error",
        SELECTION_ERROR,
        None,
        Some("codex_profile_invalid"),
    );
    with_request_id(response, &req_id)
}

pub(super) fn invalid_selection_response() -> Response {
    json_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "api_error",
        SELECTION_ERROR,
    )
}

async fn run(router: Router, req: Request) -> Response {
    match router.oneshot(req).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}
