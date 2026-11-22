use http::HeaderMap;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;

use super::client::{CodexError, CodexErrorOrigin};
use super::websocket::*;

const MAX_HANDSHAKE_ERROR_DETAIL_BYTES: usize = 1024;

pub(super) const GENERIC_HANDSHAKE_ERROR_DETAIL: &str = "WebSocket upgrade was rejected";

pub(super) fn handshake_error_detail(body: Option<&[u8]>) -> String {
    let Some(value) = body.and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok())
    else {
        return GENERIC_HANDSHAKE_ERROR_DETAIL.to_string();
    };
    let Some(message) = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(|value| value.as_str())
    else {
        return GENERIC_HANDSHAKE_ERROR_DETAIL.to_string();
    };
    let sanitized: String = message
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, ' '))
        .collect();
    let mut end = sanitized.len().min(MAX_HANDSHAKE_ERROR_DETAIL_BYTES);
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized[..end].to_string()
}

fn header_has_token(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().ok().is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case(expected))
        })
    })
}

pub(super) fn requested_subprotocols(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

pub(super) fn websocket_protocol_error(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: message.to_string(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

pub(super) fn validate_websocket_upgrade(
    version: http::Version,
    headers: &HeaderMap,
    websocket_key: &str,
    requested_subprotocols: &[String],
) -> Result<(), CodexError> {
    if version != http::Version::HTTP_11 {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response did not use HTTP/1.1",
        ));
    }
    if !header_has_token(headers, http::header::UPGRADE.as_str(), "websocket") {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response is missing Upgrade: websocket",
        ));
    }
    if !header_has_token(headers, http::header::CONNECTION.as_str(), "upgrade") {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response is missing Connection: Upgrade",
        ));
    }

    let expected_accept = derive_accept_key(websocket_key.as_bytes());
    let mut accept_values = headers.get_all(http::header::SEC_WEBSOCKET_ACCEPT).iter();
    let accept = accept_values.next().and_then(|value| value.to_str().ok());
    if accept_values.next().is_some() || accept != Some(expected_accept.as_str()) {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response has an invalid Sec-WebSocket-Accept",
        ));
    }
    if headers.contains_key(http::header::SEC_WEBSOCKET_EXTENSIONS) {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response selected an unsolicited extension",
        ));
    }

    let mut response_protocols = headers.get_all(http::header::SEC_WEBSOCKET_PROTOCOL).iter();
    let response_protocol = response_protocols
        .next()
        .map(|value| value.to_str().map(str::trim));
    if response_protocols.next().is_some() {
        return Err(websocket_protocol_error(
            "WebSocket upgrade response contains multiple subprotocols",
        ));
    }
    match response_protocol {
        None if requested_subprotocols.is_empty() => {}
        None => {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response omitted the requested subprotocol",
            ));
        }
        Some(Err(_)) => {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response contains an invalid subprotocol",
            ));
        }
        Some(Ok(_)) if requested_subprotocols.is_empty() => {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response selected an unsolicited subprotocol",
            ));
        }
        Some(Ok(protocol))
            if !requested_subprotocols
                .iter()
                .any(|requested| requested == protocol) =>
        {
            return Err(websocket_protocol_error(
                "WebSocket upgrade response selected an unsupported subprotocol",
            ));
        }
        Some(Ok(_)) => {}
    }

    Ok(())
}

pub(super) async fn bounded_handshake_error_body(
    mut response: reqwest::Response,
) -> (Vec<u8>, bool) {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = MAX_HANDSHAKE_ERROR_DETAIL_BYTES - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if chunk.len() >= remaining {
                    return (body, false);
                }
            }
            Ok(None) => return (body, true),
            Err(_) => return (body, false),
        }
    }
}

