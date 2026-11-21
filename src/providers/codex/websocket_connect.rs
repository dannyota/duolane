use http::HeaderMap;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::time::Duration;
#[cfg(test)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::protocol::Role;

use super::client::{CodexError, CodexErrorOrigin};
use super::websocket::*;
use super::websocket_handshake::*;

pub(super) const WEBSOCKET_CONNECT_START_SPACING: Duration = Duration::from_secs(1);

const WEBSOCKET_CONNECT_FORBIDDEN_COOLDOWN: Duration = Duration::from_secs(3);

pub fn to_websocket_url(url: &str) -> Result<String, CodexWebSocketError> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| CodexWebSocketError::new(format!("Failed to parse URL: {e}")))?;
    let scheme = match parsed.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" if crate::upstream_url::is_loopback_host(&parsed) => "ws",
        other => {
            return Err(CodexWebSocketError::new(format!(
                "Codex WebSocket URL must use wss, or ws only for a loopback host, not {other}"
            )));
        }
    };
    parsed.set_scheme(scheme).map_err(|_| {
        CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
    })?;
    Ok(parsed.to_string())
}

pub(super) fn to_http_upgrade_url(url: &str) -> Result<String, CodexWebSocketError> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| CodexWebSocketError::new(format!("Failed to parse URL: {e}")))?;
    match parsed.scheme() {
        "ws" => parsed.set_scheme("http").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        "wss" => parsed.set_scheme("https").map_err(|_| {
            CodexWebSocketError::new("Unsupported Codex WebSocket URL scheme".to_string())
        })?,
        other => {
            return Err(CodexWebSocketError::new(format!(
                "Unsupported Codex WebSocket URL scheme: {other}"
            )));
        }
    }
    Ok(parsed.to_string())
}

pub fn codex_websocket_headers(http_headers: &HeaderMap) -> HeaderMap {
    let mut ws = HeaderMap::new();
    for (key, value) in http_headers.iter() {
        let key_str = key.as_str().to_lowercase();
        if matches!(
            key_str.as_str(),
            "content-length"
                | "content-type"
                | "accept"
                | "connection"
                | "upgrade"
                | "proxy-authorization"
        ) {
            continue;
        }
        ws.insert(key.clone(), value.clone());
    }
    ws.insert("openai-beta", WEBSOCKET_PROTOCOL_HEADER.parse().unwrap());
    if !ws.contains_key("sec-websocket-key") {
        ws.insert("sec-websocket-key", generate_key().parse().unwrap());
    }
    ws
}

pub(super) enum ConnectAttemptError {
    Origin(CodexError),
    ProxyTunnel(CodexError),
}

impl ConnectAttemptError {
    fn is_origin_forbidden(&self) -> bool {
        matches!(
            self,
            Self::Origin(error) if error.status == http::StatusCode::FORBIDDEN.as_u16()
        )
    }

    fn into_error(self) -> CodexError {
        match self {
            Self::Origin(error) | Self::ProxyTunnel(error) => error,
        }
    }
}

async fn connect_via_http_proxy_tunnel(
    proxy_config: &WebSocketProxyConfig,
    route: WebSocketProxyRoute,
    url: &str,
    headers: &HeaderMap,
) -> Result<CodexWebSocketStream, ConnectAttemptError> {
    let (host, authority) = websocket_destination(url).map_err(ConnectAttemptError::Origin)?;
    let stream = connect_to_http_proxy(&route, proxy_config.tls_config.clone())
        .await
        .map_err(ConnectAttemptError::ProxyTunnel)?;
    let stream = establish_connect_tunnel(stream, &authority, route.basic_auth.as_ref())
        .await
        .map_err(ConnectAttemptError::ProxyTunnel)?;
    let stream = tls_connect(
        stream,
        &host,
        proxy_config.tls_config.clone(),
        "destination",
    )
    .await
    .map_err(ConnectAttemptError::Origin)?;
    let websocket_key = generate_key();
    let subprotocols = requested_subprotocols(headers);
    let request = tunneled_websocket_request(url, headers, &websocket_key)
        .map_err(ConnectAttemptError::Origin)?;
    let (websocket, response) =
        tokio_tungstenite::client_async_with_config(request, stream, Some(websocket_config()))
            .await
            .map_err(tungstenite_handshake_error)
            .map_err(ConnectAttemptError::Origin)?;
    validate_websocket_upgrade(
        response.version(),
        response.headers(),
        &websocket_key,
        &subprotocols,
    )
    .map_err(ConnectAttemptError::Origin)?;
    Ok(websocket)
}

