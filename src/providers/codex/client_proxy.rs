use std::time::Duration;

use super::client::*;

#[derive(Clone)]
pub(super) struct ProxyEnvironment {
    http_proxy: Option<String>,
    https_proxy: Option<String>,
    all_proxy: Option<String>,
    no_proxy: Option<reqwest::NoProxy>,
    no_proxy_value: Option<String>,
}

impl ProxyEnvironment {
    pub(super) fn from_env() -> Self {
        if std::env::var_os("REQUEST_METHOD").is_some() {
            return Self {
                http_proxy: None,
                https_proxy: None,
                all_proxy: None,
                no_proxy: None,
                no_proxy_value: None,
            };
        }

        let no_proxy_value = match std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy"))
        {
            Ok(value) if !value.trim().is_empty() => format!("{value},{LOOPBACK_NO_PROXY}"),
            _ => LOOPBACK_NO_PROXY.to_string(),
        };
        Self {
            http_proxy: proxy_env_value("HTTP_PROXY", "http_proxy")
                .unwrap_or_else(|name| panic!("invalid {name} proxy URL")),
            https_proxy: proxy_env_value("HTTPS_PROXY", "https_proxy")
                .unwrap_or_else(|name| panic!("invalid {name} proxy URL")),
            all_proxy: proxy_env_value("ALL_PROXY", "all_proxy")
                .unwrap_or_else(|name| panic!("invalid {name} proxy URL")),
            no_proxy: reqwest::NoProxy::from_string(&no_proxy_value),
            no_proxy_value: Some(no_proxy_value),
        }
    }

    pub(super) fn websocket_proxy_config(&self) -> super::websocket::WebSocketProxyConfig {
        super::websocket::WebSocketProxyConfig::new(
            self.http_proxy.as_deref(),
            self.https_proxy.as_deref(),
            self.all_proxy.as_deref(),
            self.no_proxy_value.as_deref(),
        )
    }

    fn apply(&self, mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        builder = builder.no_proxy();
        if let Some(proxy) = self.http_proxy.as_deref() {
            builder = builder.proxy(
                reqwest::Proxy::http(proxy)
                    .expect("validated HTTP_PROXY URL")
                    .no_proxy(self.no_proxy.clone()),
            );
        }
        if let Some(proxy) = self.https_proxy.as_deref() {
            builder = builder.proxy(
                reqwest::Proxy::https(proxy)
                    .expect("validated HTTPS_PROXY URL")
                    .no_proxy(self.no_proxy.clone()),
            );
        }
        if let Some(proxy) = self.all_proxy.as_deref() {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .expect("validated ALL_PROXY URL")
                    .no_proxy(self.no_proxy.clone()),
            );
        }
        builder
    }
}

pub(super) fn codex_http_client(proxy_environment: &ProxyEnvironment) -> reqwest::Client {
    proxy_environment
        .apply(
            crate::tls::client_builder()
                .connect_timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none()),
        )
        .build()
        .expect("failed to create Codex HTTP client")
}

fn proxy_env_value(
    uppercase: &'static str,
    lowercase: &'static str,
) -> Result<Option<String>, &'static str> {
    let Some(raw) = std::env::var_os(uppercase).or_else(|| std::env::var_os(lowercase)) else {
        return Ok(None);
    };
    let raw = raw.into_string().map_err(|_| uppercase)?;
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    normalize_proxy_url(raw).map(Some).ok_or(uppercase)
}

