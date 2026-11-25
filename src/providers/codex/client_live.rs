use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use crate::provider::RequestContext;
use crate::request_identity::ConversationIdentity;

use super::auth::token_store::StoredAuth;
use super::client::*;
use super::translate::request::ResponsesRequest;

pub(super) fn quota_for_auth(
    ctx: &RequestContext,
    auth: &StoredAuth,
) -> Option<crate::monitor::CodexQuotaObserver> {
    ctx.monitor.as_ref().and_then(|monitor| {
        let account = super::translate::reasoning_signature::ReasoningAccount::new(
            auth.account_id.as_deref(),
        );
        monitor.bind_codex_quota(&ctx.req_id, &account)
    })
}

impl CodexHttpClient {
    pub async fn stream_codex_websocket_events(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationCandidate>,
    ) -> Result<super::websocket::CodexWebSocketEventReceiver, CodexError> {
        let reservation =
            continuation.map(super::continuation::ContinuationReservation::from_public_candidate);
        self.stream_codex_websocket_events_for_owner(body, ctx, reservation.as_ref())
            .await
            .map(super::websocket::CodexWebSocketEventStream::into_receiver)
    }

    pub(crate) async fn stream_codex_websocket_events_for_owner(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        self.stream_codex_websocket_events_tracked(body, ctx, continuation, Arc::default())
            .await
    }

    pub(super) async fn stream_codex_websocket_events_tracked(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
        websocket_sent: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        self.stream_codex_websocket_events_prepared(body, ctx, continuation, websocket_sent, None)
            .await
    }

    pub(super) async fn stream_codex_websocket_events_prepared(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
        websocket_sent: Arc<std::sync::atomic::AtomicBool>,
        prepared: Option<(
            StoredAuth,
            super::websocket::ReadyWebSocket,
            Option<crate::monitor::CodexQuotaObserver>,
        )>,
    ) -> Result<super::websocket::CodexWebSocketEventStream, CodexError> {
        ctx.recovery_budget.begin_websocket();
        let (auth, ready, quota) = match prepared {
            Some((auth, ready, quota)) => (auth, Some(ready), quota),
            None => {
                let auth = if ctx.recovery_budget.is_tracked() {
                    self.auth_for_buffered_http(&body.reasoning_account, &ctx.recovery_budget)
                        .await?
                } else {
                    self.auth_for(&body.reasoning_account).await?
                };
                let quota = quota_for_auth(ctx, &auth);
                (auth, None, quota)
            }
        };

        if ctx.recovery_budget.is_tracked() {
            ctx.recovery_budget
                .set_credential(super::sources::CredentialId::for_chatgpt_auth(&auth));
        }
        let turn_id = continuation.and_then(super::continuation::ContinuationReservation::turn_id);
        if should_reset_websocket_pool(continuation)
            && let Some(owner) = websocket_pool_owner(continuation)
        {
            super::websocket::invalidate_codex_websocket_pool_turn_for_owner(owner, turn_id);
        }

        let client = self.clone();
        let body = body.clone();
        let ctx = ctx.clone();
        let continuation = continuation.cloned();
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let (rx, socket_id_publisher) = super::websocket::CodexWebSocketEventStream::pending(rx);
        tokio::spawn(async move {
            client
                .coordinate_live_websocket_events(
                    body,
                    ctx,
                    continuation,
                    auth,
                    tx,
                    socket_id_publisher,
                    websocket_sent,
                    ready,
                    quota,
                )
                .await;
        });

        Ok(rx)
    }

