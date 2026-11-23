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
    config::codex_auth::AuthSource,
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
    let Some(chatgpt) = registry.codex_provider() else {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "Codex source unavailable",
        );
    };
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
        |source| async move {
            match source {
                AuthSource::ChatGpt => chatgpt.current_credential_snapshot().await,
                AuthSource::ApiKey => Ok(api.map(|client| ObservedCredential {
                    id: client.credential_id(),
                    observation: CredentialObservation(0),
                })),
            }
        },
        |source, requested| match source {
            AuthSource::ChatGpt => {
                if !requested.starts_with("claude-")
                    && !super::translate::model_allowlist::is_valid_model_for_codex(requested)
                {
                    return None;
                }
                let resolved =
                    super::translate::model_allowlist::resolve_model_request_with_config_override(
                        requested,
                        !body.bypass_provider_model_override,
                    );
                resolved.assert_allowed().ok().map(|()| resolved)
            }
            AuthSource::ApiKey => {
                let catalog = api.and_then(|client| client.catalog());
                super::translate::model_api::resolve_api_model_with_override(
                    requested,
                    catalog.as_deref(),
                    configured_model.as_deref(),
                )
                .map(ResolvedModel::from_api)
            }
        },
        |attempt, _, resolved| async move {
            match attempt.source {
                AuthSource::ChatGpt => {
                    chatgpt
                        .tracked_messages_attempt(body, identity.cloned(), resolved, &attempt)
                        .await
                }
                AuthSource::ApiKey => {
                    let Some(api) = api else { unreachable!() };
                    if let Some(monitor) = attempt.context.monitor.as_ref() {
                        monitor.model_resolved(&attempt.context.req_id, &resolved.model);
                        monitor.upstream_started(&attempt.context.req_id);
                    }
                    let result = if search::is_standalone_search_request(body) {
                        api::search::handle(body, identity, &resolved, api, &attempt).await
                    } else {
                        api::messages::handle(body, identity, &resolved, api, &attempt).await
                    };
                    result.map_err(|failure| AttemptFailure {
                        failure,
                        response: None::<Response>,
                    })
                }
            }
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
        Selection::Failed { failure, .. } => failure
            .response
            .unwrap_or_else(|| map_codex_error_to_response(&failure.failure.error)),
        Selection::Skipped {
            reason: SkipReason::ModelUnavailable,
            ..
        } => json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Model unavailable for selected Codex source",
        ),
        Selection::Skipped {
            source,
            reason: SkipReason::Marked(SkipMark::UsageLimit { until }),
            earliest_reset,
            ..
        } => quota_response(earliest_reset.unwrap_or(until), source),
        Selection::Skipped { .. } => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            if startup
                .sources()
                .order()
                .sources()
                .contains(&AuthSource::ChatGpt)
            {
                "Codex credentials unavailable; run codex login or duolane codex auth key set; check duolane codex auth order"
            } else {
                "OpenAI API key unavailable; run duolane codex auth key set; check duolane codex auth order"
            },
        ),
        Selection::ObservationError { .. } => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "Unable to observe Codex credentials",
        ),
    }
}

fn quota_response(until: tokio::time::Instant, source: AuthSource) -> Response {
    let resets_at = std::time::SystemTime::now()
        .checked_add(until.saturating_duration_since(tokio::time::Instant::now()))
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    super::error_response::usage_limit_response(&super::events::CodexUsageLimit {
        message: match source {
            AuthSource::ApiKey => "OpenAI API quota exhausted",
            AuthSource::ChatGpt => "ChatGPT quota exhausted",
        }
        .into(),
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
            crate::config::codex_auth::AuthSource::ApiKey,
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
