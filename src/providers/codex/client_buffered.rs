use super::auth::token_store::StoredAuth;
use super::client::*;
use super::client_body::{CodexHttpBody, CodexHttpBodyCache};
use super::client_live::*;
use super::recovery::buffered_transient_exhausted;
use super::translate::request::ResponsesRequest;
use crate::config::CodexTransport;
use crate::provider::RequestContext;
#[cfg(test)]
use crate::request_identity::ConversationIdentity;
use crate::retry::{compute_backoff_delay, sleep};
use std::time::{Duration, Instant};
impl CodexHttpClient {
    pub(super) async fn post_codex_with_transport(
        &self,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
        transport: crate::config::CodexTransport,
        http_body: &CodexHttpBodyCache,
    ) -> Result<OwnerAwareCodexResponse, CodexError> {
        if transport == CodexTransport::Http {
            ctx.recovery_budget.stamp(
                super::sources::FailureKind::Final,
                super::api::SendEvidence::NotSent,
                None,
            );
        }
        if transport == CodexTransport::WebSocket
            || (transport == CodexTransport::Auto && ctx.recovery_budget.is_tracked())
        {
            ctx.recovery_budget.enable_typed_websocket();
            ctx.recovery_budget.begin_websocket();
        }
        let mut auth = if transport == CodexTransport::Http || ctx.recovery_budget.is_tracked() {
            self.auth_for_buffered_http(&body.reasoning_account, &ctx.recovery_budget)
                .await?
        } else {
            self.auth_for(&body.reasoning_account).await?
        };
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
        let mut missing_continuation_socket = false;
        loop {
            if transport == CodexTransport::Http {
                ctx.recovery_budget.begin_http_attempt();
            }
            let sent_before_attempt = http_body
                .websocket_sent
                .load(std::sync::atomic::Ordering::Acquire);
            let mut hosted_http = false;
            let mut actual_http = transport == CodexTransport::Http;
            let result = match transport {
                CodexTransport::Http => {
                    let http_body = http_body.get_or_try_init(prepare_http_body).await?;
                    self.attempt_post_http(
                        &auth,
                        http_body,
                        ctx,
                        body.client_metadata.is_some(),
                        &mut hosted_http,
                        ctx.recovery_budget.is_tracked(),
                    )
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
                        Some(super::sources::CredentialId::for_chatgpt_auth(&auth)),
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
                    if !sent_before_attempt
                        && self
                            .auto_size_http_selected(
                                body,
                                ctx,
                                active_continuation.as_ref(),
                                http_body,
                            )
                            .await? =>
                {
                    actual_http = true;
                    super::client_body::record_size_fallback(&mut missing_continuation_socket);
                    ctx.recovery_budget.begin_http_attempt();
                    http_body
                        .size_http_selected
                        .store(true, std::sync::atomic::Ordering::Release);
                    let http_body = http_body.get_or_try_init(prepare_http_body).await?;
                    self.attempt_post_http(
                        &auth,
                        http_body,
                        ctx,
                        body.client_metadata.is_some(),
                        &mut hosted_http,
                        ctx.recovery_budget.is_tracked(),
                    )
                    .await
                    .map(|response| OwnerAwareCodexResponse::new(response, None))
                }
                CodexTransport::Auto => {
                    ctx.recovery_budget.begin_websocket();
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
                        Some(super::sources::CredentialId::for_chatgpt_auth(&auth)),
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
                                && !ctx.recovery_budget.hosted()
                                && !http_body
                                    .websocket_sent
                                    .load(std::sync::atomic::Ordering::Acquire)
                                && should_fallback_to_http(&err) =>
                        {
                            if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                                return Err(CodexError::recovery_exhausted());
                            }
                            super::websocket_pool::HTTP_FALLBACK_COUNT
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            actual_http = true;
                            ctx.recovery_budget.begin_http_attempt();
                            let http_body = http_body.get_or_try_init(prepare_http_body).await?;
                            self.attempt_post_http(
                                &auth,
                                http_body,
                                ctx,
                                body.client_metadata.is_some(),
                                &mut hosted_http,
                                ctx.recovery_budget.is_tracked(),
                            )
                            .await
                            .map(|response| OwnerAwareCodexResponse::new(response, None))
                        }
                        Err(err) => Err(err),
                    }
                }
            };
            if actual_http
                && (transport == CodexTransport::Http || ctx.recovery_budget.is_tracked())
            {
                let uncertain = match &result {
                    Ok(response) => response.status >= 500,
                    Err(_) => true,
                };
                if uncertain
                    && ctx.recovery_budget.send() == super::api::SendEvidence::OutcomeUnknown
                {
                    ctx.recovery_budget.mark_ambiguous_hosted();
                }
            }
            if hosted_http && result.is_err() {
                return result;
            }
            let result = result.map(|mut response| {
                response.hosted_tool_executed |= hosted_http;
                response
            });
            if !actual_http
                && (transport == CodexTransport::WebSocket || ctx.recovery_budget.is_tracked())
                && let Ok(response) = &result
                && let Some(error) = ctx.recovery_budget.websocket_permanent_error(response)
            {
                return Err(error);
            }
            let result = match result {
                Ok(response) if actual_http && ctx.recovery_budget.is_tracked() => {
                    if super::client::codex_status_error_message(&response.body)
                        .is_some_and(|message| super::events::is_context_overflow(None, &message))
                    {
                        ctx.recovery_budget.mark_final_only();
                        return Err(codex_status_error(response.into_response()));
                    }
                    if let Some(failure) =
                        super::client::explicit_permanent_buffered_error(&response.body)
                    {
                        if (200..300).contains(&response.status) {
                            let message = failure.anthropic_message();
                            return Err(CodexError {
                                status: failure.status,
                                message: message.clone(),
                                detail: Some(message),
                                retry_after: failure.retry_after,
                                usage_limit: None,
                                origin: CodexErrorOrigin::BufferedHttp,
                            });
                        }
                        return Err(codex_status_error(response.into_response()));
                    }
                    Ok(response)
                }
                other => other,
            };
            let typed_transient = if !actual_http && ctx.recovery_budget.is_tracked() {
                match &result {
                    Ok(response) => super::recovery::buffered_websocket_transient(&response.body),
                    Err(_) => true,
                }
            } else {
                true
            };
            let exhausted = || buffered_transient_exhausted(ctx, transport, typed_transient);
            let replay_allowed =
                super::recovery::buffered_replay_allowed(&result, &ctx.recovery_budget);
            if replay_allowed
                && should_refresh_after_unauthorized(&result, auth_refresh_attempted, transport)
            {
                auth_refresh_attempted = true;
                ctx.recovery_budget.start();
                let refreshed =
                    if transport == CodexTransport::Http || ctx.recovery_budget.is_tracked() {
                        self.refreshed_auth_buffered_http(&auth, &ctx.recovery_budget)
                            .await
                    } else {
                        self.refreshed_auth(&auth).await
                    };
                auth = refreshed.inspect_err(|error| {
                    if error.detail.as_deref() == Some(CODEX_ACCOUNT_CHANGED_DETAIL) {
                        ctx.recovery_budget.cancel_unspent_start();
                    }
                    if !sent_before_attempt
                        && result.as_ref().is_ok_and(|response| {
                            response.transport == ActualTransport::WebSocket
                                && response.status == 401
                                && !crate::anthropic::sse::parse_sse_events(&response.body)
                                    .into_iter()
                                    .filter_map(|event| serde_json::from_str(&event.data).ok())
                                    .any(|event| {
                                        super::events::classify_stream_event(&event)
                                            == super::events::CodexStreamEventKind::Semantic
                                    })
                        })
                        && error.detail.as_deref() == Some(CODEX_ACCOUNT_CHANGED_DETAIL)
                    {
                        http_body
                            .websocket_sent
                            .store(false, std::sync::atomic::Ordering::Release);
                    }
                })?;
                if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                    return Err(CodexError::recovery_exhausted());
                }
                invalidate_live_continuation_pool(active_continuation.as_ref());
                active_continuation = full_context_continuation(active_continuation.as_ref());
                continue;
            }
            if let Ok(response) = &result
                && let Some(limit) =
                    super::events::usage_limit_from_response(&response.body, &response.headers)
            {
                if !actual_http && ctx.recovery_budget.is_tracked() {
                    ctx.recovery_budget
                        .set_kind(super::sources::FailureKind::UsageLimit);
                    ctx.recovery_budget.mark_ambiguous_hosted();
                } else if actual_http
                    && (transport == CodexTransport::Http || ctx.recovery_budget.is_tracked())
                {
                    ctx.recovery_budget
                        .set_kind(super::sources::FailureKind::UsageLimit);
                    let pre_execution =
                        super::recovery::RecoveryBudget::quota_denied_before_execution(
                            response.status,
                            &response.body,
                            response.hosted_tool_executed,
                        );
                    if pre_execution {
                        ctx.recovery_budget
                            .set_send(super::api::SendEvidence::DeniedBeforeExecution);
                    } else {
                        ctx.recovery_budget.mark_ambiguous_hosted();
                    }
                }
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
                if actual_http
                    && (transport == CodexTransport::Http || ctx.recovery_budget.is_tracked())
                {
                    ctx.recovery_budget.mark_ambiguous_hosted();
                }
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
                    if !ctx
                        .recovery_budget
                        .try_retry(Duration::from_millis(delay.wait_ms), false)
                    {
                        return Err(exhausted());
                    }
                    transport_failures += 1;
                    active_continuation = full_context_continuation(active_continuation.as_ref());
                    sleep(delay.wait_ms).await;
                    if ctx.recovery_budget.expired() {
                        return Err(exhausted());
                    }
                    continue;
                }
                if replay_allowed && transport_failures >= MAX_BUFFERED_TRANSPORT_RETRIES {
                    return Err(exhausted());
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
                        if !ctx
                            .recovery_budget
                            .try_retry(Duration::from_millis(delay.wait_ms), false)
                        {
                            return Err(exhausted());
                        }
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        if ctx.recovery_budget.expired() {
                            return Err(exhausted());
                        }
                        continue;
                    }
                    if replay_allowed && transport_failures >= MAX_BUFFERED_TRANSPORT_RETRIES {
                        return Err(exhausted());
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
                        if !ctx
                            .recovery_budget
                            .try_retry(Duration::from_millis(delay.wait_ms), false)
                        {
                            return Err(exhausted());
                        }
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        if ctx.recovery_budget.expired() {
                            return Err(exhausted());
                        }
                        continue;
                    }
                    if replay_allowed && transport_failures >= MAX_BUFFERED_TRANSPORT_RETRIES {
                        return Err(exhausted());
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
                    if !ctx.recovery_budget.try_retry(Duration::ZERO, false) {
                        return Err(CodexError::recovery_exhausted());
                    }
                    missing_continuation_socket = err.detail.as_deref()
                        == Some(super::websocket::WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL);
                    active_continuation = full_context_continuation(active_continuation.as_ref());
                    continue;
                }
                Err(err) => {
                    if !actual_http
                        && (transport == CodexTransport::WebSocket
                            || ctx.recovery_budget.is_tracked())
                    {
                        ctx.recovery_budget.record_websocket_handshake_error(&err);
                    }
                    let retryable = !err.is_recovery_exhausted()
                        && err.usage_limit.is_none()
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
                        if !ctx
                            .recovery_budget
                            .try_retry(Duration::from_millis(delay.wait_ms), false)
                        {
                            return Err(exhausted());
                        }
                        transport_failures += 1;
                        sleep(delay.wait_ms).await;
                        if ctx.recovery_budget.expired() {
                            return Err(exhausted());
                        }
                        continue;
                    }
                    if retryable && transport_failures >= MAX_BUFFERED_TRANSPORT_RETRIES {
                        return Err(exhausted());
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
        hosted_http: &mut bool,
        tracked: bool,
    ) -> Result<CodexResponse, CodexError> {
        let (resp, started_at) = self
            .start_post_http_inner(auth, http_body, ctx, use_responses_lite, tracked)
            .await?;
        self.collect_http_response_inner(resp, started_at, ctx, hosted_http, tracked)
            .await
    }
    pub(super) async fn start_post_http(
        &self,
        auth: &StoredAuth,
        http_body: &CodexHttpBody,
        ctx: &RequestContext,
        use_responses_lite: bool,
    ) -> Result<(reqwest::Response, Instant), CodexError> {
        self.start_post_http_inner(auth, http_body, ctx, use_responses_lite, false)
            .await
    }
    pub(super) async fn start_post_http_inner(
        &self,
        auth: &StoredAuth,
        http_body: &CodexHttpBody,
        ctx: &RequestContext,
        use_responses_lite: bool,
        tracked: bool,
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
        if tracked {
            ctx.recovery_budget
                .set_send(super::api::SendEvidence::OutcomeUnknown);
        }
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
                if tracked && e.is_connect() {
                    ctx.recovery_budget
                        .set_send(super::api::SendEvidence::NotSent);
                }
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

    #[tokio::test]
    async fn continuation_off_reuses_only_the_same_conversation_socket() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut first = tokio_tungstenite::accept_async(socket).await.unwrap();
            for id in ["first", "second"] {
                let request = next_websocket_json(&mut first).await;
                assert!(request.get("previous_response_id").is_none());
                send_completed_websocket_response(&mut first, id).await;
            }
            let (socket, _) = listener.accept().await.unwrap();
            let mut second = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut second).await;
            send_completed_websocket_response(&mut second, "third").await;
        });
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let context = http_test_context();
        let request = buffered_request_with_texts(&["one"]);
        let owner = ConversationIdentity::Main("off-sequential".into());
        let other = ConversationIdentity::Main("off-other".into());
        let mut socket_ids = Vec::new();
        for (owner, id) in [(&owner, "first"), (&owner, "second"), (&other, "third")] {
            let reservation = super::super::continuation::continuation_candidate_for_owner(
                Some(owner),
                &request,
                false,
            );
            let response = client
                .post_codex_with_transport(
                    &request,
                    &context,
                    Some(&reservation),
                    crate::config::CodexTransport::WebSocket,
                    &Default::default(),
                )
                .await
                .unwrap();
            assert!(String::from_utf8_lossy(&response.body).contains(id));
            socket_ids.push(response.socket_id.unwrap());
        }
        assert_eq!(socket_ids[0], socket_ids[1]);
        assert_ne!(socket_ids[1], socket_ids[2]);
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn stale_output_on_reused_socket_is_discarded_and_retried() {
        use futures_util::SinkExt;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut first = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut first).await;
            send_completed_websocket_response(&mut first, "first").await;
            let _ = next_websocket_json(&mut first).await;
            first
                .send(tokio_tungstenite::tungstenite::Message::text(
                    r#"{"type":"response.output_text.delta","delta":"stale-marker"}"#,
                ))
                .await
                .unwrap();
            let (socket, _) = listener.accept().await.unwrap();
            let mut retry = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut retry).await;
            send_completed_websocket_response(&mut retry, "retry").await;
        });
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let context = http_test_context();
        let request = buffered_request_with_texts(&["one"]);
        let owner = ConversationIdentity::Main("off-stale".into());
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        client
            .post_codex_with_transport(
                &request,
                &context,
                Some(&reservation),
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
            .unwrap();
        let response = client
            .post_codex_with_transport(
                &request,
                &context,
                Some(&reservation),
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&response.body).contains("stale-marker"));
        assert!(String::from_utf8_lossy(&response.body).contains("retry"));
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn changed_credential_and_lane_close_the_previous_socket() {
        use futures_util::StreamExt;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (index, id) in ["first", "second", "third"].into_iter().enumerate() {
                let (socket, _) = listener.accept().await.unwrap();
                let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = next_websocket_json(&mut websocket).await;
                send_completed_websocket_response(&mut websocket, id).await;
                if index < 2 {
                    let closed =
                        tokio::time::timeout(std::time::Duration::from_secs(5), websocket.next())
                            .await
                            .unwrap();
                    assert!(matches!(
                        closed,
                        None | Some(Err(_))
                            | Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
                    ));
                }
            }
        });
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let context = http_test_context();
        let mut request = buffered_request_with_texts(&["one"]);
        let owner = ConversationIdentity::Main("off-identity".into());
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        client
            .post_codex_with_transport(
                &request,
                &context,
                Some(&reservation),
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
            .unwrap();
        let mut changed_auth = http_test_auth();
        changed_auth.access = "synthetic-other".into();
        client.auth_manager().set_test_auth(changed_auth);
        client
            .post_codex_with_transport(
                &request,
                &context,
                Some(&reservation),
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
            .unwrap();
        request.client_metadata = Some(Default::default());
        client
            .post_codex_with_transport(
                &request,
                &context,
                Some(&reservation),
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
            .unwrap();
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn refreshed_access_reuses_socket_but_changed_refresh_credential_does_not() {
        use futures_util::StreamExt;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            for id in ["first", "second"] {
                let _ = next_websocket_json(&mut ws).await;
                send_completed_websocket_response(&mut ws, id).await;
            }
            let closed = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
                .await
                .unwrap();
            assert!(matches!(
                closed,
                None | Some(Err(_)) | Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))
            ));
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut ws).await;
            send_completed_websocket_response(&mut ws, "third").await;
        });
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let mut auth = http_test_auth();
        auth.refresh = "synthetic-refresh-one".into();
        client.auth_manager().set_test_auth(auth.clone());
        let request = buffered_request_with_texts(&["one"]);
        let context = http_test_context();
        let owner = ConversationIdentity::Main("refresh-identity".into());
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        for index in 0..3 {
            if index == 1 {
                auth.access = "synthetic-access-two".into();
                client.auth_manager().set_test_auth(auth.clone());
            }
            if index == 2 {
                auth.refresh = "synthetic-refresh-two".into();
                client.auth_manager().set_test_auth(auth.clone());
            }
            client
                .post_codex_with_transport(
                    &request,
                    &context,
                    Some(&reservation),
                    crate::config::CodexTransport::WebSocket,
                    &Default::default(),
                )
                .await
                .unwrap();
        }
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn fresh_responses_without_created_or_status_succeed_without_pooling() {
        use futures_util::SinkExt;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for with_created in [false, true] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = next_websocket_json(&mut ws).await;
                if with_created {
                    ws.send(tokio_tungstenite::tungstenite::Message::text(
                        r#"{"type":"response.created","response":{"id":"response"}}"#,
                    ))
                    .await
                    .unwrap();
                }
                ws.send(tokio_tungstenite::tungstenite::Message::text(
                    r#"{"type":"response.output_text.delta","output_index":0,"delta":"answer"}"#,
                ))
                .await
                .unwrap();
                let terminal = if with_created {
                    serde_json::json!({"type":"response.completed","response":{"id":"response"}})
                } else {
                    serde_json::json!({"type":"response.completed","response":{"id":"response","status":"completed"}})
                };
                ws.send(tokio_tungstenite::tungstenite::Message::text(
                    terminal.to_string(),
                ))
                .await
                .unwrap();
            }
        });
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let context = http_test_context();
        let request = buffered_request_with_texts(&["one"]);
        for name in ["missing-created", "missing-status"] {
            let owner = ConversationIdentity::Main(name.into());
            let reservation = super::super::continuation::continuation_candidate_for_owner(
                Some(&owner),
                &request,
                false,
            );
            let response = client
                .post_codex_with_transport(
                    &request,
                    &context,
                    Some(&reservation),
                    crate::config::CodexTransport::WebSocket,
                    &Default::default(),
                )
                .await
                .unwrap();
            assert_eq!(response.status, 200);
            assert!(super::super::websocket::pooled_socket_id_for_tests(&owner).is_none());
        }
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

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
    async fn tracked_websocket_handshake_failure_is_not_sent_with_credential() {
        use super::super::sources::{ReplayGate, SourceAttempt};
        let client = authenticated_http_test_client("not-a-url".to_string());
        let expected = client
            .auth_manager()
            .current_credential_snapshot()
            .await
            .unwrap()
            .unwrap()
            .id;
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(true),
        );
        let error = match client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                crate::config::CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
        {
            Ok(_) => panic!("expected handshake failure"),
            Err(error) => error,
        };
        let failure = attempt.failure(error);
        assert!(failure.send == super::super::api::SendEvidence::NotSent);
        assert!(failure.kind == super::super::sources::FailureKind::Final);
        assert!(failure.credential == Some(expected));
    }

    #[tokio::test]
    async fn tracked_buffered_websocket_handshake_quota_requires_structured_origin_body() {
        use super::super::api::SendEvidence;
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        for case in [0, 1, 2] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_http_request(&mut socket).await;
                let body: &[u8] = match case {
                    0 => br#"{"error":{"type":"usage_limit_reached","message":"private diagnostic","resets_at":1234567890}}"#,
                    1 => br#"{"error":{"message":"rate limit"}}"#,
                    _ => b"data: {\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"limit\"}}\n\n",
                };
                let headers = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-length: {}\r\nretry-after: 120\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
            });
            let gate = ReplayGate::new(true);
            let attempt = SourceAttempt::new(
                crate::config::codex_auth::AuthSource::ChatGpt,
                &http_test_context(),
                gate.clone(),
            );
            let client = authenticated_http_test_client(format!("http://{addr}/responses"));
            let error = match client
                .post_codex_with_transport(
                    &buffered_test_request(),
                    &attempt.context,
                    None,
                    CodexTransport::WebSocket,
                    &Default::default(),
                )
                .await
            {
                Ok(_) => panic!("expected handshake denial"),
                Err(error) => error,
            };
            let failure = attempt.failure(error);
            assert!(failure.send == SendEvidence::NotSent);
            if case == 0 {
                assert!(failure.kind == FailureKind::UsageLimit);
                assert!(
                    !failure
                        .error
                        .detail
                        .as_deref()
                        .unwrap_or("")
                        .contains("private diagnostic")
                );
                assert_eq!(
                    failure
                        .error
                        .usage_limit
                        .as_ref()
                        .and_then(|limit| limit.resets_at),
                    Some(1234567890)
                );
                assert!(gate.may_switch(failure.kind, failure.send));
            } else {
                assert!(failure.kind == FailureKind::Final);
                assert!(failure.error.usage_limit.is_none());
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn tracked_buffered_websocket_quota_is_unknown_after_send() {
        use super::super::api::SendEvidence;
        use super::super::provider_test_support::{emit_live_event, next_live_websocket_request};
        use super::super::sources::{CredentialId, FailureKind, ReplayGate, SourceAttempt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            next_live_websocket_request(&mut socket).await;
            emit_live_event(
                &mut socket,
                &serde_json::json!({
                    "type": "response.failed",
                    "response": {"error": {"type": "usage_limit_reached", "message": "limit"}}
                }),
            )
            .await;
        });
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let error = match client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
        {
            Ok(_) => panic!("expected quota failure"),
            Err(error) => error,
        };
        let failure = attempt.failure(error);
        assert!(failure.kind == FailureKind::UsageLimit);
        assert!(failure.send == SendEvidence::OutcomeUnknown);
        assert!(failure.credential == Some(CredentialId::for_chatgpt_auth(&http_test_auth())));
        assert!(gate.ambiguous_hosted());
        assert!(!gate.may_switch(failure.kind, failure.send));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_buffered_websocket_context_overrides_quota_hint() {
        use super::super::provider_test_support::{emit_live_event, next_live_websocket_request};
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            next_live_websocket_request(&mut socket).await;
            emit_live_event(&mut socket, &serde_json::json!({
                "type": "error", "status": 429,
                "error": {"type": "usage_limit_reached", "code": "context_length_exceeded", "message": "too long"}
            })).await;
        });
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(true),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let error = match client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
        {
            Ok(_) => panic!("expected context failure"),
            Err(error) => error,
        };
        let failure = attempt.failure(error);
        assert!(failure.kind == FailureKind::Final);
        assert!(failure.error.usage_limit.is_none());
        assert_eq!(failure.error.status, 400);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_websocket_terminal_output_hosted_blocks_retry() {
        use super::super::provider_test_support::{emit_live_event, next_live_websocket_request};
        use super::super::sources::{ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            next_live_websocket_request(&mut socket).await;
            emit_live_event(&mut socket, &serde_json::json!({
                "type": "response.failed", "response": {"output": [{"type": "web_search_call"}], "error": {"code": "server_is_overloaded", "message": "busy"}}
            })).await;
        });
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let result = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                CodexTransport::WebSocket,
                &Default::default(),
            )
            .await;
        assert!(result.is_err());
        assert!(gate.hosted());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_websocket_invalid_upgrade_is_final() {
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sends = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = sends.clone();
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                read_http_request(&mut socket).await;
                socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nconnection: Upgrade\r\nupgrade: websocket\r\nsec-websocket-accept: invalid\r\n\r\n").await.unwrap();
            }
        });
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(false),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let error = match client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                CodexTransport::WebSocket,
                &Default::default(),
            )
            .await
        {
            Ok(_) => panic!("expected validation failure"),
            Err(error) => error,
        };
        assert!(error.origin == CodexErrorOrigin::WebSocketValidation);
        assert!(!error.is_recovery_exhausted());
        assert!(attempt.failure(error).kind == FailureKind::Final);
        assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 1);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn tracked_websocket_retains_hosted_evidence_on_drop() {
        use super::super::provider_test_support::assert_no_websocket_replay_after_hosted_search;
        use super::super::sources::{ReplayGate, SourceAttempt};

        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        assert_no_websocket_replay_after_hosted_search(
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "web_search_call", "id": "search_1", "status": "completed"}
            }),
            |client: CodexHttpClient| async move {
                let error = match client
                    .post_codex_with_transport(
                        &buffered_test_request(),
                        &attempt.context,
                        None,
                        crate::config::CodexTransport::WebSocket,
                        &Default::default(),
                    )
                    .await
                {
                    Ok(_) => panic!("expected socket failure"),
                    Err(error) => error,
                };
                let failure = attempt.failure(error);
                assert!(failure.send == super::super::api::SendEvidence::OutcomeUnknown);
                super::super::map_codex_error_to_response(&failure.error)
                    .status()
                    .as_u16()
            },
        )
        .await;
        assert!(gate.hosted());
        assert!(gate.ambiguous_hosted());
    }

    #[tokio::test]
    async fn tracked_auto_websocket_send_after_http_retry_remains_unknown() {
        use super::super::provider_test_support::{emit_live_event, next_live_websocket_request};
        use super::super::sources::{ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n").await.unwrap();
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            next_live_websocket_request(&mut ws).await;
            emit_live_event(&mut ws, &serde_json::json!({"type":"error", "status":401, "error":{"message":"unauthorized"}})).await;
        });
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(false),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let result = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                CodexTransport::Auto,
                &Default::default(),
            )
            .await;
        assert!(result.is_err());
        attempt.context.recovery_budget.stamp(
            super::super::sources::FailureKind::RejectedCredential,
            super::super::api::SendEvidence::NotSent,
            None,
        );
        assert!(
            attempt.context.recovery_budget.send()
                == super::super::api::SendEvidence::OutcomeUnknown
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_auto_unknown_websocket_terminal_is_not_transient_exhaustion() {
        use super::super::provider_test_support::{emit_live_event, next_live_websocket_request};
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            next_live_websocket_request(&mut socket).await;
            emit_live_event(
                &mut socket,
                &serde_json::json!({"type":"error", "error":{"message":"mystery"}}),
            )
            .await;
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
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let result = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                CodexTransport::Auto,
                &Default::default(),
            )
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("expected failure"),
        };
        assert!(attempt.failure(error).kind == FailureKind::Final);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_auto_websocket_retains_hosted_evidence_on_drop() {
        use super::super::provider_test_support::assert_no_websocket_replay_after_hosted_search;
        use super::super::sources::{ReplayGate, SourceAttempt};

        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        assert_no_websocket_replay_after_hosted_search(
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "web_search_call", "id": "search_1", "status": "completed"}
            }),
            |client: CodexHttpClient| async move {
                let error = match client
                    .post_codex_with_transport(
                        &buffered_test_request(),
                        &attempt.context,
                        None,
                        crate::config::CodexTransport::Auto,
                        &Default::default(),
                    )
                    .await
                {
                    Ok(_) => panic!("expected socket failure"),
                    Err(error) => error,
                };
                let failure = attempt.failure(error);
                assert!(failure.send == super::super::api::SendEvidence::OutcomeUnknown);
                assert!(failure.credential.is_some());
                super::super::map_codex_error_to_response(&failure.error)
                    .status()
                    .as_u16()
            },
        )
        .await;
        assert!(gate.hosted());
        assert!(gate.ambiguous_hosted());
    }

    #[tokio::test]
    async fn buffered_http_does_not_replay_truncated_hosted_search() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sends = std::cell::Cell::new(0);
        let server = async {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_request(&mut stream).await;
                sends.set(sends.get() + 1);
                if sends.get() == 2 {
                    return;
                }
                let body = b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"search_1\",\"status\":\"completed\"}}\n\n";
                let headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len() + 1
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        };
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let request = buffered_test_request();
        let context = http_test_context();
        let cache = CodexHttpBodyCache::new();
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = server => None,
                result = client.post_codex_with_transport(&request, &context, None, crate::config::CodexTransport::Http, &cache) => Some(result),
            }
        }).await.unwrap();
        assert_eq!(sends.get(), 1);
        assert!(result.unwrap().is_err());
    }

    #[tokio::test]
    async fn buffered_http_does_not_replay_hosted_search_without_terminal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sends = std::cell::Cell::new(0);
        let server = async {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_request(&mut stream).await;
                sends.set(sends.get() + 1);
                if sends.get() == 2 {
                    return;
                }
                let body = b"data: {\"type\":\"response.web_search_call.completed\"}\n\n";
                let headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        };
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let request = buffered_test_request();
        let context = http_test_context();
        let cache = CodexHttpBodyCache::new();
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = server => None,
                result = client.post_codex_with_transport(&request, &context, None, crate::config::CodexTransport::Http, &cache) => Some(result),
            }
        }).await.unwrap();
        assert_eq!(sends.get(), 1);
        assert!(result.unwrap().is_err());
    }

    #[tokio::test]
    async fn buffered_http_does_not_replay_hosted_after_invalid_frame() {
        assert_hosted_error_without_replay(
            b"data: not-json\n\ndata: {\"type\":\"response.web_search_call.completed\"}\n\n",
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn buffered_http_rejects_hosted_with_unterminated_terminal_frame() {
        assert_hosted_error_without_replay(
            b"data: {\"type\":\"response.web_search_call.completed\"}\n\ndata: {\"type\":\"response.completed\"}\n",
            false,
        ).await;
    }

    async fn assert_hosted_error_without_replay(body: &'static [u8], truncate: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sends = std::cell::Cell::new(0);
        let server = async {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_request(&mut stream).await;
                sends.set(sends.get() + 1);
                if sends.get() == 2 {
                    return;
                }
                let headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len() + usize::from(truncate)
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        };
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let request = buffered_test_request();
        let gate = super::super::sources::ReplayGate::new(true);
        let attempt = super::super::sources::SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let cache = CodexHttpBodyCache::new();
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                _ = server => None,
                result = client.post_codex_with_transport(&request, &attempt.context, None, crate::config::CodexTransport::Http, &cache) => Some(result),
            }
        }).await.unwrap();
        assert_eq!(sends.get(), 1);
        assert!(result.unwrap().is_err());
        assert!(gate.hosted());
    }

    #[tokio::test]
    async fn buffered_http_marks_completed_hosted_search() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            let body = b"data: {\"type\":\"response.web_search_call.completed\"}\n\ndata: {\"type\":\"response.completed\"}\n\n";
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let response = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
                &CodexHttpBodyCache::new(),
            )
            .await
            .unwrap();
        assert!(response.hosted_tool_executed);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn buffered_http_retries_truncated_success_without_hosted_search() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_request(&mut stream).await;
                let body = b"data: {\"type\":\"response.completed\"}\n\n";
                let length = body.len() + usize::from(attempt == 0);
                let headers = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n"
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let response = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
                &CodexHttpBodyCache::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        server.await.unwrap();
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
    async fn buffered_http_marks_hosted_tool_in_terminal_output() {
        assert_hosted_error_without_replay(
            b"data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"web_search_call\"}]}}\n\n",
            true,
        ).await;
    }

    #[tokio::test]
    async fn buffered_http_quota_after_semantic_output_is_ambiguous() {
        assert_buffered_quota_evidence(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\ndata: {\"type\":\"error\",\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"limit\"}}\n\n",
            false,
        ).await;
    }

    #[tokio::test]
    async fn buffered_http_final_unterminated_hosted_quota_vetoes_switch() {
        assert_buffered_quota_evidence(
            b"data: {\"type\":\"response.failed\",\"response\":{\"output\":[{\"type\":\"web_search_call\"}],\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"limit\"}}}\n",
            true,
        ).await;
    }

    #[tokio::test]
    async fn buffered_http_read_failure_retains_unterminated_hosted_evidence() {
        assert_hosted_error_without_replay(
            b"data: {\"type\":\"response.failed\",\"response\":{\"output\":[{\"type\":\"web_search_call\"}],\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"limit\"}}}",
            true,
        ).await;
    }

    async fn assert_buffered_quota_evidence(body: &'static [u8], hosted: bool) {
        use super::super::api::SendEvidence;
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let error = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                crate::config::CodexTransport::Http,
                &Default::default(),
            )
            .await
            .err()
            .unwrap();
        let failure = attempt.failure(error);
        assert!(failure.kind == FailureKind::UsageLimit);
        assert!(failure.send == SendEvidence::OutcomeUnknown);
        assert!(failure.credential.is_some());
        assert!(!gate.may_switch(failure.kind, failure.send));
        if hosted {
            assert!(gate.hosted());
        } else {
            assert!(gate.ambiguous_hosted());
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_auto_size_http_retains_quota_evidence() {
        assert_auto_buffered_quota_evidence(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\ndata: {\"type\":\"error\",\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"limit\"}}\n\n",
            false,
        ).await;
    }

    async fn assert_auto_buffered_quota_evidence(body: &'static [u8], hosted: bool) {
        use super::super::api::SendEvidence;
        use super::super::sources::{CredentialId, FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
        client.request_compression = crate::config::CodexRequestCompression::Gzip;
        client.auto_ws_max_bytes = 1;
        let error = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                crate::config::CodexTransport::Auto,
                &Default::default(),
            )
            .await
            .err()
            .unwrap();
        let failure = attempt.failure(error);
        assert!(failure.kind == FailureKind::UsageLimit);
        assert!(failure.send == SendEvidence::OutcomeUnknown);
        let auth = super::super::client_test_support::http_test_auth();
        let expected = CredentialId::for_chatgpt_auth(&auth);
        assert!(failure.credential == Some(expected));
        assert!(!gate.may_switch(failure.kind, failure.send));
        if hosted {
            assert!(gate.hosted());
        } else {
            assert!(gate.ambiguous_hosted());
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn buffered_http_context_error_on_500_is_not_retried() {
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            let body = br#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#;
            let headers = format!(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            tokio::time::timeout(Duration::from_millis(500), listener.accept())
                .await
                .is_err()
        });
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(false),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let error = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                crate::config::CodexTransport::Http,
                &Default::default(),
            )
            .await
            .err()
            .unwrap();
        assert!(attempt.failure(error).kind == FailureKind::Final);
        assert!(server.await.unwrap());
    }

    #[tokio::test]
    async fn buffered_http_sse_invalid_type_with_500_status_is_final() {
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            let body = b"data: {\"type\":\"response.failed\",\"status\":500,\"response\":{\"error\":{\"type\":\"invalid_request_error\",\"message\":\"invalid\"}}}\n\n";
            let headers = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            tokio::time::timeout(Duration::from_millis(500), listener.accept())
                .await
                .is_err()
        });
        let gate = ReplayGate::new(false);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let error = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                crate::config::CodexTransport::Http,
                &Default::default(),
            )
            .await
            .err()
            .unwrap();
        let failure = attempt.failure(error);
        assert!(failure.kind == FailureKind::Final);
        assert_eq!(failure.error.status, 400);
        assert!(!gate.may_switch(failure.kind, failure.send));
        assert!(server.await.unwrap());
    }

    #[tokio::test]
    async fn buffered_http_invalid_type_with_500_status_is_not_retried() {
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            let body = br#"{"status":500,"type":"error","error":{"type":"invalid_request_error","message":"invalid"}}"#;
            let headers = format!(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            tokio::time::timeout(Duration::from_millis(500), listener.accept())
                .await
                .is_err()
        });
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            ReplayGate::new(false),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let error = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                crate::config::CodexTransport::Http,
                &Default::default(),
            )
            .await
            .err()
            .unwrap();
        assert!(attempt.failure(error).kind == FailureKind::Final);
        assert!(server.await.unwrap());
    }

    #[tokio::test]
    async fn buffered_http_oversize_unterminated_hosted_frame_is_not_retried() {
        use super::super::sources::{ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = b"data: {\"type\":\"response.web_search_call.completed\"}";
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            let headers = format!(
                "HTTP/1.1 503 Service Unavailable\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            tokio::time::timeout(Duration::from_millis(500), listener.accept())
                .await
                .is_err()
        });
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let mut client = authenticated_http_test_client(format!("http://{addr}/responses"));
        client.buffered_response_limit = body.len() - 1;
        let error = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                crate::config::CodexTransport::Http,
                &Default::default(),
            )
            .await
            .err()
            .unwrap();
        assert!(attempt.failure(error).kind == super::super::sources::FailureKind::Final);
        assert!(gate.hosted());
        assert!(server.await.unwrap());
    }

    #[tokio::test]
    async fn buffered_http_ambiguity_survives_later_plan_denial() {
        use super::super::api::SendEvidence;
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_request(&mut stream).await;
                let (status, body): (&str, &[u8]) = if attempt == 0 {
                    ("503 Service Unavailable", b"")
                } else {
                    (
                        "429 Too Many Requests",
                        br#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#,
                    )
                };
                let headers = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let error = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                crate::config::CodexTransport::Http,
                &Default::default(),
            )
            .await
            .err()
            .unwrap();
        let failure = attempt.failure(error);
        assert!(failure.kind == FailureKind::UsageLimit);
        assert!(failure.send == SendEvidence::DeniedBeforeExecution);
        assert!(
            failure.credential
                == Some(super::super::sources::CredentialId::for_chatgpt_auth(
                    &http_test_auth()
                ))
        );
        assert!(gate.ambiguous_hosted());
        assert!(!gate.may_switch(failure.kind, failure.send));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn buffered_http_transient_exhaustion_has_typed_failure() {
        use super::super::api::SendEvidence;
        use super::super::sources::{FailureKind, ReplayGate, SourceAttempt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_request(&mut stream).await;
                stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nretry-after: 0\r\nconnection: close\r\n\r\n").await.unwrap();
            }
        });
        let gate = ReplayGate::new(false);
        let attempt = SourceAttempt::new(
            crate::config::codex_auth::AuthSource::ChatGpt,
            &http_test_context(),
            gate.clone(),
        );
        let client = authenticated_http_test_client(format!("http://{addr}/responses"));
        let error = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &attempt.context,
                None,
                crate::config::CodexTransport::Http,
                &Default::default(),
            )
            .await
            .err()
            .unwrap();
        let failure = attempt.failure(error);
        assert!(failure.kind == FailureKind::TransientExhausted);
        assert!(failure.send == SendEvidence::OutcomeUnknown);
        assert!(failure.credential.is_some());
        assert!(gate.may_switch(failure.kind, failure.send));
        server.await.unwrap();
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
        let mut ctx = http_test_context();
        let monitor = crate::monitor::MonitorHandle::default();
        monitor.request_started(
            &ctx.req_id,
            None,
            None,
            crate::monitor::EndpointKind::Messages,
        );
        ctx.monitor = Some(monitor.clone());
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
        assert!(
            monitor.snapshot().active[0]
                .first_upstream_event_at
                .is_some()
        );
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
        let before = super::super::websocket::codex_connection_counters().http_fallbacks;
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
        assert_eq!(
            super::super::websocket::codex_connection_counters().http_fallbacks,
            before + 1
        );
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
    async fn account_change_after_websocket_401_clears_only_the_rejected_send() {
        use futures_util::SinkExt;
        for (prior_send, output) in [(false, false), (true, false), (false, true)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = std::sync::Arc::new(authenticated_http_test_client(format!(
                "http://{addr}/responses"
            )));
            let server_client = client.clone();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = next_websocket_json(&mut ws).await;
                if output {
                    ws.send(tokio_tungstenite::tungstenite::Message::text(
                        serde_json::json!({"type":"response.output_text.delta", "delta":"output"})
                            .to_string(),
                    ))
                    .await
                    .unwrap();
                }
                let mut auth = http_test_auth();
                auth.access = "different-test-token".into();
                auth.account_id = Some("different-account".into());
                server_client.auth_manager().set_test_auth(auth);
                ws.send(tokio_tungstenite::tungstenite::Message::text(
                    serde_json::json!({"type":"error", "status":401, "error":{"message":"unauthorized"}}).to_string()
                )).await.unwrap();
            });
            let cache = CodexHttpBodyCache::new();
            cache
                .websocket_sent
                .store(prior_send, std::sync::atomic::Ordering::Release);
            let result = client
                .post_codex_with_transport(
                    &buffered_test_request(),
                    &http_test_context(),
                    None,
                    crate::config::CodexTransport::WebSocket,
                    &cache,
                )
                .await;
            assert_eq!(
                result.err().unwrap().detail.as_deref(),
                Some(CODEX_ACCOUNT_CHANGED_DETAIL)
            );
            assert_eq!(
                cache
                    .websocket_sent
                    .load(std::sync::atomic::Ordering::Acquire),
                prior_send || output
            );
            server.await.unwrap();
        }
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
            .attempt_post_http(
                &http_test_auth(),
                &body,
                &http_test_context(),
                false,
                &mut false,
                false,
            )
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
            .attempt_post_http(
                &http_test_auth(),
                &body,
                &http_test_context(),
                false,
                &mut false,
                false,
            )
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
            .attempt_post_http(
                &http_test_auth(),
                &body,
                &http_test_context(),
                false,
                &mut false,
                false,
            )
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
