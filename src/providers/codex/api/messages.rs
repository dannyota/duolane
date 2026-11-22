use anyhow::Result;
use serde_json::{Value, json};

use crate::anthropic::schema::MessagesRequest;
use crate::providers::codex::translate::{
    model_allowlist::ResolvedModel,
    reasoning_signature::ReasoningAccount,
    request::{ServiceTier, TranslateOptions, translate_request},
};

pub fn build_request(
    body: &MessagesRequest,
    resolved: &ResolvedModel,
    session_id: Option<String>,
    reasoning: &ReasoningAccount,
) -> Result<Value> {
    let mut translated = translate_request(
        body,
        TranslateOptions {
            session_id,
            service_tier: resolved.service_tier.clone(),
            model: resolved.model.clone(),
            use_responses_lite: false,
            reasoning_account: reasoning.clone(),
        },
    )?;
    if translated.service_tier == Some(ServiceTier::Ultrafast) {
        translated.service_tier = Some(ServiceTier::Priority);
    }
    let mut value = serde_json::to_value(translated)?;
    if let Some(max_tokens) = body.max_tokens {
        value["max_output_tokens"] = json!(max_tokens);
    }
    Ok(value)
}

#[allow(clippy::result_large_err)]
pub async fn handle_buffered(
    body: &MessagesRequest,
    resolved: &ResolvedModel,
    api: &super::ApiClient,
    attempt: &crate::providers::codex::sources::SourceAttempt,
) -> std::result::Result<axum::response::Response, crate::providers::codex::sources::SourceFailure>
{
    use crate::providers::codex::api::{ApiFailure, ApiFailureKind, SendEvidence};
    use crate::providers::codex::sources::FailureKind;
    let local_failure = || ApiFailure::local(ApiFailureKind::Other, SendEvidence::NotSent, false);
    let request = build_request(
        body,
        resolved,
        attempt.context.session_id.clone(),
        api.reasoning_account(),
    )
    .map_err(|_| source_failure(api, local_failure(), FailureKind::Final))?;
    let has_hosted = request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("type").and_then(Value::as_str) != Some("function"))
        });
    let budget = &attempt.context.recovery_budget;
    if attempt.gate.hosted() || attempt.gate.ambiguous_hosted() || attempt.gate.committed() {
        return Err(source_failure(api, local_failure(), FailureKind::Final));
    }
    let mut retries = 0;
    loop {
        let result = match api.responses(&request).await {
            Ok(events) => collect_buffered(events, body, api, attempt).await,
            Err(failure) => Err(failure),
        };
        let failure = match result {
            Ok(response) => return Ok(response),
            Err(failure) => failure,
        };
        if failure.hosted_executed {
            attempt.gate.mark_hosted();
        }
        if has_hosted && failure.send == SendEvidence::OutcomeUnknown {
            attempt.gate.mark_ambiguous_hosted();
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
        if backoff.exceeds_budget || !budget.wait_retry(backoff.wait_ms, false).await {
            return Err(source_failure(
                api,
                failure,
                FailureKind::TransientExhausted,
            ));
        }
        retries += 1;
    }
}

fn source_failure(
    api: &super::ApiClient,
    failure: super::ApiFailure,
    kind: crate::providers::codex::sources::FailureKind,
) -> crate::providers::codex::sources::SourceFailure {
    crate::providers::codex::sources::SourceFailure {
        kind,
        send: failure.send,
        error: failure.error,
        credential: Some(api.credential_id()),
    }
}

fn append_event(bytes: &mut Vec<u8>, event: &Value) -> std::result::Result<(), super::ApiFailure> {
    let invalid = || {
        super::ApiFailure::local(
            super::ApiFailureKind::Other,
            super::SendEvidence::OutcomeUnknown,
            false,
        )
    };
    let encoded = serde_json::to_vec(event).map_err(|_| invalid())?;
    if encoded.len().saturating_add(8)
        > crate::provider::MAX_BUFFERED_RESPONSE_BYTES.saturating_sub(bytes.len())
    {
        return Err(invalid());
    }
    bytes.extend_from_slice(b"data: ");
    bytes.extend_from_slice(&encoded);
    bytes.extend_from_slice(b"\n\n");
    Ok(())
}

async fn collect_buffered(
    mut events: super::ApiEvents,
    body: &MessagesRequest,
    api: &super::ApiClient,
    attempt: &crate::providers::codex::sources::SourceAttempt,
) -> std::result::Result<axum::response::Response, super::ApiFailure> {
    use crate::providers::codex::{
        api::{ApiFailure, ApiFailureKind, SendEvidence},
        client_http_tools::HttpToolState,
        events as event_helpers,
        translate::accumulate::accumulate_response_with_traffic,
    };
    use axum::{Json, response::IntoResponse};
    let invalid = || ApiFailure::local(ApiFailureKind::Other, SendEvidence::OutcomeUnknown, false);
    let mut bytes = Vec::new();
    let mut tools = HttpToolState::default();
    let mut terminal = false;
    let mut indices = std::collections::HashMap::from([(0_u64, 0_u64)]);
    let mut next_index = 1_u64;
    while let Some(event) = events.next().await {
        let mut event = event?;
        if event_helpers::event_confirms_hosted_tool_execution(&event)
            || event
                .pointer("/response/output")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item["type"] == "web_search_call"))
        {
            attempt.gate.mark_hosted();
        }
        let is_terminal = event_helpers::event_is_success_terminal(&event);
        if is_terminal {
            tools.validate_terminal(&event).map_err(|_| invalid())?;
        }
        if tools.observe_strict(&mut event).map_err(|_| invalid())? {
            let tool_done = event.get("type").and_then(Value::as_str)
                == Some("response.output_item.done")
                && event.pointer("/item/type").and_then(Value::as_str) == Some("function_call");
            if let Some(index) = event.get("output_index").and_then(Value::as_u64) {
                if !indices.contains_key(&index) {
                    if indices.len() >= crate::provider::MAX_BUFFERED_RESPONSE_BYTES / 64 {
                        return Err(invalid());
                    }
                    indices.insert(index, next_index);
                    next_index = next_index.checked_add(1).ok_or_else(invalid)?;
                }
                event["output_index"] = json!(indices[&index]);
            } else if tool_done {
                event["output_index"] = json!(next_index);
                next_index = next_index.checked_add(1).ok_or_else(invalid)?;
            }
            if tool_done {
                let mut added = event.clone();
                added["type"] = json!("response.output_item.added");
                append_event(&mut bytes, &added)?;
            }
            append_event(&mut bytes, &event)?;
        }
        if is_terminal {
            terminal = true;
        }
    }
    if !terminal {
        return Err(invalid());
    }
    let message_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
    let value = accumulate_response_with_traffic(
        &bytes,
        &message_id,
        body.model.as_deref().unwrap_or("opus"),
        api.reasoning_account(),
        None,
    )
    .map_err(|_| invalid())?;
    if value
        .get("content")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Err(invalid());
    }
    let response = Json(value).into_response();
    attempt.gate.mark_committed();
    Ok(response)
}
