use std::convert::Infallible;

use axum::response::Response;
use http::StatusCode;

use super::{
    api,
    auth::manager::ObservedCredential,
    error_response::map_codex_error_to_response,
    search,
    source_selector::{self, AttemptFailure, Selection, SkipReason},
    sources::{CredentialObservation, SkipMark},
    translate::{model_allowlist::ResolvedModel, request_tools::has_hosted_web_search},
};
use crate::{
    anthropic::{error::json_error, schema::MessagesRequest},
    provider::RequestContext,
    registry::Registry,
    request_identity::ConversationIdentity,
};

pub(crate) async fn handle(
    registry: &Registry,
    body: &MessagesRequest,
    context: &RequestContext,
    identity: Option<&ConversationIdentity>,
) -> Response {
    let Some(startup) = registry.source_startup() else {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "OpenAI API key unavailable; run duolane codex auth key set",
        );
    };
    let api = startup.api_client();
    let model = body.model.as_deref().unwrap_or("opus");
    let configured_model = (!body.bypass_provider_model_override)
        .then(crate::config::codex_model)
        .flatten();
    let hosted = has_hosted_web_search(body) || search::is_standalone_search_request(body);
    let result = source_selector::select(
        startup.sources(),
        context,
        hosted,
        model,
        |_| async {
            Ok::<_, Infallible>(api.map(|client| ObservedCredential {
                id: client.credential_id(),
                observation: CredentialObservation(0),
            }))
        },
        |_, requested| {
            let catalog = api.and_then(|client| client.catalog());
            super::translate::model_api::resolve_api_model_with_override(
                requested,
                catalog.as_deref(),
                configured_model.as_deref(),
            )
            .map(ResolvedModel::from_api)
        },
        |attempt, _, resolved| async move {
            let Some(api) = api else { unreachable!() };
            let result = if search::is_standalone_search_request(body) {
                api::search::handle(body, identity, &resolved, api, &attempt).await
            } else {
                api::messages::handle(body, identity, &resolved, api, &attempt).await
            };
            result.map_err(|failure| AttemptFailure {
                failure,
                response: None::<Response>,
            })
        },
    )
    .await;
    match result {
        Selection::Success { value, .. } => value,
        Selection::Failed { failure, .. } => failure
            .response
            .unwrap_or_else(|| map_codex_error_to_response(&failure.failure.error)),
        Selection::Skipped {
            reason: SkipReason::ModelUnavailable,
            ..
        } => json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Model unavailable for OpenAI API",
        ),
        Selection::Skipped {
            reason: SkipReason::Marked(SkipMark::UsageLimit { until }),
            earliest_reset,
            ..
        } => quota_response(earliest_reset.unwrap_or(until)),
        Selection::Skipped { .. } => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "OpenAI API key unavailable; run duolane codex auth key set",
        ),
        Selection::ObservationError(err) => match err {},
    }
}

fn quota_response(until: tokio::time::Instant) -> Response {
    let resets_at = std::time::SystemTime::now()
        .checked_add(until.saturating_duration_since(tokio::time::Instant::now()))
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    super::error_response::usage_limit_response(&super::events::CodexUsageLimit {
        message: "OpenAI API quota exhausted".into(),
        resets_at,
        window: None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn cached_quota_includes_reset_headers_without_window() {
        let response = super::quota_response(
            tokio::time::Instant::now() + std::time::Duration::from_secs(3600),
        );
        assert_eq!(response.status(), http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get("x-should-retry")
                .and_then(|h| h.to_str().ok()),
            Some("false")
        );
        assert_eq!(
            response
                .headers()
                .get("anthropic-ratelimit-unified-status")
                .and_then(|h| h.to_str().ok()),
            Some("rejected")
        );
        assert!(
            response
                .headers()
                .contains_key("anthropic-ratelimit-unified-reset")
        );
        assert!(
            !response
                .headers()
                .contains_key("anthropic-ratelimit-unified-representative-claim")
        );
    }
}
