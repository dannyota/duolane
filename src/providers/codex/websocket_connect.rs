use http::HeaderMap;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering as AtomicOrdering},
};
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
    tracked: bool,
    headers_latch: Arc<OriginQuotaHeaders>,
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
    if tracked {
        return tracked_tunnel_upgrade(
            stream,
            request,
            &websocket_key,
            &subprotocols,
            &headers_latch,
        )
        .await
        .map_err(ConnectAttemptError::Origin);
    }
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
    .map_err(|mut error| {
        error.origin = CodexErrorOrigin::WebSocketValidation;
        ConnectAttemptError::Origin(error)
    })?;
    Ok(websocket)
}

async fn connect_via_http_upgrade(
    websocket_client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    gate: &WebSocketConnectGate,
    forbidden_cooldown: Duration,
    forbidden_seen: Arc<AtomicBool>,
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
        if status == http::StatusCode::FORBIDDEN.as_u16() {
            gate.record_forbidden(forbidden_cooldown);
            forbidden_seen.store(true, AtomicOrdering::Release);
        }
        let retry_after = response
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let (detail, usage_limit) =
            if status == http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16() {
                (GENERIC_HANDSHAKE_ERROR_DETAIL.to_string(), None)
            } else {
                let (body, complete) = bounded_handshake_error_body(response).await;
                (
                    handshake_error_detail(Some(&body)),
                    handshake_quota(status, &body, complete),
                )
            };
        return Err(CodexError {
            status,
            message: format!("WebSocket upgrade rejected with status {status}"),
            detail: Some(detail),
            retry_after,
            usage_limit,
            origin: CodexErrorOrigin::WebSocketHandshake,
        });
    }

    validate_websocket_upgrade(
        response.version(),
        response.headers(),
        &websocket_key,
        &subprotocols,
    )
    .map_err(|mut error| {
        error.origin = CodexErrorOrigin::WebSocketValidation;
        error
    })?;
    let upgraded = response
        .upgrade()
        .await
        .map_err(|_| websocket_protocol_error("WebSocket upgrade stream was not available"))?;
    let upgraded: BoxedWebSocketIo = Box::new(upgraded);
    Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(websocket_config())).await)
}

