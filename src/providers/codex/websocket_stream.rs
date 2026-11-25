use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
#[cfg(test)]
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
#[cfg(test)]
use tokio::io::ReadBuf;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
#[cfg(test)]
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::provider::MAX_BUFFERED_RESPONSE_BYTES;
use crate::traffic::TrafficCapture;

use super::client::{CodexError, CodexErrorOrigin};
use super::websocket::*;

enum WebSocketRead {
    Frame(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
    Timeout,
    KeepaliveError(String),
}

async fn read_ws_frame_with_keepalive<S>(
    ws: &mut WebSocketStream<S>,
    read_timeout: Duration,
    keepalive_interval: Duration,
) -> WebSocketRead
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let timeout = tokio::time::sleep(read_timeout);
    tokio::pin!(timeout);
    let mut keepalive = tokio::time::interval(keepalive_interval);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await;

    loop {
        tokio::select! {
            biased;
            frame = ws.next() => return WebSocketRead::Frame(frame),
            _ = &mut timeout => return WebSocketRead::Timeout,
            _ = keepalive.tick() => {
                let send = ws.send(Message::Ping(Default::default()));
                let send_timeout = tokio::time::sleep(WEBSOCKET_KEEPALIVE_SEND_TIMEOUT);
                tokio::pin!(send_timeout);
                tokio::select! {
                    biased;
                    result = send => {
                        if let Err(error) = result {
                            return WebSocketRead::KeepaliveError(error.to_string());
                        }
                    }
                    _ = &mut timeout => return WebSocketRead::Timeout,
                    _ = &mut send_timeout => {
                        return WebSocketRead::KeepaliveError(format!(
                            "send timed out after {}ms",
                            WEBSOCKET_KEEPALIVE_SEND_TIMEOUT.as_millis(),
                        ));
                    }
                }
            }
        }
    }
}

pub(super) struct WsEvent {
    pub(super) event_type: String,
    pub(super) payload: serde_json::Value,
    pub(super) hosted_tool_executed: bool,
    pub(super) reusable: bool,
}

#[cfg(test)]
pub(super) async fn collect_ws_events<S>(
    ws: &mut WebSocketStream<S>,
    idle_timeout_ms: u64,
    pool_owner: Option<&PoolKey>,
    pool_entry: Option<&Arc<PoolEntry>>,
    traffic: Option<&TrafficCapture>,
) -> Result<(Vec<u8>, Option<WsEvent>), CodexError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    collect_ws_events_tracked(
        ws,
        idle_timeout_ms,
        pool_owner,
        pool_entry,
        traffic,
        &Default::default(),
        false,
    )
    .await
}

