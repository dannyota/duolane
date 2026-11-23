use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;

use super::{ApiClient, ApiEvents, ApiFailure, ApiFailureKind, SendEvidence, messages};
use crate::anthropic::schema::MessagesRequest;
use crate::providers::codex::{
    client_http_tools::HttpToolState,
    client_sse::{event_stream_response, translate_live_stream_payload, translated_content_ready},
    events,
    sources::{ReplayGate, SourceAttempt},
    translate::{
        live_stream::LiveStreamTranslator,
        model_allowlist::ResolvedModel,
        request::{TranslateOptions, translate_request},
    },
};

fn invalid() -> ApiFailure {
    ApiFailure::local(ApiFailureKind::Other, SendEvidence::OutcomeUnknown, false)
}

struct Live {
    events: ApiEvents,
    translator: LiveStreamTranslator,
    tools: HttpToolState,
    gate: ReplayGate,
    hosted: bool,
    done: bool,
    accepted_terminal: bool,
    indices: std::collections::HashMap<u64, u64>,
    next_index: u64,
    projected_bytes: usize,
    projected_limit: usize,
}

impl Live {
    fn record_failure(&self, failure: &ApiFailure) {
        if failure.hosted_executed {
            self.gate.mark_hosted();
        }
        if self.hosted && failure.send == SendEvidence::OutcomeUnknown {
            self.gate.mark_ambiguous_hosted();
        }
    }

    async fn next(&mut self) -> Result<Option<Vec<u8>>, ApiFailure> {
        while let Some(event) = self.events.next().await {
            let mut event = match event {
                Ok(e) => e,
                Err(e) => {
                    self.record_failure(&e);
                    return Err(e);
                }
            };
            if events::event_confirms_hosted_tool_execution(&event)
                || event
                    .pointer("/response/output")
                    .and_then(Value::as_array)
                    .is_some_and(|items| items.iter().any(|item| item["type"] == "web_search_call"))
            {
                self.gate.mark_hosted();
            }
            let bytes = serde_json::to_vec(&event).map_err(|_| invalid())?.len();
            self.projected_bytes = self
                .projected_bytes
                .checked_add(bytes)
                .ok_or_else(invalid)?;
            if self.projected_bytes > self.projected_limit {
                return Err(invalid());
            }
            let terminal = events::event_is_success_terminal(&event);
            if terminal
                && !events::is_standard_max_output_tokens_incomplete(&event)
                && event.pointer("/response/status").and_then(Value::as_str) != Some("completed")
            {
                return Err(invalid());
            }
            if terminal {
                self.tools
                    .validate_terminal(&event)
                    .map_err(|_| invalid())?;
            }
            if !self
                .tools
                .observe_strict(&mut event)
                .map_err(|_| invalid())?
            {
                continue;
            }
            self.accepted_terminal = events::is_standard_max_output_tokens_incomplete(&event);
            let tool_done = event["type"] == "response.output_item.done"
                && event["item"]["type"] == "function_call";
            if let Some(index) = event.get("output_index").and_then(Value::as_u64) {
                if !self.indices.contains_key(&index) {
                    if self.indices.len() >= crate::provider::MAX_BUFFERED_RESPONSE_BYTES / 64 {
                        return Err(invalid());
                    }
                    self.indices.insert(index, self.next_index);
                    self.next_index = self.next_index.checked_add(1).ok_or_else(invalid)?;
                }
                event["output_index"] = self.indices[&index].into();
            } else if tool_done {
                event["output_index"] = self.next_index.into();
                self.next_index = self.next_index.checked_add(1).ok_or_else(invalid)?;
            }
            let (out, done) = translate_live_stream_payload(&mut self.translator, &event, None)
                .map_err(|_| invalid())?;
            self.done = done;
            if !out.is_empty() || done {
                return Ok(Some(out));
            }
        }
        if self.done { Ok(None) } else { Err(invalid()) }
    }
}

#[allow(clippy::result_large_err)]
pub async fn stream_once(
    body: &MessagesRequest,
    resolved: &ResolvedModel,
    api: &ApiClient,
    attempt: &SourceAttempt,
) -> Result<Response, ApiFailure> {
    stream_once_with_limit(
        body,
        resolved,
        api,
        attempt,
        crate::provider::MAX_BUFFERED_RESPONSE_BYTES,
    )
    .await
}

