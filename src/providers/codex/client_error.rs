use crate::anthropic::sse::parse_sse_events;
#[cfg(test)]
use crate::request_identity::ConversationIdentity;
use crate::retry::should_retry_status;

use super::client::*;
#[cfg(test)]
use super::client_live::*;
pub use super::events::CodexUsageLimit;

#[derive(Debug)]
pub struct CodexError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
    pub usage_limit: Option<Box<CodexUsageLimit>>,
    pub origin: CodexErrorOrigin,
}

pub(crate) const CODEX_ACCOUNT_CHANGED_DETAIL: &str = "codex_account_changed";
pub(super) const TRANSPORT_UNEXPECTED_EOF_DETAIL: &str = "transport_unexpected_eof";
const HOSTED_TOOL_EXECUTED_DETAIL: &str = "hosted_tool_executed";
const CODEX_GZIP_INPUT_LIMIT_DETAIL: &str = "codex_gzip_input_limit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexErrorOrigin {
    Http,
    WebSocket,
    WebSocketHandshake,
    WebSocketOverload,
    Auth,
    BufferedHttp,
    BufferedWebSocket,
    RecoveryBudget,
}

impl CodexError {
    pub(super) fn recovery_exhausted() -> Self {
        Self {
            origin: CodexErrorOrigin::RecoveryBudget,
            ..Self::new(503, "Codex completed without producing output".to_string())
        }
    }

    pub(super) fn is_websocket_overload(&self) -> bool {
        self.origin == CodexErrorOrigin::WebSocketOverload
    }

    pub(super) fn is_recovery_exhausted(&self) -> bool {
        self.origin == CodexErrorOrigin::RecoveryBudget
    }

    pub(super) fn after_hosted_tool_execution(mut self) -> Self {
        self.detail = Some(HOSTED_TOOL_EXECUTED_DETAIL.to_string());
        self
    }

    pub fn new(status: u16, message: String) -> Self {
        Self {
            status,
            message,
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        }
    }

    pub(super) fn gzip_input_limit() -> Self {
        Self {
            detail: Some(CODEX_GZIP_INPUT_LIMIT_DETAIL.to_string()),
            ..Self::new(
                413,
                "Codex HTTP request exceeds the 64 MiB gzip input limit".to_string(),
            )
        }
    }

    pub(super) fn is_gzip_input_limit(&self) -> bool {
        self.status == 413 && self.detail.as_deref() == Some(CODEX_GZIP_INPUT_LIMIT_DETAIL)
    }
}

impl std::fmt::Display for CodexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex error {}: {}", self.status, self.message)
    }
}

#[derive(Debug)]
pub struct CodexHeaderTimeoutError {
    pub timeout_ms: u64,
}

impl std::fmt::Display for CodexHeaderTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Timed out waiting {}ms for Codex response headers",
            self.timeout_ms
        )
    }
}

#[derive(Debug)]
pub struct CodexTransportError {
    pub message: String,
}

impl std::fmt::Display for CodexTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex transport error: {}", self.message)
    }
}

pub(super) fn codex_event_failure_error(failure: super::events::CodexEventFailure) -> CodexError {
    CodexError {
        status: failure.status,
        message: failure.message.clone(),
        detail: Some(failure.message),
        retry_after: failure.retry_after,
        usage_limit: None,
        origin: CodexErrorOrigin::Http,
    }
}

pub(super) fn codex_usage_limit_error(
    limit: CodexUsageLimit,
    origin: CodexErrorOrigin,
) -> CodexError {
    CodexError {
        status: 429,
        message: limit.message.clone(),
        detail: Some(limit.message.clone()),
        retry_after: None,
        usage_limit: Some(Box::new(limit)),
        origin,
    }
}

