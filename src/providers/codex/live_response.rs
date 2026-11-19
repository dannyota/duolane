use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::anthropic::sse::{parse_sse_events, usage_from_anthropic_sse};
use crate::config;
use crate::logging::create_logger;
use crate::provider::RequestContext;
#[cfg(test)]
use crate::request_identity::ConversationIdentity;
use crate::retry::{compute_backoff_delay, sleep};

use super::client::CodexHttpClient;
use super::compaction::CompactionAttempt;
#[cfg(test)]
use super::compaction::{begin_compaction, store_compaction};
use super::continuation::ContinuationReservation;
#[cfg(test)]
use super::continuation::continuation_candidate_for_owner;
use super::error_response::*;
use super::live_completion::*;
use super::request_state::*;
use super::translate::live_stream::LiveStreamTranslator;
#[cfg(test)]
use super::translate::reasoning_signature::ReasoningAccount;
#[cfg(test)]
use super::translate::request::{TranslateOptions, translate_request};
use super::*;

const MAX_RETRYABLE_LIVE_STREAM_RETRIES: u32 = 10;

pub(super) const MAX_EMPTY_COMPLETION_RETRIES: u32 = 10;

pub(super) const EMPTY_CODEX_COMPLETION_DETAIL: &str = "empty_codex_completion";

const LIVE_STREAM_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

enum LiveStreamStart {
    Response(Response),
    Retry {
        error: client::CodexError,
        full_context_retry_attempted: bool,
    },
}

