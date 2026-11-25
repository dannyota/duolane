use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(test)]
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;

use super::client::CodexError;
pub use super::websocket_connect::*;
pub use super::websocket_pool::*;
pub(super) use super::websocket_proxy::*;
pub(super) use super::websocket_request::*;

pub const WEBSOCKET_PROTOCOL_HEADER: &str = "responses_websockets=2026-02-06";
pub const WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
pub const WEBSOCKET_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL: &str = "websocket_response_start_timeout";
pub const WEBSOCKET_STALE_RESPONSE_DETAIL: &str = "websocket_stale_response";
pub const WEBSOCKET_MISSING_TERMINAL_DETAIL: &str = "websocket_missing_terminal";
pub const WEBSOCKET_KEEPALIVE_FAILURE_DETAIL: &str = "websocket_keepalive_failure";
pub const WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL: &str =
    "websocket_continuation_socket_missing";
pub(super) const WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL: &str = "websocket_proxy_tunnel_rejected";

pub(super) const WEBSOCKET_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
pub(super) const WEBSOCKET_KEEPALIVE_SEND_TIMEOUT: Duration = Duration::from_secs(10);

pub type CodexWebSocketEventReceiver =
    tokio::sync::mpsc::Receiver<Result<serde_json::Value, CodexError>>;

pub(crate) struct CodexWebSocketEventStream {
    receiver: CodexWebSocketEventReceiver,
    is_http: bool,
    socket_id: Arc<AtomicU64>,
    return_id: Arc<AtomicU64>,
    full_context_retry: Arc<AtomicBool>,
    provider_retry_handoff: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(crate) struct CodexWebSocketSocketIdPublisher {
    socket_id: Arc<AtomicU64>,
    return_id: Arc<AtomicU64>,
    full_context_retry: Arc<AtomicBool>,
    provider_retry_handoff: Arc<AtomicBool>,
}

impl CodexWebSocketEventStream {
    pub(crate) fn pending(
        receiver: CodexWebSocketEventReceiver,
    ) -> (Self, CodexWebSocketSocketIdPublisher) {
        let socket_id = Arc::new(AtomicU64::new(0));
        let return_id = Arc::new(AtomicU64::new(0));
        let full_context_retry = Arc::new(AtomicBool::new(false));
        let provider_retry_handoff = Arc::new(AtomicBool::new(false));
        (
            Self {
                receiver,
                is_http: false,
                socket_id: socket_id.clone(),
                return_id: return_id.clone(),
                full_context_retry: full_context_retry.clone(),
                provider_retry_handoff: provider_retry_handoff.clone(),
            },
            CodexWebSocketSocketIdPublisher {
                socket_id,
                return_id,
                full_context_retry,
                provider_retry_handoff,
            },
        )
    }

    pub(super) fn http(receiver: CodexWebSocketEventReceiver) -> Self {
        let (mut stream, _) = Self::pending(receiver);
        stream.is_http = true;
        stream
    }

    pub(super) fn is_http(&self) -> bool {
        self.is_http
    }

    pub(crate) async fn recv(&mut self) -> Option<Result<serde_json::Value, CodexError>> {
        self.receiver.recv().await
    }

    pub(crate) fn socket_id(&self) -> Option<u64> {
        match self.socket_id.load(Ordering::Acquire) {
            0 => None,
            socket_id => Some(socket_id),
        }
    }

    pub(crate) fn close_and_return_id(&mut self) -> Option<u64> {
        self.receiver.close();
        self.return_id()
    }

    pub(crate) fn return_id(&self) -> Option<u64> {
        match self.return_id.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }

    pub(crate) fn used_full_context_retry(&self) -> bool {
        self.full_context_retry.load(Ordering::Acquire)
    }

    pub(crate) fn mark_provider_retry_handoff(&self) {
        self.provider_retry_handoff.store(true, Ordering::Release);
    }

