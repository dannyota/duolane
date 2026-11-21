#[cfg(test)]
use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
use hyper_util::client::proxy::matcher::Matcher as ProxyMatcher;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
#[cfg(test)]
use tokio_tungstenite::WebSocketStream;
#[cfg(test)]
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
#[cfg(test)]
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
#[cfg(test)]
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::logging::create_logger;

use super::client::{CodexError, CodexErrorOrigin};
use super::websocket::*;
use super::websocket_handshake::*;

const MAX_CONNECT_RESPONSE_HEADER_BYTES: usize = 8 * 1024;

pub(super) struct WebSocketProxyConfig {
    matcher: ProxyMatcher,
    pub(super) tls_config: Arc<rustls::ClientConfig>,
}

#[derive(Clone)]
pub(super) struct WebSocketProxyRoute {
    uri: http::Uri,
    pub(super) basic_auth: Option<http::HeaderValue>,
}

impl WebSocketProxyConfig {
    pub(super) fn new(
        http_proxy: Option<&str>,
        https_proxy: Option<&str>,
        all_proxy: Option<&str>,
        no_proxy: Option<&str>,
    ) -> Self {
        let mut builder = ProxyMatcher::builder();
        if let Some(proxy) = all_proxy {
            builder = builder.all(proxy.to_string());
        }
        if let Some(proxy) = http_proxy {
            builder = builder.http(proxy.to_string());
        }
        if let Some(proxy) = https_proxy {
            builder = builder.https(proxy.to_string());
        }
        if let Some(no_proxy) = no_proxy {
            builder = builder.no(no_proxy.to_string());
        }
        Self {
            matcher: builder.build(),
            tls_config: websocket_tls_config(),
        }
    }

    #[cfg(test)]
    pub(super) fn direct() -> Self {
        Self::new(None, None, None, None)
    }

    pub(super) fn uses_proxy_for(&self, websocket_url: &str) -> bool {
        let Ok(http_url) = to_http_upgrade_url(websocket_url) else {
            return true;
        };
        let Ok(destination) = http_url.parse::<http::Uri>() else {
            return true;
        };
        self.matcher.intercept(&destination).is_some()
    }

    pub(super) fn http_connect_route(
        &self,
        websocket_url: &str,
    ) -> Result<Option<WebSocketProxyRoute>, CodexError> {
        let http_url = to_http_upgrade_url(websocket_url).map_err(|error| CodexError {
            status: 0,
            message: error.message,
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        })?;
        if !http_url.starts_with("https://") {
            return Ok(None);
        }
        let destination = http_url.parse::<http::Uri>().map_err(|_| {
            websocket_protocol_error("WebSocket destination URL could not be routed")
        })?;
        let Some(proxy) = self.matcher.intercept(&destination) else {
            return Ok(None);
        };
        if !matches!(proxy.uri().scheme_str(), Some("http" | "https")) {
            return Ok(None);
        }
        Ok(Some(WebSocketProxyRoute {
            uri: proxy.uri().clone(),
            basic_auth: proxy.basic_auth().cloned(),
        }))
    }
}

static WEBSOCKET_TLS_CONFIG: once_cell::sync::Lazy<Arc<rustls::ClientConfig>> =
    once_cell::sync::Lazy::new(|| {
        let mut roots = rustls::RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        let load_error_count = native.errors.len();
        let (_, parse_error_count) = roots.add_parsable_certificates(native.certs);
        if load_error_count > 0 || parse_error_count > 0 {
            let mut fields = serde_json::Map::new();
            fields.insert("loadErrorCount".into(), serde_json::json!(load_error_count));
            fields.insert(
                "parseErrorCount".into(),
                serde_json::json!(parse_error_count),
            );
            create_logger("codex").warn("native_certificate_load_errors", Some(fields));
        }
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    });

pub(super) fn websocket_tls_config() -> Arc<rustls::ClientConfig> {
    WEBSOCKET_TLS_CONFIG.clone()
}

struct PrefixedIo {
    prefix: Vec<u8>,
    position: usize,
    inner: BoxedWebSocketIo,
}

impl AsyncRead for PrefixedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.position < this.prefix.len() {
            let available = &this.prefix[this.position..];
            let len = available.len().min(buffer.remaining());
            buffer.put_slice(&available[..len]);
            this.position += len;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buffer)
    }
}

impl AsyncWrite for PrefixedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

pub(super) fn skip_websocket_request_header(name: &http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "host"
            | "content-length"
            | "proxy-authorization"
    )
}