#[derive(Clone, Copy)]
pub(super) struct LiveStreamCompaction {
    pub(super) compact_boundary: bool,
    pub(super) attempt: Option<CompactionAttempt>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn live_stream_response(
    client: Arc<CodexHttpClient>,
    message_id: String,
    model: &str,
    ctx: RequestContext,
    request_body: translate::request::ResponsesRequest,
    continuation: ContinuationReservation,
    compaction: LiveStreamCompaction,
    transport: config::CodexTransport,
    successful_tool_tail: bool,
) -> Response {
    let model = model.to_string();
    let started_at = Instant::now();
    let request_continuation = continuation.clone();
    let mut empty_completion_attempts = 0_u32;
    let mut cleanup = LiveRequestStateCleanup::new(
        request_continuation.clone(),
        ctx.session_id.clone(),
        compaction.attempt,
    );
    let mut attempt = 0_u32;
    let mut continuation = Some(continuation);

    loop {
        let upstream_events = match transport {
            config::CodexTransport::Http => {
                client
                    .stream_codex_http_events_for_owner(&request_body, &ctx)
                    .await
            }
            config::CodexTransport::WebSocket => {
                client
                    .stream_codex_websocket_events_for_owner(
                        &request_body,
                        &ctx,
                        continuation.as_ref(),
                    )
                    .await
            }
            config::CodexTransport::Auto => {
                client
                    .stream_codex_auto_events_for_owner(&request_body, &ctx, continuation.as_ref())
                    .await
            }
        };
        let upstream_events = match upstream_events {
            Ok(events) => events,
            Err(err) if err.origin == client::CodexErrorOrigin::Http => {
                cleanup.abort();
                return map_codex_error_to_response(&err);
            }
            Err(err) if retryable_live_start_codex_error(&err) => {
                empty_completion_attempts = 0;
                let dropped = drop_live_continuation_for_retry(&mut continuation);
                if dropped && is_missing_previous_response_error(&err) {
                    attempt += 1;
                    continue;
                }
                if attempt >= MAX_RETRYABLE_LIVE_STREAM_RETRIES {
                    cleanup.abort();
                    return map_codex_error_to_response(&err);
                }
                let delay = compute_backoff_delay(attempt, err.retry_after.as_deref());
                if delay.exceeds_budget {
                    cleanup.abort();
                    return map_codex_error_to_response(&err);
                }
                attempt += 1;
                sleep(delay.wait_ms).await;
                continue;
            }
            Err(err) => {
                cleanup.abort();
                return map_codex_error_to_response(&err);
            }
        };

        match live_stream_response_once(
            upstream_events,
            message_id.clone(),
            &model,
            ctx.clone(),
            request_continuation.clone(),
            request_body.clone(),
            compaction,
            transport,
            accept_empty_completion(successful_tool_tail, empty_completion_attempts),
        )
        .await
        {
            LiveStreamStart::Response(response) => {
                cleanup.disarm();
                return response;
            }
            LiveStreamStart::Retry {
                error,
                full_context_retry_attempted,
            } => {
                if error.origin == client::CodexErrorOrigin::Http {
                    cleanup.abort();
                    return map_codex_error_to_response(&error);
                }
                if error.detail.as_deref() == Some(EMPTY_CODEX_COMPLETION_DETAIL) {
                    empty_completion_attempts += 1;
                } else {
                    empty_completion_attempts = 0;
                }
                let dropped = drop_live_continuation_for_retry(&mut continuation);
                if full_context_retry_attempted && client::is_continuation_retry_error(&error) {
                    cleanup.abort();
                    return map_codex_error_to_response(&error);
                }
                if dropped && is_missing_previous_response_error(&error) {
                    attempt += 1;
                    continue;
                }
                if attempt >= MAX_RETRYABLE_LIVE_STREAM_RETRIES {
                    if is_empty_completion_error(&error) {
                        create_logger("codex").warn(
                            "codex_empty_completion_exhausted",
                            Some(empty_completion_exhausted_fields(
                                &ctx.req_id,
                                transport.as_str(),
                                &model,
                                attempt + 1,
                                "max_attempts",
                                started_at.elapsed().as_millis(),
                            )),
                        );
                    }
                    cleanup.abort();
                    return map_codex_error_to_response(&error);
                }
                let delay = compute_backoff_delay(attempt, error.retry_after.as_deref());
                if delay.exceeds_budget {
                    if is_empty_completion_error(&error) {
                        create_logger("codex").warn(
                            "codex_empty_completion_exhausted",
                            Some(empty_completion_exhausted_fields(
                                &ctx.req_id,
                                transport.as_str(),
                                &model,
                                attempt + 1,
                                "backoff_budget",
                                started_at.elapsed().as_millis(),
                            )),
                        );
                    }
                    cleanup.abort();
                    return map_codex_error_to_response(&error);
                }
                attempt += 1;
                sleep(delay.wait_ms).await;
            }
        }
    }
}

fn provider_retry(
    upstream_events: &websocket::CodexWebSocketEventStream,
    error: client::CodexError,
) -> LiveStreamStart {
    let full_context_retry_attempted = upstream_events.used_full_context_retry();
    upstream_events.mark_provider_retry_handoff();
    LiveStreamStart::Retry {
        error,
        full_context_retry_attempted,
    }
}

#[allow(clippy::too_many_arguments)]
async fn live_stream_response_once(
    mut upstream_events: websocket::CodexWebSocketEventStream,
    message_id: String,
    model: &str,
    ctx: RequestContext,
    request_continuation: ContinuationReservation,
    request_body: translate::request::ResponsesRequest,
    compaction: LiveStreamCompaction,
    configured_transport: config::CodexTransport,
    accept_empty: bool,
) -> LiveStreamStart {
    let mut translator = LiveStreamTranslator::for_request(message_id, model, &request_body);
    let mut upstream_sse_body = Vec::new();
    let mut pending_chunk = Vec::new();
    let mut generation_started = false;

    while let Some(item) = upstream_events.recv().await {
        let payload = match item {
            Ok(payload) => payload,
            Err(err) => {
                if retryable_live_start_codex_error(&err) {
                    return provider_retry(&upstream_events, err);
                }
                abort_request_state(
                    ctx.session_id.as_deref(),
                    &request_continuation,
                    compaction.attempt,
                );
                return LiveStreamStart::Response(map_codex_error_to_response(&err));
            }
        };
        if !generation_started && codex_generation_event(&payload) {
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.generation_started(&ctx.req_id);
            }
            generation_started = true;
        }
        append_upstream_sse_payload(&mut upstream_sse_body, &payload);
        let (chunk, terminal) = match translate_live_stream_payload(&mut translator, &payload, None)
        {
            Ok(result) => result,
            Err(message) => {
                if let Some(limit) = events::usage_limit_from_event(&payload) {
                    abort_request_state(
                        ctx.session_id.as_deref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    return LiveStreamStart::Response(usage_limit_response(&limit));
                }
                if let Some(failure) = events::classify_event_failure(&payload) {
                    if failure.retryable() {
                        return provider_retry(
                            &upstream_events,
                            codex_event_failure_error(
                                &failure,
                                client::CodexErrorOrigin::WebSocket,
                            ),
                        );
                    }
                    abort_request_state(
                        ctx.session_id.as_deref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    return LiveStreamStart::Response(map_codex_event_failure_to_response(
                        &failure,
                    ));
                }
                abort_request_state(
                    ctx.session_id.as_deref(),
                    &request_continuation,
                    compaction.attempt,
                );
                return LiveStreamStart::Response(map_codex_failure_to_response(&message));
            }
        };
        pending_chunk.extend_from_slice(&chunk);
        if is_codex_success_terminal_event(&payload)
            && !events::is_standard_max_output_tokens_incomplete(&payload)
            && !translator.has_semantic_output()
            && !(accept_empty && terminal_status_is_completed(&payload))
        {
            create_logger("codex").warn(
                "codex_empty_completion_retry",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), serde_json::json!(&ctx.req_id)),
                    (
                        "configuredTransport".to_string(),
                        serde_json::json!(configured_transport.as_str()),
                    ),
                    ("model".to_string(), serde_json::json!(model)),
                    ("stream".to_string(), serde_json::json!(true)),
                ])),
            );
            return provider_retry(&upstream_events, empty_live_completion_error());
        }
        if translator.has_semantic_output() && !pending_chunk.is_empty() {
            record_live_stream_downstream_capture(&ctx, &pending_chunk);
            record_live_stream_progress(&ctx, &pending_chunk);
            if terminal {
                update_continuation_from_upstream(
                    ctx.session_id.as_deref(),
                    &request_continuation,
                    compaction.attempt,
                    &request_body,
                    &upstream_sse_body,
                    upstream_events.socket_id(),
                    compaction.compact_boundary,
                );
                return LiveStreamStart::Response(single_live_stream_response(pending_chunk));
            }
            return LiveStreamStart::Response(remaining_live_stream_response(
                upstream_events,
                translator,
                pending_chunk,
                ctx,
                request_continuation,
                request_body,
                upstream_sse_body,
                compaction,
            ));
        }
        if terminal {
            update_continuation_from_upstream(
                ctx.session_id.as_deref(),
                &request_continuation,
                compaction.attempt,
                &request_body,
                &upstream_sse_body,
                upstream_events.socket_id(),
                compaction.compact_boundary,
            );
            if pending_chunk.is_empty() {
                return LiveStreamStart::Response(empty_live_stream_response());
            }
            record_live_stream_downstream_capture(&ctx, &pending_chunk);
            record_live_stream_progress(&ctx, &pending_chunk);
            return LiveStreamStart::Response(single_live_stream_response(pending_chunk));
        }
    }

    provider_retry(
        &upstream_events,
        client::CodexError {
            status: 0,
            message: "WebSocket connection closed before terminal Codex response event".to_string(),
            detail: Some(websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: client::CodexErrorOrigin::WebSocket,
        },
    )
}

