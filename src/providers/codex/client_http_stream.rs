use super::client::*;
use super::client_body::CodexHttpBodyCache;
use super::client_http_tools::{HttpToolState, STALLED_READ_FINISH_FIELD};
use super::client_sse::*;
#[cfg(test)]
pub use super::events::CodexLimitWindow;
use super::translate::request::ResponsesRequest;
use crate::logging::create_logger;
use crate::provider::RequestContext;
use crate::retry::{compute_backoff_delay, sleep};
use std::sync::Arc;
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;
impl CodexHttpClient {
    #[cfg(test)]
    async fn stream_codex_http_events(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        http_body: &CodexHttpBodyCache,
    ) -> Result<CodexHttpEventReceiver, CodexError> {
        self.stream_codex_http_events_inner(body, ctx, http_body, false)
            .await
    }
    async fn stream_codex_http_events_inner(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        http_body: &CodexHttpBodyCache,
        tracked: bool,
    ) -> Result<CodexHttpEventReceiver, CodexError> {
        if tracked {
            ctx.recovery_budget.stamp(
                super::sources::FailureKind::Final,
                super::api::SendEvidence::NotSent,
                None,
            );
        }
        let mut auth = if tracked {
            self.auth_for_buffered_http(&body.reasoning_account, &ctx.recovery_budget)
                .await?
        } else {
            self.auth_for(&body.reasoning_account).await?
        };
        let quota = ctx.monitor.as_ref().and_then(|monitor| {
            let account = super::translate::reasoning_signature::ReasoningAccount::new(
                auth.account_id.as_deref(),
            );
            monitor.bind_codex_quota(&ctx.req_id, &account)
        });
        let http_body = http_body
            .get_or_try_init(|| {
                self.prepare_http_body(body, ctx.traffic.is_some(), "Failed to serialize request")
            })
            .await?;
        let mut auth_refresh_attempted = false;
        let use_responses_lite = body.client_metadata.is_some();
        let mut retries = 0_u32;
        let (resp, response_headers, started_at) = loop {
            match self
                .start_http_event_attempt(
                    &mut auth,
                    http_body,
                    ctx,
                    use_responses_lite,
                    &mut auth_refresh_attempted,
                    tracked,
                )
                .await
            {
                Ok(attempt) => break attempt,
                Err(error)
                    if retryable_http_stream_error(&error)
                        && !(tracked
                            && (ctx.recovery_budget.is_final_only()
                                || ctx.recovery_budget.hosted())) =>
                {
                    ctx.recovery_budget.record_cause(&error);
                    if retries >= MAX_BUFFERED_TRANSPORT_RETRIES {
                        return Err(if tracked {
                            ctx.recovery_budget.transient_exhausted()
                        } else {
                            CodexError::recovery_exhausted()
                        });
                    }
                    let delay = compute_backoff_delay(retries, error.retry_after.as_deref());
                    if delay.exceeds_budget {
                        return Err(error);
                    }
                    if !ctx
                        .recovery_budget
                        .try_retry(Duration::from_millis(delay.wait_ms), false)
                    {
                        return Err(if tracked {
                            ctx.recovery_budget.transient_exhausted()
                        } else {
                            CodexError::recovery_exhausted()
                        });
                    }
                    retries += 1;
                    sleep(delay.wait_ms).await;
                    if ctx.recovery_budget.expired() {
                        return Err(if tracked {
                            ctx.recovery_budget.transient_exhausted()
                        } else {
                            CodexError::recovery_exhausted()
                        });
                    }
                }
                Err(error) => return Err(error),
            }
        };
        Ok(self.spawn_http_event_stream(
            HttpEventStreamState {
                resp,
                response_headers,
                started_at,
                http_body: http_body.clone(),
                auth,
                auth_refresh_attempted,
                use_responses_lite,
                retries,
                tracked,
            },
            ctx.clone(),
            quota,
        ))
    }
    pub(super) async fn stream_codex_http_events_for_owner_tracked(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        http_body: &CodexHttpBodyCache,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        let receiver = self
            .stream_codex_http_events_inner(body, ctx, http_body, ctx.recovery_budget.is_tracked())
            .await?;
        Ok(super::websocket::CodexWebSocketEventStream::http(receiver))
    }
    pub(super) async fn stream_codex_auto_events_for_owner(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        http_body: &CodexHttpBodyCache,
        continuation: Option<&super::continuation::ContinuationReservation>,
        allow_size_routing: bool,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        if http_body
            .size_http_selected
            .load(std::sync::atomic::Ordering::Acquire)
            || (allow_size_routing
                && !http_body
                    .websocket_sent
                    .load(std::sync::atomic::Ordering::Acquire)
                && self
                    .auto_large_full_context(body, ctx, continuation, http_body)
                    .await?)
        {
            super::client_live::invalidate_live_continuation_pool(continuation);
            http_body
                .size_http_selected
                .store(true, std::sync::atomic::Ordering::Release);
            return self
                .stream_codex_http_events_for_owner_tracked(body, ctx, http_body)
                .await;
        }
        ctx.recovery_budget.enable_typed_websocket();
        ctx.recovery_budget.begin_websocket();
        let mut prepared = None;
        if allow_size_routing
            && !http_body
                .websocket_sent
                .load(std::sync::atomic::Ordering::Acquire)
            && self.auto_http_fallback_enabled
            && self.request_compression == crate::config::CodexRequestCompression::Gzip
            && self.auto_ws_max_bytes != 0
            && continuation.is_some_and(|c| c.candidate().previous_response_id.is_some())
        {
            let auth = if ctx.recovery_budget.is_tracked() {
                self.auth_for_buffered_http(&body.reasoning_account, &ctx.recovery_budget)
                    .await
            } else {
                self.auth_for(&body.reasoning_account).await
            }
            .inspect_err(|error| {
                if error.detail.as_deref() == Some(CODEX_ACCOUNT_CHANGED_DETAIL) {
                    super::client_live::invalidate_live_continuation_pool(continuation);
                }
            })?;
            let quota = super::client_live::quota_for_auth(ctx, &auth);
            if super::client_live::should_reset_websocket_pool(continuation)
                && let Some(owner) = super::client_live::websocket_pool_owner(continuation)
            {
                super::websocket::invalidate_codex_websocket_pool_turn_for_owner(
                    owner,
                    continuation.and_then(super::continuation::ContinuationReservation::turn_id),
                );
            }
            let headers = build_codex_headers(&auth, ctx, body.client_metadata.is_some())?;
            let headers = super::websocket::codex_websocket_headers(&headers);
            match super::websocket::prepare_codex_websocket(
                &self.websocket_client,
                &self.websocket_proxy_config,
                &self.base_url,
                &headers,
                Some(super::sources::CredentialId::for_chatgpt_auth(&auth)),
                ctx.traffic.clone(),
                continuation,
                super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                &ctx.recovery_budget,
            )
            .await
            {
                Ok(ready) => prepared = Some((auth, ready, quota)),
                Err(error)
                    if error.detail.as_deref()
                        == Some(super::websocket::WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL) =>
                {
                    if self
                        .auto_large_full_context(body, ctx, None, http_body)
                        .await?
                    {
                        if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                            return Err(CodexError::recovery_exhausted());
                        }
                        super::client_live::invalidate_live_continuation_pool(continuation);
                        http_body
                            .size_http_selected
                            .store(true, std::sync::atomic::Ordering::Release);
                        return self
                            .stream_codex_http_events_for_owner_tracked(body, ctx, http_body)
                            .await;
                    }
                }
                Err(_) => {}
            }
        }
        let mut websocket = self
            .stream_codex_websocket_events_prepared(
                body,
                ctx,
                continuation,
                http_body.websocket_sent.clone(),
                prepared,
            )
            .await?;
        match websocket.recv().await {
            Some(Err(err))
                if self.auto_http_fallback_enabled
                    && !ctx.recovery_budget.hosted()
                    && !http_body
                        .websocket_sent
                        .load(std::sync::atomic::Ordering::Acquire)
                    && should_fallback_to_http(&err) =>
            {
                if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                    return Err(CodexError::recovery_exhausted());
                }
                super::client_live::invalidate_live_continuation_pool(continuation);
                self.stream_codex_http_events_for_owner_tracked(body, ctx, http_body)
                    .await
            }
            Some(item) => Ok(websocket.forward_auto(item, continuation.cloned())),
            None => Err(CodexError {
                status: 0,
                message: "WebSocket connection closed before the first Codex event".to_string(),
                detail: Some(super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::WebSocket,
            }),
        }
    }
    fn spawn_http_event_stream(
        self: &Arc<Self>,
        state: HttpEventStreamState,
        ctx: RequestContext,
        quota: Option<crate::monitor::CodexQuotaObserver>,
    ) -> CodexHttpEventReceiver {
        let HttpEventStreamState {
            mut resp,
            mut response_headers,
            mut started_at,
            http_body,
            mut auth,
            mut auth_refresh_attempted,
            use_responses_lite,
            mut retries,
            tracked,
        } = state;
        let client = self.clone();
        let body_idle_timeout_ms = self.body_idle_timeout_ms;
        let req_id = ctx.req_id.clone();
        let traffic = ctx.traffic.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            let log = create_logger("codex");
            let mut semantic_output_forwarded = false;
            let mut hosted_tool_executed = false;
            if tx
                .send(Ok(serde_json::json!({
                    "type": "keepalive",
                    "_duolane_synthetic": true
                })))
                .await
                .is_err()
            {
                return;
            }
            'attempts: loop {
                let mut decoder = HttpSseDecoder::default();
                let mut body_bytes = 0_u64;
                let mut body_chunks = 0_u64;
                let mut event_count = 0_u64;
                let mut pending_events = Vec::new();
                let mut tool_state = HttpToolState::default();
                let mut retry_error = 'read_attempt: loop {
                    let chunk = tokio::select! {
                        _ = tx.closed() => {
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_dropped",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                None,
                            );
                            return;
                        }
                        chunk = tokio::time::timeout(
                            Duration::from_millis(body_idle_timeout_ms),
                            resp.chunk(),
                        ) => chunk
                    };
                    let chunk = match chunk {
                        Ok(Ok(Some(chunk))) => chunk,
                        Ok(Ok(None)) => {
                            if tracked {
                                ctx.recovery_budget.failed_http_body(&decoder);
                                hosted_tool_executed |= ctx.recovery_budget.hosted();
                            }
                            let error = match decoder.finish() {
                                Ok(()) => http_sse_error(
                                    "Codex SSE stream ended before a terminal response event",
                                ),
                                Err(error) => error,
                            };
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !semantic_output_forwarded
                                && !hosted_tool_executed
                                && retryable_http_stream_error(&error)
                            {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                        Ok(Err(err)) => {
                            if tracked {
                                ctx.recovery_budget.failed_http_body(&decoder);
                                hosted_tool_executed |= ctx.recovery_budget.hosted();
                            }
                            let error = CodexError {
                                status: 0,
                                message: format!(
                                    "Transport error reading Codex response body: {err}"
                                ),
                                detail: Some("http_response_body".to_string()),
                                retry_after: None,
                                usage_limit: None,
                                origin: CodexErrorOrigin::Http,
                            };
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !semantic_output_forwarded && !hosted_tool_executed {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                        Err(_) => {
                            if tracked {
                                ctx.recovery_budget.failed_http_body(&decoder);
                                hosted_tool_executed |= ctx.recovery_budget.hosted();
                            }
                            let error = CodexError {
                                status: 0,
                                message: format!(
                                    "Timed out waiting {body_idle_timeout_ms}ms for the next Codex response body chunk"
                                ),
                                detail: Some("http_response_body".to_string()),
                                retry_after: None,
                                usage_limit: None,
                                origin: CodexErrorOrigin::Http,
                            };
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_failed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                Some(&error.message),
                            );
                            if !semantic_output_forwarded && !hosted_tool_executed {
                                break 'read_attempt error;
                            }
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    };

                    body_bytes = body_bytes.saturating_add(chunk.len() as u64);
                    body_chunks = body_chunks.saturating_add(1);
                    let mut events = Vec::new();
                    if let Err(error) = decoder.push(&chunk, &mut events) {
                        hosted_tool_executed |= events
                            .iter()
                            .filter_map(|event| event.payload.as_ref())
                            .any(|payload| {
                                if tracked {
                                    payload_confirms_hosted(payload)
                                } else {
                                    super::events::event_confirms_hosted_tool_execution(payload)
                                }
                            });
                        if tracked {
                            ctx.recovery_budget.failed_http_body(&decoder);
                            ctx.recovery_budget.mark_final_only();
                            if hosted_tool_executed {
                                ctx.recovery_budget.mark_hosted();
                            }
                        }
                        log_http_stream_end(
                            &log,
                            "codex_http_stream_failed",
                            &req_id,
                            started_at,
                            body_bytes,
                            body_chunks,
                            event_count,
                            Some(&error.message),
                        );
                        if !semantic_output_forwarded
                            && !hosted_tool_executed
                            && retryable_http_stream_error(&error)
                            && !(tracked && ctx.recovery_budget.is_final_only())
                        {
                            break 'read_attempt error;
                        }
                        let _ = tx.send(Err(error)).await;
                        return;
                    }

                    for event in events {
                        let Some(mut payload) = event.payload else {
                            continue;
                        };
                        if let Some(quota) = &quota {
                            quota.observe(&payload);
                        }
                        event_count = event_count.saturating_add(1);
                        if let Some(traffic) = traffic.as_deref() {
                            write_codex_http_sse_event_capture(
                                traffic,
                                event.event.as_deref(),
                                &payload,
                            );
                        }

                        hosted_tool_executed |= http_payload_hosted(&payload, tracked);
                        if tracked && hosted_tool_executed {
                            ctx.recovery_budget.mark_hosted();
                        }
                        let failure = super::events::classify_event_failure(&payload);
                        if failure.is_none() {
                            match tool_state.observe(&mut payload) {
                                Ok(false) => continue,
                                Ok(true) => {}
                                Err(error) => {
                                    let _ = tx.send(Err(error)).await;
                                    return;
                                }
                            }
                        }
                        let event_kind = super::events::classify_stream_event(&payload);
                        if tracked
                            && failure.is_some()
                            && let Some(error) = trace_http_failure(
                                &payload,
                                &response_headers,
                                &ctx.recovery_budget,
                            )
                        {
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                        if !semantic_output_forwarded {
                            if let Some(limit) = super::events::usage_limit_from_event_with_headers(
                                &payload,
                                &response_headers,
                            ) {
                                pending_events.clear();
                                drop(tool_state);
                                let _ = tx
                                    .send(Err(codex_usage_limit_error(
                                        limit,
                                        CodexErrorOrigin::Http,
                                    )))
                                    .await;
                                return;
                            }
                            if let Some(failure) = failure.as_ref()
                                && failure.retryable()
                                && !hosted_tool_executed
                            {
                                if tracked {
                                    ctx.recovery_budget.body_uncertain();
                                }
                                pending_events.clear();
                                break 'read_attempt codex_event_failure_error(failure.clone());
                            }
                        }

                        let local_read_finish = payload["type"] == "response.output_item.done"
                            && super::events::is_function_call_item(&payload)
                            && payload[STALLED_READ_FINISH_FIELD].as_bool() == Some(true);
                        let terminal =
                            local_read_finish || super::events::event_is_terminal(&payload);
                        if terminal {
                            let tool_state = std::mem::take(&mut tool_state);
                            if event_kind == super::events::CodexStreamEventKind::TerminalSuccess
                                && let Err(error) = tool_state.validate_terminal(&payload)
                            {
                                drop(tool_state);
                                let _ = tx.send(Err(error)).await;
                                return;
                            }
                        }
                        match event_kind {
                            super::events::CodexStreamEventKind::TerminalFailure => {
                                if !semantic_output_forwarded {
                                    pending_events.clear();
                                }
                                if tx.send(Ok(payload)).await.is_err() {
                                    return;
                                }
                            }
                            super::events::CodexStreamEventKind::TerminalSuccess => {
                                for pending in pending_events.drain(..) {
                                    if tx.send(Ok(pending)).await.is_err() {
                                        return;
                                    }
                                }
                                if tx.send(Ok(payload)).await.is_err() {
                                    return;
                                }
                            }
                            super::events::CodexStreamEventKind::Semantic => {
                                if !semantic_output_forwarded {
                                    semantic_output_forwarded = true;
                                    for pending in pending_events.drain(..) {
                                        if tx.send(Ok(pending)).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                if tx.send(Ok(payload)).await.is_err() {
                                    return;
                                }
                            }
                            super::events::CodexStreamEventKind::Control => {
                                if tx.send(Ok(payload)).await.is_err() {
                                    return;
                                }
                            }
                            super::events::CodexStreamEventKind::Structural => {
                                if semantic_output_forwarded {
                                    if tx.send(Ok(payload)).await.is_err() {
                                        return;
                                    }
                                } else {
                                    pending_events.push(payload);
                                }
                            }
                        }

                        if terminal {
                            log_http_stream_end(
                                &log,
                                "codex_http_stream_completed",
                                &req_id,
                                started_at,
                                body_bytes,
                                body_chunks,
                                event_count,
                                None,
                            );
                            return;
                        }
                    }
                };

                drop(tool_state);
                drop(pending_events);
                loop {
                    ctx.recovery_budget.record_cause(&retry_error);
                    if retries >= MAX_BUFFERED_TRANSPORT_RETRIES {
                        let _ = tx
                            .send(Err(ctx.recovery_budget.http_exhausted(tracked)))
                            .await;
                        return;
                    }
                    let delay = compute_backoff_delay(retries, retry_error.retry_after.as_deref());
                    if delay.exceeds_budget {
                        let _ = tx.send(Err(retry_error)).await;
                        return;
                    }
                    if !ctx
                        .recovery_budget
                        .try_retry(Duration::from_millis(delay.wait_ms), false)
                    {
                        let _ = tx
                            .send(Err(ctx.recovery_budget.http_exhausted(tracked)))
                            .await;
                        return;
                    }
                    retries += 1;
                    tokio::select! {
                        _ = tx.closed() => return,
                        _ = sleep(delay.wait_ms) => {}
                    }
                    if ctx.recovery_budget.expired() {
                        let _ = tx
                            .send(Err(ctx.recovery_budget.http_exhausted(tracked)))
                            .await;
                        return;
                    }

                    let next_attempt = tokio::select! {
                        _ = tx.closed() => return,
                        result = client.start_http_event_attempt(
                            &mut auth,
                            &http_body,
                            &ctx,
                            use_responses_lite,
                            &mut auth_refresh_attempted,
                            tracked,
                        ) => result
                    };
                    match next_attempt {
                        Ok((next_resp, next_response_headers, next_started_at)) => {
                            resp = next_resp;
                            response_headers = next_response_headers;
                            started_at = next_started_at;
                            continue 'attempts;
                        }
                        Err(error)
                            if retryable_http_stream_error(&error)
                                && !(tracked
                                    && (ctx.recovery_budget.hosted()
                                        || ctx.recovery_budget.is_final_only())) =>
                        {
                            retry_error = error;
                        }
                        Err(error) => {
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    }
                }
            }
        });

        rx
    }
}
#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::super::client_test_support::*;

    async fn send_completed_websocket_response(
        websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        response_id: &str,
    ) {
        use futures_util::SinkExt;
        websocket
            .send(tokio_tungstenite::tungstenite::Message::text(
                serde_json::json!({"type":"response.created", "response":{"id":response_id}})
                    .to_string(),
            ))
            .await
            .unwrap();
        super::super::client_test_support::send_completed_websocket_response(
            websocket,
            response_id,
        )
        .await;
    }

    use super::*;

    async fn write_http_chunk(stream: &mut tokio::net::TcpStream, body: &[u8]) {
        stream
            .write_all(format!("{:x}\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        stream.write_all(body).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
        stream.flush().await.unwrap();
    }

    #[tokio::test]
    async fn failed_http_startup_clears_previous_account_quota() {
        use super::super::translate::reasoning_signature::ReasoningAccount;
        use crate::monitor::{EndpointKind, MonitorHandle};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut socket).await;
            let body = br#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#;
            let headers = format!(
                "HTTP/1.1 429 Too Many Requests\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        });
        let monitor = MonitorHandle::default();
        monitor.request_started("old", None, None, EndpointKind::Messages);
        let observer = monitor
            .bind_codex_quota("old", &ReasoningAccount::new(Some("old")))
            .unwrap();
        observer.observe(&serde_json::json!({"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":25}}}));
        assert!(monitor.snapshot().codex_quota.is_some());
        let mut ctx = http_test_context();
        monitor.request_started(&ctx.req_id, None, None, EndpointKind::Messages);
        ctx.monitor = Some(monitor.clone());
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let error = client
            .stream_codex_http_events(&buffered_test_request(), &ctx, &Default::default())
            .await
            .err()
            .unwrap();
        assert_eq!(error.status, 429);
        assert!(monitor.snapshot().codex_quota.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_live_http_startup_exhaustion_and_header_timeout() {
        use super::super::api::SendEvidence;
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        for timeout in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                for _ in 0..if timeout { 1 } else { 4 } {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = [0_u8; 16384];
                    assert!(stream.read(&mut request).await.unwrap() > 0);
                    if timeout {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    } else {
                        stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n").await.unwrap();
                    }
                }
            });
            let gate = ReplayGate::new(timeout);
            let attempt = SourceAttempt::new(
                crate::config::codex_auth::AuthSource::ChatGpt,
                &http_test_context(),
                gate.clone(),
            );
            let mut client = http_test_client(format!("http://{addr}/responses"), 1_000);
            client.header_timeout_ms = 50;
            let client = Arc::new(client);
            client.auth_manager().set_test_auth(http_test_auth());
            let error = client
                .stream_codex_http_events_for_owner_tracked(
                    &buffered_test_request(),
                    &attempt.context,
                    &Default::default(),
                )
                .await
                .err()
                .unwrap();
            let failure = attempt.failure(error);
            assert!(
                failure.kind
                    == if timeout {
                        FailureKind::Final
                    } else {
                        FailureKind::TransientExhausted
                    }
            );
            assert!(failure.send == SendEvidence::OutcomeUnknown);
            assert!(failure.credential.is_some());
            assert_eq!(gate.ambiguous_hosted(), timeout);
            assert_eq!(gate.may_switch(failure.kind, failure.send), !timeout);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn tracked_live_websocket_401_quota_does_not_probe_or_refresh() {
        use super::super::provider_test_support::{emit_live_event, next_live_websocket_request};
        use super::super::sources::{CredentialId, FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            next_live_websocket_request(&mut ws).await;
            emit_live_event(&mut ws, &serde_json::json!({"type":"error", "status":401, "error":{"type":"usage_limit_reached","message":"limit","resets_at":1234567890}})).await;
        });
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(false),
        );
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_websocket_events_tracked(
                &buffered_test_request(),
                &attempt.context,
                None,
                Arc::default(),
            )
            .await
            .unwrap();
        assert!(events.recv().await.expect("terminal event").is_ok());
        let failure = attempt.failure(CodexError::new(400, "later error".into()));
        assert!(failure.kind == FailureKind::Final);
        let expected = CredentialId::for_chatgpt_auth(&http_test_auth());
        assert!(failure.credential == Some(expected));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_live_websocket_401_permanent_precedes_refresh() {
        use super::super::provider_test_support::{emit_live_event, next_live_websocket_request};
        use super::super::sources::{ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            next_live_websocket_request(&mut ws).await;
            emit_live_event(&mut ws, &serde_json::json!({"type":"error", "status":401, "error":{"code":"invalid_prompt","message":"bad request"}})).await;
        });
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(false),
        );
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_websocket_events_tracked(
                &buffered_test_request(),
                &attempt.context,
                None,
                Arc::default(),
            )
            .await
            .unwrap();
        let item = events.recv().await.expect("terminal event");
        assert!(item.is_ok());
        assert!(attempt.context.recovery_budget.is_final_only());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_live_401_permanent_precedes_refresh() {
        use super::super::sources::{ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut socket).await;
            let body = br#"{"error":{"type":"invalid_request_error","message":"bad request"}}"#;
            let header = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        });
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(false),
        );
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let err = client
            .stream_codex_http_events_for_owner_tracked(
                &buffered_test_request(),
                &attempt.context,
                &Default::default(),
            )
            .await
            .err()
            .expect("permanent error");
        assert!(err.origin == CodexErrorOrigin::Http);
        assert_eq!(err.status, 401);
        assert!(attempt.context.recovery_budget.is_final_only());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_live_http_startup_denial_and_permanent_error() {
        use super::super::api::SendEvidence;
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        for permanent in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 16384];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let body: &[u8] = if permanent {
                    br#"{"error":{"type":"invalid_request_error","message":"invalid"}}"#
                } else {
                    br#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#
                };
                let status = if permanent {
                    "500 Internal Server Error"
                } else {
                    "429 Too Many Requests"
                };
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                stream.write_all(body).await.unwrap();
            });
            let gate = ReplayGate::new(true);
            let attempt = SourceAttempt::new(
                crate::config::codex_auth::AuthSource::ChatGpt,
                &http_test_context(),
                gate.clone(),
            );
            let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
            client.auth_manager().set_test_auth(http_test_auth());
            let error = client
                .stream_codex_http_events_for_owner_tracked(
                    &buffered_test_request(),
                    &attempt.context,
                    &Default::default(),
                )
                .await
                .err()
                .unwrap();
            let failure = attempt.failure(error);
            if permanent {
                assert!(failure.kind == FailureKind::Final);
                assert!(gate.ambiguous_hosted());
                assert!(!gate.may_switch(failure.kind, failure.send));
            } else {
                assert!(failure.kind == FailureKind::UsageLimit);
                assert!(failure.send == SendEvidence::DeniedBeforeExecution);
                assert!(failure.credential.is_some());
                assert!(gate.may_switch(failure.kind, failure.send));
            }
            server.await.unwrap();
        }
    }

    async fn startup_review_case(case: u8) {
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut count = 0;
            while let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(600), listener.accept()).await
            {
                count += 1;
                let mut request = [0_u8; 16384];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let (status, body, extra): (&str, &[u8], usize) = match case {
                    0 => ("503 Service Unavailable", b"data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"web_search_call\"}]}}\n\n", 0),
                    1 if count == 1 => ("429 Too Many Requests", b"{", 10),
                    1 => ("429 Too Many Requests", br#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#, 0),
                    _ => ("500 Internal Server Error", br#"{"error":{"message":"input exceeds context window"}}"#, 0),
                };
                stream.write_all(format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n", body.len()+extra).as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                if count == 4 {
                    break;
                }
            }
            count
        });
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
        client.auth_manager().set_test_auth(http_test_auth());
        let error = client
            .stream_codex_http_events_for_owner_tracked(
                &buffered_test_request(),
                &attempt.context,
                &Default::default(),
            )
            .await
            .err()
            .unwrap();
        let failure = attempt.failure(error);
        let count = server.await.unwrap();
        match case {
            0 => {
                assert!(gate.hosted());
                assert_eq!(count, 1);
            }
            1 => {
                assert!(gate.ambiguous_hosted());
                assert_eq!(count, 2);
                assert!(failure.kind == FailureKind::UsageLimit);
                assert!(!gate.may_switch(failure.kind, failure.send));
            }
            _ => {
                assert_eq!(count, 1);
                assert!(failure.kind == FailureKind::Final);
            }
        }
    }

    #[tokio::test]
    async fn tracked_startup_hosted_evidence_stops_retry() {
        startup_review_case(0).await;
    }

    #[tokio::test]
    async fn tracked_startup_truncated_denial_retains_ambiguity() {
        startup_review_case(1).await;
    }

    #[tokio::test]
    async fn tracked_startup_context_message_is_final() {
        startup_review_case(2).await;
    }

    async fn tracked_live_body_case(hosted: bool) {
        use super::super::api::SendEvidence;
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut count = 0;
            while let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(700), listener.accept()).await
            {
                count += 1;
                let mut request = [0_u8; 16384];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let body: &[u8] = if hosted {
                    b"data: {\"type\":\"response.web_search_call.completed\",\"item_id\":\"s\",\"output_index\":0}\n\n"
                } else {
                    b"data: {\"type\":\"response.failed\",\"status_code\":503,\"error\":{\"message\":\"server error\",\"retry_after\":\"0\"}}\n\n"
                };
                stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()+5).as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                if count == 4 {
                    break;
                }
            }
            count
        });
        let gate = ReplayGate::new(hosted);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
        client.auth_manager().set_test_auth(http_test_auth());
        let mut events = client
            .stream_codex_http_events_for_owner_tracked(
                &buffered_test_request(),
                &attempt.context,
                &Default::default(),
            )
            .await
            .unwrap();
        let error = loop {
            if let Some(Err(error)) = events.recv().await {
                break error;
            }
        };
        let failure = attempt.failure(error);
        let count = server.await.unwrap();
        assert!(failure.send == SendEvidence::OutcomeUnknown);
        if hosted {
            assert!(gate.hosted());
            assert_eq!(count, 1);
            assert!(!gate.may_switch(failure.kind, failure.send));
        } else {
            assert_eq!(count, 4);
            assert!(failure.kind == FailureKind::TransientExhausted);
            assert!(gate.may_switch(failure.kind, failure.send));
        }
    }

    #[tokio::test]
    async fn tracked_live_body_hosted_failure_gates_replay() {
        tracked_live_body_case(true).await;
    }

    #[tokio::test]
    async fn tracked_live_body_transient_exhaustion_is_typed() {
        tracked_live_body_case(false).await;
    }

    #[tokio::test]
    async fn tracked_live_body_quota_permanent_and_corruption() {
        use super::super::api::SendEvidence;
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        for case in 0..4 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 16384];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let body: &[u8] = match case {
                    0 => b"data: {\"type\":\"error\",\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"limit\"}}\n\n",
                    1 => b"data: {\"type\":\"error\",\"status_code\":500,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"invalid\"}}\n\n",
                    2 => b"data: {\"type\":\"response.failed\",\"response\":{\"output\":[{\"type\":\"web_search_call\"}],\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"limit\"}}}\n\n",
                    _ => b"data: {broken}\n\n",
                };
                stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(300), listener.accept())
                        .await
                        .is_err()
                );
            });
            let gate = ReplayGate::new(true);
            let attempt = SourceAttempt::new(
                crate::config::codex_auth::AuthSource::ChatGpt,
                &http_test_context(),
                gate.clone(),
            );
            let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
            client.auth_manager().set_test_auth(http_test_auth());
            let mut events = client
                .stream_codex_http_events_for_owner_tracked(
                    &buffered_test_request(),
                    &attempt.context,
                    &Default::default(),
                )
                .await
                .unwrap();
            let error = loop {
                if let Some(Err(error)) = events.recv().await {
                    break error;
                }
            };
            let failure = attempt.failure(error);
            assert!(failure.send == SendEvidence::OutcomeUnknown);
            assert!(
                failure.kind
                    == if case == 0 || case == 2 {
                        FailureKind::UsageLimit
                    } else {
                        FailureKind::Final
                    }
            );
            assert!(!gate.may_switch(failure.kind, failure.send));
            if case == 2 {
                assert!(gate.hosted());
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn http_stream_forwards_event_before_terminal_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            write_http_chunk(
                &mut stream,
                b"data: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"primary\":{\"used_percent\":25,\"window_minutes\":300}}}\n\n",
            ).await;
            write_http_chunk(
                &mut stream,
                b"data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\n",
            )
            .await;
            release_rx.await.unwrap();
            write_http_chunk(
                &mut stream,
                b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{}}}\n\n",
            )
            .await;
        });

        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
        client.auth_manager().set_test_auth(http_test_auth());
        let monitor = crate::monitor::MonitorHandle::default();
        let mut ctx = http_test_context();
        monitor.request_started(
            &ctx.req_id,
            None,
            None,
            crate::monitor::EndpointKind::Messages,
        );
        ctx.monitor = Some(monitor.clone());
        let mut events = client
            .stream_codex_http_events(&buffered_test_request(), &ctx, &Default::default())
            .await
            .unwrap();

        let synthetic = events.recv().await.unwrap().unwrap();
        assert_eq!(
            synthetic.get("type").and_then(|value| value.as_str()),
            Some("keepalive")
        );
        let quota_event = events.recv().await.unwrap().unwrap();
        assert_eq!(quota_event["type"], "codex.rate_limits");
        assert_eq!(
            monitor
                .snapshot()
                .codex_quota
                .unwrap()
                .primary
                .unwrap()
                .used_percent,
            Some(25.0)
        );
        let first_upstream = tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .expect("first upstream event must arrive before the response completes")
            .unwrap()
            .unwrap();
        assert_eq!(
            first_upstream.get("type").and_then(|value| value.as_str()),
            Some("response.output_text.delta")
        );

        release_tx.send(()).unwrap();
        let terminal = events.recv().await.unwrap().unwrap();
        assert_eq!(
            terminal.get("type").and_then(|value| value.as_str()),
            Some("response.completed")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_stream_header_only_usage_limit_uses_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            let body = br#"data: {"type":"error","status_code":429,"error":{"type":"usage_limit_reached","message":"weekly \"limit\" reached","resets_in_seconds":90}}

"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nx-codex-secondary-reset-after-seconds: 90\r\nx-codex-secondary-reset-at: 1789466238\r\nx-codex-secondary-window-minutes: 10080\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
        client.auth_manager().set_test_auth(http_test_auth());
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                &Default::default(),
            )
            .await
            .unwrap();

        assert_eq!(
            events.recv().await.unwrap().unwrap().get("type"),
            Some(&serde_json::json!("keepalive"))
        );
        let error = events.recv().await.unwrap().unwrap_err();
        assert_eq!(error.status, 429);
        let limit = error.usage_limit.expect("usage limit");
        assert_eq!(limit.message, "weekly \"limit\" reached");
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn auto_missing_socket_stream_uses_gzip_http() {
        let _registry =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner =
            crate::request_identity::ConversationIdentity::Main("stream-missing-large".into());
        let long_text = "x".repeat(2048);
        let request = buffered_request_with_texts(&[&long_text, "two"]);
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let continuation = super::super::continuation::ContinuationReservation::new(
            super::super::continuation::ContinuationCandidate {
                turn_id: reserved.turn_id(),
                previous_response_id: Some("old".into()),
                input_delta: Some(vec![request.input.last().unwrap().clone()]),
                input_delta_count: 1,
                disabled_reason: None,
            },
            Some(owner),
            Some(u64::MAX),
        );
        let expected = serde_json::to_vec(&request).unwrap();
        let delta_bytes = serde_json::to_vec(&build_websocket_request(
            &request,
            Some(continuation.candidate()),
        ))
        .unwrap()
        .len();
        assert!(delta_bytes < expected.len());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let wire = read_http_request(&mut stream).await;
            assert!(wire.starts_with(b"POST "));
            assert_wire_json(&wire, &expected, true);
            let body = b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        client.auto_ws_max_bytes = delta_bytes;
        let client = Arc::new(client);
        let exhausted = http_test_context();
        for _ in 0..3 {
            assert!(exhausted.recovery_budget.try_retry(Duration::ZERO, false));
        }
        let error = match client
            .stream_codex_auto_events_for_owner(
                &request,
                &exhausted,
                &CodexHttpBodyCache::new(),
                Some(&continuation),
                true,
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("exhausted continuation recovery must stop"),
        };
        assert!(error.is_recovery_exhausted());
        let mut events = client
            .stream_codex_auto_events_for_owner(
                &request,
                &http_test_context(),
                &CodexHttpBodyCache::new(),
                Some(&continuation),
                true,
            )
            .await
            .unwrap();
        assert!(events.is_http());
        while let Some(event) = events.recv().await {
            event.unwrap();
        }
        server.await.unwrap();
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    #[tokio::test]
    async fn auto_stream_routes_dead_small_and_live_continuations() {
        let _registry =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        for (dead, live) in [(true, false), (false, false), (false, true)] {
            let owner = crate::request_identity::ConversationIdentity::Main(format!(
                "stream-route-{dead}-{live}"
            ));
            let long_text = "x".repeat(2048);
            let first = buffered_request_with_texts(&[&long_text]);
            let request = buffered_request_with_texts(&[&long_text, "two"]);
            let expected = serde_json::to_vec(&request).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = next_websocket_json(&mut ws).await;
                send_completed_websocket_response(&mut ws, "resp_old").await;
                if dead {
                    drop(ws);
                    let _ = closed_tx.send(());
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let wire = read_http_request(&mut stream).await;
                    assert!(wire.starts_with(b"POST "));
                    assert_wire_json(&wire, &expected, true);
                    let body = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"visible\",\"output_index\":0,\"content_index\":0}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http\",\"status\":\"completed\",\"output\":[]}}\n\n";
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(headers.as_bytes()).await.unwrap();
                    stream.write_all(body).await.unwrap();
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut next_ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                    let payload = next_websocket_json(&mut next_ws).await;
                    assert!(payload.get("previous_response_id").is_none());
                    assert_eq!(payload["input"].as_array().unwrap().len(), 3);
                    use futures_util::SinkExt;
                    next_ws.send(tokio_tungstenite::tungstenite::Message::text(
                        serde_json::json!({"type":"response.output_text.delta", "delta":"next", "output_index":0, "content_index":0}).to_string()
                    )).await.unwrap();
                    send_completed_websocket_response(&mut next_ws, "resp_next").await;
                } else if live {
                    let _ = closed_tx.send(());
                    let payload = next_websocket_json(&mut ws).await;
                    assert_eq!(payload["previous_response_id"], "resp_old");
                    assert_eq!(payload["input"].as_array().unwrap().len(), 1);
                    send_completed_websocket_response(&mut ws, "resp_delta").await;
                } else {
                    let _ = closed_tx.send(());
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut next_ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                    let payload = next_websocket_json(&mut next_ws).await;
                    assert!(payload.get("previous_response_id").is_none());
                    assert_eq!(payload["input"].as_array().unwrap().len(), 2);
                    send_completed_websocket_response(&mut next_ws, "resp_full").await;
                }
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_err()
                );
            });
            let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
            let ctx = http_test_context();
            let initial = super::super::continuation::continuation_candidate_for_owner(
                Some(&owner),
                &first,
                true,
            );
            let response = client
                .post_codex_with_transport(
                    &first,
                    &ctx,
                    Some(&initial),
                    crate::config::CodexTransport::WebSocket,
                    &CodexHttpBodyCache::new(),
                )
                .await
                .unwrap();
            super::super::update_continuation_from_upstream(
                None,
                &initial,
                None,
                &first,
                &response.body,
                response.socket_id,
                false,
            );
            closed_rx.await.unwrap();
            let continuation = super::super::continuation::continuation_candidate_for_owner(
                Some(&owner),
                &request,
                true,
            );
            assert_eq!(
                continuation.candidate().previous_response_id.as_deref(),
                Some("resp_old")
            );
            if !dead && !live {
                super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
            }
            client.request_compression = crate::config::CodexRequestCompression::Gzip;
            let delta_bytes = serde_json::to_vec(&build_websocket_request(
                &request,
                Some(continuation.candidate()),
            ))
            .unwrap()
            .len();
            assert!(delta_bytes < serde_json::to_vec(&request).unwrap().len());
            client.auto_ws_max_bytes = if !dead && !live { 100_000 } else { delta_bytes };
            let client = Arc::new(client);
            if dead {
                let model = request.model.clone();
                let response = super::super::live_response::live_stream_response(
                    client.clone(),
                    "message".into(),
                    &model,
                    ctx.clone(),
                    request.clone(),
                    continuation,
                    super::super::live_response::LiveStreamCompaction {
                        compact_boundary: false,
                        attempt: None,
                    },
                    crate::config::CodexTransport::Auto,
                    false,
                    Arc::default(),
                )
                .await;
                assert_eq!(response.status(), http::StatusCode::OK);
                let output = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                assert!(
                    output
                        .windows(b"visible".len())
                        .any(|window| window == b"visible")
                );
                let next_request = buffered_request_with_texts(&[&long_text, "two", "three"]);
                let next = super::super::continuation::continuation_candidate_for_owner(
                    Some(&owner),
                    &next_request,
                    true,
                );
                assert!(next.candidate().previous_response_id.is_none());
                let mut next_client =
                    authenticated_http_test_client(format!("http://{addr}/responses"));
                next_client.request_compression = crate::config::CodexRequestCompression::Gzip;
                next_client.auto_ws_max_bytes = 100_000;
                let next_response = super::super::live_response::live_stream_response(
                    Arc::new(next_client),
                    "message-next".into(),
                    &model,
                    ctx.clone(),
                    next_request,
                    next,
                    super::super::live_response::LiveStreamCompaction {
                        compact_boundary: false,
                        attempt: None,
                    },
                    crate::config::CodexTransport::Auto,
                    false,
                    Arc::default(),
                )
                .await;
                assert_eq!(next_response.status(), http::StatusCode::OK);
                let output = axum::body::to_bytes(next_response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                assert!(
                    output
                        .windows(b"next".len())
                        .any(|window| window == b"next")
                );
            } else {
                let mut events = client
                    .stream_codex_auto_events_for_owner(
                        &request,
                        &ctx,
                        &CodexHttpBodyCache::new(),
                        Some(&continuation),
                        true,
                    )
                    .await
                    .unwrap();
                assert!(!events.is_http());
                while let Some(event) = events.recv().await {
                    event.unwrap();
                }
                super::super::continuation::abort_continuation_for_owner(&continuation);
            }
            server.await.unwrap();
            super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        }
    }

    #[tokio::test]
    async fn auto_large_full_context_stream_uses_gzip_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let request = buffered_test_request();
        let expected = serde_json::to_vec(&request).unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert_wire_json(&read_http_request(&mut stream).await, &expected, true);
                let body = b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
                let headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });
        let mut client = http_test_client(format!("http://{addr}/responses"), 1000);
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        client.auto_ws_max_bytes = 1;
        client.auth_manager().set_test_auth(http_test_auth());
        let client = Arc::new(client);
        let cache = CodexHttpBodyCache::new();
        for allow_size_routing in [true, false] {
            let mut events = client
                .stream_codex_auto_events_for_owner(
                    &request,
                    &http_test_context(),
                    &cache,
                    None,
                    allow_size_routing,
                )
                .await
                .unwrap();
            assert!(events.is_http());
            while let Some(event) = events.recv().await {
                event.unwrap();
            }
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn auto_fallback_obeys_shared_budget_for_live_and_buffered_requests() {
        for live in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                assert!(read_http_request(&mut socket).await.starts_with(b"GET "));
                socket.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::from_millis(200), listener.accept())
                        .await
                        .is_err()
                );
            });
            let client = Arc::new(authenticated_http_test_client(format!(
                "http://{addr}/responses"
            )));
            let ctx = http_test_context();
            for _ in 0..3 {
                assert!(ctx.recovery_budget.try_retry(Duration::ZERO, false));
            }
            let request = buffered_test_request();
            let cache = CodexHttpBodyCache::new();
            let error = if live {
                match client
                    .stream_codex_auto_events_for_owner(&request, &ctx, &cache, None, true)
                    .await
                {
                    Err(error) => error,
                    Ok(_) => panic!("fallback must stop"),
                }
            } else {
                match client
                    .post_codex_with_transport(
                        &request,
                        &ctx,
                        None,
                        crate::config::CodexTransport::Auto,
                        &cache,
                    )
                    .await
                {
                    Err(error) => error,
                    Ok(_) => panic!("fallback must stop"),
                }
            };
            assert!(error.is_recovery_exhausted());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn tracked_auto_transient_upgrade_falls_back_for_both_modes() {
        use super::super::sources::{ReplayGate, SourceAttempt};
        for live in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let _ = read_http_request(&mut socket).await;
                socket.write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await.unwrap();
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(3), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let request = read_http_request(&mut socket).await;
                assert!(request.starts_with(b"POST "));
                let body = b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
                let header = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
            });
            let client = Arc::new(authenticated_http_test_client(format!(
                "http://{addr}/responses"
            )));
            let attempt = SourceAttempt::new(
                crate::config::codex_auth::AuthSource::ChatGpt,
                &http_test_context(),
                ReplayGate::new(false),
            );
            let request = buffered_test_request();
            let cache = CodexHttpBodyCache::new();
            if live {
                let mut events = client
                    .stream_codex_auto_events_for_owner(
                        &request,
                        &attempt.context,
                        &cache,
                        None,
                        true,
                    )
                    .await
                    .unwrap();
                assert!(events.recv().await.unwrap().is_ok());
            } else {
                let response = client
                    .post_codex_with_transport(
                        &request,
                        &attempt.context,
                        None,
                        crate::config::CodexTransport::Auto,
                        &cache,
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status, 200);
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn auto_stream_does_not_fallback_after_a_websocket_send() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut ws).await;
            send_completed_websocket_response(&mut ws, "resp_empty").await;
            let (mut socket, _) = listener.accept().await.unwrap();
            let handshake = read_http_request(&mut socket).await;
            assert!(handshake.starts_with(b"GET "));
            socket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(1200), listener.accept())
                    .await
                    .is_err()
            );
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let request = buffered_test_request();
        let cache = CodexHttpBodyCache::new();
        let mut first = client
            .stream_codex_auto_events_for_owner(&request, &http_test_context(), &cache, None, true)
            .await
            .unwrap();
        while let Some(event) = first.recv().await {
            event.unwrap();
        }
        let mut retry = client
            .stream_codex_auto_events_for_owner(&request, &http_test_context(), &cache, None, false)
            .await
            .unwrap();
        assert!(retry.recv().await.unwrap().is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_stream_retries_with_current_attempt_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let request = buffered_test_request();
        let expected = serde_json::to_vec(&request).unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert_wire_json(&read_http_request(&mut stream).await, &expected, true);
                let (body, headers) = if attempt == 0 {
                    (
                        br#"data: {"type":"response.failed","status_code":503,"error":{"message":"server error","retry_after":"0"}}

"#.as_slice(),
                        "",
                    )
                } else {
                    (
                        br#"data: {"type":"error","status_code":429,"error":{"type":"usage_limit_reached","message":"weekly limit reached","resets_in_seconds":90}}

"#.as_slice(),
                        "x-codex-secondary-reset-after-seconds: 90\r\nx-codex-secondary-reset-at: 1789466238\r\nx-codex-secondary-window-minutes: 10080\r\n",
                    )
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let mut client = http_test_client(format!("http://{addr}/responses"), 1_000);
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        let client = Arc::new(client);
        client.auth_manager().set_test_auth(http_test_auth());
        let mut events = client
            .stream_codex_http_events(&request, &http_test_context(), &Default::default())
            .await
            .unwrap();
        let _ = events.recv().await.unwrap().unwrap();
        let error = events.recv().await.unwrap().unwrap_err();
        let limit = error.usage_limit.expect("usage limit");
        assert_eq!(limit.resets_at, Some(1789466238));
        assert_eq!(limit.window, Some(CodexLimitWindow::SevenDay));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_stream_retries_tls_eof_before_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tls, http) = loopback_tls();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = tls.accept(socket).await.unwrap();
            read_http_request(&mut stream).await;
            drop(stream);

            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = tls.accept(socket).await.unwrap();
            read_http_request(&mut stream).await;
            let body = b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let client = Arc::new(CodexHttpClient::new_for_test(
            http,
            format!("https://{addr}/responses"),
            1_000,
            1_000,
        ));
        client.auth_manager().set_test_auth(http_test_auth());
        let mut events = tokio::time::timeout(
            Duration::from_secs(10),
            client.stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                &Default::default(),
            ),
        )
        .await
        .unwrap()
        .expect("TLS EOF before response headers must be retried");
        let keepalive = events.recv().await.unwrap().unwrap();
        assert_eq!(keepalive["type"], "keepalive");
        let terminal = events.recv().await.unwrap().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert!(events.recv().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_stream_does_not_replay_tls_eof_after_hosted_search() {
        check_http_stream_does_not_replay_after_hosted_search(false).await;
    }

    #[tokio::test]
    async fn http_stream_does_not_replay_malformed_frame_after_hosted_search_in_same_chunk() {
        check_http_stream_does_not_replay_after_hosted_search(true).await;
    }

    async fn check_http_stream_does_not_replay_after_hosted_search(malformed_frame: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tls, http) = loopback_tls();
        let client = Arc::new(CodexHttpClient::new_for_test(
            http,
            format!("https://{addr}/responses"),
            1_000,
            1_000,
        ));
        client.auth_manager().set_test_auth(http_test_auth());
        let sends = std::cell::Cell::new(0);
        let server = async {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut stream = tls.accept(socket).await.unwrap();
                read_http_request(&mut stream).await;
                sends.set(sends.get() + 1);
                if sends.get() == 2 {
                    return;
                }
                let event = "data: {\"type\":\"response.web_search_call.completed\",\"item_id\":\"search_1\",\"output_index\":0}\n\n";
                let (trailing, content_length) = if malformed_frame {
                    let malformed = "data: {broken json}\n\n";
                    (malformed, event.len() + malformed.len())
                } else {
                    ("", 1024)
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {content_length}\r\n\r\n{event}{trailing}"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        };
        let request = async {
            let mut events = client
                .stream_codex_http_events(
                    &buffered_test_request(),
                    &http_test_context(),
                    &Default::default(),
                )
                .await
                .unwrap();
            assert!(events.recv().await.unwrap().unwrap()["type"] == "keepalive");
            let mut saw_search = false;
            let error = loop {
                match events.recv().await.unwrap() {
                    Ok(event) => {
                        saw_search |= event["type"] == "response.web_search_call.completed"
                    }
                    Err(error) => break error,
                }
            };
            if malformed_frame {
                assert!(error.message.contains("invalid JSON"));
            } else {
                assert!(saw_search);
            }
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = server => {},
                () = request => {},
            }
        })
        .await
        .unwrap();
        assert_eq!(sends.get(), 1);
    }

    #[tokio::test]
    async fn http_stream_does_not_resend_after_a_header_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut attempts = 0_u32;
            let mut held = Vec::new();
            while let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(400), listener.accept()).await
            {
                let mut request = [0_u8; 16 * 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                attempts += 1;
                held.push(stream);
            }
            attempts
        });

        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
        client.auth_manager().set_test_auth(http_test_auth());
        let started_at = Instant::now();
        let error = match client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                &Default::default(),
            )
            .await
        {
            Ok(_) => panic!("a silent upstream must fail the request"),
            Err(error) => error,
        };
        let elapsed = started_at.elapsed();

        assert_eq!(error.status, 0);
        assert!(
            error
                .message
                .contains("Timed out waiting 100ms for Codex response headers"),
            "{}",
            error.message
        );
        assert_eq!(
            error.detail.as_deref(),
            Some(HTTP_RESPONSE_HEADERS_DETAIL),
            "the header timeout must stay distinguishable from a transport failure"
        );
        assert!(
            elapsed < Duration::from_millis(crate::retry::RETRY_INITIAL_DELAY_MS),
            "the header timeout must fail before the first retry backoff, took {elapsed:?}"
        );
        assert_eq!(
            server.await.unwrap(),
            1,
            "a header timeout must not re-send the request"
        );
    }

    #[tokio::test]
    async fn http_stream_bounds_initial_status_retries() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut attempts = 0_u32;
            while let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
            {
                let mut request = [0_u8; 16 * 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                attempts += 1;
                stream
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
            }
            attempts
        });

        let client = Arc::new(http_test_client(format!("http://{addr}/responses"), 1_000));
        client.auth_manager().set_test_auth(http_test_auth());
        let error = match client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                &Default::default(),
            )
            .await
        {
            Ok(_) => panic!("retryable status must exhaust with an error"),
            Err(error) => error,
        };

        assert_eq!(error.status, 503);
        assert_eq!(
            server.await.unwrap(),
            MAX_BUFFERED_TRANSPORT_ATTEMPTS,
            "initial status failures must share the HTTP stream retry budget"
        );
    }
}
