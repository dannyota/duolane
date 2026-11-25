use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OwnedMutexGuard, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::provider::RequestContext;
use crate::traffic::TrafficCapture;

use super::client::{
    ActualTransport, CodexError, CodexErrorOrigin, CodexResponse, MAX_UPSTREAM_FRAME_BYTES,
    OwnerAwareCodexResponse, TRANSPORT_UNEXPECTED_EOF_DETAIL, has_unexpected_eof,
};
use super::continuation::ContinuationReservation;
use super::websocket::*;
use super::websocket_stream::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn codex_websocket_request(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    credential: Option<super::sources::CredentialId>,
    body_value: &serde_json::Value,
    ctx: &RequestContext,
    traffic: Option<&TrafficCapture>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    reservation: Option<&ContinuationReservation>,
    websocket_sent: &std::sync::atomic::AtomicBool,
) -> Result<OwnerAwareCodexResponse, CodexError> {
    let continuation = reservation.map(ContinuationReservation::candidate);
    let pool_owner = reservation_pool_owner(reservation);
    let ws_url = to_websocket_url(url).map_err(|e| {
        ctx.recovery_budget.mark_final_only();
        CodexError {
            status: 0,
            message: e.message,
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        }
    })?;
    let body_json = serde_json::to_string(body_value).unwrap_or_default();
    if let Some(tc) = traffic {
        tc.write_json("020-upstream-request", body_value);
        tc.write_json(
            "021-upstream-request-metadata",
            &serde_json::json!({
                "provider": "codex",
                "transport": "websocket",
                "url": ws_url,
                "method": "GET",
                "headers": headers_to_json(headers),
                "size": summarize_json_request_size(body_value, &body_json),
                "continuation": {
                    "previousResponseId": continuation
                        .and_then(|c| c.previous_response_id.as_deref()),
                    "inputDeltaCount": continuation
                        .and_then(|c| c.input_delta.as_ref())
                        .map(|items| items.len()),
                    "disabledReason": continuation
                        .and_then(|c| c.disabled_reason.as_deref()),
                },
            }),
        );
    }
    let started_at = Instant::now();

    let requires_origin = continuation
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_some();
    let identity = credential.map(|credential| handshake_identity(credential, headers));
    let pooled = take_pool_entry_for_request(reservation, identity.as_ref())?;
    let mut used_pooled = pooled.is_some();
    let mut entry = if let Some(entry) = pooled {
        entry
    } else {
        Arc::new(new_pool_entry(
            connect_with_timeout(
                websocket_client,
                proxy_config,
                &ws_url,
                headers,
                connect_timeout_ms,
                &ctx.recovery_budget,
            )
            .await?,
            identity.clone(),
        ))
    };
    let mut guard = entry.ws.clone().lock_owned().await;

    if used_pooled
        && validate_pooled_websocket(&mut guard, connect_timeout_ms)
            .await
            .is_err()
    {
        drop(guard);
        if let Some(owner) = &pool_owner {
            pool_remove_entry(owner, &entry);
        }
        if requires_origin {
            return Err(continuation_socket_missing_error());
        }
        entry = Arc::new(new_pool_entry(
            connect_with_timeout(
                websocket_client,
                proxy_config,
                &ws_url,
                headers,
                connect_timeout_ms,
                &ctx.recovery_budget,
            )
            .await?,
            identity.clone(),
        ));
        guard = entry.ws.clone().lock_owned().await;
        used_pooled = false;
    }

    log_socket_reuse(ctx, used_pooled);
    websocket_sent.store(true, std::sync::atomic::Ordering::Release);
    ctx.recovery_budget
        .set_send(super::api::SendEvidence::OutcomeUnknown);
    guard
        .send(Message::text(body_json))
        .await
        .map_err(|error| {
            if let Some(owner) = &pool_owner {
                pool_remove_entry(owner, &entry);
            }
            ctx.recovery_budget.mark_ambiguous_hosted();
            websocket_transport_error("send", &error)
        })?;

    let collected = collect_ws_events_tracked(
        &mut guard,
        idle_timeout_ms,
        pool_owner.as_ref(),
        Some(&entry),
        traffic,
        &ctx.recovery_budget,
        used_pooled,
    )
    .await;
    drop(guard);
    let (sse_body, terminal_event) = match collected {
        Ok(result) => result,
        Err(error) => {
            if let Some(owner) = &pool_owner {
                pool_remove_entry(owner, &entry);
            }
            return Err(error);
        }
    };
    let Some(terminal_event) = terminal_event else {
        if let Some(owner) = &pool_owner {
            pool_remove_entry(owner, &entry);
        }
        return Err(missing_terminal_error());
    };

    if is_previous_response_missing(&terminal_event.payload) {
        if let Some(owner) = &pool_owner {
            pool_remove_entry(owner, &entry);
        }
        let error = CodexError {
            status: 0,
            message: "Previous response not found".to_string(),
            detail: Some("previous_response_not_found".to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        return Err(if terminal_event.hosted_tool_executed {
            error.after_hosted_tool_execution()
        } else {
            error
        });
    }

    let completed = terminal_event.reusable;
    let origin_reinserted = if completed {
        reservation.is_some_and(|reservation| pool_insert_for_turn(reservation, entry.clone()))
    } else {
        if let Some(owner) = &pool_owner {
            pool_remove_entry(owner, &entry);
        }
        false
    };
    let status = if terminal_event.event_type == "error" {
        event_error_status(&terminal_event.payload).unwrap_or(500)
    } else {
        200
    };

    if let Some(tc) = traffic {
        write_websocket_metadata_capture(tc, &ws_url, reservation, used_pooled);
        write_websocket_response_capture(tc, status, started_at.elapsed());
    }

    let mut response = OwnerAwareCodexResponse::new(
        CodexResponse {
            body: sse_body,
            status,
            headers: vec![],
            transport: ActualTransport::WebSocket,
        },
        origin_reinserted.then_some(entry.socket_id),
    );
    response.hosted_tool_executed = terminal_event.hosted_tool_executed;
    Ok(response)
}

fn log_socket_reuse(ctx: &RequestContext, reused: bool) {
    crate::logging::create_logger("codex").info(
        "codex_websocket_request",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), serde_json::json!(&ctx.req_id)),
            ("socketReused".to_string(), serde_json::json!(reused)),
        ])),
    );
}