fn translate_live_stream_payload(
    translator: &mut LiveStreamTranslator,
    payload: &serde_json::Value,
    traffic: Option<&crate::traffic::TrafficCapture>,
) -> Result<(Vec<u8>, bool), String> {
    let chunk = translator.accept(payload, traffic)?;
    let terminal = events::event_is_terminal(payload) || translator.is_finished();
    Ok((chunk, terminal))
}

fn record_live_stream_downstream_capture(ctx: &RequestContext, chunk: &[u8]) {
    let Some(traffic) = ctx.traffic.as_ref() else {
        return;
    };
    for event in parse_sse_events(chunk) {
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            continue;
        };
        traffic.write_json_event(
            "050-downstream-event",
            &serde_json::json!({
                "event": event.event.as_deref().unwrap_or("message"),
                "data": data,
            }),
        );
    }
}

fn record_live_stream_progress(ctx: &RequestContext, chunk: &[u8]) {
    if let Some(monitor) = ctx.monitor.as_ref() {
        let (input_tokens, output_tokens) = usage_from_anthropic_sse(chunk);
        monitor.stream_progress(
            &ctx.req_id,
            chunk.len() as u64,
            count_sse_events(chunk),
            input_tokens,
            output_tokens,
        );
    }
}

fn single_live_stream_response(chunk: Vec<u8>) -> Response {
    event_stream_response(futures_util::stream::once(async move {
        Ok::<Bytes, std::io::Error>(Bytes::from(chunk))
    }))
}