pub(super) fn handshake_quota(
    status: u16,
    body: &[u8],
    complete: bool,
) -> Option<Box<super::events::CodexUsageLimit>> {
    if status != 429
        || !complete
        || body.len() > MAX_HANDSHAKE_ERROR_DETAIL_BYTES
        || super::client::explicit_permanent_buffered_error(body).is_some()
        || super::client::codex_status_error_message(body)
            .is_some_and(|message| super::events::is_context_overflow(None, &message))
    {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    if value
        .pointer("/error/type")
        .and_then(serde_json::Value::as_str)
        != Some("usage_limit_reached")
    {
        return None;
    }
    let mut limit = super::events::usage_limit_from_response(body, &[])?;
    limit.message = GENERIC_HANDSHAKE_ERROR_DETAIL.to_string();
    Some(Box::new(limit))
}

#[derive(Default)]
pub(super) struct OriginQuotaHeaders(std::sync::Mutex<Option<Option<String>>>);

impl OriginQuotaHeaders {
    pub(super) fn record(&self, retry_after: Option<String>) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(retry_after);
    }

    pub(super) fn read(&self) -> Option<Option<String>> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

struct AbortHandshake(tokio::task::JoinHandle<()>);

impl Drop for AbortHandshake {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) async fn tracked_tunnel_upgrade(
    stream: BoxedWebSocketIo,
    request: http::Request<()>,
    key: &str,
    protocols: &[String],
    headers_latch: &OriginQuotaHeaders,
) -> Result<CodexWebSocketStream, CodexError> {
    use http_body_util::BodyExt;
    use hyper_util::rt::TokioIo;

    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| websocket_protocol_error("WebSocket upgrade connection failed"))?;
    let _driver = AbortHandshake(tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    }));
    let (mut parts, ()) = request.into_parts();
    parts.uri = parts
        .uri
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or("/")
        .parse()
        .map_err(|_| websocket_protocol_error("WebSocket request URL was invalid"))?;
    let request = http::Request::from_parts(parts, http_body_util::Empty::<bytes::Bytes>::new());
    let mut response = sender
        .send_request(request)
        .await
        .map_err(|_| websocket_protocol_error("WebSocket upgrade request failed"))?;
    let status = response.status().as_u16();
    if status != 101 {
        let retry_after = response
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if status == 429 {
            headers_latch.record(retry_after.clone());
        }
        let mut body = Vec::new();
        let mut complete = false;
        if status == 429 {
            loop {
                match response.body_mut().frame().await {
                    None => {
                        complete = true;
                        break;
                    }
                    Some(Err(_)) => break,
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            if body.len().saturating_add(data.len())
                                > MAX_HANDSHAKE_ERROR_DETAIL_BYTES
                            {
                                break;
                            }
                            body.extend_from_slice(&data);
                        }
                    }
                }
            }
        }
        return Err(CodexError {
            status,
            message: format!("WebSocket upgrade rejected with status {status}"),
            detail: Some(GENERIC_HANDSHAKE_ERROR_DETAIL.to_string()),
            retry_after,
            usage_limit: handshake_quota(status, &body, complete),
            origin: CodexErrorOrigin::WebSocketHandshake,
        });
    }
    validate_websocket_upgrade(response.version(), response.headers(), key, protocols).map_err(
        |mut error| {
            error.origin = CodexErrorOrigin::WebSocketValidation;
            error
        },
    )?;
    let upgraded = hyper::upgrade::on(&mut response)
        .await
        .map_err(|_| websocket_protocol_error("WebSocket upgrade stream was not available"))?;
    let stream: BoxedWebSocketIo = Box::new(TokioIo::new(upgraded));
    Ok(tokio_tungstenite::WebSocketStream::from_raw_socket(
        stream,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        Some(super::websocket::websocket_config()),
    )
    .await)
}