async fn connect_via_http_upgrade(
    websocket_client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<CodexWebSocketStream, CodexError> {
    let http_url = to_http_upgrade_url(url).map_err(|error| CodexError {
        status: 0,
        message: error.message,
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;
    let websocket_key = generate_key();
    let subprotocols = requested_subprotocols(headers);
    let mut request = websocket_client
        .get(http_url)
        .version(http::Version::HTTP_11)
        .header(http::header::CONNECTION, "Upgrade")
        .header(http::header::UPGRADE, "websocket")
        .header(http::header::SEC_WEBSOCKET_VERSION, "13")
        .header(http::header::SEC_WEBSOCKET_KEY, &websocket_key);

    for (key, value) in headers {
        if skip_websocket_request_header(key) {
            continue;
        }
        request = request.header(key.clone(), value.clone());
    }

    let response = request.send().await.map_err(reqwest_handshake_error)?;
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let detail = if status == http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16() {
            GENERIC_HANDSHAKE_ERROR_DETAIL.to_string()
        } else {
            let body = bounded_handshake_error_body(response).await;
            handshake_error_detail(Some(&body))
        };
        return Err(CodexError {
            status,
            message: format!("WebSocket upgrade rejected with status {status}"),
            detail: Some(detail),
            retry_after,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        });
    }

    validate_websocket_upgrade(
        response.version(),
        response.headers(),
        &websocket_key,
        &subprotocols,
    )?;
    let upgraded = response
        .upgrade()
        .await
        .map_err(|_| websocket_protocol_error("WebSocket upgrade stream was not available"))?;
    let upgraded: BoxedWebSocketIo = Box::new(upgraded);
    Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(websocket_config())).await)
}

pub(super) async fn connect_once(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
) -> Result<CodexWebSocketStream, ConnectAttemptError> {
    if let Some(route) = proxy_config
        .http_connect_route(url)
        .map_err(ConnectAttemptError::Origin)?
    {
        connect_via_http_proxy_tunnel(proxy_config, route, url, headers).await
    } else {
        connect_via_http_upgrade(websocket_client, url, headers)
            .await
            .map_err(ConnectAttemptError::Origin)
    }
}

fn connect_timeout_error(connect_timeout: Duration) -> CodexError {
    websocket_protocol_error(&format!(
        "WebSocket connect timeout after {}ms",
        connect_timeout.as_millis()
    ))
}

async fn connect_with_policy<T, P, PFut, F, Fut>(
    gate: &WebSocketConnectGate,
    connect_timeout: Duration,
    forbidden_cooldown: Duration,
    mut before_start: P,
    mut connect: F,
) -> Result<T, CodexError>
where
    P: FnMut() -> PFut,
    PFut: std::future::Future<Output = ()>,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ConnectAttemptError>>,
{
    for attempt in 0..=1 {
        let permit =
            tokio::time::timeout(Duration::from_secs(30), gate.wait_to_start(before_start()))
                .await
                .map_err(|_| {
                    websocket_protocol_error("WebSocket connect queue timeout after 30000ms")
                })?;
        let result = tokio::time::timeout(connect_timeout, connect())
            .await
            .unwrap_or_else(|_| {
                Err(ConnectAttemptError::Origin(connect_timeout_error(
                    connect_timeout,
                )))
            });
        drop(permit);
        if result
            .as_ref()
            .is_err_and(ConnectAttemptError::is_origin_forbidden)
        {
            gate.record_forbidden(forbidden_cooldown);
            if attempt == 0 {
                continue;
            }
        } else if result.is_ok() {
            gate.record_success();
        }
        return result.map_err(ConnectAttemptError::into_error);
    }
    unreachable!()
}