pub(super) async fn collect_ws_events_tracked<S>(
    ws: &mut WebSocketStream<S>,
    idle_timeout_ms: u64,
    pool_owner: Option<&PoolKey>,
    pool_entry: Option<&Arc<PoolEntry>>,
    traffic: Option<&TrafficCapture>,
    budget: &super::recovery::RecoveryBudget,
    reused: bool,
) -> Result<(Vec<u8>, Option<WsEvent>), CodexError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    collect_ws_events_with_keepalive_interval(
        ws,
        idle_timeout_ms,
        pool_owner,
        pool_entry,
        traffic,
        WEBSOCKET_KEEPALIVE_INTERVAL,
        budget,
        reused,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn collect_ws_events_with_keepalive_interval<S>(
    ws: &mut WebSocketStream<S>,
    idle_timeout_ms: u64,
    pool_owner: Option<&PoolKey>,
    pool_entry: Option<&Arc<PoolEntry>>,
    traffic: Option<&TrafficCapture>,
    keepalive_interval: Duration,
    budget: &super::recovery::RecoveryBudget,
    reused: bool,
) -> Result<(Vec<u8>, Option<WsEvent>), CodexError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut sse_body: Vec<u8> = Vec::new();
    let mut hosted_tool_executed = false;
    let response_event_budget = Duration::from_millis(idle_timeout_ms);
    let response_wait_started = Instant::now();
    let mut last_response_event_at = response_wait_started;
    let mut response_started = false;

    let mut created_id: Option<String> = None;
    let mut seen_created = false;
    let terminal_event = loop {
        let response_deadline_started = if response_started {
            last_response_event_at
        } else {
            response_wait_started
        };
        let read_timeout = if response_started {
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    invalidate_pool_owner(pool_owner, pool_entry);
                    break Err(websocket_idle_timeout_error(idle_timeout_ms));
                }
            }
        } else {
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    invalidate_pool_owner(pool_owner, pool_entry);
                    break Err(response_start_timeout_error(idle_timeout_ms));
                }
            }
        };

        let frame = match read_ws_frame_with_keepalive(ws, read_timeout, keepalive_interval).await {
            WebSocketRead::Frame(frame) => frame,
            WebSocketRead::Timeout => {
                invalidate_pool_owner(pool_owner, pool_entry);
                break Err(if response_started {
                    websocket_idle_timeout_error(idle_timeout_ms)
                } else {
                    response_start_timeout_error(idle_timeout_ms)
                });
            }
            WebSocketRead::KeepaliveError(error) => {
                invalidate_pool_owner(pool_owner, pool_entry);
                break Err(CodexError {
                    status: 0,
                    message: format!("WebSocket keepalive error: {error}"),
                    detail: Some(WEBSOCKET_KEEPALIVE_FAILURE_DETAIL.to_string()),
                    retry_after: None,
                    usage_limit: None,
                    origin: CodexErrorOrigin::WebSocket,
                });
            }
        };

        match frame {
            Some(Ok(Message::Text(text))) => {
                if sse_body.len().saturating_add(text.len()) > MAX_BUFFERED_RESPONSE_BYTES {
                    if budget.is_tracked()
                        && serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .is_some_and(|payload| {
                                super::client_sse::payload_confirms_hosted(&payload)
                            })
                    {
                        budget.mark_hosted();
                    }
                    invalidate_pool_owner(pool_owner, pool_entry);
                    break Err(websocket_stream_error(
                        "Codex WebSocket response exceeds the size limit",
                    ));
                }
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        if let Some(tc) = traffic {
                            tc.write_json_event(
                                "040-upstream-event",
                                &serde_json::json!({
                                    "unparseable": true,
                                    "data": text.as_str(),
                                }),
                            );
                        }
                        sse_body.extend_from_slice(&encode_sse(&text));
                        continue;
                    }
                };

                if reused && stale_event(&parsed, seen_created, created_id.as_deref(), pool_entry) {
                    hosted_tool_executed |= super::client_sse::payload_confirms_hosted(&parsed);
                    if hosted_tool_executed {
                        budget.mark_hosted();
                    }
                    budget.mark_ambiguous_hosted();
                    break Err(stale_response_error());
                }
                if parsed.get("type").and_then(|v| v.as_str()) == Some("response.created") {
                    seen_created = true;
                    created_id = parsed
                        .pointer("/response/id")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                }
                sse_body.extend_from_slice(&encode_sse(&text));
                if let Some(tc) = traffic {
                    tc.write_json_event("040-upstream-event", &parsed);
                }

                if is_response_event(&parsed) {
                    response_started = true;
                    last_response_event_at = Instant::now();
                }

                hosted_tool_executed |=
                    super::client_sse::http_payload_hosted(&parsed, budget.is_tracked());
                if hosted_tool_executed {
                    budget.mark_hosted();
                }
                if is_terminal_event(&parsed) {
                    break Ok(Some(WsEvent {
                        event_type: parsed
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        reusable: completed_response_matches(&parsed, created_id.as_deref())
                            && pool_entry.is_none_or(|entry| {
                                entry.record_completed(created_id.as_deref().unwrap_or_default())
                            }),
                        payload: parsed,
                        hosted_tool_executed,
                    }));
                }
            }
            Some(Ok(Message::Binary(_))) => {
                invalidate_pool_owner(pool_owner, pool_entry);
                break Err(websocket_stream_error(
                    "WebSocket binary frames not supported",
                ));
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = ws.send(Message::Pong(data)).await;
                continue;
            }
            Some(Ok(Message::Pong(_))) => {
                continue;
            }
            Some(Ok(Message::Frame(_))) => {
                continue;
            }
            Some(Ok(Message::Close(_))) | None => {
                invalidate_pool_owner(pool_owner, pool_entry);
                break if hosted_tool_executed {
                    Err(missing_terminal_error())
                } else {
                    Ok(None)
                };
            }
            Some(Err(e)) => {
                invalidate_pool_owner(pool_owner, pool_entry);
                break Err(websocket_transport_error("stream", &e));
            }
        }
    };

    if !matches!(&terminal_event, Ok(Some(event)) if event.event_type == "response.completed") {
        budget.mark_ambiguous_hosted();
    }
    terminal_event
        .map(|event| (sse_body, event))
        .map_err(|error| {
            if hosted_tool_executed {
                error.after_hosted_tool_execution()
            } else {
                error
            }
        })
}

