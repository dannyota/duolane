use axum::response::Response;
use http::{HeaderValue, StatusCode};
use serde_json::Value;

use super::{
    api,
    auth::manager::ObservedCredential,
    chat_completions::{ChatError, request::translate_api_request},
    source_selector::{self, Selection, SkipReason},
    sources::{CredentialObservation, SkipMark},
    translate::{model_allowlist::ResolvedModel, model_api::resolve_api_model_with_override},
};
use crate::{config::codex_auth::AuthSource, provider::RequestContext, registry::Registry};

pub(crate) async fn handle(
    registry: &Registry,
    body: &Value,
    model: &str,
    context: &RequestContext,
) -> Response {
    let Some(startup) = registry.source_startup() else {
        return authentication_error();
    };
    let api = startup.api_client();
    if let Some(client) = api {
        client.refresh_models_if_due_in_background();
    }
    let configured_model = crate::config::codex_model();
    let result = source_selector::select(
        startup.sources(),
        context,
        false,
        model,
        |source| async move {
            Ok::<_, ()>(if source == AuthSource::ApiKey {
                api.map(|client| ObservedCredential {
                    id: client.credential_id(),
                    observation: CredentialObservation(0),
                })
            } else {
                None
            })
        },
        |source, requested| {
            if source != AuthSource::ApiKey {
                return None;
            }
            let catalog = api.and_then(|client| client.catalog());
            resolve_api_model_with_override(
                requested,
                catalog.as_deref(),
                configured_model.as_deref(),
            )
            .map(ResolvedModel::from_api)
        },
        |attempt, _, resolved| async move {
            let translated = match translate_api_request(body, &resolved) {
                Ok(translated) => translated,
                Err(error) => return Ok(error.response()),
            };
            if let Some(capture) = attempt.context.traffic.as_ref() {
                capture.write_json("020-upstream-request", &translated.upstream);
            }
            if let Some(monitor) = attempt.context.monitor.as_ref() {
                monitor.model_resolved(&attempt.context.req_id, &resolved.model);
                monitor.upstream_started(&attempt.context.req_id);
            }
            let Some(api) = api else { unreachable!() };
            api::chat_completions::handle(&translated, api, &attempt).await
        },
    )
    .await;
    match &result {
        Selection::Success {
            source, fallback, ..
        }
        | Selection::Failed {
            source, fallback, ..
        } => source_selector::record_selection(context, *source, *fallback),
        Selection::Skipped { source, .. } | Selection::ObservationError { source, .. } => {
            source_selector::record_selection(context, *source, None)
        }
    }
    match result {
        Selection::Success { value, .. } => value,
        Selection::Failed { failure, .. } => failure.response.unwrap_or_else(|| {
            ChatError::new(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "OpenAI API response could not be processed",
                None,
                None,
            )
            .response()
        }),
        Selection::Skipped {
            reason: SkipReason::ModelUnavailable,
            ..
        } => ChatError::invalid(
            "Model unavailable for selected Codex source",
            Some("model"),
            None,
        )
        .response(),
        Selection::Skipped {
            reason: SkipReason::Marked(SkipMark::UsageLimit { until }),
            earliest_reset,
            ..
        } => {
            let reset = earliest_reset.unwrap_or(until);
            let mut response = ChatError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "OpenAI API quota exhausted",
                None,
                None,
            )
            .response();
            response
                .headers_mut()
                .insert("x-should-retry", HeaderValue::from_static("false"));
            response.headers_mut().insert(
                "anthropic-ratelimit-unified-status",
                HeaderValue::from_static("rejected"),
            );
            if let Some(epoch) = std::time::SystemTime::now()
                .checked_add(reset.saturating_duration_since(tokio::time::Instant::now()))
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                && let Ok(value) = HeaderValue::from_str(&epoch.to_string())
            {
                response
                    .headers_mut()
                    .insert("anthropic-ratelimit-unified-reset", value);
            }
            response
        }
        Selection::Skipped { .. } => authentication_error(),
        Selection::ObservationError { .. } => ChatError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "Unable to observe Codex credentials",
            None,
            None,
        )
        .response(),
    }
}

fn authentication_error() -> Response {
    ChatError::new(StatusCode::UNAUTHORIZED, "authentication_error", "OpenAI API key unavailable; run duolane codex auth key set; check duolane codex auth order", None, None).response()
}