fn normalize_proxy_url(raw: &str) -> Option<String> {
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let parsed = url::Url::parse(&candidate).ok()?;
    if !matches!(
        parsed.scheme(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) || parsed.host_str().is_none()
        || matches!(parsed.scheme(), "socks4" | "socks4a")
            && (!parsed.username().is_empty() || parsed.password().is_some())
    {
        return None;
    }
    Some(parsed.to_string())
}

pub(super) fn websocket_http_client(proxy_environment: &ProxyEnvironment) -> reqwest::Client {
    let tls_config = super::websocket::websocket_tls_config();
    proxy_environment
        .apply(
            crate::tls::client_builder()
                .http1_only()
                .connect_timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .use_preconfigured_tls((*tls_config).clone()),
        )
        .build()
        .expect("failed to create Codex WebSocket HTTP client")
}

pub(super) fn custom_client_auto_http_fallback_enabled(
    base_url: &str,
    proxy_config: &super::websocket::WebSocketProxyConfig,
) -> bool {
    let Ok(websocket_url) = super::websocket::to_websocket_url(base_url) else {
        return false;
    };
    !proxy_config.uses_proxy_for(&websocket_url)
}

#[cfg(test)]
pub(super) fn test_native_http_client() -> reqwest::Client {
    crate::tls::client_builder()
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("failed to create test native Responses HTTP client")
}

#[cfg(test)]
pub(super) fn test_websocket_http_client() -> reqwest::Client {
    crate::tls::client_builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("failed to create test WebSocket HTTP client")
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use super::super::client_test_support::*;
    use super::*;

    #[test]
    fn normalizes_supported_proxy_urls() {
        assert_eq!(
            normalize_proxy_url("127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080/")
        );
        assert_eq!(
            normalize_proxy_url("https://user:pass@proxy.example:8443").as_deref(),
            Some("https://user:pass@proxy.example:8443/")
        );
        for scheme in ["socks4", "socks4a"] {
            let proxy = format!("{scheme}://proxy.example:1080");
            assert_eq!(normalize_proxy_url(&proxy), Some(proxy));
        }
        for scheme in ["socks5", "socks5h"] {
            let proxy = format!("{scheme}://user:pass@proxy.example:1080");
            assert_eq!(normalize_proxy_url(&proxy), Some(proxy));
        }
    }

    #[test]
    fn rejects_malformed_or_unsupported_proxy_urls() {
        assert!(normalize_proxy_url("http://").is_none());
        assert!(normalize_proxy_url("ftp://proxy.example:21").is_none());
        assert!(normalize_proxy_url("socks4://user@proxy.example:1080").is_none());
        assert!(normalize_proxy_url("socks4a://user:pass@proxy.example:1080").is_none());
    }

    #[test]
    fn custom_client_auto_fallback_tracks_effective_proxy_route() {
        let proxy = "http://proxy.example:8080";
        let proxied =
            super::super::websocket::WebSocketProxyConfig::new(None, Some(proxy), None, None);
        assert!(!custom_client_auto_http_fallback_enabled(
            "https://codex.invalid/responses",
            &proxied
        ));

        let bypassed = super::super::websocket::WebSocketProxyConfig::new(
            None,
            Some(proxy),
            None,
            Some("codex.invalid"),
        );
        assert!(custom_client_auto_http_fallback_enabled(
            "https://codex.invalid/responses",
            &bypassed
        ));
    }

    #[tokio::test]
    async fn codex_http_client_refuses_to_follow_redirects() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://{target_addr}/collect\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let direct = ProxyEnvironment {
            http_proxy: None,
            https_proxy: None,
            all_proxy: None,
            no_proxy: None,
            no_proxy_value: None,
        };
        let client = CodexHttpClient::new_for_test(
            codex_http_client(&direct),
            format!("http://{addr}/responses"),
            1_000,
            1_000,
        );
        client.auth_manager().set_test_auth(http_test_auth());

        let result = client
            .post_codex_with_transport(
                &buffered_test_request(),
                &http_test_context(),
                None,
                crate::config::CodexTransport::Http,
            )
            .await;
        server.await.unwrap();
        let Err(error) = result else {
            panic!("a redirect must not be followed");
        };
        assert_eq!(error.status, 307);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), target.accept())
                .await
                .is_err(),
            "the redirect target must not be contacted"
        );
    }
}