pub(super) async fn connect_with_timeout(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    connect_timeout_ms: u64,
) -> Result<CodexWebSocketStream, CodexError> {
    connect_with_policy(
        &WS_CONNECT_GATE,
        Duration::from_millis(connect_timeout_ms),
        WEBSOCKET_CONNECT_FORBIDDEN_COOLDOWN,
        || async { cleanup_pool_before_connect() },
        || connect_once(websocket_client, proxy_config, url, headers),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::super::websocket_test_support::*;
    use super::*;

    fn handshake_error(status: u16) -> CodexError {
        CodexError {
            status,
            message: format!("handshake failed with status {status}"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        }
    }

    fn origin_forbidden_error() -> ConnectAttemptError {
        ConnectAttemptError::Origin(handshake_error(http::StatusCode::FORBIDDEN.as_u16()))
    }

    #[tokio::test(start_paused = true)]
    async fn connect_queue_wait_has_a_deadline() {
        let gate = WebSocketConnectGate::new(Duration::ZERO);
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(gate.wait_to_start(async {}).await);
        }
        let started = tokio::time::Instant::now();
        let result = connect_with_policy(
            &gate,
            Duration::from_secs(1),
            Duration::from_secs(3),
            || async {},
            || async { Ok::<(), ConnectAttemptError>(()) },
        )
        .await;
        assert_eq!(
            result.unwrap_err().message,
            "WebSocket connect queue timeout after 30000ms"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn connect_gate_spaces_after_non_403_failure_without_retrying() {
        let gate = WebSocketConnectGate::new(Duration::from_secs(1));
        let attempts = AtomicUsize::new(0);
        let starts = Mutex::new(Vec::new());

        let error = connect_with_policy(
            &gate,
            Duration::from_secs(10),
            Duration::from_secs(3),
            || async {},
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                starts.lock().unwrap().push(tokio::time::Instant::now());
                async {
                    Err::<(), ConnectAttemptError>(ConnectAttemptError::Origin(handshake_error(
                        http::StatusCode::BAD_GATEWAY.as_u16(),
                    )))
                }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.status, http::StatusCode::BAD_GATEWAY.as_u16());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        connect_with_policy(
            &gate,
            Duration::from_secs(10),
            Duration::from_secs(3),
            || async {},
            || {
                starts.lock().unwrap().push(tokio::time::Instant::now());
                async { Ok::<(), ConnectAttemptError>(()) }
            },
        )
        .await
        .unwrap();

        let starts = starts.lock().unwrap();
        assert!(starts[1].duration_since(starts[0]) >= Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn origin_403_waits_for_cooldown_retries_once_and_returns_second_error() {
        let gate = WebSocketConnectGate::new(Duration::from_secs(1));
        let attempts = AtomicUsize::new(0);
        let starts = Mutex::new(Vec::new());

        let error = connect_with_policy(
            &gate,
            Duration::from_secs(10),
            Duration::from_secs(3),
            || async {},
            || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                starts.lock().unwrap().push(tokio::time::Instant::now());
                async move {
                    if attempt == 0 {
                        Err::<(), ConnectAttemptError>(origin_forbidden_error())
                    } else {
                        Err::<(), ConnectAttemptError>(ConnectAttemptError::Origin(CodexError {
                            status: http::StatusCode::FORBIDDEN.as_u16(),
                            message: "second forbidden".to_string(),
                            detail: Some("second-detail".to_string()),
                            retry_after: Some("7".to_string()),
                            usage_limit: None,
                            origin: CodexErrorOrigin::WebSocketHandshake,
                        }))
                    }
                }
            },
        )
        .await
        .unwrap_err();

        assert_eq!(error.status, http::StatusCode::FORBIDDEN.as_u16());
        assert_eq!(error.message, "second forbidden");
        assert_eq!(error.detail.as_deref(), Some("second-detail"));
        assert_eq!(error.retry_after.as_deref(), Some("7"));
        assert_eq!(error.origin, CodexErrorOrigin::WebSocketHandshake);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(gate.spacing_for_tests(), Duration::from_secs(4));
        let starts = starts.lock().unwrap();
        assert!(starts[1].duration_since(starts[0]) >= Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn proxy_connect_403_is_not_retried() {
        let gate = WebSocketConnectGate::new(Duration::ZERO);
        let attempts = AtomicUsize::new(0);

        let error = connect_with_policy(
            &gate,
            Duration::from_secs(10),
            Duration::from_secs(3),
            || async {},
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<(), ConnectAttemptError>(ConnectAttemptError::ProxyTunnel(
                        handshake_error(http::StatusCode::FORBIDDEN.as_u16()),
                    ))
                }
            },
        )
        .await
        .unwrap_err();

        assert_eq!(error.status, http::StatusCode::FORBIDDEN.as_u16());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(gate.spacing_for_tests(), Duration::ZERO);
    }

    #[tokio::test]
    async fn origin_403_with_spoofed_proxy_detail_still_retries_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while connect_response_header_end(&request).is_none() {
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&buffer[..read]);
                }
                let body = format!(
                    "{{\"error\":{{\"message\":\"{WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL}\"}}}}"
                );
                let response = format!(
                    "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let client = test_websocket_client();
        let proxy_config = WebSocketProxyConfig::direct();
        let url = format!("ws://{addr}/backend-api/codex/responses");
        let headers = HeaderMap::new();
        let gate = WebSocketConnectGate::new(Duration::ZERO);

        let error = match connect_with_policy(
            &gate,
            Duration::from_secs(1),
            Duration::ZERO,
            || async {},
            || connect_once(&client, &proxy_config, &url, &headers),
        )
        .await
        {
            Ok(_) => panic!("spoofed origin rejection should fail after one retry"),
            Err(error) => error,
        };

        assert_eq!(error.status, http::StatusCode::FORBIDDEN.as_u16());
        assert_eq!(
            error.detail.as_deref(),
            Some(WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL)
        );
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_applies_to_each_network_attempt_not_waits_or_cooldown() {
        let gate = WebSocketConnectGate::new(Duration::from_secs(1));
        let attempts = AtomicUsize::new(0);
        let started_at = tokio::time::Instant::now();

        let error = connect_with_policy(
            &gate,
            Duration::from_secs(2),
            Duration::from_secs(3),
            || async {},
            || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        Err::<(), ConnectAttemptError>(origin_forbidden_error())
                    } else {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        Ok(())
                    }
                }
            },
        )
        .await
        .unwrap_err();

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(error.message, "WebSocket connect timeout after 2000ms");
        assert_eq!(started_at.elapsed(), Duration::from_secs(6));
    }

    #[test]
    fn websocket_url_conversion() {
        assert_eq!(
            to_websocket_url("https://example.test/codex").unwrap(),
            "wss://example.test/codex"
        );
        assert_eq!(
            to_websocket_url("http://127.0.0.1:4141/codex").unwrap(),
            "ws://127.0.0.1:4141/codex"
        );
        assert_eq!(
            to_websocket_url("ws://localhost/codex").unwrap(),
            "ws://localhost/codex"
        );
        assert_eq!(
            to_websocket_url("wss://example.test/codex").unwrap(),
            "wss://example.test/codex"
        );
        assert!(to_websocket_url("http://example.test/codex").is_err());
        assert!(to_websocket_url("ws://example.test/codex").is_err());
        assert!(to_websocket_url("ftp://example.test/codex").is_err());
    }

    #[test]
    fn websocket_upgrade_url_preserves_authority_path_and_query() {
        assert_eq!(
            to_http_upgrade_url("wss://chatgpt.com/backend-api/codex/responses?mode=live").unwrap(),
            "https://chatgpt.com/backend-api/codex/responses?mode=live"
        );
        assert_eq!(
            to_http_upgrade_url("ws://127.0.0.1:4141/backend-api/codex/responses").unwrap(),
            "http://127.0.0.1:4141/backend-api/codex/responses"
        );
        assert_eq!(
            to_http_upgrade_url("ws://[::1]:4141/path").unwrap(),
            "http://[::1]:4141/path"
        );
    }

    #[test]
    fn websocket_headers_rewrite_beta() {
        let mut headers = http::HeaderMap::new();
        headers.insert("openai-beta", "responses=experimental".parse().unwrap());
        headers.insert("content-length", "10".parse().unwrap());
        headers.insert("authorization", "Bearer tok".parse().unwrap());
        headers.insert(
            http::header::PROXY_AUTHORIZATION,
            "Basic dXNlcjpwYXNz".parse().unwrap(),
        );
        let ws = codex_websocket_headers(&headers);
        assert_eq!(ws.get("openai-beta").unwrap(), WEBSOCKET_PROTOCOL_HEADER);
        assert!(!ws.contains_key("content-length"));
        assert!(!ws.contains_key(http::header::PROXY_AUTHORIZATION));
        assert_eq!(ws.get("authorization").unwrap(), "Bearer tok");
    }

    #[test]
    fn websocket_headers_strips_accept() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::ACCEPT, "text/event-stream".parse().unwrap());
        let ws = codex_websocket_headers(&headers);
        assert!(!ws.contains_key(http::header::ACCEPT.as_str()));
    }

    #[test]
    fn websocket_headers_adds_sec_key() {
        let headers = http::HeaderMap::new();
        let ws = codex_websocket_headers(&headers);
        assert!(ws.contains_key("sec-websocket-key"));
    }

    #[tokio::test]
    async fn websocket_connect_401_is_pre_request_handshake_error() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 13\r\n\r\npolicy denied")
                .await
                .unwrap();
        });

        let client = test_websocket_client();
        let err = match connect_with_timeout(
            &client,
            &WebSocketProxyConfig::direct(),
            &format!("ws://{addr}/backend-api/codex/responses"),
            &HeaderMap::new(),
            1_000,
        )
        .await
        {
            Ok(_) => panic!("expected unauthorized websocket handshake to fail"),
            Err(err) => err,
        };

        assert_eq!(err.status, 401);
        assert_eq!(err.detail.as_deref(), Some(GENERIC_HANDSHAKE_ERROR_DETAIL));
        assert_eq!(err.origin, CodexErrorOrigin::WebSocketHandshake);
    }

    #[tokio::test]
    async fn websocket_connect_502_preserves_retry_metadata() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nRetry-After: 3\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let client = test_websocket_client();
        let err = match connect_with_timeout(
            &client,
            &WebSocketProxyConfig::direct(),
            &format!("ws://{addr}/backend-api/codex/responses"),
            &HeaderMap::new(),
            1_000,
        )
        .await
        {
            Ok(_) => panic!("expected websocket handshake to fail"),
            Err(err) => err,
        };

        assert_eq!(err.status, 502);
        assert_eq!(err.detail.as_deref(), Some(GENERIC_HANDSHAKE_ERROR_DETAIL));
        assert_eq!(err.retry_after.as_deref(), Some("3"));
        assert_eq!(err.origin, CodexErrorOrigin::WebSocketHandshake);
    }
}