    pub(crate) fn into_receiver(self) -> CodexWebSocketEventReceiver {
        self.receiver
    }

    pub(super) fn forward_auto(
        mut self,
        first: Result<serde_json::Value, CodexError>,
        continuation: Option<super::continuation::ContinuationReservation>,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let mut source = Self {
            receiver: std::mem::replace(&mut self.receiver, rx),
            is_http: self.is_http,
            socket_id: self.socket_id.clone(),
            return_id: self.return_id.clone(),
            full_context_retry: self.full_context_retry.clone(),
            provider_retry_handoff: self.provider_retry_handoff.clone(),
        };
        tokio::spawn(async move {
            if continuation.is_none() {
                if tx.send(first).await.is_ok() {
                    super::client_sse::forward_codex_events(source.into_receiver(), tx).await;
                }
                return;
            }
            let mut item = Some(first);
            loop {
                let next = match item.take() {
                    Some(item) => Some(item),
                    None => tokio::select! {
                        biased;
                        _ = tx.closed() => break,
                        item = source.recv() => item,
                    },
                };
                let Some(next) = next else { return };
                if tx.send(next).await.is_err() {
                    break;
                }
            }
            let return_id = source.close_and_return_id();
            if let Some(reservation) = continuation.as_ref() {
                invalidate_codex_websocket_pool_return(reservation, return_id);
            }
        });
        self
    }
}

impl CodexWebSocketSocketIdPublisher {
    pub(super) fn publish(&self, socket_id: Option<u64>) {
        self.socket_id
            .store(socket_id.unwrap_or(0), Ordering::Release);
    }

    pub(super) fn publish_return(&self, socket_id: Option<u64>, return_id: Option<u64>) {
        self.return_id
            .store(return_id.unwrap_or(0), Ordering::Release);
        self.publish(socket_id);
    }

    pub(super) fn mark_full_context_retry(&self) {
        self.full_context_retry.store(true, Ordering::Release);
    }