pub(super) fn tunneled_websocket_request(
    url: &str,
    headers: &HeaderMap,
    websocket_key: &str,
) -> Result<http::Request<()>, CodexError> {
    let mut request = url
        .into_client_request()
        .map_err(|_| websocket_protocol_error("WebSocket request URL was invalid"))?;
    *request.version_mut() = http::Version::HTTP_11;
    request.headers_mut().insert(
        http::header::SEC_WEBSOCKET_KEY,
        http::HeaderValue::from_str(websocket_key)
            .map_err(|_| websocket_protocol_error("WebSocket key was invalid"))?,
    );
    for (name, value) in headers {
        if !skip_websocket_request_header(name) {
            request.headers_mut().append(name.clone(), value.clone());
        }
    }
    Ok(request)
}

fn tunnel_error(status: u16, retry_after: Option<String>) -> CodexError {
    let proxy_auth_required = status == http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16();
    CodexError {
        status,
        message: if proxy_auth_required {
            "WebSocket proxy authentication failed".to_string()
        } else {
            "WebSocket proxy tunnel was rejected".to_string()
        },
        detail: if proxy_auth_required {
            Some(GENERIC_HANDSHAKE_ERROR_DETAIL.to_string())
        } else {
            Some(WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string())
        },
        retry_after,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

fn invalid_tunnel_response(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: message.to_string(),
        detail: Some(WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    }
}

pub(super) fn connect_response_header_end(response: &[u8]) -> Option<usize> {
    response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn parse_connect_response_head(response: &[u8]) -> Result<(u16, Option<String>), CodexError> {
    let status_line_end = response
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| invalid_tunnel_response("WebSocket proxy returned an invalid response"))?;
    let status_line = std::str::from_utf8(&response[..status_line_end])
        .map_err(|_| invalid_tunnel_response("WebSocket proxy returned an invalid response"))?;
    let mut parts = status_line.split_ascii_whitespace();
    let version = parts.next().unwrap_or_default();
    let status = parts.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || status.len() != 3
        || !status.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid_tunnel_response(
            "WebSocket proxy returned an invalid response",
        ));
    }
    let status = status
        .parse::<u16>()
        .map_err(|_| invalid_tunnel_response("WebSocket proxy returned an invalid response"))?;
    let retry_after = response[status_line_end + 2..]
        .split(|byte| *byte == b'\n')
        .find_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let separator = line.iter().position(|byte| *byte == b':')?;
            let (name, value) = line.split_at(separator);
            if !name.eq_ignore_ascii_case(b"retry-after") {
                return None;
            }
            std::str::from_utf8(&value[1..])
                .ok()
                .map(|value| value.trim().to_string())
        });
    Ok((status, retry_after))
}

pub(super) async fn establish_connect_tunnel(
    mut stream: BoxedWebSocketIo,
    authority: &str,
    basic_auth: Option<&http::HeaderValue>,
) -> Result<BoxedWebSocketIo, CodexError> {
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n").into_bytes();
    if let Some(auth) = basic_auth {
        request.extend_from_slice(b"Proxy-Authorization: ");
        request.extend_from_slice(auth.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    stream.write_all(&request).await.map_err(|_| CodexError {
        status: 0,
        message: "WebSocket proxy tunnel request failed".to_string(),
        detail: None,
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::WebSocketHandshake,
    })?;

    let mut response = Vec::new();
    loop {
        if let Some(header_end) = connect_response_header_end(&response) {
            let (status, retry_after) = parse_connect_response_head(&response[..header_end])?;
            if (100..200).contains(&status) {
                response.drain(..header_end);
                continue;
            }
            if (200..300).contains(&status) {
                let prefix = response.split_off(header_end);
                return Ok(Box::new(PrefixedIo {
                    prefix,
                    position: 0,
                    inner: stream,
                }));
            }
            return Err(tunnel_error(status, retry_after));
        }
        if response.len() == MAX_CONNECT_RESPONSE_HEADER_BYTES {
            return Err(invalid_tunnel_response(
                "WebSocket proxy response headers were too large",
            ));
        }
        let remaining = MAX_CONNECT_RESPONSE_HEADER_BYTES - response.len();
        let mut buffer = [0_u8; 1024];
        let capacity = remaining.min(buffer.len());
        let read = stream
            .read(&mut buffer[..capacity])
            .await
            .map_err(|_| invalid_tunnel_response("WebSocket proxy response could not be read"))?;
        if read == 0 {
            return Err(invalid_tunnel_response(
                "WebSocket proxy closed the tunnel response early",
            ));
        }
        response.extend_from_slice(&buffer[..read]);
    }
}

pub(super) async fn tls_connect(
    stream: BoxedWebSocketIo,
    host: &str,
    tls_config: Arc<rustls::ClientConfig>,
    peer: &str,
) -> Result<BoxedWebSocketIo, CodexError> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| websocket_protocol_error("WebSocket TLS host name was invalid"))?;
    let stream = tokio_rustls::TlsConnector::from(tls_config)
        .connect(server_name, stream)
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: format!("WebSocket TLS connection to {peer} failed"),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        })?;
    Ok(Box::new(stream))
}

