use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{ApiClient, ApiFailure, SendEvidence, invalid};
use crate::providers::codex::{
    chat_completions::{ChatError, request::TranslatedRequest, response::CompletionState},
    client_sse::event_stream_response,
    events,
    sources::{ReplayGate, SourceAttempt},
};

struct Live {
    events: super::super::ApiEvents,
    state: CompletionState,
    gate: ReplayGate,
    include_usage: bool,
    projected: usize,
    output_bytes: usize,
    output_limit: usize,
    identity: Option<(String, String, u64)>,
    done: bool,
}

fn sse(value: Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

const MAX_METADATA_BYTES: usize = 1000;

fn error_chunk() -> Bytes {
    let mut chunk =
        sse(ChatError::upstream("OpenAI API response could not be processed").value()).to_vec();
    chunk.extend_from_slice(b"data: [DONE]\n\n");
    chunk.into()
}

impl Live {
    fn freeze_identity(&mut self) {
        if self.identity.is_none() {
            self.identity = Some((
                self.state.id.clone(),
                self.state.model.clone(),
                self.state.created,
            ));
        }
    }

    fn charge(&mut self, bytes: usize, final_chunk: bool) -> Result<(), ApiFailure> {
        let reserve = if final_chunk { 0 } else { error_chunk().len() };
        let limit = self
            .output_limit
            .checked_sub(reserve)
            .ok_or_else(|| invalid(SendEvidence::OutcomeUnknown))?;
        self.output_bytes = self
            .output_bytes
            .checked_add(bytes)
            .filter(|total| *total <= limit)
            .ok_or_else(|| invalid(SendEvidence::OutcomeUnknown))?;
        Ok(())
    }

    async fn next(&mut self) -> Result<Option<Bytes>, ApiFailure> {
        while let Some(event) = self.events.next().await {
            let event = match event {
                Ok(event) => event,
                Err(failure) => {
                    if failure.hosted_executed {
                        self.gate.mark_hosted();
                    }
                    return Err(failure);
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
            let length = serde_json::to_vec(&event)
                .map_err(|_| invalid(SendEvidence::OutcomeUnknown))?
                .len();
            self.projected = self
                .projected
                .checked_add(length)
                .ok_or_else(|| invalid(SendEvidence::OutcomeUnknown))?;
            if self.projected > crate::provider::MAX_BUFFERED_RESPONSE_BYTES {
                return Err(invalid(SendEvidence::OutcomeUnknown));
            }
            let terminal = events::event_is_success_terminal(&event);
            if terminal
                && !events::is_standard_max_output_tokens_incomplete(&event)
                && event.pointer("/response/status").and_then(Value::as_str) != Some("completed")
            {
                return Err(invalid(SendEvidence::OutcomeUnknown));
            }
            if ["id", "model"].iter().any(|key| {
                event
                    .pointer("/response")
                    .and_then(|response| response.get(*key))
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.len() > MAX_METADATA_BYTES)
            }) {
                return Err(invalid(SendEvidence::OutcomeUnknown));
            }
            let delta = self
                .state
                .observe(&event)
                .map_err(|_| invalid(SendEvidence::OutcomeUnknown))?;
            if let Some(delta) = delta.filter(|delta| !delta.is_empty()) {
                self.freeze_identity();
                return Ok(Some(self.chunk(json!({"content":delta}), Value::Null)));
            }
            if terminal {
                let mut output = Vec::new();
                if self.state.text.is_empty() {
                    let text: String = event
                        .pointer("/response/output")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|item| item["type"] == "message" && item["role"] == "assistant")
                        .filter_map(|item| item.get("content").and_then(Value::as_array))
                        .flatten()
                        .filter(|part| part["type"] == "output_text")
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect();
                    if !text.is_empty() {
                        self.state.text.push_str(&text);
                        self.freeze_identity();
                        output.extend_from_slice(&self.chunk(json!({"content":text}), Value::Null));
                    }
                }
                if !self.state.completed || self.state.text.is_empty() {
                    return Err(invalid(SendEvidence::OutcomeUnknown));
                }
                self.done = true;
                let mut value = self.chunk_value(json!({}), json!(self.state.finish_reason));
                if self.include_usage {
                    value["usage"] = self.state.usage.value();
                }
                output.extend_from_slice(&sse(value));
                output.extend_from_slice(b"data: [DONE]\n\n");
                return Ok(Some(output.into()));
            }
        }
        Err(invalid(SendEvidence::OutcomeUnknown))
    }

    fn chunk_value(&self, delta: Value, finish: Value) -> Value {
        let (id, model, created) = self
            .identity
            .as_ref()
            .map(|(id, model, created)| (id, model, *created))
            .unwrap_or((&self.state.id, &self.state.model, self.state.created));
        json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }

    fn chunk(&self, delta: Value, finish: Value) -> Bytes {
        sse(self.chunk_value(delta, finish))
    }
}

pub(super) async fn once(
    request: &TranslatedRequest,
    api: &ApiClient,
    attempt: &SourceAttempt,
) -> Result<Response, ApiFailure> {
    once_with_output_limit(
        request,
        api,
        attempt,
        crate::provider::MAX_BUFFERED_RESPONSE_BYTES,
    )
    .await
}

pub(super) async fn once_with_output_limit(
    request: &TranslatedRequest,
    api: &ApiClient,
    attempt: &SourceAttempt,
    output_limit: usize,
) -> Result<Response, ApiFailure> {
    if request.model.len() > MAX_METADATA_BYTES {
        return Err(invalid(SendEvidence::NotSent));
    }
    let events = api
        .responses(&request.upstream)
        .await?
        .with_raw_limit(crate::provider::MAX_BUFFERED_RESPONSE_BYTES)
        .strict_terminal();
    let mut live = Live {
        events,
        state: CompletionState::new(&request.model),
        gate: attempt.gate.clone(),
        include_usage: request.include_usage,
        projected: 0,
        output_bytes: 0,
        output_limit,
        identity: None,
        done: false,
    };
    let first = loop {
        match live.next().await? {
            Some(chunk) => break chunk,
            None => continue,
        }
    };
    let mut initial = live
        .chunk(json!({"role":"assistant"}), Value::Null)
        .to_vec();
    initial.extend_from_slice(&first);
    live.charge(initial.len(), live.done)?;
    attempt.gate.mark_committed();
    let first =
        futures_util::stream::once(async move { Ok::<Bytes, std::io::Error>(initial.into()) });
    let rest = futures_util::stream::unfold(live, |mut live| async move {
        if live.done {
            return None;
        }
        let result = live.next().await.and_then(|chunk| {
            if let Some(chunk) = &chunk {
                live.charge(chunk.len(), live.done)?;
            }
            Ok(chunk)
        });
        let chunk = match result {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return None,
            Err(_) => {
                live.done = true;
                let chunk = error_chunk();
                if live.charge(chunk.len(), true).is_err() {
                    return None;
                }
                chunk
            }
        };
        Some((Ok::<Bytes, std::io::Error>(chunk), live))
    });
    Ok(event_stream_response(first.chain(rest)))
}