fn stale_before_created(event: &serde_json::Value) -> bool {
    let kind = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if is_terminal_event(event) {
        return matches!(kind, "response.completed" | "response.done")
            && !super::events::response_is_incomplete_terminal(event)
            && super::events::classify_event_failure(event).is_none();
    }
    kind.starts_with("response.") && !matches!(kind, "response.created" | "response.in_progress")
}

fn stale_event(
    event: &serde_json::Value,
    seen_created: bool,
    created_id: Option<&str>,
    entry: Option<&Arc<PoolEntry>>,
) -> bool {
    let kind = event.get("type").and_then(|v| v.as_str());
    let id = event
        .pointer("/response/id")
        .and_then(|v| v.as_str())
        .filter(|id| !id.is_empty());
    if kind == Some("response.created") {
        return id.is_none_or(|id| entry.is_some_and(|entry| entry.has_completed(id)));
    }
    if seen_created && is_terminal_event(event) {
        return id != created_id;
    }
    !seen_created && stale_before_created(event)
}

fn stale_response_error() -> CodexError {
    CodexError {
        status: 0,
        message: "Stale WebSocket response event".to_string(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketTransient,
    }
}

fn completed_response_matches(event: &serde_json::Value, created_id: Option<&str>) -> bool {
    is_terminal_event(event)
        && super::events::classify_event_failure(event).is_none()
        && !super::events::response_is_incomplete_terminal(event)
        && event.pointer("/response/status").and_then(|v| v.as_str()) == Some("completed")
        && created_id.is_some()
        && event.pointer("/response/id").and_then(|v| v.as_str()) == created_id
}

pub(super) async fn stream_ws_events<S>(
    ws: &mut WebSocketStream<S>,
    idle_timeout_ms: u64,
    traffic: Option<Arc<TrafficCapture>>,
    tx: &mpsc::Sender<Result<serde_json::Value, CodexError>>,
    budget: &super::recovery::RecoveryBudget,
    reused: bool,
    pool_entry: Option<&Arc<PoolEntry>>,
) -> (bool, Option<Result<serde_json::Value, CodexError>>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let started_at = Instant::now();
    let response_event_budget = Duration::from_millis(idle_timeout_ms);
    let response_wait_started = Instant::now();
    let mut last_response_event_at = response_wait_started;
    let mut response_started = false;
    let mut status = 200u16;
    let mut reusable = false;
    let mut completed = false;
    let mut hosted_tool_executed = false;
    let mut created_id: Option<String> = None;
    let mut seen_created = false;
    let mut terminal_item = None;

    loop {
        let response_deadline_started = if response_started {
            last_response_event_at
        } else {
            response_wait_started
        };
        let read_timeout =
            match response_event_budget.checked_sub(response_deadline_started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    terminal_item = Some(Err(if response_started {
                        websocket_idle_timeout_error(idle_timeout_ms)
                    } else {
                        response_start_timeout_error(idle_timeout_ms)
                    }));
                    break;
                }
            };

        let frame = tokio::select! {
            biased;
            _ = tx.closed() => break,
            frame = read_ws_frame_with_keepalive(
                ws,
                read_timeout,
                WEBSOCKET_KEEPALIVE_INTERVAL,
            ) => match frame {
                WebSocketRead::Frame(frame) => frame,
                WebSocketRead::Timeout => {
                    terminal_item = Some(Err(if response_started {
                        websocket_idle_timeout_error(idle_timeout_ms)
                    } else {
                        response_start_timeout_error(idle_timeout_ms)
                    }));
                    break;
                }
                WebSocketRead::KeepaliveError(error) => {
                    terminal_item = Some(Err(CodexError {
                        status: 0,
                        message: format!("WebSocket keepalive error: {error}"),
                        detail: None,
                        retry_after: None,
                        usage_limit: None,
                        origin: CodexErrorOrigin::WebSocket,
                    }));
                    break;
                }
            },
        };

        match frame {
            Some(Ok(Message::Text(text))) => {
                let parsed: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(value) => value,
                    Err(_) => {
                        if let Some(tc) = traffic.as_deref() {
                            tc.write_json_event(
                                "040-upstream-event",
                                &serde_json::json!({
                                    "unparseable": true,
                                    "data": text.as_str(),
                                }),
                            );
                        }
                        continue;
                    }
                };

                hosted_tool_executed |= super::client_sse::payload_confirms_hosted(&parsed);
                if hosted_tool_executed {
                    budget.mark_hosted();
                }
                if reused && stale_event(&parsed, seen_created, created_id.as_deref(), pool_entry) {
                    let error = stale_response_error();
                    terminal_item = Some(Err(if hosted_tool_executed {
                        error.after_hosted_tool_execution()
                    } else {
                        error
                    }));
                    break;
                }
                if parsed.get("type").and_then(|v| v.as_str()) == Some("response.created") {
                    seen_created = true;
                    created_id = parsed
                        .pointer("/response/id")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                }
                if budget.is_tracked() && super::client_sse::payload_confirms_hosted(&parsed) {
                    budget.mark_hosted();
                }
                if let Some(tc) = traffic.as_deref() {
                    tc.write_json_event("040-upstream-event", &parsed);
                }

                if is_response_event(&parsed) {
                    response_started = true;
                    last_response_event_at = Instant::now();
                }
                if parsed.get("type").and_then(|value| value.as_str()) == Some("error") {
                    status = event_error_status(&parsed).unwrap_or(500);
                }

                if is_terminal_event(&parsed) {
                    if is_previous_response_missing(&parsed) {
                        terminal_item = Some(Err(CodexError {
                            status: 0,
                            message: "Previous response not found".to_string(),
                            detail: Some("previous_response_not_found".to_string()),
                            retry_after: None,
                            usage_limit: None,
                            origin: CodexErrorOrigin::WebSocket,
                        }));
                    } else {
                        completed = parsed.get("type").and_then(|value| value.as_str())
                            == Some("response.completed");
                        reusable = completed_response_matches(&parsed, created_id.as_deref())
                            && pool_entry.is_none_or(|entry| {
                                entry.record_completed(created_id.as_deref().unwrap_or_default())
                            });
                        terminal_item = Some(Ok(parsed));
                    }
                    break;
                }

                if tx.send(Ok(parsed)).await.is_err() {
                    break;
                }
            }
            Some(Ok(Message::Binary(_))) => {
                terminal_item = Some(Err(CodexError {
                    status: 0,
                    message: "WebSocket binary frames not supported".to_string(),
                    detail: None,
                    retry_after: None,
                    usage_limit: None,
                    origin: CodexErrorOrigin::WebSocket,
                }));
                break;
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = ws.send(Message::Pong(data)).await;
            }
            Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
            Some(Ok(Message::Close(_))) | None => {
                terminal_item = Some(Err(missing_terminal_error()));
                break;
            }
            Some(Err(error)) => {
                let mut failure = websocket_transport_error("stream", &error);
                if budget.typed_websocket()
                    && matches!(
                        error,
                        tokio_tungstenite::tungstenite::Error::Io(_)
                            | tokio_tungstenite::tungstenite::Error::Protocol(
                                tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake
                            )
                    )
                {
                    failure.origin = CodexErrorOrigin::WebSocketTransient;
                }
                terminal_item = Some(Err(failure));
                break;
            }
        }
    }

    if !completed {
        budget.mark_ambiguous_hosted();
    }
    if let Some(tc) = traffic.as_deref() {
        write_websocket_response_capture(tc, status, started_at.elapsed());
    }
    (reusable, terminal_item)
}

