use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::logging::create_logger;
use crate::provider::RequestContext;
use crate::retry::{compute_backoff_delay, sleep};

use super::auth::token_store::StoredAuth;
use super::client::*;
use super::client_body::{CodexHttpBody, CodexHttpBodyCache};
use super::client_capture::*;
use super::client_http_tools::{HttpToolState, STALLED_READ_FINISH_FIELD};
use super::client_sse::*;
#[cfg(test)]
pub use super::events::CodexLimitWindow;
use super::translate::request::ResponsesRequest;

impl CodexHttpClient {
    async fn stream_codex_http_events(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        http_body: &CodexHttpBodyCache,
    ) -> Result<CodexHttpEventReceiver, CodexError> {
        let mut auth = self.auth_for(&body.reasoning_account).await?;
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
                )
                .await
            {
                Ok(attempt) => break attempt,
                Err(error) if retryable_http_stream_error(&error) => {
                    if retries >= MAX_BUFFERED_TRANSPORT_RETRIES {
                        return Err(error);
                    }
                    let delay = compute_backoff_delay(retries, error.retry_after.as_deref());
                    if delay.exceeds_budget {
                        return Err(error);
                    }
                    retries += 1;
                    sleep(delay.wait_ms).await;
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
            },
            ctx.clone(),
        ))
    }

    pub(super) async fn stream_codex_http_events_for_owner(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        http_body: &CodexHttpBodyCache,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        let receiver = self.stream_codex_http_events(body, ctx, http_body).await?;
        Ok(super::websocket::CodexWebSocketEventStream::http(receiver))
    }

    async fn start_http_event_attempt(
        &self,
        auth: &mut StoredAuth,
        http_body: &CodexHttpBody,
        ctx: &RequestContext,
        use_responses_lite: bool,
        auth_refresh_attempted: &mut bool,
    ) -> Result<(reqwest::Response, Vec<(String, String)>, Instant), CodexError> {
        loop {
            let (resp, started_at) = self
                .start_post_http(auth, http_body, ctx, use_responses_lite)
                .await?;

            if resp.status().as_u16() == 401 && !*auth_refresh_attempted {
                *auth_refresh_attempted = true;
                *auth = self.refreshed_auth(auth).await?;
                continue;
            }

            if !resp.status().is_success() {
                let response = self.collect_http_response(resp, started_at, ctx).await?;
                let mut error = codex_status_error(response);
                error.origin = CodexErrorOrigin::Http;
                return Err(error);
            }

            let status = resp.status().as_u16();
            let headers = response_headers(&resp);
            if let Some(traffic) = ctx.traffic.as_deref() {
                write_upstream_response_headers_capture(
                    traffic,
                    status,
                    started_at.elapsed(),
                    &headers,
                );
            }
            return Ok((resp, headers, started_at));
        }
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
            http_body
                .size_http_selected
                .store(true, std::sync::atomic::Ordering::Release);
            return self
                .stream_codex_http_events_for_owner(body, ctx, http_body)
                .await;
        }
        let mut websocket = self
            .stream_codex_websocket_events_tracked(
                body,
                ctx,
                continuation,
                http_body.websocket_sent.clone(),
            )
            .await?;
        match websocket.recv().await {
            Some(Err(err))
                if self.auto_http_fallback_enabled
                    && !http_body
                        .websocket_sent
                        .load(std::sync::atomic::Ordering::Acquire)
                    && should_fallback_to_http(&err) =>
            {
                self.stream_codex_http_events_for_owner(body, ctx, http_body)
                    .await
            }
            Some(item) => {
                let (tx, rx) = tokio::sync::mpsc::channel(64);
                let receiver = websocket.replace_receiver(rx);
                tokio::spawn(async move {
                    if tx.send(item).await.is_ok() {
                        forward_codex_events(receiver, tx).await;
                    }
                });
                Ok(websocket)
            }
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
                            .any(super::events::event_confirms_hosted_tool_execution);
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

                    for event in events {
                        let Some(mut payload) = event.payload else {
                            continue;
                        };
                        event_count = event_count.saturating_add(1);
                        if let Some(traffic) = traffic.as_deref() {
                            write_codex_http_sse_event_capture(
                                traffic,
                                event.event.as_deref(),
                                &payload,
                            );
                        }

                        hosted_tool_executed |=
                            super::events::event_confirms_hosted_tool_execution(&payload);
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
                    if retries >= MAX_BUFFERED_TRANSPORT_RETRIES {
                        let _ = tx.send(Err(retry_error)).await;
                        return;
                    }
                    let delay = compute_backoff_delay(retries, retry_error.retry_after.as_deref());
                    if delay.exceeds_budget {
                        let _ = tx.send(Err(retry_error)).await;
                        return;
                    }
                    retries += 1;
                    tokio::select! {
                        _ = tx.closed() => return,
                        _ = sleep(delay.wait_ms) => {}
                    }

                    let next_attempt = tokio::select! {
                        _ = tx.closed() => return,
                        result = client.start_http_event_attempt(
                            &mut auth,
                            &http_body,
                            &ctx,
                            use_responses_lite,
                            &mut auth_refresh_attempted,
                        ) => result
                    };
                    match next_attempt {
                        Ok((next_resp, next_response_headers, next_started_at)) => {
                            resp = next_resp;
                            response_headers = next_response_headers;
                            started_at = next_started_at;
                            continue 'attempts;
                        }
                        Err(error) if retryable_http_stream_error(&error) => {
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
        let mut events = client
            .stream_codex_http_events(
                &buffered_test_request(),
                &http_test_context(),
                &Default::default(),
            )
            .await
            .unwrap();

        let synthetic = events.recv().await.unwrap().unwrap();
        assert_eq!(
            synthetic.get("type").and_then(|value| value.as_str()),
            Some("keepalive")
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