fn new_pool_entry(stream: CodexWebSocketStream, identity: Option<HandshakeIdentity>) -> PoolEntry {
    match identity {
        Some(identity) => PoolEntry::with_identity(stream, identity),
        None => PoolEntry::new(stream),
    }
}

async fn validate_pooled_websocket<S>(
    websocket: &mut WebSocketStream<S>,
    timeout_ms: u64,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let nonce = next_monotonic_nonzero(&POOLED_VALIDATION_SEQUENCE, "pooled validation")
        .to_be_bytes()
        .to_vec();
    websocket
        .send(Message::Ping(nonce.clone().into()))
        .await
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        loop {
            match websocket.next().await {
                Some(Ok(Message::Pong(payload))) if payload == nonce => return Ok(()),
                Some(Ok(Message::Ping(payload))) => websocket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|error| error.to_string())?,
                Some(Ok(Message::Pong(_))) => {
                    return Err("unexpected Pong during pooled validation".to_string());
                }
                Some(Ok(_)) => {
                    return Err("unexpected frame during pooled validation".to_string());
                }
                Some(Err(error)) => return Err(error.to_string()),
                None => return Err("connection closed during pooled validation".to_string()),
            }
        }
    })
    .await
    .map_err(|_| "validation timeout".to_string())?
}