pub(super) fn retryable_http_stream_error(error: &CodexError) -> bool {
    if error.usage_limit.is_some() || error.is_recovery_exhausted() || error.is_websocket_overload()
    {
        return false;
    }
    if should_retry_codex_status(error.status) || is_retryable_transport_error(error) {
        return true;
    }
    if error.status != 0 {
        return false;
    }
    if error.detail.as_deref() == Some("http_response_sse") {
        return error.message != "Codex SSE frame exceeds the size limit";
    }
    let message = error.message.to_ascii_lowercase();
    message.contains("ended before a terminal response event")
        || message.contains("ended with an incomplete frame")
}

pub(super) fn auth_refresh_error(err: anyhow::Error) -> CodexError {
    CodexError {
        status: 401,
        message: "Unauthorized".to_string(),
        detail: Some(err.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::Auth,
    }
}

pub(super) fn codex_status_error(response: CodexResponse) -> CodexError {
    let usage_limit =
        super::events::usage_limit_from_response(&response.body, &response.headers).map(Box::new);
    let retry_after = response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .map(|(_, value)| value.clone());
    let message = codex_status_error_message(&response.body).unwrap_or_else(|| {
        format!(
            "Upstream Codex request failed with status {}",
            response.status
        )
    });
    CodexError {
        status: response.status,
        message: message.clone(),
        detail: Some(message),
        retry_after,
        usage_limit,
        origin: match response.transport {
            ActualTransport::Http => CodexErrorOrigin::BufferedHttp,
            ActualTransport::WebSocket => CodexErrorOrigin::BufferedWebSocket,
        },
    }
}

pub(super) fn codex_status_error_message(body: &[u8]) -> Option<String> {
    let (text, code) = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => (
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .or_else(|| value.get("detail"))
                .and_then(|value| value.as_str())
                .map(str::to_string),
            value
                .pointer("/error/code")
                .or_else(|| value.get("code"))
                .and_then(|value| value.as_str())
                .map(str::to_string),
        ),
        Err(_) => (None, None),
    };
    let (text, code) = if text.is_some() {
        (text, code)
    } else {
        parse_sse_events(body)
            .into_iter()
            .find_map(|event| {
                let payload = serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
                let failure = super::events::classify_event_failure(&payload)?;
                let code = super::events::event_error(&payload)
                    .and_then(|error| error.get("code"))
                    .and_then(|code| code.as_str())
                    .map(str::to_string);
                Some((Some(failure.message), code))
            })
            .unwrap_or((None, code))
    };
    if code.as_deref() == Some("context_length_exceeded")
        && !text
            .as_deref()
            .is_some_and(|text| super::events::is_context_overflow(None, text))
    {
        return Some(match text {
            Some(text) if !text.trim().is_empty() => format!("{text} (context_length_exceeded)"),
            _ => "context_length_exceeded".to_string(),
        });
    }
    text
}

pub(super) fn should_retry_codex_status(status: u16) -> bool {
    should_retry_status(status) || status == 529
}

pub(super) fn codex_error_origin_name(origin: CodexErrorOrigin) -> &'static str {
    match origin {
        CodexErrorOrigin::Http => "http",
        CodexErrorOrigin::WebSocket => "websocket",
        CodexErrorOrigin::WebSocketHandshake => "websocket_handshake",
        CodexErrorOrigin::WebSocketOverload => "websocket_overload",
        CodexErrorOrigin::Auth => "auth",
        CodexErrorOrigin::BufferedHttp => "buffered_http",
        CodexErrorOrigin::BufferedWebSocket => "buffered_websocket",
        CodexErrorOrigin::RecoveryBudget => "recovery_budget",
    }
}