#[allow(clippy::result_large_err)]
pub(super) async fn stream_once_with_limit(
    body: &MessagesRequest,
    resolved: &ResolvedModel,
    api: &ApiClient,
    attempt: &SourceAttempt,
    projected_limit: usize,
) -> Result<Response, ApiFailure> {
    stream_once_with_limits(
        body,
        resolved,
        api,
        attempt,
        projected_limit,
        crate::provider::MAX_BUFFERED_RESPONSE_BYTES,
    )
    .await
}

#[allow(clippy::result_large_err)]
pub(super) async fn stream_once_with_limits(
    body: &MessagesRequest,
    resolved: &ResolvedModel,
    api: &ApiClient,
    attempt: &SourceAttempt,
    projected_limit: usize,
    output_limit: usize,
) -> Result<Response, ApiFailure> {
    let local = || ApiFailure::local(ApiFailureKind::Other, SendEvidence::NotSent, false);
    if attempt.gate.hosted() || attempt.gate.ambiguous_hosted() || attempt.gate.committed() {
        return Err(local());
    }
    let request = messages::build_request(
        body,
        resolved,
        attempt.context.session_id.clone(),
        api.reasoning_account(),
    )
    .map_err(|_| local())?;
    let hosted = request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("type").and_then(Value::as_str) != Some("function"))
        });
    let typed = translate_request(
        body,
        TranslateOptions {
            session_id: attempt.context.session_id.clone(),
            service_tier: resolved.service_tier.clone(),
            model: resolved.model.clone(),
            use_responses_lite: false,
            reasoning_account: api.reasoning_account().clone(),
        },
    )
    .map_err(|_| local())?;
    let events = match api.responses(&request).await {
        Ok(events) => events,
        Err(e) => {
            if e.hosted_executed {
                attempt.gate.mark_hosted();
            }
            if hosted && e.send == SendEvidence::OutcomeUnknown {
                attempt.gate.mark_ambiguous_hosted();
            }
            return Err(e);
        }
    };
    let translator = LiveStreamTranslator::for_request(
        format!("msg_{}", uuid::Uuid::new_v4().simple()),
        body.model.as_deref().unwrap_or("opus"),
        &typed,
    )
    .with_http_tool_commits(true)
    .with_output_limit(output_limit);
    let mut live = Live {
        events,
        translator,
        tools: HttpToolState::default(),
        gate: attempt.gate.clone(),
        hosted,
        done: false,
        accepted_terminal: false,
        indices: std::collections::HashMap::from([(0, 0)]),
        next_index: 1,
        projected_bytes: 0,
        projected_limit,
    };
    let mut pending = Vec::new();
    loop {
        let chunk = match live.next().await {
            Ok(Some(c)) => c,
            Ok(None) => return Err(invalid()),
            Err(e) => {
                live.record_failure(&e);
                return Err(e);
            }
        };
        let ready = translated_content_ready(&chunk);
        if chunk.len() > crate::provider::MAX_BUFFERED_RESPONSE_BYTES.saturating_sub(pending.len())
        {
            let e = invalid();
            live.record_failure(&e);
            return Err(e);
        }
        pending.extend(chunk);
        if ready || (live.done && live.accepted_terminal) {
            break;
        }
        if live.done {
            let e = invalid();
            live.record_failure(&e);
            return Err(e);
        }
    }
    attempt.gate.mark_committed();
    let first =
        futures_util::stream::once(
            async move { Ok::<Bytes, std::io::Error>(Bytes::from(pending)) },
        );
    let rest = futures_util::stream::unfold(live, |mut live| async move {
        if live.done {
            return None;
        }
        match live.next().await {
            Ok(Some(chunk)) => Some((Ok(Bytes::from(chunk)), live)),
            Ok(None) => None,
            Err(e) => {
                live.record_failure(&e);
                live.done = true;
                let chunk = live
                    .translator
                    .error_chunk(&e.error.message, "api_error", None);
                Some((Ok(Bytes::from(chunk)), live))
            }
        }
    });
    use futures_util::StreamExt;
    Ok(event_stream_response(first.chain(rest)))
}
