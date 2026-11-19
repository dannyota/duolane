use crate::{
    anthropic::json_error,
    logging::create_logger,
    monitor::MonitorHandle,
    providers::codex::{
        chat_completions::ChatCompletionsBackend,
        images::{CodexImagesBackend, MAX_EDIT_REQUEST_BYTES},
        native::CodexNativeBackend,
        transcription::{CodexTranscriptionBackend, MAX_TRANSCRIPTION_REQUEST_BYTES},
    },
    registry::Registry,
    session::SessionState,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::sync::Arc;

mod auto_review;
mod error_capture;
mod images;
mod listener;
mod messages;
mod openai;
mod request_monitor;
mod transcription;

use images::{handler_image_edit, handler_image_generation};
pub use listener::{
    ServerConfig, bind_proxy_listener, serve, serve_listener, serve_listeners, serve_with_shutdown,
};
use messages::{handler_count_tokens, handler_messages};
use openai::{handler_chat_completions, handler_responses};
use transcription::handler_transcription;

pub fn app(registry: Arc<Registry>) -> Router {
    app_with_features(
        registry,
        None,
        AppFeatures {
            responses_api: crate::config::codex_responses_api(),
            images_api: crate::config::codex_images_api(),
            transcriptions_api: crate::config::codex_transcriptions_api(),
        },
    )
}

pub fn app_with_monitor(registry: Arc<Registry>, monitor: Option<MonitorHandle>) -> Router {
    app_with_features(
        registry,
        monitor,
        AppFeatures {
            responses_api: crate::config::codex_responses_api(),
            images_api: crate::config::codex_images_api(),
            transcriptions_api: crate::config::codex_transcriptions_api(),
        },
    )
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AppFeatures {
    pub responses_api: bool,
    pub images_api: bool,
    pub transcriptions_api: bool,
}

pub fn app_with_options(
    registry: Arc<Registry>,
    monitor: Option<MonitorHandle>,
    responses_api: bool,
) -> Router {
    app_with_features(
        registry,
        monitor,
        AppFeatures {
            responses_api,
            images_api: false,
            transcriptions_api: false,
        },
    )
}

pub fn app_with_features(
    registry: Arc<Registry>,
    monitor: Option<MonitorHandle>,
    features: AppFeatures,
) -> Router {
    let native_responses = features
        .responses_api
        .then(|| Arc::new(CodexNativeBackend::new()));
    let chat_completions = features
        .responses_api
        .then(|| Arc::new(ChatCompletionsBackend::new()));
    let images = if features.images_api {
        match CodexImagesBackend::new() {
            Ok(backend) => Some(Arc::new(backend)),
            Err(error) => {
                create_logger("server").warn(
                    "codex images backend disabled by invalid configuration",
                    Some(Map::from_iter([("error".to_string(), json!(error))])),
                );
                None
            }
        }
    } else {
        None
    };
    let transcriptions = features
        .transcriptions_api
        .then(|| Arc::new(CodexTranscriptionBackend::new()));
    let state = Arc::new(AppState {
        registry,
        monitor,
        native_responses,
        chat_completions,
        images,
        transcriptions,
    });
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/monitor", get(handler_monitor))
        .route("/v1/messages", post(handler_messages))
        .route("/v1/messages/count_tokens", post(handler_count_tokens))
        .route("/v1/models", get(handler_models));
    let router = if features.responses_api {
        router
            .route("/v1/responses", post(handler_responses))
            .route("/v1/chat/completions", post(handler_chat_completions))
    } else {
        router
    };
    let router = if features.images_api {
        router
            .route("/v1/images/generations", post(handler_image_generation))
            .route(
                "/v1/images/edits",
                post(handler_image_edit).layer(DefaultBodyLimit::max(MAX_EDIT_REQUEST_BYTES)),
            )
    } else {
        router
    };
    let router = if features.transcriptions_api {
        router.route(
            "/v1/audio/transcriptions",
            post(handler_transcription)
                .layer(DefaultBodyLimit::max(MAX_TRANSCRIPTION_REQUEST_BYTES)),
        )
    } else {
        router
    };
    router.fallback(fallback_handler).with_state(state)
}

#[derive(Clone)]
struct AppState {
    registry: Arc<Registry>,
    monitor: Option<MonitorHandle>,
    native_responses: Option<Arc<CodexNativeBackend>>,
    chat_completions: Option<Arc<ChatCompletionsBackend>>,
    images: Option<Arc<CodexImagesBackend>>,
    transcriptions: Option<Arc<CodexTranscriptionBackend>>,
}

async fn handler_monitor(
    State(state): State<Arc<AppState>>,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
) -> Response {
    let Some(axum::Extension(axum::extract::ConnectInfo(peer))) = peer else {
        return json_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Monitor access requires a local connection",
        );
    };
    if !peer.ip().to_canonical().is_loopback() {
        return json_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Monitor access requires a local connection",
        );
    }
    match &state.monitor {
        Some(monitor) => (
            [(http::header::CACHE_CONTROL, "no-store")],
            Json(crate::monitor::snapshot::MonitorResponse::from(
                monitor.snapshot(),
            )),
        )
            .into_response(),
        None => json_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Monitor collection is disabled",
        ),
    }
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

#[derive(serde::Deserialize)]
struct ModelsQuery {
    limit: Option<usize>,
}

async fn handler_models(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ModelsQuery>,
) -> Json<serde_json::Value> {
    let mut data: Vec<Value> = state
        .registry
        .all_supported_models()
        .into_iter()
        .map(|(model, provider)| {
            json!({
                "type": "model",
                "object": "model",
                "id": model,
                "display_name": format!("{model} ({provider})"),
            })
        })
        .collect();
    let has_more = query.limit.is_some_and(|limit| data.len() > limit);
    if let Some(limit) = query.limit {
        data.truncate(limit);
    }
    Json(json!({
        "object": "list",
        "data": data,
        "has_more": has_more,
        "first_id": data.first().and_then(|entry| entry.get("id")).cloned(),
        "last_id": data.last().and_then(|entry| entry.get("id")).cloned(),
    }))
}

fn parse_json_body<T>(body: &[u8]) -> Result<T, Box<Response>>
where
    T: DeserializeOwned,
{
    if body.is_empty() {
        return Err(Box::new(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Invalid JSON: empty body",
        )));
    }

    serde_json::from_slice::<T>(body).map_err(|err| {
        Box::new(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("Invalid JSON: {err}"),
        ))
    })
}

async fn fallback_handler(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    json_error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("No route for {method} {}", uri.path()),
    )
}

fn current_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[allow(dead_code)]
fn _unused(session_state: Option<&SessionState>) {
    let _ = session_state;
}