pub(super) async fn connect_to_http_proxy(
    route: &WebSocketProxyRoute,
    tls_config: Arc<rustls::ClientConfig>,
) -> Result<BoxedWebSocketIo, CodexError> {
    let scheme = route.uri.scheme_str().unwrap_or_default();
    let host = route
        .uri
        .host()
        .ok_or_else(|| websocket_protocol_error("WebSocket proxy URL did not contain a host"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = route
        .uri
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|_| CodexError {
            status: 0,
            message: "WebSocket proxy connection failed".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: CodexErrorOrigin::WebSocketHandshake,
        })?;
    let stream: BoxedWebSocketIo = Box::new(stream);
    if scheme == "https" {
        tls_connect(stream, host, tls_config, "proxy").await
    } else {
        Ok(stream)
    }
}

pub(super) fn websocket_destination(url: &str) -> Result<(String, String), CodexError> {
    let destination = url::Url::parse(url)
        .map_err(|_| websocket_protocol_error("WebSocket destination URL was invalid"))?;
    let host = destination
        .host_str()
        .ok_or_else(|| websocket_protocol_error("WebSocket destination URL had no host"))?;
    let port = destination
        .port_or_known_default()
        .ok_or_else(|| websocket_protocol_error("WebSocket destination URL had no usable port"))?;
    let authority = match destination.host() {
        Some(url::Host::Ipv6(address)) => format!("[{address}]:{port}"),
        Some(_) => format!("{host}:{port}"),
        None => {
            return Err(websocket_protocol_error(
                "WebSocket destination URL had no host",
            ));
        }
    };
    Ok((host.to_string(), authority))
}

#[cfg(test)]
mod tests {
    use super::super::websocket_test_support::*;
    use super::*;

    #[tokio::test]
    async fn explicit_proxy_connect_403_has_private_tunnel_provenance() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let proxy = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while connect_response_header_end(&request).is_none() {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let proxy_url = format!("http://{proxy_addr}");
        let proxy_config = WebSocketProxyConfig::new(None, Some(&proxy_url), None, None);
        let client = test_websocket_client();

        let error = match connect_once(
            &client,
            &proxy_config,
            "wss://codex.invalid/backend-api/codex/responses",
            &HeaderMap::new(),
            &WebSocketConnectGate::new(std::time::Duration::ZERO),
            std::time::Duration::ZERO,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        {
            Ok(_) => panic!("proxy CONNECT rejection should fail"),
            Err(error) => error,
        };

        match error {
            ConnectAttemptError::ProxyTunnel(error) => {
                assert_eq!(error.status, http::StatusCode::FORBIDDEN.as_u16());
                assert_eq!(
                    error.detail.as_deref(),
                    Some(WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL)
                );
                assert_eq!(error.origin, CodexErrorOrigin::WebSocketHandshake);
            }
            ConnectAttemptError::Origin(_) => panic!("proxy rejection was classified as origin"),
        }
        proxy.await.unwrap();
    }

    #[test]
    fn websocket_tls_configuration_is_shared() {
        let first = websocket_tls_config();
        let second = websocket_tls_config();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn websocket_connects_through_explicit_http_proxy() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
        let proxy = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            let request_text = String::from_utf8(request).unwrap();
            let key = request_text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim().to_string())
                })
                .unwrap();
            let _ = captured_tx.send(request_text);
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                derive_accept_key(key.as_bytes())
            );
            stream.write_all(response.as_bytes()).await.unwrap();

            let mut websocket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
            assert_eq!(
                websocket.next().await.unwrap().unwrap(),
                Message::text("hello")
            );
            websocket
                .send(Message::text("proxy-ok".to_string()))
                .await
                .unwrap();
        });

        let client = crate::tls::client_builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .proxy(
                reqwest::Proxy::http(format!("http://proxy-user:proxy-pass@{proxy_addr}")).unwrap(),
            )
            .build()
            .unwrap();
        let mut websocket = connect_with_timeout(
            &client,
            &WebSocketProxyConfig::direct(),
            "ws://codex.invalid/backend-api/codex/responses",
            &HeaderMap::new(),
            2_000,
            &super::super::recovery::RecoveryBudget::default(),
        )
        .await
        .unwrap();
        websocket
            .send(Message::text("hello".to_string()))
            .await
            .unwrap();
        assert_eq!(
            websocket.next().await.unwrap().unwrap(),
            Message::text("proxy-ok")
        );

        let captured = captured_rx.await.unwrap();
        assert!(
            captured.starts_with("GET http://codex.invalid/backend-api/codex/responses HTTP/1.1")
        );
        assert!(
            captured
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic ")
        );
        proxy.await.unwrap();
    }

    #[tokio::test]
    async fn connect_tunnel_accepts_fragmented_non_200_success() {
        let (client, mut proxy) = tokio::io::duplex(4096);
        let proxy_task = tokio::spawn(async move {
            let mut request = Vec::new();
            let mut buffer = [0_u8; 256];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = proxy.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            assert!(request.starts_with(b"CONNECT codex.invalid:4443 HTTP/1.1\r\n"));
            proxy.write_all(b"HTTP/1.").await.unwrap();
            tokio::task::yield_now().await;
            proxy
                .write_all(b"1 204 No Content\r\nX-Proxy: ok\r\n\r\nprefixed")
                .await
                .unwrap();
        });

        let stream: BoxedWebSocketIo = Box::new(client);
        let mut stream = establish_connect_tunnel(stream, "codex.invalid:4443", None)
            .await
            .unwrap();
        let mut prefix = [0_u8; 8];
        stream.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"prefixed");
        proxy_task.await.unwrap();
    }

    #[tokio::test]
    async fn connect_tunnel_classifies_fragmented_proxy_authentication() {
        let (client, mut proxy) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut request = [0_u8; 512];
            let _ = proxy.read(&mut request).await.unwrap();
            proxy.write_all(b"HTTP/1.1 4").await.unwrap();
            tokio::task::yield_now().await;
            proxy
                .write_all(b"07 Proxy Authentication Required\r\n\r\n")
                .await
                .unwrap();
        });

        let stream: BoxedWebSocketIo = Box::new(client);
        let error = match establish_connect_tunnel(stream, "codex.invalid:4443", None).await {
            Ok(_) => panic!("proxy authentication should be rejected"),
            Err(error) => error,
        };
        assert_eq!(
            error.status,
            http::StatusCode::PROXY_AUTHENTICATION_REQUIRED.as_u16()
        );
        assert_eq!(error.message, "WebSocket proxy authentication failed");
    }

    #[test]
    fn websocket_proxy_routing_honors_no_proxy_and_leaves_socks_to_reqwest() {
        let http_proxy = "http://proxy.example:8080";
        let config = WebSocketProxyConfig::new(None, Some(http_proxy), None, None);
        assert!(config.uses_proxy_for("wss://codex.invalid/responses"));
        assert!(
            config
                .http_connect_route("wss://codex.invalid/responses")
                .unwrap()
                .is_some()
        );

        let bypass = WebSocketProxyConfig::new(None, Some(http_proxy), None, Some("codex.invalid"));
        assert!(!bypass.uses_proxy_for("wss://codex.invalid/responses"));
        assert!(
            bypass
                .http_connect_route("wss://codex.invalid/responses")
                .unwrap()
                .is_none()
        );

        let socks =
            WebSocketProxyConfig::new(None, Some("socks5h://proxy.example:1080"), None, None);
        assert!(socks.uses_proxy_for("wss://codex.invalid/responses"));
        assert!(
            socks
                .http_connect_route("wss://codex.invalid/responses")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn websocket_wss_uses_http_connect_without_leaking_proxy_credentials() {
        let _pool_test_guard = lock_codex_websocket_pool_for_tests().await;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
        let proxy = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            let _ = captured_tx.send(String::from_utf8(request).unwrap());
            stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let proxy_url = format!("http://secret-user:secret-pass@{proxy_addr}");
        let client = crate::tls::client_builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .proxy(reqwest::Proxy::https(&proxy_url).unwrap())
            .build()
            .unwrap();
        let proxy_config = WebSocketProxyConfig::new(None, Some(&proxy_url), None, None);
        let error = match connect_with_timeout(
            &client,
            &proxy_config,
            "wss://codex.invalid:4443/backend-api/codex/responses",
            &HeaderMap::new(),
            2_000,
            &super::super::recovery::RecoveryBudget::default(),
        )
        .await
        {
            Ok(_) => panic!("proxy rejection should fail the WebSocket connection"),
            Err(error) => error,
        };

        let captured = captured_rx.await.unwrap();
        assert!(captured.starts_with("CONNECT codex.invalid:4443 HTTP/1.1"));
        assert!(
            captured
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic ")
        );
        assert!(!error.message.contains("secret-user"));
        assert!(!error.message.contains("secret-pass"));
        assert!(
            !error
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("secret")
        );
        proxy.await.unwrap();
    }
}