pub(super) struct ReadyWebSocket {
    ws_url: String,
    guard: OwnedMutexGuard<CodexWebSocketStream>,
    pub(super) entry: Arc<PoolEntry>,
    used_pooled: bool,
    reservation: Option<ContinuationReservation>,
    traffic: Option<Arc<TrafficCapture>>,
    idle_timeout_ms: u64,
    send_timeout_ms: u64,
    budget: super::recovery::RecoveryBudget,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_codex_websocket(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    credential: Option<super::sources::CredentialId>,
    traffic: Option<Arc<TrafficCapture>>,
    reservation: Option<&ContinuationReservation>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    budget: &super::recovery::RecoveryBudget,
) -> Result<ReadyWebSocket, CodexError> {
    let pool_owner = reservation_pool_owner(reservation);
    let continuation = reservation.map(ContinuationReservation::candidate);
    let ws_url = to_websocket_url(url).map_err(|error| CodexError {
        status: 0,
        message: error.message,
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let requires_origin = continuation
        .and_then(|candidate| candidate.previous_response_id.as_deref())
        .is_some();
    let identity = credential.map(|credential| handshake_identity(credential, headers));
    let pooled = take_pool_entry_for_request(reservation, identity.as_ref())?;
    let used_pooled = pooled.is_some();
    let entry = if let Some(entry) = pooled {
        entry
    } else {
        let stream = connect_with_timeout(
            websocket_client,
            proxy_config,
            &ws_url,
            headers,
            connect_timeout_ms,
            budget,
        )
        .await?;
        Arc::new(new_pool_entry(stream, identity))
    };
    let mut guard = entry.ws.clone().lock_owned().await;
    if used_pooled
        && let Err(detail) = validate_pooled_websocket(&mut guard, connect_timeout_ms).await
    {
        drop(guard);
        if let Some(owner) = &pool_owner {
            pool_remove_entry(owner, &entry);
        }
        return Err(if requires_origin {
            continuation_socket_missing_error()
        } else {
            pooled_validation_error(detail)
        });
    }
    Ok(ReadyWebSocket {
        ws_url,
        guard,
        entry,
        used_pooled,
        reservation: reservation.cloned(),
        traffic,
        idle_timeout_ms,
        send_timeout_ms: connect_timeout_ms,
        budget: budget.clone(),
    })
}

fn pooled_validation_error(detail: String) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket pooled connection validation failed: {detail}"),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

pub(super) fn start_codex_websocket_events(
    ready: ReadyWebSocket,
    body_value: &serde_json::Value,
    body_json: String,
    headers: &HeaderMap,
    reservation: Option<&ContinuationReservation>,
    websocket_sent: Arc<std::sync::atomic::AtomicBool>,
) -> CodexWebSocketEventStream {
    if let Some(tc) = ready.traffic.as_deref() {
        write_websocket_metadata_capture(tc, &ready.ws_url, reservation, ready.used_pooled);
        tc.write_json("020-upstream-request", body_value);
        tc.write_json(
            "021-upstream-request-metadata",
            &serde_json::json!({
                "provider": "codex",
                "transport": "websocket",
                "headers": headers_to_json(headers),
                "size": summarize_json_request_size(body_value, &body_json),
            }),
        );
    }
    let (tx, rx) = mpsc::channel(64);
    let (receiver, socket_id_publisher) = CodexWebSocketEventStream::pending(rx);
    tokio::spawn(async move {
        let ReadyWebSocket {
            ws_url: _,
            mut guard,
            entry,
            used_pooled,
            reservation,
            traffic,
            idle_timeout_ms,
            send_timeout_ms,
            budget,
        } = ready;
        let pool_owner = reservation_pool_owner(reservation.as_ref());
        websocket_sent.store(true, std::sync::atomic::Ordering::Release);
        let send_result = tokio::select! {
            biased;
            _ = tx.closed() => None,
            result = async {
                budget.set_send(super::api::SendEvidence::OutcomeUnknown);
                tokio::time::timeout(
                    Duration::from_millis(send_timeout_ms),
                    guard.send(Message::text(body_json)),
                ).await
            } => Some(result),
        };
        if !matches!(send_result, Some(Ok(Ok(())))) {
            if budget.send() == super::api::SendEvidence::OutcomeUnknown {
                budget.mark_ambiguous_hosted();
            }
            drop(guard);
            if let Some(owner) = &pool_owner {
                pool_remove_entry(owner, &entry);
            }
            socket_id_publisher.publish(None);
            let error = match send_result {
                Some(Ok(Err(error))) => {
                    let mut failure = websocket_transport_error("send", &error);
                    if budget.typed_websocket()
                        && matches!(error, tokio_tungstenite::tungstenite::Error::Io(_))
                    {
                        failure.origin = CodexErrorOrigin::WebSocketTransient;
                    }
                    Some(failure)
                }
                Some(Err(_)) => Some(websocket_stream_error("WebSocket send timed out")),
                _ => None,
            };
            if let Some(error) = error {
                let _ = tx.send(Err(error)).await;
            }
            return;
        }
        let (reusable, terminal_item) = stream_ws_events(
            &mut guard,
            idle_timeout_ms,
            traffic,
            &tx,
            &budget,
            used_pooled,
            Some(&entry),
        )
        .await;
        drop(guard);

        let origin_reinserted = if reusable {
            reservation
                .as_ref()
                .is_some_and(|reservation| pool_insert_for_turn(reservation, entry.clone()))
        } else {
            if let Some(owner) = &pool_owner {
                pool_remove_entry(owner, &entry);
            }
            false
        };
        socket_id_publisher.publish(origin_reinserted.then_some(entry.socket_id));
        if let Some(item) = terminal_item
            && tx.send(item).await.is_err()
            && origin_reinserted
            && let Some(owner) = &pool_owner
        {
            pool_remove_entry(owner, &entry);
        }
    });
    receiver
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn codex_websocket_event_stream(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    credential: Option<super::sources::CredentialId>,
    body_value: &serde_json::Value,
    ctx: &RequestContext,
    traffic: Option<Arc<TrafficCapture>>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    reservation: Option<&ContinuationReservation>,
    websocket_sent: Arc<std::sync::atomic::AtomicBool>,
) -> Result<CodexWebSocketEventStream, CodexError> {
    codex_websocket_event_stream_prepared(
        None,
        websocket_client,
        proxy_config,
        url,
        headers,
        credential,
        body_value,
        ctx,
        traffic,
        connect_timeout_ms,
        idle_timeout_ms,
        reservation,
        websocket_sent,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn codex_websocket_event_stream_prepared(
    ready: Option<ReadyWebSocket>,
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    credential: Option<super::sources::CredentialId>,
    body_value: &serde_json::Value,
    ctx: &RequestContext,
    traffic: Option<Arc<TrafficCapture>>,
    connect_timeout_ms: u64,
    idle_timeout_ms: u64,
    reservation: Option<&ContinuationReservation>,
    websocket_sent: Arc<std::sync::atomic::AtomicBool>,
) -> Result<CodexWebSocketEventStream, CodexError> {
    let body_json = serde_json::to_string(body_value).map_err(|error| CodexError {
        status: 500,
        message: "Failed to serialize WebSocket request".to_string(),
        detail: Some(error.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let ready = match ready {
        Some(ready) => ready,
        None => {
            prepare_codex_websocket(
                websocket_client,
                proxy_config,
                url,
                headers,
                credential,
                traffic,
                reservation,
                connect_timeout_ms,
                idle_timeout_ms,
                &ctx.recovery_budget,
            )
            .await?
        }
    };
    log_socket_reuse(ctx, ready.used_pooled);
    Ok(start_codex_websocket_events(
        ready,
        body_value,
        body_json,
        headers,
        reservation,
        websocket_sent,
    ))
}

pub(super) fn continuation_socket_missing_error() -> CodexError {
    CodexError {
        status: 0,
        message: "Previous response socket is no longer available".to_string(),
        detail: Some(WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

pub(super) fn missing_terminal_error() -> CodexError {
    CodexError {
        status: 0,
        message: "WebSocket connection closed before terminal Codex response event".to_string(),
        detail: Some(WEBSOCKET_MISSING_TERMINAL_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

pub(super) fn websocket_stream_error(message: impl Into<String>) -> CodexError {
    CodexError {
        status: 0,
        message: message.into(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

pub(super) fn websocket_transport_error(
    operation: &str,
    error: &tokio_tungstenite::tungstenite::Error,
) -> CodexError {
    let mut failure = websocket_stream_error(format!("WebSocket {operation} error: {error}"));
    if has_unexpected_eof(error) {
        failure.detail = Some(TRANSPORT_UNEXPECTED_EOF_DETAIL.to_string());
    }
    failure
}

pub(super) fn websocket_idle_timeout_error(timeout_ms: u64) -> CodexError {
    websocket_stream_error(format!("WebSocket idle timeout after {timeout_ms}ms"))
}

pub(super) fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_UPSTREAM_FRAME_BYTES))
        .max_frame_size(Some(MAX_UPSTREAM_FRAME_BYTES))
}

pub(super) fn response_start_timeout_error(timeout_ms: u64) -> CodexError {
    CodexError {
        status: 0,
        message: format!("WebSocket response start timeout after {timeout_ms}ms"),
        detail: Some(WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocket,
    }
}

fn write_websocket_metadata_capture(
    traffic: &TrafficCapture,
    ws_url: &str,
    reservation: Option<&ContinuationReservation>,
    pooled: bool,
) {
    let pool_owner = reservation_pool_owner(reservation);
    let continuation = reservation.map(ContinuationReservation::candidate);
    traffic.write_json(
        "022-upstream-websocket-metadata",
        &serde_json::json!({
            "provider": "codex",
            "transport": "websocket",
            "url": ws_url,
            "poolingEnabled": pool_owner.is_some(),
            "pooled": pooled,
            "continuation": {
                "previousResponseId": continuation
                    .and_then(|c| c.previous_response_id.as_deref()),
                "inputDeltaCount": continuation
                    .and_then(|c| c.input_delta.as_ref())
                    .map(|items| items.len()),
                "disabledReason": continuation
                    .and_then(|c| c.disabled_reason.as_deref()),
            },
        }),
    );
}

pub(super) fn write_websocket_response_capture(
    traffic: &TrafficCapture,
    status: u16,
    elapsed: Duration,
) {
    traffic.write_json(
        "030-upstream-response-headers",
        &serde_json::json!({
            "status": status,
            "elapsedMs": elapsed.as_millis(),
            "headers": {
                "content-type": "text/event-stream",
            },
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::super::websocket_test_support::*;
    use super::*;

    #[test]
    fn websocket_metadata_does_not_serialize_typed_owner() {
        let temp = tempfile::tempdir().unwrap();
        let traffic = crate::traffic::test_capture(temp.path().join("traffic"));
        let owner = agent_owner("session-secret", "agent-secret");
        let reservation = test_continuation(Some(owner), None, None, None);

        write_websocket_metadata_capture(
            &traffic,
            "wss://example.invalid/responses",
            Some(&reservation),
            false,
        );

        let artifact = std::fs::read_dir(traffic.root())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let captured = std::fs::read_to_string(artifact).unwrap();
        assert!(captured.contains("poolingEnabled"));
        assert!(!captured.contains("poolKey"));
        assert!(!captured.contains("session-secret"));
        assert!(!captured.contains("agent-secret"));
    }

    #[tokio::test]
    async fn disconnect_during_stalled_send_releases_and_closes_socket() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (drain_tx, drain_rx) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            drain_rx.await.unwrap();
            let mut buffer = [0_u8; 65536];
            loop {
                match websocket.get_mut().read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        let ready = prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("http://{addr}/responses"),
            &HeaderMap::new(),
            None,
            None,
            None,
            10_000,
            10_000,
            &super::super::recovery::RecoveryBudget::default(),
        )
        .await
        .unwrap();
        let weak_entry = Arc::downgrade(&ready.entry);
        let events = start_codex_websocket_events(
            ready,
            &serde_json::Value::Null,
            "x".repeat(32 * 1024 * 1024),
            &HeaderMap::new(),
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(weak_entry.upgrade().is_some());
        drop(events);
        tokio::time::timeout(Duration::from_secs(2), async {
            while weak_entry.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drain_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), peer)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn stalled_send_hits_deadline_and_closes_unpooled_socket() {
        use tokio::io::AsyncReadExt;

        let _registry_guard =
            super::super::continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = lock_codex_websocket_pool_for_tests().await;
        let owner = agent_owner("send-deadline-session", "send-deadline-agent");
        invalidate_codex_websocket_pool_owner(&owner);
        let request = super::super::client_test_support::buffered_test_request();
        let reservation = super::super::continuation::continuation_candidate_for_owner(
            Some(&owner),
            &request,
            true,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (drain_tx, drain_rx) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            drain_rx.await.unwrap();
            let mut buffer = [0_u8; 65536];
            loop {
                match websocket.get_mut().read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        let mut ready = prepare_codex_websocket(
            &test_websocket_client(),
            &WebSocketProxyConfig::direct(),
            &format!("http://{addr}/responses"),
            &HeaderMap::new(),
            None,
            None,
            Some(&reservation),
            1_000,
            10_000,
            &super::super::recovery::RecoveryBudget::default(),
        )
        .await
        .unwrap();
        ready.send_timeout_ms = 200;
        let weak_entry = Arc::downgrade(&ready.entry);
        let mut events = start_codex_websocket_events(
            ready,
            &serde_json::Value::Null,
            "x".repeat(32 * 1024 * 1024),
            &HeaderMap::new(),
            Some(&reservation),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let error = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.message, "WebSocket send timed out");
        assert_eq!(events.socket_id(), None);
        assert_eq!(pooled_socket_id_for_tests(&owner), None);
        tokio::time::timeout(Duration::from_secs(2), async {
            while weak_entry.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drain_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), peer)
            .await
            .unwrap()
            .unwrap();
        super::super::continuation::clear_continuation_for_owner(Some(&owner));
    }

    #[tokio::test]
    async fn pooled_validation_uses_unique_nonce_and_rejects_stale_pong() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (queue_stale_tx, queue_stale_rx) = tokio::sync::oneshot::channel();
        let (stale_queued_tx, stale_queued_rx) = tokio::sync::oneshot::channel();
        let (release_peer_tx, release_peer_rx) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let prior_nonce = match websocket.next().await {
                Some(Ok(Message::Ping(payload))) => payload,
                other => panic!("unexpected first validation frame: {other:?}"),
            };
            assert_eq!(prior_nonce.len(), std::mem::size_of::<u64>());
            assert_ne!(prior_nonce[..], 0_u64.to_be_bytes());
            websocket
                .send(Message::Pong(prior_nonce.clone()))
                .await
                .unwrap();

            queue_stale_rx.await.unwrap();
            websocket
                .send(Message::Pong(prior_nonce.clone()))
                .await
                .unwrap();
            stale_queued_tx.send(()).unwrap();

            let current_nonce = match websocket.next().await {
                Some(Ok(Message::Ping(payload))) => payload,
                other => panic!("unexpected second validation frame: {other:?}"),
            };
            assert_ne!(current_nonce, prior_nonce);
            assert_ne!(current_nonce[..], 0_u64.to_be_bytes());
            release_peer_rx.await.unwrap();
        });
        let (mut websocket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .unwrap();

        validate_pooled_websocket(&mut websocket, 1_000)
            .await
            .unwrap();
        queue_stale_tx.send(()).unwrap();
        stale_queued_rx.await.unwrap();
        let error = validate_pooled_websocket(&mut websocket, 100)
            .await
            .unwrap_err();
        assert_eq!(error, "unexpected Pong during pooled validation");

        release_peer_tx.send(()).unwrap();
        peer.await.unwrap();
    }
}