    #[allow(clippy::too_many_arguments)]
    async fn coordinate_live_websocket_events(
        &self,
        body: ResponsesRequest,
        ctx: RequestContext,
        mut continuation: Option<super::continuation::ContinuationReservation>,
        mut auth: StoredAuth,
        tx: tokio::sync::mpsc::Sender<Result<serde_json::Value, CodexError>>,
        socket_id_publisher: super::websocket::CodexWebSocketSocketIdPublisher,
        websocket_sent: Arc<std::sync::atomic::AtomicBool>,
        mut ready: Option<super::websocket::ReadyWebSocket>,
        quota: Option<crate::monitor::CodexQuotaObserver>,
    ) {
        let mut auth_refresh_attempted = false;
        let mut continuation_retry_available = continuation
            .as_ref()
            .and_then(|reservation| reservation.candidate().previous_response_id.as_deref())
            .is_some();
        let mut forwarded_any = false;
        let mut hosted_tool_executed = false;

        'attempt: loop {
            let sent_before_attempt = websocket_sent.load(std::sync::atomic::Ordering::Acquire);
            socket_id_publisher.publish_return(None, None);
            let ws_headers = match build_codex_headers(&auth, &ctx, body.client_metadata.is_some())
            {
                Ok(headers) => super::websocket::codex_websocket_headers(&headers),
                Err(err) => {
                    if tx.send(Err(err)).await.is_err() {
                        abort_abandoned_live_continuation(
                            continuation.as_ref(),
                            &socket_id_publisher,
                        );
                    }
                    return;
                }
            };
            let ws_body = build_websocket_request(
                &body,
                continuation
                    .as_ref()
                    .map(super::continuation::ContinuationReservation::candidate),
            );
            let start = super::websocket::codex_websocket_event_stream_prepared(
                ready.take(),
                &self.websocket_client,
                &self.websocket_proxy_config,
                &self.base_url,
                &ws_headers,
                Some(super::sources::CredentialId::for_chatgpt_auth(&auth)),
                &ws_body,
                &ctx,
                ctx.traffic.clone(),
                super::websocket::WEBSOCKET_CONNECT_TIMEOUT_MS,
                super::websocket::WEBSOCKET_IDLE_TIMEOUT_MS,
                continuation.as_ref(),
                websocket_sent.clone(),
            );
            let mut stream = tokio::select! {
                biased;
                _ = tx.closed() => {
                    abort_abandoned_live_continuation(
                        continuation.as_ref(),
                        &socket_id_publisher,
                    );
                    return;
                }
                result = start => match result {
                    Ok(stream) => stream,
                    Err(err) if err.status == 401 && !auth_refresh_attempted && !forwarded_any && !ctx.recovery_budget.hosted() => {
                        auth_refresh_attempted = true;
                        ctx.recovery_budget.start();
                        invalidate_live_continuation_pool(continuation.as_ref());
                        let refresh = if ctx.recovery_budget.is_tracked() { self.refreshed_auth_buffered_http(&auth, &ctx.recovery_budget).await } else { self.refreshed_auth(&auth).await };
                        auth = match refresh {
                            Ok(auth) => {
                                if tx.is_closed() {
                                    abort_abandoned_live_continuation(
                                        continuation.as_ref(),
                                        &socket_id_publisher,
                                    );
                                    return;
                                }
                                auth
                            },
                            Err(refresh_err) => {
                                if refresh_err.detail.as_deref() == Some(CODEX_ACCOUNT_CHANGED_DETAIL) {
                                    ctx.recovery_budget.cancel_unspent_start();
                                }
                                if tx.send(Err(refresh_err)).await.is_err() {
                                    abort_abandoned_live_continuation(
                                        continuation.as_ref(),
                                        &socket_id_publisher,
                                    );
                                }
                                return;
                            }
                        };
                        if !ctx.recovery_budget.try_retry(std::time::Duration::ZERO, false) {
                            let _ = tx.send(Err(CodexError::recovery_exhausted())).await;
                            return;
                        }
                        if continuation_retry_available {
                            socket_id_publisher.mark_full_context_retry();
                        }
                        continuation = full_context_continuation(continuation.as_ref());
                        continuation_retry_available = false;
                        continue 'attempt;
                    }
                    Err(err) if continuation_retry_available && !ctx.recovery_budget.hosted() && is_continuation_retry_error(&err) => {
                        if !ctx.recovery_budget.try_retry(std::time::Duration::ZERO, false) {
                            let _ = tx.send(Err(CodexError::recovery_exhausted())).await;
                            return;
                        }
                        socket_id_publisher.mark_full_context_retry();
                        continuation = full_context_continuation(continuation.as_ref());
                        continuation_retry_available = false;
                        continue 'attempt;
                    }
                    Err(err) => {
                        if tx.send(Err(err)).await.is_err() {
                            abort_abandoned_live_continuation(
                                continuation.as_ref(),
                                &socket_id_publisher,
                            );
                        }
                        return;
                    }
                }
            };

            let mut pending_events = Vec::new();
            loop {
                let item = tokio::select! {
                    biased;
                    _ = tx.closed() => {
                        if let Some(reservation) = continuation.as_ref() {
                            super::websocket::invalidate_codex_websocket_pool_return(
                                reservation,
                                stream.close_and_return_id(),
                            );
                        }
                        #[cfg(test)]
                        if let Some(hook) = tests::race_hook(continuation.as_ref()) {
                            hook.coordinator_reached.add_permits(1);
                            hook.coordinator_release.acquire().await.unwrap().forget();
                        }
                        abort_abandoned_live_continuation(
                            continuation.as_ref(),
                            &socket_id_publisher,
                        );
                        return;
                    }
                    item = stream.recv() => item,
                };
                if tx.is_closed() {
                    if let Some(reservation) = continuation.as_ref() {
                        super::websocket::invalidate_codex_websocket_pool_return(
                            reservation,
                            stream.close_and_return_id(),
                        );
                    }
                    abort_abandoned_live_continuation(continuation.as_ref(), &socket_id_publisher);
                    return;
                }
                let Some(item) = item else {
                    return;
                };
                if let (Some(quota), Ok(payload)) = (&quota, &item) {
                    quota.observe(payload);
                }
                socket_id_publisher.publish_return(stream.socket_id(), stream.return_id());

                hosted_tool_executed |= item
                    .as_ref()
                    .is_ok_and(super::events::event_confirms_hosted_tool_execution);
                let unauthorized = match &item {
                    Err(err) => err.status == 401,
                    Ok(payload) => super::websocket::event_error_status(payload) == Some(401),
                };
                let allow_refresh = if unauthorized && ctx.recovery_budget.is_tracked() {
                    item.as_ref().map_or(true, |payload| {
                        let (allow, permanent) = unauthorized_event_policy(payload);
                        if permanent {
                            ctx.recovery_budget.mark_final_only();
                        }
                        allow
                    })
                } else {
                    true
                };
                if unauthorized
                    && allow_refresh
                    && !auth_refresh_attempted
                    && !forwarded_any
                    && !hosted_tool_executed
                    && !ctx.recovery_budget.hosted()
                {
                    auth_refresh_attempted = true;
                    ctx.recovery_budget.start();
                    invalidate_live_continuation_pool(continuation.as_ref());
                    let refresh = if ctx.recovery_budget.is_tracked() {
                        self.refreshed_auth_buffered_http(&auth, &ctx.recovery_budget)
                            .await
                    } else {
                        self.refreshed_auth(&auth).await
                    };
                    auth = match refresh {
                        Ok(auth) => {
                            if tx.is_closed() {
                                if let Some(reservation) = continuation.as_ref() {
                                    super::websocket::invalidate_codex_websocket_pool_return(
                                        reservation,
                                        stream.close_and_return_id(),
                                    );
                                }
                                abort_abandoned_live_continuation(
                                    continuation.as_ref(),
                                    &socket_id_publisher,
                                );
                                return;
                            }
                            auth
                        }
                        Err(refresh_err) => {
                            if refresh_err.detail.as_deref() == Some(CODEX_ACCOUNT_CHANGED_DETAIL) {
                                ctx.recovery_budget.cancel_unspent_start();
                            }
                            if !sent_before_attempt
                                && refresh_err.detail.as_deref()
                                    == Some(CODEX_ACCOUNT_CHANGED_DETAIL)
                            {
                                websocket_sent.store(false, std::sync::atomic::Ordering::Release);
                            }
                            if tx.send(Err(refresh_err)).await.is_err() {
                                if let Some(reservation) = continuation.as_ref() {
                                    super::websocket::invalidate_codex_websocket_pool_return(
                                        reservation,
                                        stream.close_and_return_id(),
                                    );
                                }
                                abort_abandoned_live_continuation(
                                    continuation.as_ref(),
                                    &socket_id_publisher,
                                );
                            }
                            return;
                        }
                    };
                    if !ctx
                        .recovery_budget
                        .try_retry(std::time::Duration::ZERO, false)
                    {
                        let _ = tx.send(Err(CodexError::recovery_exhausted())).await;
                        return;
                    }
                    if continuation_retry_available {
                        socket_id_publisher.mark_full_context_retry();
                    }
                    continuation = full_context_continuation(continuation.as_ref());
                    continuation_retry_available = false;
                    continue 'attempt;
                }

                if let Err(err) = &item
                    && continuation_retry_available
                    && is_continuation_retry_error(err)
                    && !forwarded_any
                    && !hosted_tool_executed
                    && !ctx.recovery_budget.hosted()
                {
                    if !ctx
                        .recovery_budget
                        .try_retry(std::time::Duration::ZERO, false)
                    {
                        let _ = tx.send(Err(CodexError::recovery_exhausted())).await;
                        return;
                    }
                    socket_id_publisher.mark_full_context_retry();
                    continuation = full_context_continuation(continuation.as_ref());
                    continuation_retry_available = false;
                    continue 'attempt;
                }

                let terminal = item.as_ref().is_err()
                    || item.as_ref().is_ok_and(super::websocket::is_terminal_event);
                match item {
                    Ok(payload) => match super::events::classify_stream_event(&payload) {
                        super::events::CodexStreamEventKind::TerminalFailure => {
                            if !forwarded_any {
                                pending_events.clear();
                            }
                            if tx.send(Ok(payload)).await.is_err() {
                                cleanup_failed_live_forward(
                                    continuation.as_ref(),
                                    stream.close_and_return_id(),
                                    &socket_id_publisher,
                                );
                                return;
                            }
                        }
                        super::events::CodexStreamEventKind::TerminalSuccess => {
                            for pending in pending_events.drain(..) {
                                if tx.send(Ok(pending)).await.is_err() {
                                    cleanup_failed_live_forward(
                                        continuation.as_ref(),
                                        stream.close_and_return_id(),
                                        &socket_id_publisher,
                                    );
                                    return;
                                }
                            }
                            if tx.send(Ok(payload)).await.is_err() {
                                cleanup_failed_live_forward(
                                    continuation.as_ref(),
                                    stream.close_and_return_id(),
                                    &socket_id_publisher,
                                );
                                return;
                            }
                        }
                        super::events::CodexStreamEventKind::Semantic => {
                            if !forwarded_any {
                                forwarded_any = true;
                                for pending in pending_events.drain(..) {
                                    if tx.send(Ok(pending)).await.is_err() {
                                        cleanup_failed_live_forward(
                                            continuation.as_ref(),
                                            stream.close_and_return_id(),
                                            &socket_id_publisher,
                                        );
                                        return;
                                    }
                                }
                            }
                            if tx.send(Ok(payload)).await.is_err() {
                                cleanup_failed_live_forward(
                                    continuation.as_ref(),
                                    stream.close_and_return_id(),
                                    &socket_id_publisher,
                                );
                                return;
                            }
                        }
                        super::events::CodexStreamEventKind::Control => {
                            if tx.send(Ok(payload)).await.is_err() {
                                cleanup_failed_live_forward(
                                    continuation.as_ref(),
                                    stream.close_and_return_id(),
                                    &socket_id_publisher,
                                );
                                return;
                            }
                        }
                        super::events::CodexStreamEventKind::Structural => {
                            if forwarded_any {
                                if tx.send(Ok(payload)).await.is_err() {
                                    cleanup_failed_live_forward(
                                        continuation.as_ref(),
                                        stream.close_and_return_id(),
                                        &socket_id_publisher,
                                    );
                                    return;
                                }
                            } else {
                                pending_events.push(payload);
                            }
                        }
                    },
                    Err(err) => {
                        if tx.send(Err(err)).await.is_err() {
                            cleanup_failed_live_forward(
                                continuation.as_ref(),
                                stream.close_and_return_id(),
                                &socket_id_publisher,
                            );
                            return;
                        }
                    }
                }
                if terminal {
                    #[cfg(test)]
                    if continuation.as_ref().and_then(|r| r.owner())
                        == Some(&ConversationIdentity::Main("auto-terminal-drop".into()))
                    {
                        tests::AUTO_TERMINAL_QUEUED.add_permits(1);
                    }
                    return;
                }
            }
        }
    }
}