pub(super) fn is_retryable_transport_error(err: &CodexError) -> bool {
    if err.is_recovery_exhausted() || err.is_websocket_overload() {
        return false;
    }
    if matches!(
        err.detail.as_deref(),
        Some(HTTP_RESPONSE_HEADERS_DETAIL | HOSTED_TOOL_EXECUTED_DETAIL)
    ) {
        return false;
    }
    if err.origin == CodexErrorOrigin::WebSocketHandshake {
        if err.detail.as_deref() == Some(super::websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL) {
            return false;
        }
        return err.status == 0 || should_retry_codex_status(err.status);
    }
    if err.detail.as_deref() == Some("websocket_pre_request") {
        return err.status == 0 || should_retry_codex_status(err.status);
    }
    if err.detail.as_deref() == Some(super::websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL) {
        return true;
    }
    if err.status != 0 {
        return false;
    }

    let message = err.message.to_ascii_lowercase();
    err.detail.as_deref() == Some(TRANSPORT_UNEXPECTED_EOF_DETAIL)
        || message.contains("timed out waiting")
        || message.contains("transport error")
        || message.contains("connection reset")
        || message.contains("connection closed")
        || message.contains("timed out")
        || message.contains("econnreset")
        || message.contains("etimedout")
        || message.contains("broken pipe")
        || message.contains("epipe")
}

pub(super) fn has_unexpected_eof(error: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(error), |error| error.source()).any(|source| {
        source
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof)
    })
}

pub(super) fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() || has_unexpected_eof(err) {
        return true;
    }
    let msg = err.to_string().to_lowercase();
    msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("econnreset")
        || msg.contains("etimedout")
        || msg.contains("epipe")
}

pub(super) fn should_refresh_after_unauthorized(
    result: &Result<OwnerAwareCodexResponse, CodexError>,
    auth_refresh_attempted: bool,
    transport: crate::config::CodexTransport,
) -> bool {
    if auth_refresh_attempted {
        return false;
    }
    match result {
        Ok(response) => response.status == 401,
        Err(err) => {
            err.status == 401
                && (err.origin != CodexErrorOrigin::WebSocketHandshake
                    || transport == crate::config::CodexTransport::WebSocket)
        }
    }
}

pub(super) fn should_fallback_to_http(err: &CodexError) -> bool {
    (err.origin == CodexErrorOrigin::WebSocketHandshake || err.is_websocket_overload())
        && err.status != http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16()
        && err.detail.as_deref() != Some(super::websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL)
}

pub(super) fn should_retry_without_continuation(
    err: &CodexError,
    continuation: Option<&super::continuation::ContinuationReservation>,
) -> bool {
    if continuation
        .and_then(|reservation| reservation.candidate().previous_response_id.as_deref())
        .is_none()
    {
        return false;
    }

    is_continuation_retry_error(err)
}

pub(super) fn is_continuation_retry_error(err: &CodexError) -> bool {
    matches!(
        err.detail.as_deref(),
        Some("previous_response_not_found")
            | Some(super::websocket::WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL)
            | Some(super::websocket::WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL)
            | Some(super::websocket::WEBSOCKET_MISSING_TERMINAL_DETAIL)
            | Some(super::websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL)
    )
}

#[cfg(test)]
mod tests {
    use super::super::client_test_support::*;
    use super::*;

    #[test]
    fn status_error_preserves_buffered_websocket_event_message() {
        let error = codex_status_error(CodexResponse {
            body: b"data: {\"type\":\"error\",\"error\":{\"status\":400,\"message\":\"bad request\"}}\n\n"
                .to_vec(),
            status: 400,
            headers: Vec::new(),
            transport: ActualTransport::WebSocket,
        });

        assert_eq!(error.status, 400);
        assert_eq!(error.detail.as_deref(), Some("bad request"));
        assert_eq!(error.origin, CodexErrorOrigin::BufferedWebSocket);
    }

