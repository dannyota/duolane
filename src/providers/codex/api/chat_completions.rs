#[path = "chat_completions_live.rs"]
mod live;

#[cfg(test)]
#[path = "chat_completions_live_tests.rs"]
mod live_tests;

use axum::{
    Json,
    response::{IntoResponse, Response},
};
use serde_json::Value;

use super::{ApiClient, ApiFailure, ApiFailureKind, SendEvidence, messages::source_failure};
use crate::providers::codex::{
    chat_completions::{
        request::TranslatedRequest,
        response::{CompletionState, completion_value},
    },
    events,
    sources::{FailureKind, SourceAttempt, SourceFailure},
};

fn invalid(send: SendEvidence) -> ApiFailure {
    ApiFailure::local(ApiFailureKind::Other, send, false)
}

#[allow(clippy::result_large_err)]
pub async fn handle(
    request: &TranslatedRequest,
    api: &ApiClient,
    attempt: &SourceAttempt,
) -> Result<Response, SourceFailure> {
    if !request.stream {
        return handle_buffered(request, api, attempt).await;
    }
    if attempt.gate.hosted() || attempt.gate.ambiguous_hosted() || attempt.gate.committed() {
        return Err(source_failure(
            api,
            invalid(SendEvidence::NotSent),
            FailureKind::Final,
        ));
    }
    let mut retries = 0;
    loop {
        let failure = match live::once(request, api, attempt).await {
            Ok(response) => return Ok(response),
            Err(failure) => failure,
        };
        if failure.hosted_executed {
            attempt.gate.mark_hosted();
        }
        let kind = match failure.kind {
            ApiFailureKind::Quota => FailureKind::UsageLimit,
            ApiFailureKind::Rejected => FailureKind::RejectedCredential,
            _ => FailureKind::Final,
        };
        if failure.kind != ApiFailureKind::Transient
            || attempt.gate.hosted()
            || attempt.gate.ambiguous_hosted()
            || attempt.gate.committed()
        {
            return Err(source_failure(api, failure, kind));
        }
        let backoff =
            crate::retry::compute_backoff_delay(retries, failure.error.retry_after.as_deref());
        if backoff.exceeds_budget
            || !attempt
                .context
                .recovery_budget
                .wait_retry(backoff.wait_ms, false)
                .await
        {
            return Err(source_failure(
                api,
                failure,
                FailureKind::TransientExhausted,
            ));
        }
        retries += 1;
    }
}

#[allow(clippy::result_large_err)]
pub async fn handle_buffered(
    request: &TranslatedRequest,
    api: &ApiClient,
    attempt: &SourceAttempt,
) -> Result<Response, SourceFailure> {
    if request.stream
        || attempt.gate.hosted()
        || attempt.gate.ambiguous_hosted()
        || attempt.gate.committed()
    {
        return Err(source_failure(
            api,
            invalid(SendEvidence::NotSent),
            FailureKind::Final,
        ));
    }
    let mut retries = 0;
    loop {
        let result = match api.responses(&request.upstream).await {
            Ok(events) => collect(events, request, attempt).await,
            Err(failure) => Err(failure),
        };
        let failure = match result {
            Ok(response) => return Ok(response),
            Err(failure) => failure,
        };
        if failure.hosted_executed {
            attempt.gate.mark_hosted();
        }
        let kind = match failure.kind {
            ApiFailureKind::Quota => FailureKind::UsageLimit,
            ApiFailureKind::Rejected => FailureKind::RejectedCredential,
            _ => FailureKind::Final,
        };
        if failure.kind != ApiFailureKind::Transient
            || attempt.gate.hosted()
            || attempt.gate.ambiguous_hosted()
            || attempt.gate.committed()
        {
            return Err(source_failure(api, failure, kind));
        }
        let backoff =
            crate::retry::compute_backoff_delay(retries, failure.error.retry_after.as_deref());
        if backoff.exceeds_budget
            || !attempt
                .context
                .recovery_budget
                .wait_retry(backoff.wait_ms, false)
                .await
        {
            return Err(source_failure(
                api,
                failure,
                FailureKind::TransientExhausted,
            ));
        }
        retries += 1;
    }
}

async fn collect(
    events: super::ApiEvents,
    request: &TranslatedRequest,
    attempt: &SourceAttempt,
) -> Result<Response, ApiFailure> {
    let mut events = events
        .with_raw_limit(crate::provider::MAX_BUFFERED_RESPONSE_BYTES)
        .strict_terminal();
    let mut state = CompletionState::new(&request.model);
    let mut projected = 0usize;
    while let Some(event) = events.next().await {
        let event = event?;
        if events::event_confirms_hosted_tool_execution(&event)
            || event
                .pointer("/response/output")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item["type"] == "web_search_call"))
        {
            attempt.gate.mark_hosted();
        }
        let length = serde_json::to_vec(&event)
            .map_err(|_| invalid(SendEvidence::OutcomeUnknown))?
            .len();
        projected = projected
            .checked_add(length)
            .ok_or_else(|| invalid(SendEvidence::OutcomeUnknown))?;
        if projected > crate::provider::MAX_BUFFERED_RESPONSE_BYTES {
            return Err(invalid(SendEvidence::OutcomeUnknown));
        }
        let terminal = events::event_is_success_terminal(&event);
        if terminal
            && !events::is_standard_max_output_tokens_incomplete(&event)
            && event.pointer("/response/status").and_then(Value::as_str) != Some("completed")
        {
            return Err(invalid(SendEvidence::OutcomeUnknown));
        }
        state
            .observe(&event)
            .map_err(|_| invalid(SendEvidence::OutcomeUnknown))?;
        if terminal {
            if !state.completed || state.text.is_empty() {
                return Err(invalid(SendEvidence::OutcomeUnknown));
            }
            let response = Json(completion_value(&state)).into_response();
            attempt.gate.mark_committed();
            return Ok(response);
        }
    }
    Err(invalid(SendEvidence::OutcomeUnknown))
}