fn empty_live_stream_response() -> Response {
    event_stream_response(futures_util::stream::empty::<Result<Bytes, std::io::Error>>())
}

#[allow(clippy::too_many_arguments)]
fn remaining_live_stream_response(
    mut upstream_events: websocket::CodexWebSocketEventStream,
    mut translator: LiveStreamTranslator,
    first_chunk: Vec<u8>,
    ctx: RequestContext,
    request_continuation: ContinuationReservation,
    request_body: translate::request::ResponsesRequest,
    mut upstream_sse_body: Vec<u8>,
    compaction: LiveStreamCompaction,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
        if tx.send(Ok(Bytes::from(first_chunk))).await.is_err() {
            abort_request_state(
                ctx.session_id.as_deref(),
                &request_continuation,
                compaction.attempt,
            );
            return;
        }
        let mut heartbeat = tokio::time::interval(LIVE_STREAM_HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.tick().await;
        loop {
            let item = tokio::select! {
                biased;
                _ = tx.closed() => {
                    abort_request_state(
                        ctx.session_id.as_deref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    return;
                }
                item = upstream_events.recv() => item,
                _ = heartbeat.tick() => {
                    let chunk = translator.ping_chunk(ctx.traffic.as_deref());
                    if !chunk.is_empty() {
                        record_live_stream_progress(&ctx, &chunk);
                        if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                            abort_request_state(
                                ctx.session_id.as_deref(),
                                &request_continuation,
                                compaction.attempt,
                            );
                            return;
                        }
                    }
                    continue;
                }
            };
            let Some(item) = item else {
                break;
            };
            match item {
                Ok(payload) => {
                    append_upstream_sse_payload(&mut upstream_sse_body, &payload);
                    let (chunk, terminal) = match translate_live_stream_payload(
                        &mut translator,
                        &payload,
                        ctx.traffic.as_deref(),
                    ) {
                        Ok(result) => result,
                        Err(message) => {
                            abort_request_state(
                                ctx.session_id.as_deref(),
                                &request_continuation,
                                compaction.attempt,
                            );
                            let failure = events::classify_event_failure(&payload);
                            let error_type = failure
                                .as_ref()
                                .map_or("api_error", events::CodexEventFailure::client_error_type);
                            let error_message = failure.as_ref().map_or_else(
                                || message.clone(),
                                events::CodexEventFailure::anthropic_message,
                            );
                            let chunk = translator.error_chunk(
                                &error_message,
                                error_type,
                                ctx.traffic.as_deref(),
                            );
                            if !chunk.is_empty() {
                                record_live_stream_progress(&ctx, &chunk);
                                let _ = tx.send(Ok(Bytes::from(chunk))).await;
                            }
                            return;
                        }
                    };
                    if !chunk.is_empty() {
                        record_live_stream_progress(&ctx, &chunk);
                        if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                            abort_request_state(
                                ctx.session_id.as_deref(),
                                &request_continuation,
                                compaction.attempt,
                            );
                            return;
                        }
                    }
                    if terminal {
                        update_continuation_from_upstream(
                            ctx.session_id.as_deref(),
                            &request_continuation,
                            compaction.attempt,
                            &request_body,
                            &upstream_sse_body,
                            upstream_events.socket_id(),
                            compaction.compact_boundary,
                        );
                        return;
                    }
                }
                Err(err) => {
                    abort_request_state(
                        ctx.session_id.as_deref(),
                        &request_continuation,
                        compaction.attempt,
                    );
                    let error_type = codex_stream_error_type(&err);
                    let chunk = translator.error_chunk(
                        codex_error_message(&err),
                        error_type,
                        ctx.traffic.as_deref(),
                    );
                    if !chunk.is_empty() {
                        record_live_stream_progress(&ctx, &chunk);
                        let _ = tx.send(Ok(Bytes::from(chunk))).await;
                    }
                    return;
                }
            }
        }

        abort_request_state(
            ctx.session_id.as_deref(),
            &request_continuation,
            compaction.attempt,
        );
        let chunk = translator.error_chunk(
            "Upstream event stream closed before terminal Codex response event",
            "api_error",
            ctx.traffic.as_deref(),
        );
        if !chunk.is_empty() {
            record_live_stream_progress(&ctx, &chunk);
            let _ = tx.send(Ok(Bytes::from(chunk))).await;
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    });
    event_stream_response(stream)
}