    #[test]
    fn status_error_keeps_overflow_code() {
        let json = |code: &str| {
            format!("{{\"error\":{{\"code\":\"{code}\",\"message\":\"request rejected\"}}}}")
                .into_bytes()
        };
        let sse = |code: &str| {
            format!(
                "data: {{\"type\":\"error\",\"error\":{{\"code\":\"{code}\",\"message\":\"request rejected\"}}}}\n\n"
            )
            .into_bytes()
        };
        let map = |body: Vec<u8>, transport| {
            let error = codex_status_error(CodexResponse {
                body,
                status: 500,
                headers: Vec::new(),
                transport,
            });
            let status = super::super::error_response::map_codex_error_to_response(&error).status();
            (status, error.message.contains("context_length_exceeded"))
        };
        for (body, transport) in [
            (json("context_length_exceeded"), ActualTransport::Http),
            (sse("context_length_exceeded"), ActualTransport::WebSocket),
        ] {
            let (status, has_code) = map(body, transport);
            assert_eq!(status, http::StatusCode::BAD_REQUEST);
            assert!(has_code);
        }
        for (body, transport) in [
            (json("other"), ActualTransport::Http),
            (sse("other"), ActualTransport::WebSocket),
        ] {
            let (status, has_code) = map(body, transport);
            assert_eq!(status, http::StatusCode::INTERNAL_SERVER_ERROR);
            assert!(!has_code);
        }
    }

    #[test]
    fn codex_error_display() {
        let err = CodexError {
            status: 429,
            message: "Rate limited".to_string(),
            detail: Some("body".to_string()),
            retry_after: Some("5".to_string()),
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        };
        let display = format!("{err}");
        assert!(display.contains("429"));
        assert!(display.contains("Rate limited"));
    }

