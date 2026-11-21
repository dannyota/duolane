use std::time::{Duration, Instant};

use crate::provider::{MAX_BUFFERED_RESPONSE_BYTES, RequestContext};
#[cfg(test)]
use crate::request_identity::ConversationIdentity;
use crate::retry::{compute_backoff_delay, sleep};

use super::auth::token_store::StoredAuth;
use super::client::*;
use super::client_body::{CodexHttpBody, CodexHttpBodyCache};
use super::client_capture::*;
use super::client_live::*;
use super::translate::request::ResponsesRequest;

impl CodexHttpClient {
    pub(super) async fn post_codex_with_transport(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
        transport: crate::config::CodexTransport,
        http_body: &CodexHttpBodyCache,
    ) -> Result<OwnerAwareCodexResponse, CodexError> {
        use crate::config::CodexTransport;

        let mut auth = self.auth_for(&body.reasoning_account).await?;

        let initial_pool_owner = websocket_pool_owner(continuation).cloned();
        if should_reset_websocket_pool(continuation)
            && let Some(owner) = initial_pool_owner.as_ref()
        {
            super::websocket::invalidate_codex_websocket_pool_turn_for_owner(
                owner,
                continuation.and_then(super::continuation::ContinuationReservation::turn_id),
            );
        }

        let prepare_http_body =
            || self.prepare_http_body(body, ctx.traffic.is_some(), "Failed to serialize request");
        let mut active_continuation = continuation.cloned();
        let mut auth_refresh_attempted = false;
        let mut transport_failures = 0u32;
        loop {
            let result = match transport {
                CodexTransport::Http => {
                    let http_body = http_body.get_or_try_init(prepare_http_body).await?;
                    self.attempt_post_http(&auth, http_body, ctx, body.client_metadata.is_some())
                        .await
                        .map(|response| OwnerAwareCodexResponse::new(response, None))
                }
                CodexTransport::WebSocket => {
                    let ws_headers =
                        build_codex_headers(&auth, ctx, body.client_metadata.is_some())?;
                    let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
                    let ws_body = build_websocket_request(
                        body,
                        active_continuation
                            .as_ref()
                            .map(super::continuation::ContinuationReservation::candidate),
                    );

                    super::websocket::codex_websocket_request(
                        &self.websocket_client,
                        &self.websocket_proxy_config,
                        &self.base_url,
                        &ws_headers,
                        &ws_body,
                        ctx,
                        ctx.traffic.as_deref(),
                        super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                        super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                        active_continuation.as_ref(),
                        &http_body.websocket_sent,
                    )
                    .await
                }
                CodexTransport::Auto
                    if !http_body
                        .websocket_sent
                        .load(std::sync::atomic::Ordering::Acquire)
                        && (http_body
                            .size_http_selected
                            .load(std::sync::atomic::Ordering::Acquire)
                            || self
                                .auto_large_full_context(
                                    body,
                                    ctx,
                                    active_continuation.as_ref(),
                                    http_body,
                                )
                                .await?) =>
                {
                    http_body
                        .size_http_selected
                        .store(true, std::sync::atomic::Ordering::Release);
                    let http_body = http_body.get_or_try_init(prepare_http_body).await?;
                    self.attempt_post_http(&auth, http_body, ctx, body.client_metadata.is_some())
                        .await
                        .map(|response| OwnerAwareCodexResponse::new(response, None))
                }
                CodexTransport::Auto => {
                    let ws_headers =
                        build_codex_headers(&auth, ctx, body.client_metadata.is_some())?;
                    let ws_headers = super::websocket::codex_websocket_headers(&ws_headers);
                    let ws_body = build_websocket_request(
                        body,
                        active_continuation
                            .as_ref()
                            .map(super::continuation::ContinuationReservation::candidate),
                    );

                    let ws_result = super::websocket::codex_websocket_request(
                        &self.websocket_client,
                        &self.websocket_proxy_config,
                        &self.base_url,
                        &ws_headers,
                        &ws_body,
                        ctx,
                        ctx.traffic.as_deref(),
                        super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                        super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                        active_continuation.as_ref(),
                        &http_body.websocket_sent,
                    )
                    .await;

                    match ws_result {
                        Ok(response) => Ok(response),
                        Err(err)
                            if should_retry_without_continuation(
                                &err,
                                active_continuation.as_ref(),
                            ) =>
                        {
                            Err(err)
                        }
                        Err(err)
                            if self.auto_http_fallback_enabled
                                && !http_body
                                    .websocket_sent
                                    .load(std::sync::atomic::Ordering::Acquire)
                                && should_fallback_to_http(&err) =>
                        {
                            let http_body = http_body.get_or_try_init(prepare_http_body).await?;
                            self.attempt_post_http(
                                &auth,
                                http_body,
                                ctx,
                                body.client_metadata.is_some(),
                            )
                            .await
                            .map(|response| OwnerAwareCodexResponse::new(response, None))
                        }
                        Err(err) => Err(err),
                    }
                }
            };

            let replay_allowed = !result
                .as_ref()
                .is_ok_and(|response| response.hosted_tool_executed);
            if replay_allowed
                && should_refresh_after_unauthorized(&result, auth_refresh_attempted, transport)
            {
                auth_refresh_attempted = true;
                auth = self.refreshed_auth(&auth).await?;
                invalidate_live_continuation_pool(active_continuation.as_ref());
                active_continuation = full_context_continuation(active_continuation.as_ref());
                continue;
            }

            if let Ok(response) = &result
                && let Some(limit) =
                    super::events::usage_limit_from_response(&response.body, &response.headers)
            {
                let origin = match response.transport {
                    ActualTransport::Http => CodexErrorOrigin::BufferedHttp,
                    ActualTransport::WebSocket => CodexErrorOrigin::BufferedWebSocket,
                };
                return Err(codex_usage_limit_error(limit, origin));
            }

            if let Ok(response) = &result
                && (200..300).contains(&response.status)
                && let Some(failure) = super::events::first_retryable_failure(&response.body)
            {
                if replay_allowed && transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                    let delay =
                        compute_backoff_delay(transport_failures, failure.retry_after.as_deref());
                    if delay.exceeds_budget {
                        return Err(CodexError {
                            status: failure.status,
                            message: failure.message.clone(),
                            detail: Some(failure.message),
                            retry_after: failure.retry_after,
                            usage_limit: None,
                            origin: match response.transport {
                                ActualTransport::Http => CodexErrorOrigin::BufferedHttp,
                                ActualTransport::WebSocket => CodexErrorOrigin::BufferedWebSocket,
                            },
                        });
                    }
                    log_buffered_retry(
                        ctx,
                        transport,
                        transport_failures + 1,
                        delay.wait_ms,
                        failure.status,
                        "upstream_event",
                        &failure.message,
                    );
                    transport_failures += 1;
                    active_continuation = full_context_continuation(active_continuation.as_ref());
                    sleep(delay.wait_ms).await;
                    continue;
                }

                log_buffered_retry_exhausted(
                    ctx,
                    transport,
                    failure.status,
                    "upstream_event",
                    &failure.message,
                );
                return Err(CodexError {
                    status: failure.status,
                    message: failure.message.clone(),
                    detail: Some(failure.message),
                    retry_after: failure.retry_after,
                    usage_limit: None,
                    origin: CodexErrorOrigin::Http,
                });
            }

            match result {
                Ok(response) if response.status == 401 => {
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 401,
                        message: "Unauthorized".to_string(),
                        detail: Some(detail),
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if response.status == 403 => {
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    return Err(CodexError {
                        status: 403,
                        message: "Forbidden".to_string(),
                        detail: Some(detail),
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if response.status == 429 => {
                    let retry_after = response
                        .headers
                        .iter()
                        .find(|(k, _)| k.to_lowercase() == "retry-after")
                        .map(|(_, v)| v.clone());
                    if replay_allowed && transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                        let delay =
                            compute_backoff_delay(transport_failures, retry_after.as_deref());
                        if delay.exceeds_budget {
                            let detail = String::from_utf8_lossy(&response.body).to_string();
                            return Err(CodexError {
                                status: 429,
                                message: "Rate limited".to_string(),
                                detail: Some(detail),
                                retry_after,
                                usage_limit: None,
                                origin: CodexErrorOrigin::Http,
                            });
                        }
                        log_buffered_retry(
                            ctx,
                            transport,
                            transport_failures + 1,
                            delay.wait_ms,
                            response.status,
                            "upstream",
                            "rate limited",
                        );
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    let detail = String::from_utf8_lossy(&response.body).to_string();
                    log_buffered_retry_exhausted(
                        ctx,
                        transport,
                        response.status,
                        "upstream",
                        "rate limited",
                    );
                    return Err(CodexError {
                        status: 429,
                        message: "Rate limited".to_string(),
                        detail: Some(detail),
                        retry_after,
                        usage_limit: None,
                        origin: CodexErrorOrigin::Http,
                    });
                }
                Ok(response) if should_retry_codex_status(response.status) => {
                    if replay_allowed && transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                        let retry_after = response
                            .headers
                            .iter()
                            .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
                            .map(|(_, value)| value.as_str());
                        let delay = compute_backoff_delay(transport_failures, retry_after);
                        if delay.exceeds_budget {
                            return Err(codex_status_error(response.into_response()));
                        }
                        log_buffered_retry(
                            ctx,
                            transport,
                            transport_failures + 1,
                            delay.wait_ms,
                            response.status,
                            "upstream",
                            "retryable upstream status",
                        );
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    log_buffered_retry_exhausted(
                        ctx,
                        transport,
                        response.status,
                        "upstream",
                        "retryable upstream status",
                    );
                    return Err(codex_status_error(response.into_response()));
                }
                Ok(response) if !(200..300).contains(&response.status) => {
                    return Err(codex_status_error(response.into_response()));
                }
                Ok(response) => return Ok(response),
                Err(err)
                    if should_retry_without_continuation(&err, active_continuation.as_ref()) =>
                {
                    active_continuation = full_context_continuation(active_continuation.as_ref());
                    continue;
                }
                Err(err) => {
                    let retryable = err.usage_limit.is_none()
                        && (is_retryable_transport_error(&err)
                            || (err.origin == CodexErrorOrigin::Http
                                && should_retry_codex_status(err.status)));
                    if retryable && transport_failures < MAX_BUFFERED_TRANSPORT_RETRIES {
                        let delay =
                            compute_backoff_delay(transport_failures, err.retry_after.as_deref());
                        if delay.exceeds_budget {
                            return Err(err);
                        }
                        log_buffered_retry(
                            ctx,
                            transport,
                            transport_failures + 1,
                            delay.wait_ms,
                            err.status,
                            codex_error_origin_name(err.origin),
                            &err.message,
                        );
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        continue;
                    }
                    if retryable {
                        log_buffered_retry_exhausted(
                            ctx,
                            transport,
                            err.status,
                            codex_error_origin_name(err.origin),
                            &err.message,
                        );
                    }
                    return Err(err);
                }
            }
        }
    }

    async fn attempt_post_http(
        &self,
        auth: &StoredAuth,
        http_body: &CodexHttpBody,
        ctx: &RequestContext,
        use_responses_lite: bool,
    ) -> Result<CodexResponse, CodexError> {
        let (resp, started_at) = self
            .start_post_http(auth, http_body, ctx, use_responses_lite)
            .await?;
        self.collect_http_response(resp, started_at, ctx).await
    }

    pub(super) async fn start_post_http(
        &self,
        auth: &StoredAuth,
        http_body: &CodexHttpBody,
        ctx: &RequestContext,
        use_responses_lite: bool,
    ) -> Result<(reqwest::Response, Instant), CodexError> {
        let url = &self.base_url;
        let headers = build_codex_headers(auth, ctx, use_responses_lite)?;

        if let Some(traffic) = ctx.traffic.as_deref() {
            http_body.capture(traffic, url, &headers);
        }

        let mut req_builder = self.client.post(url);
        for (key, value) in headers.iter() {
            req_builder = req_builder.header(key.as_str(), value.as_bytes());
        }

        let started_at = Instant::now();
        let send_fut = http_body.apply(req_builder).send();
        let header_timeout_dur = Duration::from_millis(self.header_timeout_ms);

        let resp = tokio::time::timeout(header_timeout_dur, send_fut)
            .await
            .map_err(|_| CodexError {
                status: 0,
                message: format!(
                    "Timed out waiting {}ms for Codex response headers",
                    self.header_timeout_ms
                ),
                detail: Some(HTTP_RESPONSE_HEADERS_DETAIL.to_string()),
                retry_after: None,
                usage_limit: None,
                origin: CodexErrorOrigin::Http,
            })?
            .map_err(|e| {
                if is_retryable_reqwest_error(&e) {
                    CodexError {
                        status: 0,
                        message: format!("Transport error: {e}"),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::Http,
                    }
                } else {
                    CodexError {
                        status: 0,
                        message: format!("Network error: {e}"),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::Http,
                    }
                }
            })?;

        Ok((resp, started_at))
    }

    pub(super) async fn collect_http_response(
        &self,
        mut resp: reqwest::Response,
        started_at: Instant,
        ctx: &RequestContext,
    ) -> Result<CodexResponse, CodexError> {
        let status = resp.status();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let body_bytes = self
            .read_buffered_body(&mut resp, ctx, "Codex response body")
            .await
            .map_err(|mut error| {
                if !status.is_success() {
                    error.status = status.as_u16();
                    error.detail = Some(error.message.clone());
                    error.retry_after = headers.iter().find_map(|(name, value)| {
                        name.eq_ignore_ascii_case("retry-after")
                            .then(|| value.clone())
                    });
                }
                error
            })?;
        let status = status.as_u16();

        if let Some(traffic) = ctx.traffic.as_deref() {
            write_upstream_response_capture(
                traffic,
                status,
                started_at.elapsed(),
                &headers,
                &body_bytes,
            );
        }

        Ok(CodexResponse {
            body: body_bytes,
            status,
            headers,
            transport: ActualTransport::Http,
        })
    }

    pub(super) async fn read_buffered_body(
        &self,
        resp: &mut reqwest::Response,
        ctx: &RequestContext,
        label: &str,
    ) -> Result<Vec<u8>, CodexError> {
        let body_error = |message: String| CodexError {
            status: 0,
            message,
            detail: Some("http_response_body".to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        };
        let mut body = Vec::new();
        let mut response_started = false;
        loop {
            let chunk = tokio::time::timeout(
                Duration::from_millis(self.body_idle_timeout_ms),
                resp.chunk(),
            )
            .await
            .map_err(|_| {
                body_error(format!(
                    "Timed out waiting {}ms for the next {label} chunk",
                    self.body_idle_timeout_ms
                ))
            })?
            .map_err(|e| body_error(format!("Transport error reading {label}: {e}")))?;
            let Some(chunk) = chunk else {
                return Ok(body);
            };
            if body.len().saturating_add(chunk.len()) > MAX_BUFFERED_RESPONSE_BYTES {
                return Err(body_error(format!("{label} exceeds the size limit")));
            }
            if !response_started {
                if let Some(monitor) = ctx.monitor.as_ref() {
                    monitor.generation_started(&ctx.req_id);
                }
                response_started = true;
            }
            body.extend_from_slice(&chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::super::client_test_support::*;
    use super::*;

    #[tokio::test]
    async fn buffered_missing_origin_retries_full_context_and_rebinds_exact_socket() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Agent(
            "buffered-recovery-session".to_string(),
            "buffered-recovery-agent".to_string(),
        );
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (first_socket, _) = listener.accept().await.unwrap();
            let mut first_websocket = tokio_tungstenite::accept_async(first_socket).await.unwrap();
            request_tx
                .send(next_websocket_json(&mut first_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut first_websocket, "resp_a").await;
            drop(first_websocket);

            let (second_socket, _) = listener.accept().await.unwrap();
            let mut second_websocket = tokio_tungstenite::accept_async(second_socket)
                .await
                .unwrap();
            request_tx
                .send(next_websocket_json(&mut second_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut second_websocket, "resp_b").await;
            request_tx
                .send(next_websocket_json(&mut second_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut second_websocket, "resp_c").await;
        });

        let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let context = http_test_context();
        let first_request = buffered_request_with_texts(&["one"]);
        let first_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &first_request,
            true,
        );
        let first_response = client
            .post_codex_with_transport(
                &first_request,
                &context,
                Some(&first_candidate),
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
            .unwrap();
        let first_socket_id = first_response
            .socket_id
            .expect("first socket must be reusable");
        super::super::update_continuation_from_upstream(
            None,
            &first_candidate,
            None,
            &first_request,
            &first_response.body,
            first_response.socket_id,
            false,
        );

        let second_request = buffered_request_with_texts(&["one", "two"]);
        let second_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &second_request,
            true,
        );
        assert_eq!(second_candidate.origin_socket_id(), Some(first_socket_id));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let second_response = client
            .post_codex_with_transport(
                &second_request,
                &context,
                Some(&second_candidate),
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
            .unwrap();
        let second_socket_id = second_response
            .socket_id
            .expect("full-context retry socket must be reusable");
        assert_ne!(second_socket_id, first_socket_id);
        super::super::update_continuation_from_upstream(
            None,
            &second_candidate,
            None,
            &second_request,
            &second_response.body,
            second_response.socket_id,
            false,
        );

        let third_request = buffered_request_with_texts(&["one", "two", "three"]);
        let third_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &third_request,
            true,
        );
        assert_eq!(
            third_candidate.candidate().previous_response_id.as_deref(),
            Some("resp_b")
        );
        assert_eq!(third_candidate.origin_socket_id(), Some(second_socket_id));
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        client.auto_ws_max_bytes = 1;
        let third_response = client
            .post_codex_with_transport(
                &third_request,
                &context,
                Some(&third_candidate),
                crate::config::CodexTransport::Auto,
                &Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(third_response.socket_id, Some(second_socket_id));

        let first_payload = request_rx.recv().await.unwrap();
        let retry_payload = request_rx.recv().await.unwrap();
        let continued_payload = request_rx.recv().await.unwrap();
        assert!(first_payload.get("previous_response_id").is_none());
        assert_eq!(first_payload["input"].as_array().unwrap().len(), 1);
        assert!(retry_payload.get("previous_response_id").is_none());
        assert_eq!(retry_payload["input"].as_array().unwrap().len(), 2);
        assert_eq!(continued_payload["previous_response_id"], "resp_b");
        assert_eq!(continued_payload["input"].as_array().unwrap().len(), 1);
        server.await.unwrap();

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&third_candidate);
    }

    #[tokio::test]
    async fn buffered_nested_missing_response_retries_once_without_stale_previous_id() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("buffered-nested-recovery-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (first_socket, _) = listener.accept().await.unwrap();
            let mut first_websocket = tokio_tungstenite::accept_async(first_socket).await.unwrap();
            request_tx
                .send(next_websocket_json(&mut first_websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut first_websocket, "resp_nested_origin").await;
            request_tx
                .send(next_websocket_json(&mut first_websocket).await)
                .unwrap();
            send_nested_previous_response_missing(&mut first_websocket).await;

            let (retry_socket, _) = listener.accept().await.unwrap();
            let mut retry_websocket = tokio_tungstenite::accept_async(retry_socket).await.unwrap();
            request_tx
                .send(next_websocket_json(&mut retry_websocket).await)
                .unwrap();
            send_nested_previous_response_missing(&mut retry_websocket).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(1_200), listener.accept())
                    .await
                    .is_err(),
                "nested missing-response recovery must not open a third socket"
            );
        });

        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let context = http_test_context();
        let first_request = buffered_request_with_texts(&["one"]);
        let first_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &first_request,
            true,
        );
        let first_response = client
            .post_codex_with_transport(
                &first_request,
                &context,
                Some(&first_candidate),
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
            .unwrap();
        super::super::update_continuation_from_upstream(
            None,
            &first_candidate,
            None,
            &first_request,
            &first_response.body,
            first_response.socket_id,
            false,
        );

        let second_request = buffered_request_with_texts(&["one", "two"]);
        let second_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &second_request,
            true,
        );
        assert_eq!(
            second_candidate.candidate().previous_response_id.as_deref(),
            Some("resp_nested_origin")
        );
        let error = match client
            .post_codex_with_transport(
                &second_request,
                &context,
                Some(&second_candidate),
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
        {
            Ok(_) => {
                panic!("the bounded full-context retry must surface a repeated nested failure")
            }
            Err(error) => error,
        };
        assert_eq!(error.detail.as_deref(), Some("previous_response_not_found"));

        let first_payload = request_rx.recv().await.unwrap();
        let continued_payload = request_rx.recv().await.unwrap();
        let retry_payload = request_rx.recv().await.unwrap();
        assert!(first_payload.get("previous_response_id").is_none());
        assert_eq!(
            continued_payload["previous_response_id"],
            "resp_nested_origin"
        );
        assert_eq!(continued_payload["input"].as_array().unwrap().len(), 1);
        assert!(retry_payload.get("previous_response_id").is_none());
        assert_eq!(retry_payload["input"].as_array().unwrap().len(), 2);
        assert!(request_rx.try_recv().is_err());
        server.await.unwrap();

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&second_candidate);
    }

    #[tokio::test]
    async fn auto_clears_missing_origin_before_ordinary_http_fallback() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Agent(
            "auto-recovery-session".to_string(),
            "auto-recovery-agent".to_string(),
        );
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = buffered_request_with_texts(&["one", "two"]);
        let reserved = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let continuation = super::super::continuation::ContinuationReservation::new(
            super::super::continuation::ContinuationCandidate {
                turn_id: reserved.turn_id(),
                previous_response_id: Some("resp_missing".to_string()),
                input_delta: Some(vec![request.input.last().unwrap().clone()]),
                input_delta_count: 1,
                disabled_reason: None,
            },
            Some(owner.clone()),
            Some(u64::MAX),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut websocket, _) = listener.accept().await.unwrap();
            let websocket_request = read_http_request(&mut websocket).await;
            assert!(String::from_utf8_lossy(&websocket_request).starts_with("GET "));
            websocket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            drop(websocket);

            let (mut http, _) = listener.accept().await.unwrap();
            let http_request = read_http_request(&mut http).await;
            let body_start = http_request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap()
                + 4;
            let body: serde_json::Value =
                serde_json::from_slice(&http_request[body_start..]).unwrap();
            assert!(body.get("previous_response_id").is_none());
            assert_eq!(body["input"].as_array().unwrap().len(), 2);
            let response_body =
                b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http\"}}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response_body.len()
            );
            http.write_all(response.as_bytes()).await.unwrap();
            http.write_all(response_body).await.unwrap();
        });

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &request,
                &http_test_context(),
                Some(&continuation),
                crate::config::CodexTransport::Auto,
                &Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(response.transport, ActualTransport::Http);
        assert_eq!(response.socket_id, None);
        server.await.unwrap();

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    #[tokio::test]
    async fn buffered_websocket_does_not_replay_tls_eof_after_hosted_search() {
        use super::super::provider_test_support::assert_no_websocket_replay_after_hosted_search;

        assert_no_websocket_replay_after_hosted_search(
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "web_search_call", "id": "search_1", "status": "completed"}
            }),
            |client: CodexHttpClient| async move {
                match client
                    .post_codex_with_transport(
                        &buffered_test_request(),
                        &http_test_context(),
                        None,
                        crate::config::CodexTransport::WebSocket,
                        &Default::default(),
                    )
                    .await
                {
                    Ok(response) => response.status,
                    Err(error) => super::super::map_codex_error_to_response(&error)
                        .status()
                        .as_u16(),
                }
            },
        )
        .await;
    }

    #[tokio::test]
    async fn buffered_http_does_not_replay_truncated_forbidden_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let sends = std::cell::Cell::new(0);
        let server = async {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_request(&mut stream).await;
                sends.set(sends.get() + 1);
                if sends.get() == 2 {
                    return;
                }
                stream.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 10\r\nconnection: close\r\n\r\n").await.unwrap();
            }
        };
        let request = buffered_test_request();
        let context = http_test_context();
        let http_body = CodexHttpBodyCache::new();
        let response = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = server => None,
                result = client.post_codex_with_transport(
                    &request,
                    &context,
                    None,
                    crate::config::CodexTransport::Http,
                    &http_body,
                ) => Some(result),
            }
        })
        .await
        .unwrap();
        assert_eq!(sends.get(), 1);
        assert_eq!(response.unwrap().err().unwrap().status, 403);
    }

    #[tokio::test]
    async fn buffered_http_reuses_gzip_after_retryable_status() {
        check_buffered_http_retries_retryable_status(false).await;
    }

    #[tokio::test]
    async fn buffered_http_retries_truncated_service_unavailable_response() {
        check_buffered_http_retries_retryable_status(true).await;
    }

    async fn check_buffered_http_retries_retryable_status(truncate_error_body: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let request = buffered_request_with_texts(&[&"long conversation ".repeat(500)]);
        let expected = serde_json::to_vec(&request).unwrap();
        let server = tokio::spawn(async move {
            let mut first = None;
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                let body = assert_wire_json(&request, &expected, true);
                if let Some(first) = &first {
                    assert!(first == &body, "retry changed gzip bytes");
                } else {
                    first = Some(body);
                }
                let (status, body): (&str, &[u8]) = if attempt == 0 {
                    ("503 Service Unavailable", b"retry")
                } else {
                    ("200 OK", b"data: keep\n\n")
                };
                let content_length = body.len() + usize::from(attempt == 0 && truncate_error_body);
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {content_length}\r\nretry-after: 0\r\nconnection: close\r\n\r\n"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });

        let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        let capture_dir = tempfile::TempDir::new().unwrap();
        let mut ctx = http_test_context();
        ctx.traffic = Some(std::sync::Arc::new(crate::traffic::test_capture(
            capture_dir.path().to_path_buf(),
        )));
        let response = client
            .post_codex_with_transport(
                &request,
                &ctx,
                None,
                crate::config::CodexTransport::Http,
                &Default::default(),
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"data: keep\n\n");
        let capture: serde_json::Value = serde_json::from_slice(
            &std::fs::read(capture_dir.path().join("001-020-upstream-request.json")).unwrap(),
        )
        .unwrap();
        let expected = serde_json::to_value(&request).unwrap();
        assert!(capture == expected, "captured JSON changed");
        let metadata: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                capture_dir
                    .path()
                    .join("002-021-upstream-request-metadata.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(metadata["headers"].get("content-encoding").is_none());
        assert!(metadata["size"].get("compressedBytes").is_none());
    }

    #[tokio::test]
    async fn auto_large_full_context_uses_gzip_http_without_websocket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let request = buffered_request_with_texts(&[&"x".repeat(4 * 1024 * 1024)]);
        let expected = serde_json::to_vec(&request).unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let wire = read_http_request(&mut stream).await;
                assert_wire_json(&wire, &expected, true);
                if attempt == 0 {
                    stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n").await.unwrap();
                } else {
                    stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\nconnection: close\r\n\r\ndata: keep\n\n").await.unwrap();
                }
            }
        });
        let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        let cache = CodexHttpBodyCache::new();
        let response = client
            .post_codex_with_transport(
                &request,
                &http_test_context(),
                None,
                crate::config::CodexTransport::Auto,
                &cache,
            )
            .await;
        server.await.unwrap();
        assert_eq!(response.unwrap().transport, ActualTransport::Http);
        assert!(
            cache
                .size_http_selected
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert_eq!(
            cache
                .size_measurements
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn auto_under_limit_measures_once_across_handshake_and_http_retries() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let request = buffered_test_request();
        let expected = serde_json::to_vec(&request).unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut ws, _) = listener.accept().await.unwrap();
                let handshake = read_http_request(&mut ws).await;
                assert!(handshake.starts_with(b"GET "));
                ws.write_all(
                    b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
                let (mut http, _) = listener.accept().await.unwrap();
                assert_wire_json(&read_http_request(&mut http).await, &expected, true);
                let response = if attempt == 0 {
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n".as_slice()
                } else {
                    b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\nconnection: close\r\n\r\ndata: keep\n\n".as_slice()
                };
                http.write_all(response).await.unwrap();
            }
        });
        let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        client.auto_ws_max_bytes = 100_000;
        let cache = CodexHttpBodyCache::new();
        let response = client
            .post_codex_with_transport(
                &request,
                &http_test_context(),
                None,
                crate::config::CodexTransport::Auto,
                &cache,
            )
            .await
            .unwrap();
        assert_eq!(response.transport, ActualTransport::Http);
        assert_eq!(
            cache
                .size_measurements
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn auto_gzip_off_or_zero_limit_keeps_full_context_on_websocket() {
        for (compression, limit, fallback) in [
            (crate::config::CodexRequestCompression::Off, 1, true),
            (crate::config::CodexRequestCompression::Gzip, 0, true),
            (crate::config::CodexRequestCompression::Gzip, 1, false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = next_websocket_json(&mut websocket).await;
                send_completed_websocket_response(&mut websocket, "resp_ws").await;
            });
            let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
            client.request_compression = compression;
            client.auto_ws_max_bytes = limit;
            client.auto_http_fallback_enabled = fallback;
            let response = client
                .post_codex_with_transport(
                    &buffered_request_with_texts(&["large enough"]),
                    &http_test_context(),
                    None,
                    crate::config::CodexTransport::Auto,
                    &Default::default(),
                )
                .await
                .unwrap();
            assert_eq!(response.transport, ActualTransport::WebSocket);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn auto_missing_continuation_socket_uses_gzip_http() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("missing-large-socket".to_string());
        let first = buffered_request_with_texts(&["one"]);
        let request = buffered_request_with_texts(&["one", "two"]);
        let expected = serde_json::to_vec(&request).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut ws).await;
            send_completed_websocket_response(&mut ws, "resp_old").await;
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_wire_json(&read_http_request(&mut stream).await, &expected, true);
            let body = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http\",\"status\":\"completed\"}}\n\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let ctx = http_test_context();
        let first_candidate = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &first,
            true,
        );
        let first_response = client
            .post_codex_with_transport(
                &first,
                &ctx,
                Some(&first_candidate),
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
            .unwrap();
        super::super::update_continuation_from_upstream(
            None,
            &first_candidate,
            None,
            &first,
            &first_response.body,
            first_response.socket_id,
            false,
        );
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        assert_eq!(
            continuation.candidate().previous_response_id.as_deref(),
            Some("resp_old")
        );
        assert_eq!(
            continuation.candidate().input_delta.as_ref().map(Vec::len),
            Some(1)
        );
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        client.auto_ws_max_bytes = 1;
        let response = client
            .post_codex_with_transport(
                &request,
                &ctx,
                Some(&continuation),
                crate::config::CodexTransport::Auto,
                &Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(response.transport, ActualTransport::Http);
        super::super::update_continuation_from_upstream(
            None,
            &continuation,
            None,
            &request,
            &response.body,
            response.socket_id,
            false,
        );
        let next_request = buffered_request_with_texts(&["one", "two", "three"]);
        let next = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &next_request,
            true,
        );
        assert_ne!(
            next.candidate().previous_response_id.as_deref(),
            Some("resp_old")
        );
        server.await.unwrap();
        super::super::continuation::abort_continuation_for_owner(&next);
    }

    #[tokio::test]
    async fn auto_never_uses_http_after_websocket_delta_was_sent() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        for missing_response in [false, true] {
            let owner = ConversationIdentity::Main(format!("sent-delta-{missing_response}"));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = next_websocket_json(&mut ws).await;
                send_completed_websocket_response(&mut ws, "resp_origin").await;
                let delta = next_websocket_json(&mut ws).await;
                assert_eq!(delta["previous_response_id"], "resp_origin");
                if missing_response {
                    send_nested_previous_response_missing(&mut ws).await;
                } else {
                    send_completed_websocket_response(&mut ws, "resp_empty").await;
                }
                let (mut socket, _) = listener.accept().await.unwrap();
                if missing_response {
                    let handshake = read_http_request(&mut socket).await;
                    assert!(handshake.starts_with(b"GET "));
                    socket.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await.unwrap();
                    assert!(
                        tokio::time::timeout(Duration::from_millis(1200), listener.accept())
                            .await
                            .is_err()
                    );
                } else {
                    let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                    let full = next_websocket_json(&mut ws).await;
                    assert!(full.get("previous_response_id").is_none());
                    send_completed_websocket_response(&mut ws, "resp_retry").await;
                }
            });
            let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
            let ctx = http_test_context();
            let first = buffered_request_with_texts(&["one"]);
            let first_candidate = super::super::continuation::continuation_candidate_for_owner(
                Some(&owner),
                &first,
                true,
            );
            let first_response = client
                .post_codex_with_transport(
                    &first,
                    &ctx,
                    Some(&first_candidate),
                    crate::config::CodexTransport::WebSocket,
                    &Default::default(),
                )
                .await
                .unwrap();
            super::super::update_continuation_from_upstream(
                None,
                &first_candidate,
                None,
                &first,
                &first_response.body,
                first_response.socket_id,
                false,
            );
            let request = buffered_request_with_texts(&["one", "two"]);
            let candidate = super::super::continuation::continuation_candidate_for_owner(
                Some(&owner),
                &request,
                true,
            );
            client.request_compression = crate::config::CodexRequestCompression::Gzip;
            client.auto_ws_max_bytes = 1;
            let cache = CodexHttpBodyCache::new();
            let result = client
                .post_codex_with_transport(
                    &request,
                    &ctx,
                    Some(&candidate),
                    crate::config::CodexTransport::Auto,
                    &cache,
                )
                .await;
            if missing_response {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().transport, ActualTransport::WebSocket);
                let full = candidate.full_context_retry();
                let retry = client
                    .post_codex_with_transport(
                        &request,
                        &ctx,
                        Some(&full),
                        crate::config::CodexTransport::Auto,
                        &cache,
                    )
                    .await
                    .unwrap();
                assert_eq!(retry.transport, ActualTransport::WebSocket);
            }
            server.await.unwrap();
            super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
            super::super::continuation::abort_continuation_for_owner(&candidate);
        }
    }

    #[test]
    fn buffered_turn_keeps_transport_when_config_changes() {
        const CHILD: &str = "TEST_TRANSPORT_FREEZE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "providers::codex::client_buffered::tests::buffered_turn_keeps_transport_when_config_changes"])
                .env(CHILD, "1")
                .status().unwrap();
            assert!(status.success());
            return;
        }
        let config_dir = crate::paths::config_dir();
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.json");
        std::fs::write(&config_path, r#"{"codex":{"transport":"websocket"}}"#).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                for _ in 0..2 {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                    let _ = next_websocket_json(&mut ws).await;
                    send_completed_websocket_response(&mut ws, "resp_empty").await;
                }
            });
            let client = authenticated_http_test_client(format!("http://{addr}/responses"));
            let request = buffered_test_request();
            let ctx = http_test_context();
            let cache = CodexHttpBodyCache::new();
            let transport = crate::config::codex_transport();
            assert_eq!(transport, crate::config::CodexTransport::WebSocket);
            let first = client
                .post_codex_for_owner(&request, &ctx, None, transport, &cache)
                .await
                .unwrap();
            assert_eq!(first.transport, ActualTransport::WebSocket);
            std::fs::write(&config_path, r#"{"codex":{"transport":"http"}}"#).unwrap();
            assert_eq!(
                crate::config::codex_transport(),
                crate::config::CodexTransport::Http
            );
            let second = client
                .post_codex_for_owner(&request, &ctx, None, transport, &cache)
                .await
                .unwrap();
            assert_eq!(second.transport, ActualTransport::WebSocket);
            server.await.unwrap();
        });
    }

    #[tokio::test]
    async fn auto_falls_back_to_http_after_statusful_websocket_handshake_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut websocket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            let read = websocket.read(&mut request).await.unwrap();
            assert!(read > 0);
            assert!(
                String::from_utf8_lossy(&request[..read])
                    .to_ascii_lowercase()
                    .contains("upgrade: websocket")
            );
            websocket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 13\r\nconnection: close\r\n\r\npolicy denied",
                )
                .await
                .unwrap();
            drop(websocket);

            let (mut http, _) = listener.accept().await.unwrap();
            let read = http.read(&mut request).await.unwrap();
            assert!(read > 0);
            assert!(String::from_utf8_lossy(&request[..read]).starts_with("POST "));
            let body = b"data: keep\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            http.write_all(response.as_bytes()).await.unwrap();
            http.write_all(body).await.unwrap();
        });

        let response = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Auto,
                &Default::default(),
            )
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"data: keep\n\n");
    }

    #[tokio::test]
    async fn over_budget_retry_after_stops_without_replay() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nretry-after: 120\r\nconnection: close\r\n\r\nstop",
                )
                .await
                .unwrap();
        });

        let error = match authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
                &Default::default(),
            )
            .await
        {
            Ok(_) => panic!("over-budget Retry-After should propagate"),
            Err(error) => error,
        };
        server.await.unwrap();
        assert_eq!(error.status, 503);
        assert_eq!(error.retry_after.as_deref(), Some("120"));
    }

    #[tokio::test]
    async fn buffered_http_rejects_non_retryable_error_status_before_sse_parsing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let body = br#"{"error":{"message":"Model not found gpt-test"}}"#;
            let response = format!(
                "HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let result = authenticated_http_test_client(format!("http://{addr}/responses"))
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
                &Default::default(),
            )
            .await;
        server.await.unwrap();
        let error = match result {
            Ok(_) => panic!("non-success HTTP status must not reach the SSE reducer"),
            Err(error) => error,
        };

        assert_eq!(error.status, 404);
        assert_eq!(error.detail.as_deref(), Some("Model not found gpt-test"));
        assert_eq!(error.origin, CodexErrorOrigin::BufferedHttp);
    }

    async fn empty_http_body(client: &CodexHttpClient) -> CodexHttpBody {
        client
            .prepare_http_body(&serde_json::json!({}), false, "serialize test request")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn active_http_body_can_exceed_header_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for chunk in [b"a".as_slice(), b"b", b"c"] {
                stream.write_all(b"1\r\n").await.unwrap();
                stream.write_all(chunk).await.unwrap();
                stream.write_all(b"\r\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(45)).await;
            }
            stream.write_all(b"0\r\n\r\n").await.unwrap();
        });

        let client = http_test_client(format!("http://{addr}/responses"), 80);
        let body = empty_http_body(&client).await;
        let response = client
            .attempt_post_http(&http_test_auth(), &body, &http_test_context(), false)
            .await
            .expect("active body should not hit a whole-request timeout");
        server.await.unwrap();

        assert_eq!(response.body, b"abc");
    }

    #[tokio::test]
    async fn stalled_http_body_hits_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let client = http_test_client(format!("http://{addr}/responses"), 30);
        let body = empty_http_body(&client).await;
        let result = client
            .attempt_post_http(&http_test_auth(), &body, &http_test_context(), false)
            .await;
        server.await.unwrap();
        let error = result.err().expect("stalled body should time out");

        assert!(error.message.contains("next Codex response body chunk"));
        assert_eq!(error.detail.as_deref(), Some("http_response_body"));
    }

    #[tokio::test]
    async fn reset_http_body_returns_transport_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\npartial")
                .await
                .unwrap();
        });

        let client = http_test_client(format!("http://{addr}/responses"), 100);
        let body = empty_http_body(&client).await;
        let result = client
            .attempt_post_http(&http_test_auth(), &body, &http_test_context(), false)
            .await;
        server.await.unwrap();
        let error = result.err().expect("truncated body should fail");

        assert!(
            error
                .message
                .contains("Transport error reading Codex response body")
        );
        assert_eq!(error.detail.as_deref(), Some("http_response_body"));
    }
}