fn error_chain_contains(error: &(dyn std::error::Error + 'static), expected: &str) -> bool {
    let expected = expected.to_ascii_lowercase();
    let mut current = Some(error);
    while let Some(error) = current {
        if error.to_string().to_ascii_lowercase().contains(&expected) {
            return true;
        }
        current = error.source();
    }
    false
}

pub(super) fn reqwest_handshake_error(error: reqwest::Error) -> CodexError {
    let proxy_auth_required =
        error.is_connect() && error_chain_contains(&error, "proxy authorization required");
    let proxy_tunnel_rejected =
        error.is_connect() && error_chain_contains(&error, "tunnel error: unsuccessful");
    let status = if proxy_auth_required {
        http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16()
    } else {
        error.status().map(|status| status.as_u16()).unwrap_or(0)
    };
    let message = if proxy_auth_required {
        "WebSocket proxy authentication failed"
    } else if proxy_tunnel_rejected {
        "WebSocket proxy tunnel was rejected"
    } else if error.is_timeout() {
        "WebSocket upgrade request timed out"
    } else if error.is_connect() {
        "WebSocket connection failed"
    } else {
        "WebSocket upgrade request failed"
    };
    CodexError {
        status,
        message: message.to_string(),
        detail: proxy_tunnel_rejected.then(|| WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

pub(super) fn tungstenite_handshake_error(
    error: tokio_tungstenite::tungstenite::Error,
) -> CodexError {
    if let tokio_tungstenite::tungstenite::Error::Http(response) = error {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let usage_limit = response.body().as_ref().and_then(|body| {
            let complete = response
                .headers()
                .get_all(http::header::CONTENT_LENGTH)
                .iter()
                .count()
                == 1
                && !response
                    .headers()
                    .contains_key(http::header::TRANSFER_ENCODING)
                && response
                    .headers()
                    .get(http::header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<usize>().ok())
                    == Some(body.len());
            handshake_quota(status, body, complete)
        });
        return CodexError {
            status,
            message: format!("WebSocket upgrade rejected with status {status}"),
            detail: Some(GENERIC_HANDSHAKE_ERROR_DETAIL.to_string()),
            retry_after,
            usage_limit,
            origin: CodexErrorOrigin::WebSocketHandshake,
        };
    }
    let validation = matches!(
        error,
        tokio_tungstenite::tungstenite::Error::Protocol(
            tokio_tungstenite::tungstenite::error::ProtocolError::MissingUpgradeWebSocketHeader
                | tokio_tungstenite::tungstenite::error::ProtocolError::MissingConnectionUpgradeHeader
                | tokio_tungstenite::tungstenite::error::ProtocolError::SecWebSocketAcceptKeyMismatch
                | tokio_tungstenite::tungstenite::error::ProtocolError::SecWebSocketSubProtocolError(_)
        )
    );
    CodexError {
        status: 0,
        message: "WebSocket upgrade request failed".to_string(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: if validation {
            CodexErrorOrigin::WebSocketValidation
        } else {
            CodexErrorOrigin::WebSocketHandshake
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn split_tunnel_quota_body_is_read_to_completion() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut server) = tokio::io::duplex(4096);
        let peer = tokio::spawn(async move {
            let mut request = [0; 2048];
            let _ = server.read(&mut request).await.unwrap();
            let body = br#"{"error":{"type":"usage_limit_reached","message":"limit","resets_at":1234567890}}"#;
            let headers = format!(
                "HTTP/1.1 429 Too Many Requests\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            server.write_all(headers.as_bytes()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = server.write_all(body).await;
        });
        let request = super::super::websocket::tunneled_websocket_request(
            "wss://example.invalid/responses",
            &HeaderMap::new(),
            "dGhlIHNhbXBsZSBub25jZQ==",
        )
        .unwrap();
        let error = match tracked_tunnel_upgrade(
            Box::new(client),
            request,
            "dGhlIHNhbXBsZSBub25jZQ==",
            &[],
            &OriginQuotaHeaders::default(),
        )
        .await
        {
            Ok(_) => panic!("expected quota"),
            Err(error) => error,
        };
        assert!(error.usage_limit.is_some());
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn tracked_tunnel_rejects_truncated_and_fake_quota() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for truncated in [true, false] {
            let (client, mut server) = tokio::io::duplex(4096);
            let peer = tokio::spawn(async move {
                let mut request = [0; 2048];
                let _ = server.read(&mut request).await.unwrap();
                let body: &[u8] = if truncated {
                    br#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#
                } else {
                    b"data: {\"error\":{\"type\":\"usage_limit_reached\"}}\n\n"
                };
                let headers = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-length: {}\r\n\r\n",
                    body.len() + usize::from(truncated)
                );
                server.write_all(headers.as_bytes()).await.unwrap();
                server.write_all(body).await.unwrap();
            });
            let request = super::super::websocket::tunneled_websocket_request(
                "wss://example.invalid/responses",
                &HeaderMap::new(),
                "dGhlIHNhbXBsZSBub25jZQ==",
            )
            .unwrap();
            let error = match tracked_tunnel_upgrade(
                Box::new(client),
                request,
                "dGhlIHNhbXBsZSBub25jZQ==",
                &[],
                &OriginQuotaHeaders::default(),
            )
            .await
            {
                Ok(_) => panic!("expected denial"),
                Err(error) => error,
            };
            assert!(error.usage_limit.is_none());
            peer.await.unwrap();
        }
    }

    #[tokio::test]
    async fn tracked_tunnel_upgrade_retains_buffered_websocket_frame() {
        use futures_util::StreamExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut server) = tokio::io::duplex(4096);
        let peer = tokio::spawn(async move {
            let mut request = [0; 2048];
            let _ = server.read(&mut request).await.unwrap();
            let mut response = b"HTTP/1.1 101 Switching Protocols\r\nconnection: upgrade\r\nupgrade: websocket\r\nsec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n".to_vec();
            response.extend_from_slice(&[0x81, 0x02, b'o', b'k']);
            server.write_all(&response).await.unwrap();
        });
        let request = super::super::websocket::tunneled_websocket_request(
            "wss://example.invalid/responses",
            &HeaderMap::new(),
            "dGhlIHNhbXBsZSBub25jZQ==",
        )
        .unwrap();
        let mut socket = tracked_tunnel_upgrade(
            Box::new(client),
            request,
            "dGhlIHNhbXBsZSBub25jZQ==",
            &[],
            &OriginQuotaHeaders::default(),
        )
        .await
        .unwrap();
        assert!(
            matches!(socket.next().await, Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) if text == "ok")
        );
        peer.await.unwrap();
    }

    #[test]
    fn tunneled_invalid_accept_has_validation_origin() {
        let error = tungstenite_handshake_error(tokio_tungstenite::tungstenite::Error::Protocol(
            tokio_tungstenite::tungstenite::error::ProtocolError::SecWebSocketAcceptKeyMismatch,
        ));
        assert_eq!(error.origin, CodexErrorOrigin::WebSocketValidation);
    }

    #[test]
    fn handshake_quota_rejects_sse_frames() {
        let body =
            b"data: {\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"limit\"}}\n\n";
        assert!(handshake_quota(429, body, true).is_none());
    }

    #[test]
    fn validates_tokenized_websocket_upgrade_headers() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let mut headers = HeaderMap::new();
        headers.insert(http::header::UPGRADE, "h2c, WebSocket".parse().unwrap());
        headers.insert(
            http::header::CONNECTION,
            "keep-alive, Upgrade".parse().unwrap(),
        );
        headers.insert(
            http::header::SEC_WEBSOCKET_ACCEPT,
            derive_accept_key(key.as_bytes()).parse().unwrap(),
        );
        headers.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            "responses".parse().unwrap(),
        );

        validate_websocket_upgrade(
            http::Version::HTTP_11,
            &headers,
            key,
            &["responses".to_string()],
        )
        .unwrap();
    }

    #[test]
    fn rejects_invalid_websocket_upgrade_headers() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let valid = || {
            let mut headers = HeaderMap::new();
            headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
            headers.insert(http::header::CONNECTION, "Upgrade".parse().unwrap());
            headers.insert(
                http::header::SEC_WEBSOCKET_ACCEPT,
                derive_accept_key(key.as_bytes()).parse().unwrap(),
            );
            headers
        };

        let mut missing_upgrade = valid();
        missing_upgrade.remove(http::header::UPGRADE);
        assert!(
            validate_websocket_upgrade(http::Version::HTTP_11, &missing_upgrade, key, &[]).is_err()
        );

        let mut missing_connection = valid();
        missing_connection.remove(http::header::CONNECTION);
        assert!(
            validate_websocket_upgrade(http::Version::HTTP_11, &missing_connection, key, &[])
                .is_err()
        );

        let mut wrong_accept = valid();
        wrong_accept.insert(http::header::SEC_WEBSOCKET_ACCEPT, "wrong".parse().unwrap());
        assert!(
            validate_websocket_upgrade(http::Version::HTTP_11, &wrong_accept, key, &[]).is_err()
        );

        let unsolicited_extension = {
            let mut headers = valid();
            headers.insert(
                http::header::SEC_WEBSOCKET_EXTENSIONS,
                "permessage-deflate".parse().unwrap(),
            );
            headers
        };
        assert!(
            validate_websocket_upgrade(http::Version::HTTP_11, &unsolicited_extension, key, &[],)
                .is_err()
        );

        assert!(validate_websocket_upgrade(http::Version::HTTP_10, &valid(), key, &[]).is_err());

        let mut unsolicited_protocol = valid();
        unsolicited_protocol.insert(
            http::header::SEC_WEBSOCKET_PROTOCOL,
            "unexpected".parse().unwrap(),
        );
        assert!(
            validate_websocket_upgrade(http::Version::HTTP_11, &unsolicited_protocol, key, &[])
                .is_err()
        );
    }

    #[test]
    fn handshake_details_are_structured_sanitized_and_bounded() {
        let message = format!("safe\n{}", "x".repeat(MAX_HANDSHAKE_ERROR_DETAIL_BYTES * 2));
        let body = serde_json::to_vec(&serde_json::json!({
            "error": { "message": message }
        }))
        .unwrap();
        let detail = handshake_error_detail(Some(&body));
        assert!(!detail.contains('\n'));
        assert!(detail.len() <= MAX_HANDSHAKE_ERROR_DETAIL_BYTES);
        assert!(detail.starts_with("safe"));
    }

    #[test]
    fn handshake_details_reject_unstructured_and_binary_bodies() {
        for body in [
            b"<html>denied</html>".to_vec(),
            vec![0xff, 0xfe],
            b"{".to_vec(),
        ] {
            assert_eq!(
                handshake_error_detail(Some(&body)),
                GENERIC_HANDSHAKE_ERROR_DETAIL
            );
        }
    }
}
