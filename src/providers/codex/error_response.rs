use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};

use crate::anthropic::error::json_error;

#[cfg(test)]
use super::live_completion::*;
use super::live_response::*;
use super::*;

pub(super) fn codex_event_failure_error(
    failure: &events::CodexEventFailure,
    origin: client::CodexErrorOrigin,
) -> client::CodexError {
    client::CodexError {
        status: failure.status,
        message: failure.message.clone(),
        detail: Some(failure.message.clone()),
        retry_after: failure.retry_after.clone(),
        usage_limit: None,
        origin,
    }
}

pub(super) fn codex_stream_error_type(err: &client::CodexError) -> &'static str {
    match err.status {
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ if codex_error_message(err)
            .to_lowercase()
            .contains("overloaded") =>
        {
            "overloaded_error"
        }
        _ => "api_error",
    }
}

pub(super) fn usage_limit_response(limit: &events::CodexUsageLimit) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-should-retry"),
        HeaderValue::from_static("false"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-ratelimit-unified-status"),
        HeaderValue::from_static("rejected"),
    );
    if let Some(resets_at) = limit.resets_at
        && let Ok(value) = HeaderValue::from_str(&resets_at.to_string())
    {
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-reset"),
            value,
        );
    }
    if let Some(window) = limit.window {
        headers.insert(
            HeaderName::from_static("anthropic-ratelimit-unified-representative-claim"),
            HeaderValue::from_static(window.claim()),
        );
    }

    (
        headers,
        json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            &limit.message,
        ),
    )
        .into_response()
}

#[derive(Clone)]
pub(super) struct CodexAccountChanged;

pub(super) fn map_codex_error_to_response(err: &client::CodexError) -> Response {
    if let Some(limit) = err.usage_limit.as_ref() {
        return usage_limit_response(limit);
    }
    if err.detail.as_deref() == Some(client::CODEX_ACCOUNT_CHANGED_DETAIL) {
        let mut response = json_error(StatusCode::CONFLICT, "api_error", &err.message);
        response.extensions_mut().insert(CodexAccountChanged);
        return response;
    }
    if err.is_gzip_input_limit() {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            &err.message,
        );
    }
    let message = codex_error_message(err);
    if is_context_window_overflow(message) {
        return map_codex_failure_to_response(message);
    }
    if err.is_recovery_exhausted()
        || err.is_websocket_overload()
        || err.detail.as_deref() == Some(EMPTY_CODEX_COMPLETION_DETAIL)
    {
        return (
            [(
                HeaderName::from_static("x-should-retry"),
                HeaderValue::from_static("false"),
            )],
            json_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", &err.message),
        )
            .into_response();
    }

    match err.status {
        401 => json_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            err.detail.as_deref().unwrap_or("Authentication failed"),
        ),
        403 => json_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            err.detail.as_deref().unwrap_or("Permission denied"),
        ),
        429 => {
            let response = json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                &err.message,
            );
            if let Some(retry_after) = err.retry_after.as_deref() {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        status @ (400..=599) => {
            let response = json_error(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                if status == 529 {
                    "overloaded_error"
                } else {
                    "api_error"
                },
                codex_error_message(err),
            );
            if let Some(retry_after) = err.retry_after.as_deref() {
                ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
            } else {
                response
            }
        }
        _ => json_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            codex_error_message(err),
        ),
    }
}

pub(super) fn map_codex_failure_to_response(message: &str) -> Response {
    if is_context_window_overflow(message) {
        json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            events::context_window_message(true, message),
        )
    } else {
        json_error(StatusCode::BAD_GATEWAY, "api_error", message)
    }
}

pub(super) fn map_codex_event_failure_to_response(failure: &events::CodexEventFailure) -> Response {
    let status = StatusCode::from_u16(failure.client_status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let response = json_error(
        status,
        failure.client_error_type(),
        failure.anthropic_message(),
    );
    if let Some(retry_after) = failure.retry_after.as_deref() {
        ([(http::header::RETRY_AFTER, retry_after)], response).into_response()
    } else {
        response
    }
}

fn is_context_window_overflow(message: &str) -> bool {
    events::is_context_overflow(None, message)
}

pub(super) fn codex_error_message(err: &client::CodexError) -> &str {
    if err.status == 0 || err.is_gzip_input_limit() {
        err.message.as_str()
    } else {
        err.detail.as_deref().unwrap_or("Upstream error")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_error_text_cannot_impersonate_recovery_exhaustion() {
        let mut error = client::CodexError::new(400, "Bad request".to_string());
        error.detail = Some("codex_recovery_exhausted".to_string());
        let response = map_codex_error_to_response(&error);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(!response.headers().contains_key("x-should-retry"));
    }

    #[test]
    fn context_overflow_maps_to_prompt_too_long() {
        for text in ["input exceeds context window", "Context length exceeded"] {
            let response = map_codex_failure_to_response(text);
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        assert!(events::is_context_overflow(
            Some("context_length_exceeded"),
            "rejected"
        ));
        assert!(!events::is_context_overflow(None, "rejected"));
    }

    #[tokio::test]
    async fn live_upstream_status_and_retry_after_are_preserved() {
        let err = client::CodexError {
            status: 422,
            message: "invalid request".to_string(),
            detail: Some("invalid request".to_string()),
            retry_after: Some("7".to_string()),
            usage_limit: None,
            origin: client::CodexErrorOrigin::WebSocketHandshake,
        };
        let response = map_codex_error_to_response(&err);
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            response.headers().get(http::header::RETRY_AFTER).unwrap(),
            "7"
        );
    }

    #[tokio::test]
    async fn local_gzip_input_limit_returns_request_too_large() {
        let message = "Codex HTTP request exceeds the 64 MiB gzip input limit";
        let err = client::CodexError::gzip_input_limit();
        assert_eq!(codex_error_message(&err), message);
        let response = map_codex_error_to_response(&err);
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"].as_str(), Some("request_too_large"));
        assert_eq!(body["error"]["message"].as_str(), Some(message));
    }

    #[tokio::test]
    async fn statusless_codex_error_returns_source_message() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket connect error: HTTP error: 502 Bad Gateway".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: client::CodexErrorOrigin::WebSocket,
        };

        let response = map_codex_error_to_response(&err);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body.pointer("/error/message").and_then(|v| v.as_str()),
            Some("WebSocket connect error: HTTP error: 502 Bad Gateway")
        );
    }

    #[tokio::test]
    async fn empty_live_completion_maps_to_explicit_service_unavailable() {
        let err = empty_live_completion_error();

        assert_eq!(err.status, 503);
        assert_eq!(err.detail.as_deref(), Some(EMPTY_CODEX_COMPLETION_DETAIL));
        let response = map_codex_error_to_response(&err);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["x-should-retry"], "false");
    }
}