fn append_upstream_sse_payload(buffer: &mut Vec<u8>, payload: &serde_json::Value) {
    let text = payload.to_string();
    for line in text.lines() {
        buffer.extend_from_slice(b"data: ");
        buffer.extend_from_slice(line.as_bytes());
        buffer.push(b'\n');
    }
    buffer.push(b'\n');
}

fn event_stream_response<S>(stream: S) -> Response
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let headers = [
        (http::header::CONTENT_TYPE, "text/event-stream"),
        (http::header::CACHE_CONTROL, "no-cache"),
        (http::header::CONNECTION, "keep-alive"),
    ];
    (headers, Body::from_stream(stream)).into_response()
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use http_body_util::BodyExt;
    use tokio::net::TcpListener;

    use super::super::provider_test_support::*;
    use super::*;

    #[tokio::test]
    async fn terminal_only_incomplete_upstream_is_not_empty_completion() {
        let payload = serde_json::json!({
            "type": "response.incomplete",
            "response": {"id": "resp_1", "incomplete_details": {"reason": "max_output_tokens"}, "usage": {}}
        });
        assert!(!is_empty_codex_success_completion(
            &upstream_sse(std::slice::from_ref(&payload)),
            &ReasoningAccount::default()
        ));

        let (tx, rx, ctx, request_body) = empty_terminal_fixture();
        tx.send(Ok(payload)).await.unwrap();
        let LiveStreamStart::Response(response) = live_stream_response_once(
            rx,
            "msg_test".to_string(),
            "gpt-5.6-sol",
            ctx,
            ContinuationReservation::for_owner_turn(None, None),
            request_body,
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::WebSocket,
            false,
        )
        .await
        else {
            panic!("a max_tokens stop must be delivered, not retried");
        };
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let sse = String::from_utf8_lossy(&body);
        assert!(sse.contains(r#""stop_reason":"max_tokens""#), "{sse}");
        assert!(sse.contains("event: message_stop"), "{sse}");
    }

    #[tokio::test]
    async fn empty_acceptance_requires_explicit_completed_status() {
        for status in [
            serde_json::json!("cancelled"),
            serde_json::json!("in_progress"),
            serde_json::Value::Null,
        ] {
            let (tx, rx, ctx, request_body) = empty_terminal_fixture();
            let payload = serde_json::json!({
                "type":"response.completed", "response":{"id":"r", "status":status,"output":[]}
            });
            assert!(!buffered_terminal_is_completed(&upstream_sse(
                std::slice::from_ref(&payload)
            )));
            tx.send(Ok(payload)).await.unwrap();
            match live_stream_response_once(
                rx,
                "msg_test".into(),
                "gpt-5.6-sol",
                ctx,
                ContinuationReservation::for_owner_turn(None, None),
                request_body,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::WebSocket,
                true,
            )
            .await
            {
                LiveStreamStart::Retry { error, .. } => assert!(is_empty_completion_error(&error)),
                LiveStreamStart::Response(_) => {
                    panic!("non-completed status must not end the turn")
                }
            }
        }
        assert!(!buffered_terminal_is_completed(&upstream_sse(&[
            serde_json::json!({
                "type":"response.done", "response":{"output":[]}
            })
        ])));
    }

    #[tokio::test]
    async fn accepting_empty_does_not_accept_failed_or_incomplete_terminals() {
        for status in ["failed", "incomplete"] {
            let (tx, rx, ctx, request_body) = empty_terminal_fixture();
            let payload = serde_json::json!({
                "type":"response.done", "response":{"id":"r", "status":status,"output":[]}
            });
            assert!(!is_empty_codex_success_completion(
                &upstream_sse(std::slice::from_ref(&payload)),
                &ReasoningAccount::default()
            ));
            tx.send(Ok(payload)).await.unwrap();
            match live_stream_response_once(
                rx,
                "msg_test".into(),
                "gpt-5.6-sol",
                ctx,
                ContinuationReservation::for_owner_turn(None, None),
                request_body,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::WebSocket,
                true,
            )
            .await
            {
                LiveStreamStart::Retry { error, .. } => assert!(!is_empty_completion_error(&error)),
                LiveStreamStart::Response(response) => assert!(!response.status().is_success()),
            }
        }
    }

    fn empty_terminal_fixture() -> (
        tokio::sync::mpsc::Sender<Result<serde_json::Value, client::CodexError>>,
        websocket::CodexWebSocketEventStream,
        RequestContext,
        translate::request::ResponsesRequest,
    ) {
        let body = request_with_tools(serde_json::json!([]));
        let request_body = translate_request(
            &body,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
                reasoning_account: ReasoningAccount::default(),
            },
        )
        .unwrap();
        let ctx = RequestContext {
            req_id: "empty-terminal".to_string(),
            session_id: None,
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (rx, _) = websocket::CodexWebSocketEventStream::pending(rx);
        (tx, rx, ctx, request_body)
    }

    #[tokio::test]
    async fn empty_terminal_without_accept_flag_still_retries() {
        let (tx, rx, ctx, request_body) = empty_terminal_fixture();
        tx.send(Ok(serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "status": "completed", "output": [], "usage": {}}
        })))
        .await
        .unwrap();
        let continuation = ContinuationReservation::for_owner_turn(None, None);
        match live_stream_response_once(
            rx,
            "msg_test".to_string(),
            "claude-opus-4-8",
            ctx,
            continuation,
            request_body,
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::WebSocket,
            false,
        )
        .await
        {
            LiveStreamStart::Retry { error, .. } => {
                assert_eq!(error.detail.as_deref(), Some(EMPTY_CODEX_COMPLETION_DETAIL));
            }
            LiveStreamStart::Response(_) => {
                panic!("an empty completion without the accept flag must stay a retry")
            }
        }
    }

    #[tokio::test]
    async fn empty_terminal_with_accept_flag_is_a_valid_response() {
        use http_body_util::BodyExt as _;
        let (tx, rx, ctx, request_body) = empty_terminal_fixture();
        tx.send(Ok(serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "status": "completed", "output": [], "usage": {}}
        })))
        .await
        .unwrap();
        let continuation = ContinuationReservation::for_owner_turn(None, None);
        let response = match live_stream_response_once(
            rx,
            "msg_test".to_string(),
            "claude-opus-4-8",
            ctx,
            continuation,
            request_body,
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::WebSocket,
            true,
        )
        .await
        {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { error, .. } => {
                panic!(
                    "accept flag set: the empty completion must be delivered, got retry: {error}"
                )
            }
        };
        let mut collected = Vec::new();
        let mut body = response.into_body();
        while let Ok(Some(frame)) =
            tokio::time::timeout(Duration::from_millis(300), body.frame()).await
        {
            if let Ok(data) = frame.unwrap().into_data() {
                collected.extend_from_slice(&data);
            }
        }
        let sse = String::from_utf8(collected).unwrap();
        assert!(
            sse.contains("event: message_stop"),
            "delivered stream must terminate properly, got: {sse:?}"
        );
    }

    #[tokio::test]
    async fn live_stream_ending_after_completed_tool_call_is_not_success() {
        let websocket_closed = client::CodexError {
            status: 0,
            message: "WebSocket connection closed before terminal Codex response event".into(),
            detail: Some(websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: client::CodexErrorOrigin::WebSocket,
        };
        for ending in [None, Some(websocket_closed)] {
            let (tx, rx, ctx, request_body) = empty_terminal_fixture();
            for event in [
                serde_json::json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}
                }),
                serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "Bash",
                        "arguments": "{\"command\":\"ls\"}"
                    }
                }),
            ] {
                tx.send(Ok(event)).await.unwrap();
            }
            if let Some(error) = ending {
                tx.send(Err(error)).await.unwrap();
            }
            drop(tx);
            let LiveStreamStart::Response(response) = live_stream_response_once(
                rx,
                "msg_test".to_string(),
                "gpt-5.6-sol",
                ctx,
                ContinuationReservation::for_owner_turn(None, None),
                request_body,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::WebSocket,
                false,
            )
            .await
            else {
                panic!("a stream with semantic output must be delivered");
            };
            let body = tokio::time::timeout(
                Duration::from_secs(2),
                axum::body::to_bytes(response.into_body(), usize::MAX),
            )
            .await
            .unwrap()
            .unwrap();
            let sse = String::from_utf8_lossy(&body);
            assert!(sse.contains("event: error"), "{sse}");
            assert!(!sse.contains("event: message_stop"), "{sse}");
        }
    }

    #[test]
    fn live_stream_progress_records_terminal_usage() {
        let monitor = crate::monitor::MonitorHandle::new(10);
        monitor.request_started(
            "request",
            None,
            None,
            crate::monitor::EndpointKind::Messages,
        );
        let ctx = RequestContext {
            req_id: "request".to_string(),
            session_id: None,
            traffic: None,
            monitor: Some(monitor.clone()),
            passthrough: None,
        };
        let chunk = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":12,\"output_tokens\":48}}\n\n";

        record_live_stream_progress(&ctx, chunk);

        let state = monitor.snapshot();
        assert_eq!(state.active[0].input_tokens, Some(12));
        assert_eq!(state.active[0].output_tokens, Some(48));
    }

    #[tokio::test]
    async fn live_stream_response_emits_downstream_frames_before_terminal_event() {
        use http_body_util::BodyExt as _;

        let body = request_with_tools(serde_json::json!([]));
        let request_body = translate_request(
            &body,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
                reasoning_account: ReasoningAccount::default(),
            },
        )
        .unwrap();
        let ctx = RequestContext {
            req_id: "incremental-http".to_string(),
            session_id: None,
            traffic: None,
            monitor: None,
            passthrough: None,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(Ok(serde_json::json!({"type": "keepalive"})))
            .await
            .unwrap();
        tx.send(Ok(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": "msg_up"}
        })))
        .await
        .unwrap();
        tx.send(Ok(serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "first"
        })))
        .await
        .unwrap();

        let (rx, _) = websocket::CodexWebSocketEventStream::pending(rx);
        let continuation = ContinuationReservation::for_owner_turn(None, None);
        let response = match live_stream_response_once(
            rx,
            "msg_test".to_string(),
            "claude-opus-4-8",
            ctx,
            continuation,
            request_body,
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::Http,
            false,
        )
        .await
        {
            LiveStreamStart::Response(response) => response,
            LiveStreamStart::Retry { error, .. } => panic!("unexpected retry: {error}"),
        };
        let mut body = response.into_body();
        let first = tokio::time::timeout(Duration::from_millis(200), body.frame())
            .await
            .expect("initial downstream frame must be available immediately")
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        let first = String::from_utf8(first.to_vec()).unwrap();
        assert!(first.contains("event: message_start"));
        assert!(first.contains("event: ping"));
        assert!(first.contains("event: content_block_start"));
        assert!(first.contains("event: content_block_delta"));

        tx.send(Ok(serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "second"
        })))
        .await
        .unwrap();
        let second = tokio::time::timeout(Duration::from_millis(200), body.frame())
            .await
            .expect("text delta must arrive before the terminal event")
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert!(
            String::from_utf8(second.to_vec())
                .unwrap()
                .contains("event: content_block_delta")
        );

        for payload in [
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message"}
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "incomplete_details": null,
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                }
            }),
        ] {
            tx.send(Ok(payload)).await.unwrap();
        }
        drop(tx);
        while let Some(frame) = body.frame().await {
            frame.unwrap();
        }
    }

    #[tokio::test]
    async fn dropping_live_stream_during_retry_backoff_aborts_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let session_id = "live-retry-backoff-cleanup";
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = live_test_request("one");
        let continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let compaction_attempt = begin_compaction(session_id, &request);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (event_sent_tx, event_sent_rx) = tokio::sync::oneshot::channel();
        let (socket_closed_tx, socket_closed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_live_websocket_request(&mut websocket).await;
            emit_live_event(
                &mut websocket,
                &serde_json::json!({
                    "type": "codex.rate_limits",
                    "rate_limits": {"allowed": false, "limit_reached": true}
                }),
            )
            .await;
            event_sent_tx.send(()).unwrap();
            drop(websocket);
            socket_closed_tx.send(()).unwrap();
        });
        let client = authenticated_live_test_client(format!("http://{addr}/responses"));
        let task_request = request.clone();
        let task_continuation = continuation.clone();
        let response_task = tokio::spawn(async move {
            let model = task_request.model.clone();
            live_stream_response(
                client,
                "message".to_string(),
                &model,
                live_test_context(session_id),
                task_request,
                task_continuation,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: Some(compaction_attempt),
                },
                config::CodexTransport::WebSocket,
                false,
            )
            .await
        });

        event_sent_rx.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), socket_closed_rx)
            .await
            .expect("retry handoff did not close the abandoned attempt socket")
            .expect("retry handoff socket-close sender dropped");
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !response_task.is_finished(),
            "logical request must still be waiting in retry backoff"
        );
        response_task.abort();
        assert!(response_task.await.unwrap_err().is_cancelled());

        assert!(!continuation::is_current_turn_for_owner(&continuation));
        assert!(!store_compaction(
            session_id,
            compaction_attempt,
            Vec::new()
        ));
        server.await.unwrap();
        websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    #[tokio::test]
    async fn dropping_live_response_body_after_first_chunk_aborts_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let session_id = "live-response-body-drop-cleanup";
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = live_test_request("one");
        let continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let compaction_attempt = begin_compaction(session_id, &request);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (socket_closed_tx, socket_closed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_live_websocket_request(&mut websocket).await;
            emit_live_event(
                &mut websocket,
                &serde_json::json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "message", "id": "msg-partial"}
                }),
            )
            .await;
            emit_live_event(
                &mut websocket,
                &serde_json::json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": "partial"
                }),
            )
            .await;
            while websocket.next().await.is_some() {}
            socket_closed_tx.send(()).unwrap();
        });
        let client = authenticated_live_test_client(format!("http://{addr}/responses"));

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            live_stream_response(
                client,
                "message".to_string(),
                &request.model,
                live_test_context(session_id),
                request.clone(),
                continuation.clone(),
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: Some(compaction_attempt),
                },
                config::CodexTransport::WebSocket,
                false,
            ),
        )
        .await
        .expect("live response did not publish the first chunk");
        let mut body = response.into_body();
        tokio::time::timeout(std::time::Duration::from_secs(10), body.frame())
            .await
            .expect("first downstream chunk timed out")
            .expect("live response body ended before the first chunk")
            .expect("first downstream chunk failed");
        drop(body);

        tokio::time::timeout(std::time::Duration::from_secs(10), socket_closed_rx)
            .await
            .expect("dropping the downstream body did not close the upstream socket")
            .expect("socket-close acknowledgement sender dropped");
        assert!(!continuation::is_current_turn_for_owner(&continuation));
        assert!(!store_compaction(
            session_id,
            compaction_attempt,
            Vec::new()
        ));
        server.await.unwrap();
        websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    #[tokio::test]
    async fn cancellation_while_replacement_startup_is_blocked_aborts_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let session_id = "live-blocked-replacement-cleanup";
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = live_test_request("one");
        let continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let compaction_attempt = begin_compaction(session_id, &request);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (replacement_accepted_tx, replacement_accepted_rx) = tokio::sync::oneshot::channel();
        let (release_replacement_tx, release_replacement_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (first_socket, _) = listener.accept().await.unwrap();
            let mut first_websocket = tokio_tungstenite::accept_async(first_socket).await.unwrap();
            let _ = next_live_websocket_request(&mut first_websocket).await;
            emit_live_event(
                &mut first_websocket,
                &serde_json::json!({
                    "type": "response.failed",
                    "response": {
                        "status": "failed",
                        "error": {
                            "status": 429,
                            "message": "rate limit exceeded",
                            "retry_after_seconds": 0
                        }
                    }
                }),
            )
            .await;
            drop(first_websocket);

            let (_replacement_socket, _) = listener.accept().await.unwrap();
            replacement_accepted_tx.send(()).unwrap();
            let _ = release_replacement_rx.await;
        });
        let client = authenticated_live_test_client(format!("http://{addr}/responses"));
        let task_request = request.clone();
        let task_continuation = continuation.clone();
        let response_task = tokio::spawn(async move {
            let model = task_request.model.clone();
            live_stream_response(
                client,
                "message".to_string(),
                &model,
                live_test_context(session_id),
                task_request,
                task_continuation,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: Some(compaction_attempt),
                },
                config::CodexTransport::WebSocket,
                false,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(10), replacement_accepted_rx)
            .await
            .expect("replacement startup did not reach the blocked handshake")
            .expect("replacement startup acknowledgement sender dropped");
        response_task.abort();
        assert!(response_task.await.unwrap_err().is_cancelled());
        let _ = release_replacement_tx.send(());
        server.await.unwrap();

        assert!(!continuation::is_current_turn_for_owner(&continuation));
        assert!(!store_compaction(
            session_id,
            compaction_attempt,
            Vec::new()
        ));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
    }
}