    #[test]
    fn header_timeout_is_not_retried_by_either_classifier() {
        let err = CodexError {
            status: 0,
            message: "Timed out waiting 300000ms for Codex response headers".to_string(),
            detail: Some(HTTP_RESPONSE_HEADERS_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::Http,
        };

        assert!(!is_retryable_transport_error(&err));
        assert!(!retryable_http_stream_error(&err));
    }

    #[test]
    fn websocket_pre_request_502_is_retryable() {
        let err = CodexError {
            status: 502,
            message: "WebSocket connect error".to_string(),
            detail: Some("websocket_pre_request".to_string()),
            retry_after: Some("3".to_string()),
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn auto_fallback_requires_an_eligible_pre_send_failure() {
        for status in [0, 400, 401, 403, 407, 429, 502] {
            for origin in [
                CodexErrorOrigin::WebSocketHandshake,
                CodexErrorOrigin::WebSocket,
                CodexErrorOrigin::Auth,
                CodexErrorOrigin::Http,
            ] {
                let err = CodexError {
                    status,
                    message: "transport failure".to_string(),
                    detail: None,
                    retry_after: None,
                    usage_limit: None,
                    origin,
                };
                assert_eq!(
                    should_fallback_to_http(&err),
                    origin == CodexErrorOrigin::WebSocketHandshake && status != 407,
                    "status={status}, origin={origin:?}"
                );
            }
        }
    }

    #[test]
    fn proxy_tunnel_rejection_is_not_retried_or_used_for_http_fallback() {
        let err = CodexError {
            status: 0,
            message: "WebSocket proxy tunnel was rejected".to_string(),
            detail: Some(
                super::super::websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string(),
            ),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };

        assert!(!is_retryable_transport_error(&err));
        assert!(!should_fallback_to_http(&err));
    }

    #[test]
    fn websocket_pre_request_statusless_error_is_retryable() {
        let err = CodexError {
            status: 0,
            message: "WebSocket connect timeout after 15000ms".to_string(),
            detail: Some("websocket_pre_request".to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn websocket_pre_request_400_is_not_retryable() {
        let err = CodexError {
            status: 400,
            message: "WebSocket connect error".to_string(),
            detail: Some("websocket_pre_request".to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(!is_retryable_transport_error(&err));
    }

    #[test]
    fn statusless_transport_error_matching_is_case_insensitive() {
        let err = CodexError {
            status: 0,
            message: "WebSocket protocol error: Connection reset without closing handshake"
                .to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn keepalive_failure_is_retryable_with_full_context() {
        let err = CodexError {
            status: 0,
            message: "WebSocket keepalive error: test write failed".to_string(),
            detail: Some(super::super::websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
        assert!(is_continuation_retry_error(&err));
    }

    #[test]
    fn statusless_broken_pipe_is_retryable() {
        let err = CodexError {
            status: 0,
            message: "WebSocket stream error: IO error: Broken pipe (os error 32)".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        assert!(is_retryable_transport_error(&err));
    }

    #[test]
    fn codex_header_timeout_error_display() {
        let err = CodexHeaderTimeoutError { timeout_ms: 60000 };
        let display = format!("{err}");
        assert!(display.contains("60000"));
    }

    #[test]
    fn codex_transport_error_display() {
        let err = CodexTransportError {
            message: "connection reset".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("connection reset"));
    }

    #[test]
    fn unauthorized_retry_distinguishes_auto_and_strict_websocket_handshakes() {
        let http_unauthorized = Ok(OwnerAwareCodexResponse::new(
            CodexResponse {
                body: Vec::new(),
                status: 401,
                headers: Vec::new(),
                transport: ActualTransport::Http,
            },
            None,
        ));
        let websocket_unauthorized = Err(CodexError {
            status: 401,
            message: "WebSocket connect error".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        });
        let forbidden = Err(CodexError {
            status: 403,
            message: "Forbidden".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        });
        let rejected_handshake = Err(CodexError {
            status: 401,
            message: "WebSocket connect error".to_string(),
            detail: Some("policy denied".to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        });
        let rejected_handshake_err = match &rejected_handshake {
            Err(error) => error,
            Ok(_) => panic!("expected rejected handshake"),
        };

        assert!(should_refresh_after_unauthorized(
            &http_unauthorized,
            false,
            crate::config::CodexTransport::Auto
        ));
        assert!(should_refresh_after_unauthorized(
            &websocket_unauthorized,
            false,
            crate::config::CodexTransport::Auto
        ));
        assert!(!should_refresh_after_unauthorized(
            &forbidden,
            false,
            crate::config::CodexTransport::Auto
        ));
        assert!(!should_refresh_after_unauthorized(
            &rejected_handshake,
            false,
            crate::config::CodexTransport::Auto
        ));
        assert!(should_refresh_after_unauthorized(
            &rejected_handshake,
            false,
            crate::config::CodexTransport::WebSocket
        ));
        assert!(!should_refresh_after_unauthorized(
            &http_unauthorized,
            true,
            crate::config::CodexTransport::Auto
        ));
        assert!(should_fallback_to_http(rejected_handshake_err));
    }

    #[test]
    fn continuation_retry_requires_previous_response_id() {
        let owner = ConversationIdentity::Main("session".into());
        let append =
            test_continuation(Some(owner.clone()), Some(17), Some("resp_1"), Some(1), None);
        let initial =
            test_continuation(Some(owner.clone()), None, None, None, Some("missing_state"));
        let timeout = CodexError {
            status: 0,
            message: "WebSocket response start timeout after 60000ms".to_string(),
            detail: Some(
                super::super::websocket::WEBSOCKET_RESPONSE_START_TIMEOUT_DETAIL.to_string(),
            ),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let missing = CodexError {
            status: 0,
            message: "Previous response not found".to_string(),
            detail: Some("previous_response_not_found".to_string()),
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        };
        let idle = CodexError {
            status: 0,
            message: "WebSocket idle timeout after 60000ms".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocket,
        };

        let full_context = full_context_continuation(Some(&append)).unwrap();
        assert_eq!(full_context.owner(), Some(&owner));
        assert_eq!(full_context.turn_id(), Some(17));
        assert_eq!(full_context.candidate().previous_response_id, None);
        assert_eq!(full_context.origin_socket_id(), None);
        assert_eq!(
            full_context.candidate().disabled_reason.as_deref(),
            Some("full_context_retry")
        );
        assert!(should_retry_without_continuation(&timeout, Some(&append)));
        assert!(should_retry_without_continuation(&missing, Some(&append)));
        assert!(!should_retry_without_continuation(&idle, Some(&append)));
        assert!(!should_retry_without_continuation(&timeout, Some(&initial)));
        assert!(!should_retry_without_continuation(&timeout, None));
    }
}