#[cfg(test)]
pub(super) async fn connect_once(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    gate: &WebSocketConnectGate,
    forbidden_cooldown: Duration,
    forbidden_seen: Arc<AtomicBool>,
) -> Result<CodexWebSocketStream, ConnectAttemptError> {
    connect_once_inner(
        websocket_client,
        proxy_config,
        url,
        headers,
        gate,
        forbidden_cooldown,
        forbidden_seen,
        false,
        Arc::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn connect_once_inner(
    websocket_client: &reqwest::Client,
    proxy_config: &WebSocketProxyConfig,
    url: &str,
    headers: &HeaderMap,
    gate: &WebSocketConnectGate,
    forbidden_cooldown: Duration,
    forbidden_seen: Arc<AtomicBool>,
    tracked: bool,
    headers_latch: Arc<OriginQuotaHeaders>,
) -> Result<CodexWebSocketStream, ConnectAttemptError> {
    if let Some(route) = proxy_config
        .http_connect_route(url)
        .map_err(ConnectAttemptError::Origin)?
    {
        connect_via_http_proxy_tunnel(proxy_config, route, url, headers, tracked, headers_latch)
            .await
    } else {
        connect_via_http_upgrade(
            websocket_client,
            url,
            headers,
            gate,
            forbidden_cooldown,
            forbidden_seen,
        )
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
    budget: &super::recovery::RecoveryBudget,
) -> Result<T, CodexError>
where
    P: FnMut() -> PFut,
    PFut: std::future::Future<Output = ()>,
    F: FnMut(Arc<AtomicBool>, Arc<OriginQuotaHeaders>) -> Fut,
    Fut: std::future::Future<Output = Result<T, ConnectAttemptError>>,
{
    let mut forbidden = None;
    for attempt in 0..=1 {
        let queue_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let budget_deadline = budget.deadline();
        let deadline =
            budget_deadline.map_or(queue_deadline, |deadline| deadline.min(queue_deadline));
        let permit = match tokio::time::timeout_at(
            deadline,
            gate.wait_to_start_until(before_start(), deadline),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            _ => {
                if budget_deadline
                    .is_some_and(|end| end <= queue_deadline && tokio::time::Instant::now() >= end)
                {
                    return Err(CodexError::recovery_exhausted());
                }
                return Err(forbidden.unwrap_or_else(|| CodexError {
                    status: 503,
                    origin: CodexErrorOrigin::WebSocketOverload,
                    ..websocket_protocol_error("WebSocket connect queue timeout after 30000ms")
                }));
            }
        };
        if budget.expired() {
            return Err(CodexError::recovery_exhausted());
        }
        let forbidden_seen = Arc::new(AtomicBool::new(false));
        let headers_latch = Arc::new(OriginQuotaHeaders::default());
        let result = tokio::time::timeout(
            connect_timeout,
            connect(forbidden_seen.clone(), headers_latch.clone()),
        )
        .await
        .unwrap_or_else(|_| {
            let error = if let Some(retry_after) = headers_latch.read() {
                CodexError {
                    status: 429,
                    message: "WebSocket upgrade rejected with status 429".to_string(),
                    detail: Some(GENERIC_HANDSHAKE_ERROR_DETAIL.to_string()),
                    retry_after,
                    usage_limit: None,
                    origin: CodexErrorOrigin::WebSocketHandshake,
                }
            } else if forbidden_seen.load(AtomicOrdering::Acquire) {
                CodexError {
                    status: 403,
                    message: "WebSocket upgrade rejected with status 403".to_string(),
                    detail: Some(GENERIC_HANDSHAKE_ERROR_DETAIL.to_string()),
                    ..websocket_protocol_error("WebSocket upgrade rejected with status 403")
                }
            } else {
                connect_timeout_error(connect_timeout)
            };
            Err(ConnectAttemptError::Origin(error))
        });
        if result
            .as_ref()
            .is_err_and(ConnectAttemptError::is_origin_forbidden)
        {
            if !forbidden_seen.load(AtomicOrdering::Acquire) {
                gate.record_forbidden(forbidden_cooldown);
            }
            drop(permit);
            if attempt == 0 {
                forbidden = result.err().map(ConnectAttemptError::into_error);
                if !budget.try_retry(Duration::ZERO, false) {
                    return Err(CodexError::recovery_exhausted());
                }
                continue;
            }
        } else {
            if result.is_ok() {
                gate.record_success();
            }
            drop(permit);
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
    budget: &super::recovery::RecoveryBudget,
) -> Result<CodexWebSocketStream, CodexError> {
    connect_with_policy(
        &WS_CONNECT_GATE,
        Duration::from_millis(connect_timeout_ms),
        WEBSOCKET_CONNECT_FORBIDDEN_COOLDOWN,
        || async { cleanup_pool_before_connect() },
        |seen, headers_latch| {
            connect_once_inner(
                websocket_client,
                proxy_config,
                url,
                headers,
                &WS_CONNECT_GATE,
                WEBSOCKET_CONNECT_FORBIDDEN_COOLDOWN,
                seen,
                budget.buffered_websocket(),
                headers_latch,
            )
        },
        budget,
    )
    .await
    .map_err(|mut error| {
        if error.origin == CodexErrorOrigin::WebSocketValidation {
            if budget.buffered_websocket() {
                budget.mark_final_only();
            } else {
                error.origin = CodexErrorOrigin::WebSocketHandshake;
            }
        }
        if budget.buffered_websocket() {
            if error.usage_limit.is_some() {
                error.detail = Some(GENERIC_HANDSHAKE_ERROR_DETAIL.to_string());
            }
        } else {
            error.usage_limit = None;
        }
        error
    })
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

    #[tokio::test]
    async fn tracked_tunnel_stalled_quota_body_preserves_origin_headers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut server) = tokio::io::duplex(4096);
        let peer = tokio::spawn(async move {
            let mut request = [0; 2048];
            let _ = server.read(&mut request).await.unwrap();
            server.write_all(b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 100\r\nretry-after: 120\r\n\r\n").await.unwrap();
            std::future::pending::<()>().await;
        });
        let gate = WebSocketConnectGate::new(Duration::ZERO);
        let mut client = Some(client);
        let error = match connect_with_policy(
            &gate,
            Duration::from_millis(100),
            Duration::ZERO,
            || async {},
            |_, headers_latch| {
                let client = client.take().unwrap();
                async move {
                    let request = tunneled_websocket_request(
                        "wss://example.invalid/responses",
                        &HeaderMap::new(),
                        "dGhlIHNhbXBsZSBub25jZQ==",
                    )
                    .unwrap();
                    tracked_tunnel_upgrade(
                        Box::new(client),
                        request,
                        "dGhlIHNhbXBsZSBub25jZQ==",
                        &[],
                        &headers_latch,
                    )
                    .await
                    .map_err(ConnectAttemptError::Origin)
                }
            },
            &super::super::recovery::RecoveryBudget::default(),
        )
        .await
        {
            Ok(_) => panic!("expected timeout"),
            Err(error) => error,
        };
        assert_eq!(error.status, 429);
        assert_eq!(error.retry_after.as_deref(), Some("120"));
        assert!(error.usage_limit.is_none());
        peer.abort();
        let _ = peer.await;
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
            |_, _| async { Ok::<(), ConnectAttemptError>(()) },
            &super::super::recovery::RecoveryBudget::default(),
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(error.status, 503);
        assert!(!super::super::client::retryable_http_stream_error(&error));
        assert!(!super::super::client::is_retryable_transport_error(&error));
        assert!(!super::super::live_completion::retryable_live_start_codex_error(&error));
        assert!(super::super::client::should_fallback_to_http(&error));
        let response = super::super::error_response::map_codex_error_to_response(&error);
        assert_eq!(response.status(), 503);
        assert_eq!(response.headers()["x-should-retry"], "false");

        assert_eq!(started.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn forbidden_updates_cooldown_before_permit_is_released() {
        let gate = std::sync::Arc::new(WebSocketConnectGate::new(Duration::ZERO));
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(gate.wait_to_start(async {}).await);
        }
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let connecting_gate = gate.clone();
        let connecting = tokio::spawn(async move {
            let mut release_rx = Some(release_rx);
            let mut started_tx = Some(started_tx);
            connect_with_policy(
                &connecting_gate,
                Duration::from_secs(10),
                Duration::from_secs(3),
                || async {},
                |_, _| {
                    let rx = release_rx.take();
                    if let Some(tx) = started_tx.take() {
                        tx.send(()).unwrap();
                    }
                    async move {
                        if let Some(rx) = rx {
                            rx.await.unwrap();
                        }
                        Err::<(), _>(origin_forbidden_error())
                    }
                },
                &super::super::recovery::RecoveryBudget::default(),
            )
            .await
        });
        started_rx.await.unwrap();
        let waiter_gate = gate.clone();
        let waiter = tokio::spawn(async move {
            let _permit = waiter_gate.wait_to_start(async {}).await;
            tokio::time::Instant::now()
        });
        tokio::task::yield_now().await;
        let now = tokio::time::Instant::now();
        release_tx.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        let started = waiter.await.unwrap();
        assert!(started.duration_since(now) >= Duration::from_secs(3));
        connecting.abort();
        let _ = connecting.await;
        drop(held);
    }

    #[tokio::test(start_paused = true)]
    async fn first_connect_does_not_start_recovery_window() {
        let gate = WebSocketConnectGate::new(Duration::ZERO);
        let budget = super::super::recovery::RecoveryBudget::with_window(Duration::from_secs(1));
        tokio::time::advance(Duration::from_secs(2)).await;
        connect_with_policy(
            &gate,
            Duration::from_secs(1),
            Duration::from_secs(3),
            || async {},
            |_, _| async { Ok::<(), ConnectAttemptError>(()) },
            &budget,
        )
        .await
        .unwrap();
        assert!(budget.deadline().is_none());
        assert!(budget.try_retry(Duration::ZERO, false));
    }

    #[tokio::test(start_paused = true)]
    async fn existing_recovery_deadline_stops_a_queued_first_attempt() {
        let gate = WebSocketConnectGate::new(Duration::from_secs(1));
        let budget = super::super::recovery::RecoveryBudget::default();
        budget.start();
        tokio::time::advance(Duration::from_secs(58)).await;
        gate.record_forbidden(Duration::from_secs(5));
        let calls = AtomicUsize::new(0);
        let result = connect_with_policy(
            &gate,
            Duration::from_secs(10),
            Duration::from_secs(3),
            || async {},
            |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok::<(), ConnectAttemptError>(()) }
            },
            &budget,
        )
        .await;
        assert!(result.unwrap_err().is_recovery_exhausted());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(gate.last_start_for_tests(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_timeout_after_forbidden_preserves_origin_error() {
        let gate = WebSocketConnectGate::new(Duration::from_secs(1));
        let budget = super::super::recovery::RecoveryBudget::default();
        let result = connect_with_policy(
            &gate,
            Duration::from_secs(1),
            Duration::from_secs(31),
            || async {},
            |_, _| async { Err::<(), _>(origin_forbidden_error()) },
            &budget,
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(error.status, 403);
        assert_eq!(error.origin, CodexErrorOrigin::WebSocketHandshake);
    }

    #[tokio::test(start_paused = true)]
    async fn forbidden_retry_consumes_request_recovery_budget() {
        let gate = WebSocketConnectGate::new(Duration::ZERO);
        let budget = super::super::recovery::RecoveryBudget::default();
        for _ in 0..3 {
            assert!(budget.try_retry(Duration::ZERO, false));
        }
        let attempts = AtomicUsize::new(0);
        let result = connect_with_policy(
            &gate,
            Duration::from_secs(1),
            Duration::ZERO,
            || async {},
            |_, _| {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { Err::<(), _>(origin_forbidden_error()) }
            },
            &budget,
        )
        .await;
        assert!(result.unwrap_err().is_recovery_exhausted());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
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
            |_, _| {
                attempts.fetch_add(1, Ordering::SeqCst);
                starts.lock().unwrap().push(tokio::time::Instant::now());
                async {
                    Err::<(), ConnectAttemptError>(ConnectAttemptError::Origin(handshake_error(
                        http::StatusCode::BAD_GATEWAY.as_u16(),
                    )))
                }
            },
            &super::super::recovery::RecoveryBudget::default(),
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
            |_, _| {
                starts.lock().unwrap().push(tokio::time::Instant::now());
                async { Ok::<(), ConnectAttemptError>(()) }
            },
            &super::super::recovery::RecoveryBudget::default(),
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
            |_, _| {
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
            &super::super::recovery::RecoveryBudget::default(),
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
            |_, _| {
                attempts.fetch_add(1, Ordering::SeqCst);
                async {
                    Err::<(), ConnectAttemptError>(ConnectAttemptError::ProxyTunnel(
                        handshake_error(http::StatusCode::FORBIDDEN.as_u16()),
                    ))
                }
            },
            &super::super::recovery::RecoveryBudget::default(),
        )
        .await
        .unwrap_err();

        assert_eq!(error.status, http::StatusCode::FORBIDDEN.as_u16());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(gate.spacing_for_tests(), Duration::ZERO);
    }

    #[tokio::test]
    async fn stalled_origin_403_body_records_headers_and_preserves_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while connect_response_header_end(&request).is_none() {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 10\r\n\r\n")
                .await
                .unwrap();
            tx.send(()).unwrap();
            futures_util::future::pending::<()>().await;
        });
        let client = test_websocket_client();
        let proxy = WebSocketProxyConfig::direct();
        let url = format!("ws://{addr}/responses");
        let headers = HeaderMap::new();
        let gate = WebSocketConnectGate::new(Duration::from_millis(10));
        let budget = super::super::recovery::RecoveryBudget::default();
        let attempts = AtomicUsize::new(0);
        let request = connect_with_policy(
            &gate,
            Duration::from_secs(60),
            Duration::from_millis(20),
            || async {},
            |seen, _| {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                let (client, proxy, url, headers, gate) = (&client, &proxy, &url, &headers, &gate);
                async move {
                    if attempt == 0 {
                        connect_once(
                            client,
                            proxy,
                            url,
                            headers,
                            gate,
                            Duration::from_millis(20),
                            seen,
                        )
                        .await
                    } else {
                        Err(origin_forbidden_error())
                    }
                }
            },
            &budget,
        );
        tokio::pin!(request);
        let observation = async {
            rx.recv().await.unwrap();
            while gate.spacing_for_tests() == Duration::from_millis(10) {
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            () = observation => {},
            _ = &mut request => panic!("handshake ended before observing the origin 403"),
        }
        assert_eq!(gate.spacing_for_tests(), Duration::from_millis(20));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(60)).await;
        let error = request.await.err().unwrap();
        assert_eq!(error.status, 403);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn origin_body_cannot_spoof_overload_classification() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while connect_response_header_end(&request).is_none() {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            let body = r#"{"error":{"message":"websocket_connect_overload"}}"#;
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let client = test_websocket_client();
        let gate = WebSocketConnectGate::new(Duration::ZERO);
        let error = match connect_once(
            &client,
            &WebSocketProxyConfig::direct(),
            &format!("ws://{addr}/responses"),
            &HeaderMap::new(),
            &gate,
            Duration::ZERO,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        {
            Ok(_) => panic!("expected rejection"),
            Err(error) => error.into_error(),
        };
        assert_eq!(error.status, 403);
        assert_eq!(error.detail.as_deref(), Some("websocket_connect_overload"));
        assert!(!error.is_websocket_overload());
        let response = super::super::error_response::map_codex_error_to_response(&error);
        assert_eq!(response.status(), 403);
        assert!(!response.headers().contains_key("x-should-retry"));
        server.await.unwrap();
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
            |seen, _| {
                connect_once(
                    &client,
                    &proxy_config,
                    &url,
                    &headers,
                    &gate,
                    Duration::ZERO,
                    seen,
                )
            },
            &super::super::recovery::RecoveryBudget::default(),
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
            |_, _| {
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
            &super::super::recovery::RecoveryBudget::default(),
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
            &super::super::recovery::RecoveryBudget::default(),
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
            &super::super::recovery::RecoveryBudget::default(),
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
