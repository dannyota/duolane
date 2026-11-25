use super::client::CodexHttpClient;
use super::client_sse::{
    append_live_metadata_payload, event_stream_response, record_live_stream_downstream_capture,
    record_live_stream_first_output, record_live_stream_progress, single_live_stream_response,
    translate_live_stream_payload,
};
use super::compaction::CompactionAttempt;
#[cfg(test)]
use super::compaction::{begin_compaction, store_compaction};
use super::continuation::ContinuationReservation;
#[cfg(test)]
use super::continuation::continuation_candidate_for_owner;
use super::error_response::*;
use super::live_completion::*;
use super::recovery::live_retry_exhausted;
use super::request_state::*;
use super::translate::live_stream::LiveStreamTranslator;
#[cfg(test)]
use super::translate::reasoning_signature::ReasoningAccount;
#[cfg(test)]
use super::translate::request::{TranslateOptions, translate_request};
use super::*;
use crate::config;
use crate::logging::create_logger;
use crate::provider::RequestContext;
#[cfg(test)]
use crate::request_identity::ConversationIdentity;
use crate::retry::{compute_backoff_delay, sleep};
use axum::response::Response;
use bytes::Bytes;
use std::sync::Arc;
use std::time::{Duration, Instant};
const MAX_RETRYABLE_LIVE_STREAM_RETRIES: u32 = 3;
pub(super) const MAX_EMPTY_COMPLETION_RETRIES: u32 = 2;
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
    websocket_sent: Arc<std::sync::atomic::AtomicBool>,
) -> Response {
    if transport == config::CodexTransport::WebSocket {
        ctx.recovery_budget.enable_typed_websocket();
    }
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
    let http_body = client_body::CodexHttpBodyCache::with_websocket_sent(websocket_sent);
    loop {
        let upstream_events = match transport {
            config::CodexTransport::Http => {
                client
                    .stream_codex_http_events_for_owner_tracked(&request_body, &ctx, &http_body)
                    .await
            }
            config::CodexTransport::WebSocket => {
                client
                    .stream_codex_websocket_events_tracked(
                        &request_body,
                        &ctx,
                        continuation.as_ref(),
                        http_body.websocket_sent.clone(),
                    )
                    .await
            }
            config::CodexTransport::Auto => {
                client
                    .stream_codex_auto_events_for_owner(
                        &request_body,
                        &ctx,
                        &http_body,
                        continuation.as_ref(),
                        attempt == 0,
                    )
                    .await
            }
        };
        let upstream_events = match upstream_events {
            Ok(events) => events,
            Err(err) if err.origin == client::CodexErrorOrigin::Http => {
                cleanup.abort();
                return tracked_error_response(&ctx, &err);
            }
            Err(err) if retryable_live_start_codex_error(&err) => {
                empty_completion_attempts = 0;
                let dropped = drop_live_continuation_for_retry(&mut continuation);
                if dropped && is_missing_previous_response_error(&err) {
                    if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                        cleanup.abort();
                        return map_codex_error_to_response(
                            &client::CodexError::recovery_exhausted(),
                        );
                    }
                    attempt += 1;
                    continue;
                }
                if attempt >= MAX_RETRYABLE_LIVE_STREAM_RETRIES {
                    cleanup.abort();
                    return exhausted_response(&ctx, transport, &err);
                }
                let delay = compute_backoff_delay(attempt, err.retry_after.as_deref());
                if delay.exceeds_budget {
                    cleanup.abort();
                    return tracked_error_response(&ctx, &err);
                }
                if !ctx
                    .recovery_budget
                    .try_retry(Duration::from_millis(delay.wait_ms), false)
                {
                    cleanup.abort();
                    return exhausted_response(&ctx, transport, &err);
                }
                attempt += 1;
                sleep(delay.wait_ms).await;
                if ctx.recovery_budget.expired() {
                    cleanup.abort();
                    return exhausted_response(&ctx, transport, &err);
                }
                continue;
            }
            Err(err) => {
                cleanup.abort();
                return tracked_error_response(&ctx, &err);
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
                    return tracked_error_response(&ctx, &error);
                }
                if transport != config::CodexTransport::Http
                    && ctx.recovery_budget.is_tracked()
                    && ctx.recovery_budget.hosted()
                {
                    cleanup.abort();
                    return tracked_error_response(&ctx, &error);
                }
                if error.detail.as_deref() == Some(EMPTY_CODEX_COMPLETION_DETAIL) {
                    empty_completion_attempts += 1;
                } else {
                    empty_completion_attempts = 0;
                }
                let dropped = drop_live_continuation_for_retry(&mut continuation);
                if full_context_retry_attempted && is_missing_previous_response_error(&error) {
                    cleanup.abort();
                    return tracked_error_response(&ctx, &error);
                }
                if dropped && is_missing_previous_response_error(&error) {
                    if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                        cleanup.abort();
                        return map_codex_error_to_response(
                            &client::CodexError::recovery_exhausted(),
                        );
                    }
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
                    return exhausted_response(&ctx, transport, &error);
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
                    return tracked_error_response(&ctx, &error);
                }
                if !ctx.recovery_budget.try_retry(
                    Duration::from_millis(delay.wait_ms),
                    is_empty_completion_error(&error),
                ) {
                    cleanup.abort();
                    return exhausted_response(&ctx, transport, &error);
                }
                attempt += 1;
                sleep(delay.wait_ms).await;
                if transport != config::CodexTransport::Http && ctx.recovery_budget.hosted() {
                    cleanup.abort();
                    return tracked_error_response(&ctx, &error);
                }
                if ctx.recovery_budget.expired() {
                    cleanup.abort();
                    return exhausted_response(&ctx, transport, &error);
                }
            }
        }
    }
}
fn exhausted_response(
    ctx: &RequestContext,
    transport: config::CodexTransport,
    cause: &client::CodexError,
) -> Response {
    let exhausted = live_retry_exhausted(ctx, transport, cause);
    let mut response = map_codex_error_to_response(&exhausted);
    if ctx.recovery_budget.is_tracked() {
        response
            .extensions_mut()
            .insert(Arc::new(ctx.recovery_budget.failure(cause.clone())));
    }
    response
}
pub(super) fn tracked_error_response(ctx: &RequestContext, error: &client::CodexError) -> Response {
    let mut response = map_codex_error_to_response(error);
    if ctx.recovery_budget.is_tracked() {
        response.extensions_mut().insert(Arc::new(
            ctx.recovery_budget.failure_with_cause(error.clone()),
        ));
    }
    response
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
    let is_http = upstream_events.is_http();
    let tracked_http = ctx.recovery_budget.is_tracked();
    let mut semantic_ready = false;
    let mut translator = LiveStreamTranslator::for_request(message_id, model, &request_body)
        .with_http_tool_commits(is_http);
    let mut upstream_sse_body = Vec::new();
    let mut pending_chunk = Vec::new();
    let mut generation_started = false;
    let mut hosted_tool_executed = false;
    while let Some(item) = upstream_events.recv().await {
        hosted_tool_executed |= tracked_http && ctx.recovery_budget.hosted();
        let payload = match item {
            Ok(payload) => payload,
            Err(mut err) => {
                if !is_http && tracked_http {
                    ctx.recovery_budget.record_websocket_handshake_error(&err);
                    if err.origin == client::CodexErrorOrigin::WebSocket
                        && matches!(
                            err.detail.as_deref(),
                            Some(
                                websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL
                                    | client::TRANSPORT_UNEXPECTED_EOF_DETAIL
                                    | websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL
                            )
                        )
                    {
                        ctx.recovery_budget.body_uncertain();
                        err.origin = client::CodexErrorOrigin::WebSocketTransient;
                    }
                }
                if !hosted_tool_executed
                    && !(tracked_http && ctx.recovery_budget.hosted())
                    && !(tracked_http
                        && (err.usage_limit.is_some() || ctx.recovery_budget.is_final_only()))
                    && retryable_live_start_codex_error(&err)
                {
                    return provider_retry(&upstream_events, err);
                }
                abort_request_state(
                    ctx.session_id.as_deref(),
                    &request_continuation,
                    compaction.attempt,
                );
                return LiveStreamStart::Response(tracked_error_response(&ctx, &err));
            }
        };
        hosted_tool_executed |= events::event_confirms_hosted_tool_execution(&payload);
        if tracked_http
            && !is_http
            && super::client_sse::trace_http_failure(&payload, &[], &ctx.recovery_budget).is_some()
        {
            abort_request_state(
                ctx.session_id.as_deref(),
                &request_continuation,
                compaction.attempt,
            );
            if let Some(mut failure) = events::classify_event_failure(&payload) {
                if failure.status >= 500 {
                    failure.status = 400;
                }
                return LiveStreamStart::Response(map_codex_event_failure_to_response(&failure));
            }
            return LiveStreamStart::Response(map_codex_failure_to_response("Invalid request"));
        }
        if !generation_started && codex_generation_event(&payload) {
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.generation_started(&ctx.req_id);
            }
            generation_started = true;
        }
        append_live_metadata_payload(&mut upstream_sse_body, &payload, is_http);
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
                    let mut response = usage_limit_response(&limit);
                    if tracked_http {
                        let origin = if is_http {
                            client::CodexErrorOrigin::BufferedHttp
                        } else {
                            client::CodexErrorOrigin::BufferedWebSocket
                        };
                        let error = client::codex_usage_limit_error(limit, origin);
                        response
                            .extensions_mut()
                            .insert(Arc::new(ctx.recovery_budget.failure(error)));
                    }
                    return LiveStreamStart::Response(response);
                }
                if let Some(failure) = events::classify_event_failure(&payload) {
                    if !hosted_tool_executed
                        && !(tracked_http && ctx.recovery_budget.hosted())
                        && failure.retryable()
                    {
                        return provider_retry(
                            &upstream_events,
                            codex_event_failure_error(
                                &failure,
                                super::recovery::websocket_event_origin(
                                    tracked_http && !is_http,
                                    &failure,
                                    &payload,
                                ),
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
        if tracked_http {
            semantic_ready |= super::client_sse::translated_content_ready(&chunk);
        }
        pending_chunk.extend_from_slice(&chunk);
        if is_codex_success_terminal_event(&payload)
            && !events::is_standard_max_output_tokens_incomplete(&payload)
            && !translator.has_semantic_output()
            && !(accept_empty && terminal_status_is_completed(&payload))
        {
            let error = empty_live_completion_error();
            if hosted_tool_executed || (tracked_http && ctx.recovery_budget.hosted()) {
                abort_request_state(
                    ctx.session_id.as_deref(),
                    &request_continuation,
                    compaction.attempt,
                );
                return LiveStreamStart::Response(tracked_error_response(&ctx, &error));
            }
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
            return provider_retry(&upstream_events, error);
        }
        if translator.has_semantic_output()
            && !pending_chunk.is_empty()
            && (!tracked_http || semantic_ready)
        {
            if tracked_http {
                ctx.recovery_budget.mark_committed();
            }
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
                return LiveStreamStart::Response(single_live_stream_response(pending_chunk, ctx));
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
            if tracked_http && is_codex_success_terminal_event(&payload) {
                ctx.recovery_budget.mark_committed();
            }
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
                return LiveStreamStart::Response(event_stream_response(
                    futures_util::stream::empty::<Result<Bytes, std::io::Error>>(),
                ));
            }
            record_live_stream_downstream_capture(&ctx, &pending_chunk);
            record_live_stream_progress(&ctx, &pending_chunk);
            return LiveStreamStart::Response(single_live_stream_response(pending_chunk, ctx));
        }
    }
    let mut error = websocket::missing_terminal_error();
    if tracked_http && !is_http {
        ctx.recovery_budget.body_uncertain();
        error.origin = client::CodexErrorOrigin::WebSocketTransient;
    }
    if hosted_tool_executed || (tracked_http && ctx.recovery_budget.hosted()) {
        abort_request_state(
            ctx.session_id.as_deref(),
            &request_continuation,
            compaction.attempt,
        );
        return LiveStreamStart::Response(tracked_error_response(&ctx, &error));
    }
    provider_retry(&upstream_events, error)
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
    let is_http = upstream_events.is_http();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let stream_ctx = ctx.clone();
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
                    append_live_metadata_payload(&mut upstream_sse_body, &payload, is_http);
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
    let stream = futures_util::stream::unfold((rx, stream_ctx), |(mut rx, ctx)| async {
        rx.recv().await.map(|item| {
            if let Ok(chunk) = &item {
                record_live_stream_first_output(&ctx, chunk);
            }
            (item, (rx, ctx))
        })
    });
    event_stream_response(stream)
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use http_body_util::BodyExt;
    use tokio::net::TcpListener;

    use super::super::provider_test_support::*;
    use super::*;

    #[tokio::test]
    async fn tracked_http_terminal_hosted_and_context_use_one_send() {
        use super::super::client_test_support::{http_test_auth, http_test_client};
        use super::super::sources::{ReplayGate, SourceAttempt};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for hosted in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 16384];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let body: &[u8] = if hosted {
                    b"data: {\"type\":\"response.failed\",\"status_code\":503,\"response\":{\"output\":[{\"type\":\"web_search_call\"}],\"error\":{\"message\":\"upstream failed\"}}}\n\n"
                } else {
                    b"data: {\"type\":\"error\",\"status_code\":500,\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"too long\"}}\n\n"
                };
                stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(400), listener.accept())
                        .await
                        .is_err()
                );
            });
            let (_, _, base, mut request_body) = empty_terminal_fixture();
            request_body.reasoning_account = ReasoningAccount::new(Some("acct"));
            let gate = ReplayGate::new(hosted);
            let attempt = SourceAttempt::new(
                crate::config::codex_auth::AuthSource::ChatGpt,
                &base,
                gate.clone(),
            );
            let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
            client.auth_manager().set_test_auth(http_test_auth());
            let events = client
                .stream_codex_http_events_for_owner_tracked(
                    &request_body,
                    &attempt.context,
                    &Default::default(),
                )
                .await
                .unwrap();
            let result = live_stream_response_once(
                events,
                "msg_test".to_string(),
                "gpt-test",
                attempt.context,
                ContinuationReservation::for_owner_turn(None, None),
                request_body,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::Http,
                false,
            )
            .await;
            let LiveStreamStart::Response(response) = result else {
                panic!("terminal failure must not retry");
            };
            assert!(!gate.committed());
            if hosted {
                assert!(gate.hosted());
            } else {
                assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"]["type"], "invalid_request_error");
                assert!(
                    body["error"]["message"]
                        .as_str()
                        .unwrap()
                        .starts_with("prompt is too long")
                );
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn tracked_hosted_prelude_never_retries_or_commits_held_search() {
        use super::super::sources::{ReplayGate, SourceAttempt};
        for case in 0..3 {
            let (_, _, base, request_body) = empty_terminal_fixture();
            let gate = ReplayGate::new(true);
            let attempt = SourceAttempt::new(
                crate::config::codex_auth::AuthSource::ChatGpt,
                &base,
                gate.clone(),
            );
            gate.mark_hosted();
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            if case == 2 {
                tx.send(Ok(serde_json::json!({"type":"response.created"})))
                    .await
                    .unwrap();
                tx.send(Ok(serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"s"}}))).await.unwrap();
            }
            let payload = if case == 1 {
                serde_json::json!({"type":"response.completed", "response":{"id":"r", "status":"completed", "output":[{"type":"web_search_call"}]}})
            } else {
                serde_json::json!({"type":"response.failed", "status_code":503, "response":{"output":[{"type":"web_search_call"}],"error":{"message":"upstream failed"}}})
            };
            tx.send(Ok(payload)).await.unwrap();
            let result = live_stream_response_once(
                websocket::CodexWebSocketEventStream::http(rx),
                "msg_test".to_string(),
                "gpt-test",
                attempt.context,
                ContinuationReservation::for_owner_turn(None, None),
                request_body,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::Http,
                false,
            )
            .await;
            assert!(matches!(result, LiveStreamStart::Response(_)));
            assert!(!gate.committed());
        }
    }

    #[tokio::test]
    async fn tracked_context_code_keeps_invalid_request_envelope() {
        let (_, _, base, _) = empty_terminal_fixture();
        let gate = super::super::sources::ReplayGate::new(false);
        let attempt = super::super::sources::SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &base,
            gate,
        );
        let payload = serde_json::json!({"type":"error","status_code":500,"error":{"code":"context_length_exceeded","message":"too long"}});
        let error = super::super::client_sse::trace_http_failure(
            &payload,
            &[],
            &attempt.context.recovery_budget,
        )
        .unwrap();
        let response = map_codex_error_to_response(&error);
        assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("prompt is too long")
        );
    }

    #[tokio::test]
    async fn tracked_websocket_quota_preserves_unknown_send_and_ambiguity() {
        use super::super::api::SendEvidence;
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let (tx, rx, base, request_body) = empty_terminal_fixture();
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &base,
            gate.clone(),
        );
        attempt
            .context
            .recovery_budget
            .set_send(SendEvidence::OutcomeUnknown);
        tx.send(Ok(serde_json::json!({"type":"error", "error":{"type":"usage_limit_reached","message":"limit"}}))).await.unwrap();
        let result = live_stream_response_once(
            rx,
            "msg_test".to_string(),
            "gpt-test",
            attempt.context.clone(),
            ContinuationReservation::for_owner_turn(None, None),
            request_body,
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::WebSocket,
            false,
        )
        .await;
        assert!(matches!(result, LiveStreamStart::Response(_)));
        let failure = attempt.failure(client::CodexError::recovery_exhausted());
        assert_eq!(failure.kind, FailureKind::UsageLimit);
        assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
        assert!(gate.ambiguous_hosted());
        assert!(!gate.committed());
        assert!(!gate.may_switch(failure.kind, failure.send));
    }

    #[tokio::test]
    async fn tracked_auto_websocket_quota_preserves_unknown_send_and_ambiguity() {
        use super::super::api::SendEvidence;
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let (tx, rx, base, request_body) = empty_terminal_fixture();
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &base,
            gate.clone(),
        );
        attempt
            .context
            .recovery_budget
            .set_send(SendEvidence::OutcomeUnknown);
        tx.send(Ok(serde_json::json!({"type":"error", "status":401, "error":{"type":"usage_limit_reached","message":"limit","resets_at":1234567890}}))).await.unwrap();
        let result = live_stream_response_once(
            rx,
            "msg_test".to_string(),
            "gpt-test",
            attempt.context.clone(),
            ContinuationReservation::for_owner_turn(None, None),
            request_body,
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::Auto,
            false,
        )
        .await;
        let LiveStreamStart::Response(response) = result else {
            panic!("response expected")
        };
        let failure = response
            .extensions()
            .get::<std::sync::Arc<super::super::sources::SourceFailure>>()
            .expect("typed failure");
        assert!(failure.kind == FailureKind::UsageLimit);
        assert!(
            failure
                .error
                .usage_limit
                .as_ref()
                .and_then(|limit| limit.resets_at)
                == Some(1234567890)
        );
        let failure = attempt.failure(client::CodexError::recovery_exhausted());
        assert_eq!(failure.kind, FailureKind::UsageLimit);
        assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
        assert!(gate.ambiguous_hosted());
        assert!(!gate.committed());
        assert!(!gate.may_switch(failure.kind, failure.send));
    }

    #[tokio::test]
    async fn tracked_websocket_unknown_terminal_does_not_become_transient() {
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let (tx, rx, base, body) = empty_terminal_fixture();
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &base,
            ReplayGate::new(false),
        );
        tx.send(Ok(serde_json::json!({"type":"response.failed","response":{"error":{"message":"mystery"}}}))).await.unwrap();
        let LiveStreamStart::Retry { error, .. } = live_stream_response_once(
            rx,
            "m".into(),
            "gpt-test",
            attempt.context.clone(),
            ContinuationReservation::for_owner_turn(None, None),
            body,
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::WebSocket,
            false,
        )
        .await
        else {
            panic!("legacy retry expected")
        };
        let exhausted =
            live_retry_exhausted(&attempt.context, config::CodexTransport::WebSocket, &error);
        assert_eq!(attempt.failure(exhausted).kind, FailureKind::Final);
    }

    #[tokio::test]
    async fn unknown_terminal_does_not_poison_later_websocket_drop() {
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let (tx, rx, base, body) = empty_terminal_fixture();
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &base,
            gate.clone(),
        );
        tx.send(Ok(serde_json::json!({"type":"response.failed","response":{"error":{"message":"mystery"}}}))).await.unwrap();
        let LiveStreamStart::Retry { .. } = live_stream_response_once(
            rx,
            "m".into(),
            "gpt-test",
            attempt.context.clone(),
            ContinuationReservation::for_owner_turn(None, None),
            body.clone(),
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::WebSocket,
            false,
        )
        .await
        else {
            panic!("retry expected")
        };
        let (tx, rx, _, _) = empty_terminal_fixture();
        tx.send(Err(websocket::missing_terminal_error()))
            .await
            .unwrap();
        let LiveStreamStart::Retry { error, .. } = live_stream_response_once(
            rx,
            "m".into(),
            "gpt-test",
            attempt.context.clone(),
            ContinuationReservation::for_owner_turn(None, None),
            body,
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::WebSocket,
            false,
        )
        .await
        else {
            panic!("retry expected")
        };
        let exhausted =
            live_retry_exhausted(&attempt.context, config::CodexTransport::WebSocket, &error);
        assert_eq!(
            attempt.failure(exhausted).kind,
            FailureKind::TransientExhausted
        );
        assert!(gate.ambiguous_hosted());
    }

    #[tokio::test]
    async fn tracked_websocket_transient_and_empty_have_distinct_exhaustion() {
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        for transient in [true, false] {
            let (tx, rx, base, body) = empty_terminal_fixture();
            let attempt = SourceAttempt::new(
                crate::config::codex_auth::AuthSource::ChatGpt,
                &base,
                ReplayGate::new(false),
            );
            let payload = if transient {
                serde_json::json!({"type":"error","status_code":503,"error":{"message":"Gateway timeout"}})
            } else {
                serde_json::json!({"type":"response.completed","response":{"status":"completed","output":[]}})
            };
            tx.send(Ok(payload)).await.unwrap();
            let LiveStreamStart::Retry { error, .. } = live_stream_response_once(
                rx,
                "m".into(),
                "gpt-test",
                attempt.context.clone(),
                ContinuationReservation::for_owner_turn(None, None),
                body,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::WebSocket,
                false,
            )
            .await
            else {
                panic!("retry expected")
            };
            let exhausted =
                live_retry_exhausted(&attempt.context, config::CodexTransport::WebSocket, &error);
            assert_eq!(
                attempt.failure(exhausted).kind,
                if transient {
                    FailureKind::TransientExhausted
                } else {
                    FailureKind::Final
                }
            );
        }
    }

    #[tokio::test]
    async fn tracked_websocket_commits_only_deliverable_output() {
        use super::super::sources::{ReplayGate, SourceAttempt};
        for output in [false, true] {
            let (tx, rx, base, body) = empty_terminal_fixture();
            let gate = ReplayGate::new(false);
            let attempt = SourceAttempt::new(
                crate::config::codex_auth::AuthSource::ChatGpt,
                &base,
                gate.clone(),
            );
            tx.send(Ok(serde_json::json!({"type":"response.created"})))
                .await
                .unwrap();
            if output {
                tx.send(Ok(serde_json::json!({"type":"response.output_text.delta","output_index":0,"delta":"hello"}))).await.unwrap();
            } else {
                tx.send(Ok(serde_json::json!({"type":"error","error":{"type":"usage_limit_reached","message":"limit"}}))).await.unwrap();
            }
            assert!(matches!(
                live_stream_response_once(
                    rx,
                    "m".into(),
                    "gpt-test",
                    attempt.context,
                    ContinuationReservation::for_owner_turn(None, None),
                    body,
                    LiveStreamCompaction {
                        compact_boundary: false,
                        attempt: None
                    },
                    config::CodexTransport::WebSocket,
                    false
                )
                .await,
                LiveStreamStart::Response(_)
            ));
            assert_eq!(gate.committed(), output);
        }
    }

    #[tokio::test]
    async fn tracked_websocket_permanent_event_preserves_envelope() {
        use super::super::sources::{ReplayGate, SourceAttempt};
        let (tx, rx, base, body) = empty_terminal_fixture();
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &base,
            ReplayGate::new(false),
        );
        tx.send(Ok(serde_json::json!({"type":"error","status_code":422,"error":{"code":"invalid_prompt","message":"invalid input","retry_after":7}}))).await.unwrap();
        let LiveStreamStart::Response(response) = live_stream_response_once(
            rx,
            "m".into(),
            "gpt-test",
            attempt.context,
            ContinuationReservation::for_owner_turn(None, None),
            body,
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::WebSocket,
            false,
        )
        .await
        else {
            panic!("expected terminal response")
        };
        assert_eq!(response.status(), 422);
        assert_eq!(
            response.headers().get(http::header::RETRY_AFTER).unwrap(),
            "7"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn tracked_http_commits_only_deliverable_output() {
        use super::super::sources::{ReplayGate, SourceAttempt};
        for case in 0..3 {
            let (_, _, base, request_body) = empty_terminal_fixture();
            let gate = ReplayGate::new(false);
            let attempt = SourceAttempt::new(
                crate::config::codex_auth::AuthSource::ChatGpt,
                &base,
                gate.clone(),
            );
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let payload = match case {
                0 => {
                    serde_json::json!({"type":"error", "error":{"type":"usage_limit_reached","message":"limit"}})
                }
                1 => {
                    serde_json::json!({"type":"response.output_text.delta", "output_index":0, "delta":"hello"})
                }
                _ => {
                    serde_json::json!({"type":"response.completed", "response":{"id":"r", "status":"completed", "output":[], "usage":{}}})
                }
            };
            tx.send(Ok(payload)).await.unwrap();
            let result = live_stream_response_once(
                websocket::CodexWebSocketEventStream::http(rx),
                "msg_test".to_string(),
                "gpt-test",
                attempt.context,
                ContinuationReservation::for_owner_turn(None, None),
                request_body,
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::Http,
                case == 2,
            )
            .await;
            assert!(matches!(result, LiveStreamStart::Response(_)));
            assert_eq!(gate.committed(), case != 0);
        }
    }

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
            recovery_budget: Default::default(),
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
    async fn first_output_is_recorded_only_when_body_is_polled() {
        for poll in [false, true] {
            let (tx, rx, mut ctx, request_body) = empty_terminal_fixture();
            let monitor = crate::monitor::MonitorHandle::new(10);
            monitor.request_started(
                &ctx.req_id,
                None,
                None,
                crate::monitor::EndpointKind::Messages,
            );
            ctx.monitor = Some(monitor.clone());
            tx.send(Ok(serde_json::json!({"type":"response.output_text.delta", "delta":"hello", "output_index":0, "content_index":0}))).await.unwrap();
            let LiveStreamStart::Response(response) = live_stream_response_once(
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
                false,
            )
            .await
            else {
                panic!("expected stream")
            };
            assert!(monitor.snapshot().active[0].first_output_at.is_none());
            if poll {
                use http_body_util::BodyExt as _;
                let mut body = response.into_body();
                assert!(body.frame().await.is_some());
                assert!(monitor.snapshot().active[0].first_output_at.is_some());
            } else {
                drop(response);
                assert!(monitor.snapshot().active[0].first_output_at.is_none());
            }
        }
    }

    #[tokio::test]
    async fn slow_body_reader_does_not_extend_generation_duration() {
        let (tx, rx, mut ctx, request_body) = empty_terminal_fixture();
        let monitor = crate::monitor::MonitorHandle::new(10);
        monitor.request_started(
            &ctx.req_id,
            None,
            None,
            crate::monitor::EndpointKind::Messages,
        );
        ctx.monitor = Some(monitor.clone());
        tx.send(Ok(serde_json::json!({"type":"response.output_text.delta", "delta":"hello", "output_index":0, "content_index":0}))).await.unwrap();
        let LiveStreamStart::Response(response) = live_stream_response_once(
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
            false,
        )
        .await
        else {
            panic!("expected stream")
        };
        tx.send(Ok(serde_json::json!({"type":"response.completed", "response":{"id":"r", "status":"completed", "usage":{"input_tokens":1,"output_tokens":2}}}))).await.unwrap();
        drop(tx);
        tokio::time::sleep(Duration::from_secs(2)).await;
        response.into_body().collect().await.unwrap();
        let duration = monitor.snapshot().active[0].generation_duration.unwrap();
        assert!(
            duration < Duration::from_secs(1),
            "generation took {duration:?}"
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
            recovery_budget: Default::default(),
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
            recovery_budget: Default::default(),
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
    async fn tracked_auto_http_startup_exhaustion_preserves_upstream_error() {
        use super::super::client_test_support::{
            authenticated_http_test_client, buffered_test_request, http_test_context,
            read_http_request,
        };
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt, SourceFailure};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut socket).await;
            use tokio::io::AsyncWriteExt;
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n").await.unwrap();
        });
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(false),
        );
        for _ in 0..3 {
            assert!(
                attempt
                    .context
                    .recovery_budget
                    .try_retry(Duration::ZERO, false)
            );
        }
        let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        client.auto_ws_max_bytes = 1;
        let response = live_stream_response(
            Arc::new(client),
            "msg".into(),
            "gpt-test",
            attempt.context,
            buffered_test_request(),
            ContinuationReservation::for_owner_turn(None, None),
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::Auto,
            false,
            Arc::default(),
        )
        .await;
        let failure = response
            .extensions()
            .get::<Arc<SourceFailure>>()
            .expect("typed source failure");
        assert!(failure.kind == FailureKind::TransientExhausted);
        assert!(failure.error.origin == client::CodexErrorOrigin::Http);
        assert_eq!(failure.error.status, 503);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_auto_live_exhaustion_preserves_upstream_error() {
        use super::super::client_test_support::{
            authenticated_http_test_client, buffered_test_request, http_test_context,
        };
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt, SourceFailure};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            next_live_websocket_request(&mut ws).await;
            emit_live_event(&mut ws, &serde_json::json!({"type":"response.created"})).await;
            emit_live_event(&mut ws, &serde_json::json!({"type":"error", "status":503, "error":{"code":"server_error","message":"upstream busy"}})).await;
        });
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(false),
        );
        for _ in 0..3 {
            assert!(
                attempt
                    .context
                    .recovery_budget
                    .try_retry(Duration::ZERO, false)
            );
        }
        let response = live_stream_response(
            Arc::new(authenticated_http_test_client(format!(
                "http://{addr}/responses"
            ))),
            "msg".into(),
            "gpt-test",
            attempt.context,
            buffered_test_request(),
            ContinuationReservation::for_owner_turn(None, None),
            LiveStreamCompaction {
                compact_boundary: false,
                attempt: None,
            },
            config::CodexTransport::Auto,
            false,
            Arc::default(),
        )
        .await;
        assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        let failure = response
            .extensions()
            .get::<Arc<SourceFailure>>()
            .expect("typed source failure");
        assert!(failure.kind == FailureKind::TransientExhausted);
        assert_eq!(failure.error.status, 503);
        assert!(failure.error.origin == client::CodexErrorOrigin::WebSocketTransient);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn live_websocket_retries_tls_eof_before_output() {
        check_live_websocket_tls_eof(false).await;
    }

    #[tokio::test]
    async fn live_websocket_does_not_replay_tls_eof_after_output() {
        check_live_websocket_tls_eof(true).await;
    }

    #[tokio::test]
    async fn live_websocket_does_not_replay_tls_eof_after_hosted_search() {
        assert_no_websocket_replay_after_hosted_search(
            serde_json::json!({
                "type": "response.web_search_call.completed",
                "item_id": "search_1",
                "output_index": 0
            }),
            |client| async move {
                let req = live_test_request("search");
                let model = req.model.clone();
                live_stream_response(
                    Arc::new(client),
                    "message".to_string(),
                    &model,
                    live_test_context("hosted-search-live"),
                    req,
                    ContinuationReservation::for_owner_turn(None, None),
                    LiveStreamCompaction {
                        compact_boundary: false,
                        attempt: None,
                    },
                    config::CodexTransport::WebSocket,
                    false,
                    Arc::default(),
                )
                .await
                .status()
                .as_u16()
            },
        )
        .await;
    }

    async fn check_live_websocket_tls_eof(after_output: bool) {
        use super::super::client_test_support::{http_test_auth, loopback_tls};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tls, http) = loopback_tls();
        let mut client = CodexHttpClient::new_for_test(
            http.clone(),
            format!("https://{addr}/responses"),
            1_000,
            1_000,
        );
        client.websocket_client = http;
        client.auth_manager().set_test_auth(http_test_auth());
        let (visible_tx, visible_rx) = tokio::sync::oneshot::channel();
        let server = async {
            for attempt in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let stream = tls.accept(socket).await.unwrap();
                let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let _ = next_live_websocket_request(&mut websocket).await;
                if after_output || attempt == 1 {
                    for event in [
                        serde_json::json!({
                            "type": "response.output_item.added",
                            "output_index": 0,
                            "item": {"type": "message", "id": "upstream-message"}
                        }),
                        serde_json::json!({
                            "type": "response.output_text.delta",
                            "output_index": 0,
                            "delta": "visible"
                        }),
                    ] {
                        emit_live_event(&mut websocket, &event).await;
                    }
                }
                if attempt == 1 {
                    emit_live_event(
                        &mut websocket,
                        &serde_json::json!({
                            "type": "response.completed",
                            "response": {"id": "upstream-response", "status": "completed", "output": []}
                        }),
                    )
                    .await;
                }
                if after_output {
                    visible_rx.await.unwrap();
                    break;
                }
            }
        };
        let run = async {
            let request = live_test_request("one");
            let model = request.model.clone();
            let response = live_stream_response(
                Arc::new(client),
                "message".to_string(),
                &model,
                live_test_context("tls-eof"),
                request,
                ContinuationReservation::for_owner_turn(None, None),
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: None,
                },
                config::CodexTransport::WebSocket,
                false,
                Arc::default(),
            )
            .await;
            assert_eq!(response.status(), http::StatusCode::OK);
            let mut body = response.into_body();
            let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
            assert!(std::str::from_utf8(&first).unwrap().contains("visible"));
            if after_output {
                visible_tx.send(()).unwrap();
            }
            let remaining = body.collect().await.unwrap().to_bytes();
            let sse = std::str::from_utf8(&remaining).unwrap();
            assert_eq!(sse.contains("event: error"), after_output);
            assert_eq!(sse.contains("event: message_stop"), !after_output);
        };
        tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(run, server) })
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
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
                Arc::default(),
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
                Arc::default(),
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
                Arc::default(),
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