    pub(super) fn is_provider_retry_handoff(&self) -> bool {
        self.provider_retry_handoff.load(Ordering::Acquire)
    }
}

pub(super) trait WebSocketIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> WebSocketIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub(super) type BoxedWebSocketIo = Box<dyn WebSocketIo>;
pub(super) type CodexWebSocketStream = WebSocketStream<BoxedWebSocketIo>;

#[derive(Debug, Clone)]
pub struct CodexWebSocketError {
    pub message: String,
    pub status: Option<u16>,
    pub code: Option<String>,
    pub retry_after: Option<String>,
    pub request_sent: bool,
}

impl CodexWebSocketError {
    pub fn new(message: String) -> Self {
        Self {
            message,
            status: None,
            code: None,
            retry_after: None,
            request_sent: false,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_code(mut self, code: String) -> Self {
        self.code = Some(code);
        self
    }
}

impl std::fmt::Display for CodexWebSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex WebSocket error: {}", self.message)
    }
}

pub(super) fn encode_sse(text: &str) -> Vec<u8> {
    let mut out = String::new();
    for line in text.lines() {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.into_bytes()
}

pub(super) fn is_terminal_event(payload: &serde_json::Value) -> bool {
    super::events::event_is_terminal(payload)
}

pub(super) fn is_response_event(payload: &serde_json::Value) -> bool {
    match payload.get("type").and_then(|v| v.as_str()) {
        Some("error") => true,
        Some(t) => t.starts_with("response."),
        None => false,
    }
}

pub(super) fn is_previous_response_missing(payload: &serde_json::Value) -> bool {
    let error = super::events::event_error(payload);
    if error
        .and_then(|error| error.get("code"))
        .and_then(|value| value.as_str())
        == Some("previous_response_not_found")
    {
        return true;
    }
    if let Some(msg) = error
        .and_then(|error| error.get("message"))
        .and_then(|value| value.as_str())
    {
        let lower = msg.to_lowercase();
        if lower.contains("previous response") && lower.contains("not found") {
            return true;
        }
    }
    false
}

pub(super) fn event_error_status(payload: &serde_json::Value) -> Option<u16> {
    super::events::classify_event_failure(payload).and_then(|failure| failure.explicit_status)
}

#[allow(dead_code)]
fn extract_retry_after(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("error")
        .and_then(|e| e.get("retry_after"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_retry_handoff_is_attempt_local() {
        let (_tx_a, rx_a) = mpsc::channel(1);
        let (stream_a, publisher_a) = CodexWebSocketEventStream::pending(rx_a);
        let (_tx_b, rx_b) = mpsc::channel(1);
        let (_stream_b, publisher_b) = CodexWebSocketEventStream::pending(rx_b);

        assert!(!publisher_a.is_provider_retry_handoff());
        assert!(!publisher_b.is_provider_retry_handoff());
        stream_a.mark_provider_retry_handoff();
        assert!(publisher_a.is_provider_retry_handoff());
        assert!(!publisher_b.is_provider_retry_handoff());
    }

    #[test]
    fn event_error_status_requires_error_event_and_checks_numeric_fallbacks() {
        assert_eq!(
            event_error_status(&serde_json::json!({
                "type": "response.failed",
                "status": "failed",
                "status_code": 401
            })),
            Some(401)
        );
        assert_eq!(
            event_error_status(&serde_json::json!({
                "type": "response.completed",
                "status_code": 401
            })),
            None
        );
        assert_eq!(
            event_error_status(&serde_json::json!({
                "type": "error",
                "error": {"status": 401}
            })),
            Some(401)
        );
    }

    #[test]
    fn encode_sse_single_line() {
        let result = encode_sse(r#"{"type":"test","data":"hello"}"#);
        let expected = b"data: {\"type\":\"test\",\"data\":\"hello\"}\n\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn encode_sse_multi_line() {
        let result = encode_sse("line1\nline2");
        assert_eq!(
            String::from_utf8(result).unwrap(),
            "data: line1\ndata: line2\n\n"
        );
    }

    #[test]
    fn is_terminal_event_detection() {
        let completed = serde_json::json!({"type": "response.completed"});
        assert!(is_terminal_event(&completed));

        let done = serde_json::json!({"type": "response.done"});
        assert!(is_terminal_event(&done));

        let delta = serde_json::json!({"type": "response.output_text.delta"});
        assert!(!is_terminal_event(&delta));

        let error = serde_json::json!({"type": "error", "error": {"message": "fail"}});
        assert!(is_terminal_event(&error));

        let response_error = serde_json::json!({
            "type": "response.error",
            "response": {"error": {"message": "fail"}}
        });
        assert!(is_terminal_event(&response_error));
    }

    #[test]
    fn is_response_event_detection() {
        let rate_limits = serde_json::json!({"type": "codex.rate_limits"});
        assert!(!is_response_event(&rate_limits));

        let output = serde_json::json!({"type": "response.output_text.delta"});
        assert!(is_response_event(&output));

        let error = serde_json::json!({"type": "error", "error": {"message": "fail"}});
        assert!(is_response_event(&error));
    }

    #[test]
    fn is_previous_response_missing_detection() {
        let by_code = serde_json::json!({
            "type": "error",
            "error": {"code": "previous_response_not_found", "message": "not found"}
        });
        assert!(is_previous_response_missing(&by_code));

        let by_msg = serde_json::json!({
            "type": "error",
            "error": {"message": "The previous response was not found"}
        });
        assert!(is_previous_response_missing(&by_msg));

        let nested = serde_json::json!({
            "type": "response.failed",
            "response": {
                "error": {
                    "code": "previous_response_not_found",
                    "message": "Previous response not found"
                }
            }
        });
        assert!(is_previous_response_missing(&nested));

        let unrelated = serde_json::json!({"type": "error", "error": {"message": "rate limited"}});
        assert!(!is_previous_response_missing(&unrelated));
    }
}