pub(super) fn full_context_continuation(
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> Option<super::continuation::ContinuationReservation> {
    continuation.map(super::continuation::ContinuationReservation::full_context_retry)
}

fn cleanup_failed_live_forward(
    continuation: Option<&super::continuation::ContinuationReservation>,
    return_id: Option<u64>,
    publisher: &super::websocket::CodexWebSocketSocketIdPublisher,
) {
    if let Some(reservation) = continuation {
        super::websocket::invalidate_codex_websocket_pool_return(reservation, return_id);
    }
    abort_abandoned_live_continuation(continuation, publisher);
}

fn abort_live_continuation(continuation: Option<&super::continuation::ContinuationReservation>) {
    if let Some(continuation) = continuation {
        super::continuation::abort_continuation_for_owner(continuation);
    }
}

fn abort_abandoned_live_continuation(
    continuation: Option<&super::continuation::ContinuationReservation>,
    socket_id_publisher: &super::websocket::CodexWebSocketSocketIdPublisher,
) {
    if !socket_id_publisher.is_provider_retry_handoff() {
        abort_live_continuation(continuation);
    }
}

pub(super) fn invalidate_live_continuation_pool(
    continuation: Option<&super::continuation::ContinuationReservation>,
) {
    let Some(continuation) = continuation else {
        return;
    };
    let Some(owner) = websocket_pool_owner(Some(continuation)) else {
        return;
    };
    super::websocket::invalidate_codex_websocket_pool_turn_for_owner(owner, continuation.turn_id());
}

#[cfg(test)]
fn event_closes_live_retry_window(payload: &serde_json::Value) -> bool {
    super::events::classify_stream_event(payload) == super::events::CodexStreamEventKind::Semantic
}

pub(super) fn websocket_pool_owner(
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> Option<&ConversationIdentity> {
    let continuation = continuation?;
    continuation.owner()
}

pub(super) fn should_reset_websocket_pool(
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> bool {
    let Some(reason) =
        continuation.and_then(|continuation| continuation.candidate().disabled_reason.as_deref())
    else {
        return false;
    };
    reason == "full_context_retry"
}

fn unauthorized_event_policy(payload: &serde_json::Value) -> (bool, bool) {
    let permanent = super::client_sse::explicit_permanent_event(payload).is_some()
        || super::events::classify_event_failure(payload)
            .is_some_and(|failure| super::events::is_context_overflow(None, &failure.message));
    let quota = super::events::usage_limit_from_event(payload).is_some();
    (!permanent && !quota, permanent)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;
    use tokio::sync::Semaphore;

    pub(crate) struct RaceHook {
        pub inserted: Semaphore,
        pub worker_release: Semaphore,
        pub worker_done: Semaphore,
        pub coordinator_reached: Semaphore,
        pub coordinator_release: Semaphore,
    }

    static RACE_HOOK: Mutex<Option<Arc<RaceHook>>> = Mutex::new(None);
    pub(super) static AUTO_TERMINAL_QUEUED: Semaphore = Semaphore::const_new(0);
    pub(crate) static AUTO_FORWARD_EOF: Semaphore = Semaphore::const_new(0);
    pub(crate) static AUTO_FORWARD_TERMINAL: Semaphore = Semaphore::const_new(0);
    pub(crate) static AUTO_FORWARD_RELEASE: Semaphore = Semaphore::const_new(0);
    pub(crate) static AUTO_FORWARD_DONE: Semaphore = Semaphore::const_new(0);
    pub(crate) static AUTO_RACE_FORWARD_DONE: Semaphore = Semaphore::const_new(0);
    static AUTO_RACE_DROP_REACHED: Semaphore = Semaphore::const_new(0);
    static AUTO_RACE_DROP_RELEASE: std::sync::Barrier = std::sync::Barrier::new(2);

    pub(crate) fn is_auto_drop_race(
        reservation: Option<&super::super::continuation::ContinuationReservation>,
    ) -> bool {
        reservation.and_then(|r| r.owner())
            == Some(&super::ConversationIdentity::Main(
                "auto-drop-publish-race".into(),
            ))
    }

    pub(crate) fn pause_auto_drop(
        reservation: &super::super::continuation::ContinuationReservation,
    ) {
        if is_auto_drop_race(Some(reservation)) {
            AUTO_RACE_DROP_REACHED.add_permits(1);
            AUTO_RACE_DROP_RELEASE.wait();
        }
    }

    pub(crate) fn race_hook(
        reservation: Option<&super::super::continuation::ContinuationReservation>,
    ) -> Option<Arc<RaceHook>> {
        if reservation.and_then(|r| r.owner())
            != Some(&super::ConversationIdentity::Main("publish-race".into()))
        {
            return None;
        }
        RACE_HOOK.lock().unwrap().clone()
    }

    #[test]
    fn plain_unauthorized_event_remains_refreshable() {
        let payload =
            serde_json::json!({"type":"error", "status":401, "error":{"message":"unauthorized"}});
        assert_eq!(super::unauthorized_event_policy(&payload), (true, false));
    }

    use futures_util::StreamExt;
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
    async fn downstream_disconnect_before_return_token_publication_removes_socket() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let hook = Arc::new(RaceHook {
            inserted: Semaphore::new(0),
            worker_release: Semaphore::new(0),
            worker_done: Semaphore::new(0),
            coordinator_reached: Semaphore::new(0),
            coordinator_release: Semaphore::new(0),
        });
        *RACE_HOOK.lock().unwrap() = Some(hook.clone());
        let owner = ConversationIdentity::Main("publish-race".into());
        let request = buffered_test_request();
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut ws).await;
            send_completed_websocket_response(&mut ws, "response").await;
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let events = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&reservation),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            hook.inserted.acquire().await.unwrap().forget();
            assert!(super::super::websocket::pooled_socket_id_for_tests(&owner).is_some());
            drop(events);
            hook.coordinator_reached.acquire().await.unwrap().forget();
            hook.worker_release.add_permits(1);
            hook.worker_done.acquire().await.unwrap().forget();
            hook.coordinator_release.add_permits(1);
            server.await.unwrap();
            assert!(super::super::websocket::pooled_socket_id_for_tests(&owner).is_none());
        })
        .await
        .unwrap();
        *RACE_HOOK.lock().unwrap() = None;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn downstream_disconnect_during_buffered_terminal_forwarding_removes_socket() {
        use futures_util::SinkExt;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let owner = ConversationIdentity::Main("terminal-forward-drop".into());
        let request = buffered_test_request();
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut ws).await;
            ws.send(tokio_tungstenite::tungstenite::Message::text(
                serde_json::json!({"type":"response.created","response":{"id":"first"}})
                    .to_string(),
            ))
            .await
            .unwrap();
            for _ in 0..70 {
                ws.send(tokio_tungstenite::tungstenite::Message::text(
                    r#"{"type":"response.in_progress"}"#,
                ))
                .await
                .unwrap();
            }
            ws.send(tokio_tungstenite::tungstenite::Message::text(serde_json::json!({"type":"response.completed","response":{"id":"first","status":"completed"}}).to_string())).await.unwrap();
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let events = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&reservation),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while super::super::websocket::pooled_socket_id_for_tests(&owner).is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(events);
        tokio::time::timeout(Duration::from_secs(2), async {
            while super::super::websocket::pooled_socket_id_for_tests(&owner).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn auto_disconnect_after_terminal_queued_removes_socket() {
        use futures_util::SinkExt;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let owner = ConversationIdentity::Main("auto-terminal-drop".into());
        let request = buffered_test_request();
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server =
            tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = next_websocket_json(&mut ws).await;
                ws.send(tokio_tungstenite::tungstenite::Message::text(
                    r#"{"type":"response.created","response":{"id":"first"}}"#,
                ))
                .await
                .unwrap();
                ws.send(tokio_tungstenite::tungstenite::Message::text(
                r#"{"type":"response.completed","response":{"id":"first","status":"completed"}}"#,
            )).await.unwrap();
            });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let events = client
            .stream_codex_auto_events_for_owner(
                &request,
                &http_test_context(),
                &super::super::client_body::CodexHttpBodyCache::new(),
                Some(&reservation),
                false,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            AUTO_TERMINAL_QUEUED.acquire().await.unwrap().forget();
            AUTO_FORWARD_EOF.acquire().await.unwrap().forget();
        })
        .await
        .unwrap();
        assert!(events.return_id().is_some());
        assert!(super::super::websocket::pooled_socket_id_for_tests(&owner).is_some());
        drop(events);
        tokio::time::timeout(Duration::from_secs(2), async {
            while super::super::websocket::pooled_socket_id_for_tests(&owner).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn auto_drop_before_return_publication_removes_socket() {
        use futures_util::SinkExt;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let owner = ConversationIdentity::Main("auto-drop-publish-race".into());
        let request = buffered_test_request();
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut ws).await;
            ws.send(tokio_tungstenite::tungstenite::Message::text(
                r#"{"type":"response.created","response":{"id":"first"}}"#,
            ))
            .await
            .unwrap();
            release_rx.await.unwrap();
            super::super::client_test_support::send_completed_websocket_response(&mut ws, "first")
                .await;
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let events = client
            .stream_codex_auto_events_for_owner(
                &request,
                &http_test_context(),
                &super::super::client_body::CodexHttpBodyCache::new(),
                Some(&reservation),
                false,
            )
            .await
            .unwrap();
        assert!(events.return_id().is_none());
        let dropping = tokio::task::spawn_blocking(move || drop(events));
        tokio::time::timeout(Duration::from_secs(5), async {
            AUTO_RACE_DROP_REACHED.acquire().await.unwrap().forget();
            release_tx.send(()).unwrap();
            AUTO_RACE_FORWARD_DONE.acquire().await.unwrap().forget();
        })
        .await
        .unwrap();
        let pooled = super::super::websocket::pooled_socket_id_for_tests(&owner).is_some();
        AUTO_RACE_DROP_RELEASE.wait();
        dropping.await.unwrap();
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        assert!(!pooled);
    }

    #[tokio::test]
    async fn auto_delivered_terminal_keeps_socket_pooled() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let owner = ConversationIdentity::Main("auto-terminal-delivered".into());
        let request = buffered_test_request();
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut ws).await;
            send_completed_websocket_response(&mut ws, "first").await;
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_auto_events_for_owner(
                &request,
                &http_test_context(),
                &super::super::client_body::CodexHttpBodyCache::new(),
                Some(&reservation),
                false,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap().unwrap();
                if super::super::websocket::is_terminal_event(&event) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        AUTO_FORWARD_TERMINAL.acquire().await.unwrap().forget();
        drop(events);
        AUTO_FORWARD_RELEASE.add_permits(1);
        AUTO_FORWARD_DONE.acquire().await.unwrap().forget();
        server.await.unwrap();
        assert!(super::super::websocket::pooled_socket_id_for_tests(&owner).is_some());
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn terminal_forward_failure_preserves_later_return_of_same_socket() {
        use futures_util::SinkExt;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let owner = ConversationIdentity::Main("terminal-return-race".into());
        let request = buffered_test_request();
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            false,
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut ws).await;
            ws.send(tokio_tungstenite::tungstenite::Message::text(
                r#"{"type":"response.created","response":{"id":"first"}}"#,
            ))
            .await
            .unwrap();
            for _ in 0..63 {
                ws.send(tokio_tungstenite::tungstenite::Message::text(
                    r#"{"type":"response.in_progress"}"#,
                ))
                .await
                .unwrap();
            }
            ws.send(tokio_tungstenite::tungstenite::Message::text(
                r#"{"type":"response.completed","response":{"id":"first","status":"completed"}}"#,
            ))
            .await
            .unwrap();
            let _ = next_websocket_json(&mut ws).await;
            send_completed_websocket_response(&mut ws, "second").await;
            let _ = done_rx.await;
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let first = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&reservation),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while first.socket_id().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let socket_id = first.socket_id().unwrap();
        let mut second = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&reservation),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(item) = second.recv().await {
                if super::super::websocket::is_terminal_event(&item.unwrap()) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(second.socket_id(), Some(socket_id));
        drop(first);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            super::super::websocket::pooled_socket_id_for_tests(&owner),
            Some(socket_id)
        );
        let _ = done_tx.send(());
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn websocket_binds_quota_before_event_task_runs() {
        use super::super::translate::reasoning_signature::ReasoningAccount;
        use crate::monitor::{EndpointKind, MonitorHandle};

        let monitor = MonitorHandle::default();
        monitor.request_started("old", None, None, EndpointKind::Messages);
        let old = monitor
            .bind_codex_quota("old", &ReasoningAccount::new(Some("old")))
            .unwrap();
        let event = serde_json::json!({"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":25}}});
        old.observe(&event);
        let mut ctx = http_test_context();
        monitor.request_started(&ctx.req_id, None, None, EndpointKind::Messages);
        ctx.monitor = Some(monitor.clone());
        let client = Arc::new(authenticated_http_test_client(
            "http://127.0.0.1:1/responses".into(),
        ));
        let stream = client
            .stream_codex_websocket_events_for_owner(&buffered_test_request(), &ctx, None)
            .await
            .unwrap();
        old.observe(&event);
        assert!(monitor.snapshot().codex_quota.is_none());
        drop(stream);
    }

    #[tokio::test]
    async fn live_nested_missing_response_retries_once_without_stale_previous_id() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Agent(
            "live-nested-recovery-session".to_string(),
            "live-nested-recovery-agent".to_string(),
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
            send_completed_websocket_response(&mut first_websocket, "resp_live_nested_origin")
                .await;
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
                "live nested missing-response recovery must not open a third socket"
            );
        });

        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
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
            Some("resp_live_nested_origin")
        );
        let mut events = client
            .stream_codex_websocket_events_for_owner(
                &second_request,
                &context,
                Some(&second_candidate),
            )
            .await
            .unwrap();
        let error = events.recv().await.unwrap().unwrap_err();
        assert_eq!(error.detail.as_deref(), Some("previous_response_not_found"));
        assert!(events.used_full_context_retry());
        assert_eq!(events.socket_id(), None);

        let first_payload = request_rx.recv().await.unwrap();
        let continued_payload = request_rx.recv().await.unwrap();
        let retry_payload = request_rx.recv().await.unwrap();
        assert!(first_payload.get("previous_response_id").is_none());
        assert_eq!(
            continued_payload["previous_response_id"],
            "resp_live_nested_origin"
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
    async fn live_missing_origin_retries_once_with_full_context_and_actual_socket() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-recovery-session".to_string());
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
        let (payload_tx, payload_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            payload_tx
                .send(next_websocket_json(&mut websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut websocket, "resp_live_retry").await;
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });

        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&continuation),
            )
            .await
            .unwrap();
        let created = events.recv().await.unwrap().unwrap();
        assert_eq!(created["type"], "response.created");
        let terminal = events.recv().await.unwrap().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert!(events.used_full_context_retry());
        let socket_id = events
            .socket_id()
            .expect("successful internal retry must publish its socket");
        let payload = payload_rx.await.unwrap();
        assert!(payload.get("previous_response_id").is_none());
        assert_eq!(payload["input"].as_array().unwrap().len(), 2);
        server.await.unwrap();
        assert_eq!(
            super::super::websocket::pooled_socket_id_for_tests(&owner),
            Some(socket_id)
        );

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    #[tokio::test]
    async fn public_ownerless_candidate_never_writes_previous_id_to_websocket() {
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        super::super::websocket::clear_codex_websocket_pool_for_tests();
        let request = buffered_request_with_texts(&["one", "two"]);
        let continuation = super::super::continuation::ContinuationCandidate {
            turn_id: Some(17),
            previous_response_id: Some("resp_unproven".to_string()),
            input_delta: Some(vec![request.input.last().unwrap().clone()]),
            input_delta_count: 1,
            disabled_reason: None,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (payload_tx, payload_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            payload_tx
                .send(next_websocket_json(&mut websocket).await)
                .unwrap();
            send_completed_websocket_response(&mut websocket, "resp_public_full_context").await;
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "an ownerless stale candidate must use one full-context socket"
            );
        });

        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut context = http_test_context();
        context.session_id = Some("must-not-be-derived-as-owner".to_string());
        let mut events = client
            .stream_codex_websocket_events(&request, &context, Some(&continuation))
            .await
            .unwrap();
        let created = events.recv().await.unwrap().unwrap();
        assert_eq!(created["type"], "response.created");
        let terminal = events.recv().await.unwrap().unwrap();
        assert_eq!(terminal["type"], "response.completed");

        let payload = payload_rx.await.unwrap();
        assert!(payload.get("previous_response_id").is_none());
        assert_eq!(payload["input"].as_array().unwrap().len(), 2);
        server.await.unwrap();
        super::super::websocket::clear_codex_websocket_pool_for_tests();
    }

    #[tokio::test]
    async fn live_missing_origin_does_not_enter_second_full_context_loop() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-bounded-recovery-session".to_string());
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
        let (payload_tx, payload_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            payload_tx
                .send(next_websocket_json(&mut websocket).await)
                .unwrap();
            websocket.close(None).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });

        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));
        let mut events = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&continuation),
            )
            .await
            .unwrap();
        let error = events.recv().await.unwrap().unwrap_err();
        assert_eq!(
            error.detail.as_deref(),
            Some(super::super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL)
        );
        assert!(events.used_full_context_retry());
        assert_eq!(events.socket_id(), None);
        let payload = payload_rx.await.unwrap();
        assert!(payload.get("previous_response_id").is_none());
        assert_eq!(payload["input"].as_array().unwrap().len(), 2);
        server.await.unwrap();

        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        super::super::continuation::abort_continuation_for_owner(&reserved);
    }

    #[tokio::test]
    async fn dropping_live_receiver_clears_reserved_turn() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-drop-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = buffered_request_with_texts(&["one"]);
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut websocket).await;
            request_seen_tx.send(()).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = websocket.close(None).await;
        });

        let events = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )))
        .stream_codex_websocket_events_for_owner(
            &request,
            &http_test_context(),
            Some(&continuation),
        )
        .await
        .unwrap();
        request_seen_rx.await.unwrap();
        drop(events);

        tokio::time::timeout(Duration::from_secs(1), async {
            while super::super::continuation::is_current_turn_for_owner(&continuation) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the live receiver must clear its reserved turn");
        server.await.unwrap();
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    #[tokio::test]
    async fn dropping_retry_handoff_receiver_preserves_reserved_turn() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-retry-handoff-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = buffered_request_with_texts(&["one"]);
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let (socket_closed_tx, socket_closed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let _ = next_websocket_json(&mut websocket).await;
            request_seen_tx.send(()).unwrap();
            while websocket.next().await.is_some() {}
            socket_closed_tx.send(()).unwrap();
        });

        let events = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )))
        .stream_codex_websocket_events_for_owner(
            &request,
            &http_test_context(),
            Some(&continuation),
        )
        .await
        .unwrap();
        request_seen_rx.await.unwrap();
        events.mark_provider_retry_handoff();
        drop(events);

        tokio::time::timeout(Duration::from_secs(1), socket_closed_rx)
            .await
            .expect("marked receiver drop must close only the abandoned attempt socket")
            .expect("socket-close acknowledgement sender dropped");
        assert!(super::super::continuation::is_current_turn_for_owner(
            &continuation
        ));
        server.await.unwrap();
        super::super::continuation::abort_continuation_for_owner(&continuation);
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    #[tokio::test]
    async fn delayed_retry_handoff_cleanup_preserves_replacement_state_and_socket() {
        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = super::super::websocket::lock_codex_websocket_pool_for_tests().await;
        let owner = ConversationIdentity::Main("live-retry-cleanup-race-session".to_string());
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = buffered_request_with_texts(&["one"]);
        let continuation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_replacement_tx, release_replacement_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (first_socket, _) = listener.accept().await.unwrap();
            let mut first_websocket = tokio_tungstenite::accept_async(first_socket).await.unwrap();
            let _ = next_websocket_json(&mut first_websocket).await;
            send_completed_websocket_response(&mut first_websocket, "resp_attempt_a").await;
            drop(first_websocket);

            let (replacement_socket, _) = listener.accept().await.unwrap();
            let mut replacement_websocket = tokio_tungstenite::accept_async(replacement_socket)
                .await
                .unwrap();
            let _ = next_websocket_json(&mut replacement_websocket).await;
            send_completed_websocket_response(&mut replacement_websocket, "resp_attempt_b").await;
            let _ = release_replacement_rx.await;
        });
        let client = Arc::new(authenticated_http_test_client(format!(
            "http://{addr}/responses"
        )));

        let mut attempt_a = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&continuation),
            )
            .await
            .unwrap();
        let created_a = attempt_a.recv().await.unwrap().unwrap();
        assert_eq!(created_a["type"], "response.created");
        let terminal_a = attempt_a.recv().await.unwrap().unwrap();
        assert_eq!(terminal_a["response"]["id"], "resp_attempt_a");
        let socket_a = attempt_a.socket_id().expect("attempt A socket ID");
        let return_a = attempt_a.return_id().expect("attempt A return ID");
        super::super::websocket::invalidate_codex_websocket_pool_return(
            &continuation,
            Some(return_a),
        );
        assert!(super::super::websocket::pooled_socket_id_for_tests(&owner).is_none());

        let mut attempt_b = client
            .stream_codex_websocket_events_for_owner(
                &request,
                &http_test_context(),
                Some(&continuation),
            )
            .await
            .unwrap();
        let created_b = attempt_b.recv().await.unwrap().unwrap();
        assert_eq!(created_b["type"], "response.created");
        let terminal_b = attempt_b.recv().await.unwrap().unwrap();
        assert_eq!(terminal_b["response"]["id"], "resp_attempt_b");
        let socket_b = attempt_b.socket_id().expect("attempt B socket ID");
        assert_ne!(socket_a, socket_b);
        super::super::continuation::record_continuation_for_owner(
            &continuation,
            &request,
            Some("resp_attempt_b"),
            Some(socket_b),
            &[],
        );

        let (_handoff_tx, handoff_rx) =
            tokio::sync::mpsc::channel::<Result<serde_json::Value, CodexError>>(1);
        let (handoff_stream, handoff_publisher) =
            super::super::websocket::CodexWebSocketEventStream::pending(handoff_rx);
        handoff_stream.mark_provider_retry_handoff();
        let cleanup_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let cleanup_task_barrier = cleanup_barrier.clone();
        let cleanup_continuation = continuation.clone();
        let cleanup = tokio::spawn(async move {
            cleanup_task_barrier.wait().await;
            super::super::websocket::invalidate_codex_websocket_pool_return(
                &cleanup_continuation,
                Some(return_a),
            );
            abort_abandoned_live_continuation(Some(&cleanup_continuation), &handoff_publisher);
        });

        cleanup_barrier.wait().await;
        cleanup.await.unwrap();
        assert!(super::super::continuation::is_current_turn_for_owner(
            &continuation
        ));
        assert!(super::super::continuation::has_continuation_for_owner_for_tests(&owner));
        assert_eq!(
            super::super::websocket::pooled_socket_id_for_tests(&owner),
            Some(socket_b)
        );

        let _ = release_replacement_tx.send(());
        server.await.unwrap();
        super::super::continuation::abort_continuation_for_owner(&continuation);
        super::super::websocket::invalidate_codex_websocket_pool_owner(&owner);
    }

    #[test]
    fn websocket_pool_owner_tracks_typed_continuation_opt_in() {
        let owner = ConversationIdentity::Agent("session".into(), "agent".into());
        let disabled = test_continuation(Some(owner.clone()), None, None, None, Some("disabled"));
        let first_enabled = test_continuation(
            Some(owner.clone()),
            Some(1),
            None,
            None,
            Some("missing_state"),
        );
        let append = test_continuation(Some(owner.clone()), Some(2), Some("resp_1"), Some(1), None);
        let missing_identity = test_continuation(None, None, None, None, Some("missing_identity"));

        assert_eq!(websocket_pool_owner(Some(&disabled)), Some(&owner));
        assert_eq!(websocket_pool_owner(Some(&first_enabled)), Some(&owner));
        assert_eq!(websocket_pool_owner(Some(&append)), Some(&owner));
        assert_eq!(websocket_pool_owner(Some(&missing_identity)), None);
    }

    #[test]
    fn websocket_pool_reset_only_for_full_context_retry() {
        let owner = Some(ConversationIdentity::Main("session".into()));
        let missing_state =
            test_continuation(owner.clone(), None, None, None, Some("missing_state"));
        let disabled = test_continuation(owner.clone(), None, None, None, Some("disabled"));
        let prompt_changed =
            test_continuation(owner.clone(), None, None, None, Some("prompt_changed"));
        let retry = test_continuation(owner, None, None, None, Some("full_context_retry"));

        assert!(!should_reset_websocket_pool(Some(&missing_state)));
        assert!(!should_reset_websocket_pool(Some(&disabled)));
        assert!(!should_reset_websocket_pool(Some(&prompt_changed)));
        assert!(should_reset_websocket_pool(Some(&retry)));
    }

    #[test]
    fn informational_events_keep_live_continuation_retry_available() {
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "codex.rate_limits",
            "rate_limits": {"limit_reached": false}
        })));
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "keepalive"
        })));
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "response.created"
        })));
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "message", "id": "msg_1"}
        })));
        assert!(!event_closes_live_retry_window(&serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_1", "name": "Read"}
        })));
        assert!(event_closes_live_retry_window(&serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "hello"
        })));
        assert!(event_closes_live_retry_window(&serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{}"
        })));
    }
}