pub(super) fn headers_to_json(headers: &HeaderMap) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (key, value) in headers.iter() {
        out.insert(
            key.to_string(),
            serde_json::Value::String(value.to_str().unwrap_or("").to_string()),
        );
    }
    serde_json::Value::Object(out)
}

pub(super) fn summarize_json_request_size(
    body: &serde_json::Value,
    body_json: &str,
) -> serde_json::Value {
    serde_json::json!({
        "bytes": body_json.len(),
        "inputCount": body
            .get("input")
            .and_then(|v| v.as_array())
            .map(|items| items.len()),
        "toolCount": body
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|items| items.len()),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    struct FailingWriteIo;

    impl AsyncRead for FailingWriteIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for FailingWriteIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test write failed",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn stale_response_events_exclude_failures_and_keepalives() {
        assert!(stale_before_created(
            &serde_json::json!({"type":"response.output_text.delta"})
        ));
        assert!(stale_before_created(
            &serde_json::json!({"type":"response.completed","response":{"status":"completed"}})
        ));
        assert!(!stale_before_created(
            &serde_json::json!({"type":"response.failed"})
        ));
        assert!(!stale_before_created(&serde_json::json!({"type":"error"})));
        assert!(!stale_before_created(
            &serde_json::json!({"type":"codex.rate_limits"})
        ));
    }

    #[tokio::test]
    async fn reused_socket_rejects_stale_output_before_delivery() {
        let (client_io, server_io) = tokio::io::duplex(2048);
        let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        server
            .send(Message::text(
                r#"{"type":"response.output_text.delta","delta":"stale"}"#,
            ))
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(2);
        let (reusable, terminal) = stream_ws_events(
            &mut client,
            1_000,
            None,
            &tx,
            &Default::default(),
            true,
            None,
        )
        .await;
        assert!(!reusable);
        assert!(matches!(
            terminal,
            Some(Err(CodexError {
                origin: CodexErrorOrigin::WebSocketTransient,
                ..
            }))
        ));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stale_hosted_event_preserves_no_replay_evidence() {
        for tracked in [false, true] {
            let (client_io, server_io) = tokio::io::duplex(2048);
            let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
            let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
            server
                .send(Message::text(
                    r#"{"type":"response.web_search_call.in_progress"}"#,
                ))
                .await
                .unwrap();
            let budget = if tracked {
                super::super::recovery::RecoveryBudget::with_gate(
                    super::super::sources::ReplayGate::new(false),
                )
            } else {
                Default::default()
            };
            let error =
                collect_ws_events_tracked(&mut client, 1_000, None, None, None, &budget, true)
                    .await
                    .err()
                    .unwrap();
            assert_eq!(error.detail.as_deref(), Some("hosted_tool_executed"));
            let result = Err(error);
            assert!(!super::super::recovery::buffered_replay_allowed(
                &result, &budget
            ));

            let (client_io, server_io) = tokio::io::duplex(2048);
            let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
            let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
            server
                .send(Message::text(
                    r#"{"type":"response.web_search_call.in_progress"}"#,
                ))
                .await
                .unwrap();
            let (tx, mut rx) = mpsc::channel(2);
            let (_, terminal) =
                stream_ws_events(&mut client, 1_000, None, &tx, &budget, true, None).await;
            let error = terminal.unwrap().unwrap_err();
            assert_eq!(error.detail.as_deref(), Some("hosted_tool_executed"));
            assert!(!super::super::live_completion::retryable_live_start_codex_error(&error));
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn reused_socket_rejects_terminal_for_another_response() {
        let (client_io, server_io) = tokio::io::duplex(2048);
        let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        server
            .send(Message::text(
                r#"{"type":"response.created","response":{"id":"new"}}"#,
            ))
            .await
            .unwrap();
        server
            .send(Message::text(
                r#"{"type":"response.completed","response":{"id":"old","status":"completed"}}"#,
            ))
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(2);
        let (reusable, terminal) = stream_ws_events(
            &mut client,
            1_000,
            None,
            &tx,
            &Default::default(),
            true,
            None,
        )
        .await;
        assert!(!reusable);
        assert!(matches!(
            terminal,
            Some(Err(CodexError {
                origin: CodexErrorOrigin::WebSocketTransient,
                ..
            }))
        ));
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn untracked_fresh_terminal_output_keeps_existing_hosted_classification() {
        let (client_io, server_io) = tokio::io::duplex(2048);
        let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        server.send(Message::text(serde_json::json!({"type":"response.completed","response":{"output":[{"type":"web_search_call"}]}}).to_string())).await.unwrap();
        let (_, terminal) = collect_ws_events(&mut client, 1_000, None, None, None)
            .await
            .unwrap();
        assert!(!terminal.unwrap().hosted_tool_executed);
    }

    #[tokio::test]
    async fn fresh_stream_without_created_or_terminal_status_succeeds_without_reuse() {
        for with_created in [false, true] {
            let (client_io, server_io) = tokio::io::duplex(2048);
            let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
            let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
            if with_created {
                server
                    .send(Message::text(
                        r#"{"type":"response.created","response":{"id":"first"}}"#,
                    ))
                    .await
                    .unwrap();
            }
            server
                .send(Message::text(
                    r#"{"type":"response.completed","response":{"id":"first"}}"#,
                ))
                .await
                .unwrap();
            let (tx, _rx) = mpsc::channel(2);
            let (reusable, terminal) = stream_ws_events(
                &mut client,
                1_000,
                None,
                &tx,
                &Default::default(),
                false,
                None,
            )
            .await;
            assert!(!reusable);
            assert!(matches!(terminal, Some(Ok(_))));
        }
    }

    #[tokio::test]
    async fn failed_and_incomplete_terminals_are_delivered_without_reuse() {
        for (kind, status) in [
            ("response.failed", "failed"),
            ("response.incomplete", "incomplete"),
        ] {
            let (client_io, server_io) = tokio::io::duplex(2048);
            let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
            let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
            server
                .send(Message::text(
                    serde_json::json!({"type":"response.created","response":{"id":"response"}})
                        .to_string(),
                ))
                .await
                .unwrap();
            let terminal = serde_json::json!({"type":kind,"response":{"id":"response","status":status,"incomplete_details":{"reason":"max_output_tokens"}}});
            server
                .send(Message::text(terminal.to_string()))
                .await
                .unwrap();
            let (tx, _rx) = mpsc::channel(2);
            let (reusable, result) = stream_ws_events(
                &mut client,
                1_000,
                None,
                &tx,
                &Default::default(),
                false,
                None,
            )
            .await;
            assert!(!reusable);
            assert_eq!(result.unwrap().unwrap(), terminal);
        }
    }

    #[tokio::test]
    async fn reuse_requires_matching_created_and_completed_response() {
        for (created, terminal, expected) in [("one", "one", true), ("one", "two", false)] {
            let (client_io, server_io) = tokio::io::duplex(2048);
            let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
            let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
            let task = tokio::spawn(async move {
                server
                    .send(Message::text(
                        serde_json::json!({"type":"response.created", "response":{"id":created}})
                            .to_string(),
                    ))
                    .await
                    .unwrap();
                server.send(Message::text(serde_json::json!({"type":"response.completed", "response":{"id":terminal,"status":"completed"}}).to_string())).await.unwrap();
            });
            let (tx, _rx) = mpsc::channel(2);
            let (reusable, terminal) = stream_ws_events(
                &mut client,
                1_000,
                None,
                &tx,
                &Default::default(),
                false,
                None,
            )
            .await;
            assert_eq!(reusable, expected);
            assert!(matches!(terminal, Some(Ok(_))));
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn abrupt_loopback_eof_is_transient_only_for_tracked_websocket() {
        use super::super::recovery::{RecoveryBudget, live_retry_exhausted};
        use super::super::sources::{FailureKind, ReplayGate};
        for (tracked, hosted) in [(true, false), (true, true), (false, false)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let peer = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let socket = WebSocketStream::from_raw_socket(socket, Role::Server, None).await;
                drop(socket);
            });
            let socket = tokio::net::TcpStream::connect(addr).await.unwrap();
            let mut socket = WebSocketStream::from_raw_socket(socket, Role::Client, None).await;
            let gate = ReplayGate::new(hosted);
            let budget = if tracked {
                RecoveryBudget::with_gate(gate.clone())
            } else {
                RecoveryBudget::default()
            };
            if tracked {
                budget.enable_typed_websocket();
            }
            let (tx, _rx) = mpsc::channel(1);
            let (_, terminal) =
                stream_ws_events(&mut socket, 1_000, None, &tx, &budget, false, None).await;
            let error = terminal.unwrap().unwrap_err();
            assert_eq!(
                error.origin,
                if tracked {
                    CodexErrorOrigin::WebSocketTransient
                } else {
                    CodexErrorOrigin::WebSocket
                }
            );
            if hosted {
                gate.mark_hosted();
            }
            let ctx = crate::provider::RequestContext {
                recovery_budget: budget.clone(),
                req_id: "test".into(),
                session_id: None,
                traffic: None,
                monitor: None,
                passthrough: None,
            };
            let exhausted =
                live_retry_exhausted(&ctx, crate::config::CodexTransport::WebSocket, &error);
            assert_eq!(
                budget.failure(exhausted).kind,
                if tracked && !hosted {
                    FailureKind::TransientExhausted
                } else {
                    FailureKind::Final
                }
            );
            peer.await.unwrap();
        }
    }

    #[tokio::test]
    async fn tracked_live_socket_retains_terminal_output_hosted_before_forwarding() {
        let (client_io, server_io) = tokio::io::duplex(2048);
        let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let gate = super::super::sources::ReplayGate::new(true);
        let budget = super::super::recovery::RecoveryBudget::with_gate(gate.clone());
        let (tx, mut rx) = mpsc::channel(1);
        let send = tokio::spawn(async move {
            server.send(Message::text(r#"{"type":"response.failed","response":{"output":[{"type":"web_search_call"}]}}"#)).await.unwrap();
        });
        let (_, terminal) =
            stream_ws_events(&mut client, 1_000, None, &tx, &budget, false, None).await;
        assert!(terminal.is_some());
        assert!(gate.hosted());
        assert!(rx.try_recv().is_err());
        send.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn silent_response_wait_sends_keepalive_ping() {
        let (client_io, server_io) = tokio::io::duplex(1024);
        let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let server_task = tokio::spawn(async move {
            let frame = server.next().await.unwrap().unwrap();
            assert!(matches!(frame, Message::Ping(_)));
            server
                .send(Message::text(r#"{"type":"response.created"}"#))
                .await
                .unwrap();
        });

        let frame = read_ws_frame_with_keepalive(
            &mut client,
            Duration::from_secs(60),
            WEBSOCKET_KEEPALIVE_INTERVAL,
        )
        .await;

        assert!(matches!(
            frame,
            WebSocketRead::Frame(Some(Ok(Message::Text(text))))
                if text.contains("response.created")
        ));
        server_task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_keepalive_write_returns_stable_error() {
        let mut client = WebSocketStream::from_raw_socket(FailingWriteIo, Role::Client, None).await;

        let error = match collect_ws_events_with_keepalive_interval(
            &mut client,
            60_000,
            None,
            None,
            None,
            WEBSOCKET_KEEPALIVE_INTERVAL,
            &Default::default(),
            false,
        )
        .await
        {
            Ok(_) => panic!("expected keepalive failure"),
            Err(error) => error,
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_KEEPALIVE_FAILURE_DETAIL)
        );
        assert!(error.message.contains("keepalive error"));
    }

    #[tokio::test]
    async fn keepalive_pongs_do_not_extend_response_start_timeout() {
        let (client_io, server_io) = tokio::io::duplex(1024);
        let mut client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let ping_count = Arc::new(AtomicUsize::new(0));
        let server_ping_count = ping_count.clone();
        let server_task = tokio::spawn(async move {
            while let Some(Ok(frame)) = server.next().await {
                if let Message::Ping(payload) = frame {
                    server_ping_count.fetch_add(1, Ordering::SeqCst);
                    if server.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                }
            }
        });

        let error = match collect_ws_events_with_keepalive_interval(
            &mut client,
            50,
            None,
            None,
            None,
            Duration::from_millis(10),
            &Default::default(),
            false,
        )
        .await
        {
            Ok(_) => panic!("expected response start timeout"),
            Err(error) => error,
        };

        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL)
        );
        assert!(ping_count.load(Ordering::SeqCst) >= 2);
        server_task.abort();
    }
}
