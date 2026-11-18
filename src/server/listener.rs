use super::app_with_monitor;
use crate::{logging::create_logger, monitor::MonitorHandle, registry::Registry};
use axum::{Router, http::Request};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::{conn::auto::Builder, graceful::GracefulShutdown},
    service::TowerToHyperService,
};
use serde_json::json;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::ServiceExt;

const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OPEN_CONNECTIONS: usize = 128;
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_secs(1);

pub struct ServerConfig {
    pub bind_address: String,
    pub port: u16,
    pub monitor: Option<MonitorHandle>,
}

pub async fn serve(config: ServerConfig) -> anyhow::Result<()> {
    serve_inner(config, std::future::pending::<()>()).await
}

pub async fn serve_with_shutdown(
    config: ServerConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    serve_inner(config, shutdown).await
}

async fn serve_inner(
    config: ServerConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = bind_proxy_listener(&config.bind_address, config.port).await?;
    serve_listener(listener, config.monitor, shutdown).await
}

pub async fn bind_proxy_listener(bind_address: &str, port: u16) -> anyhow::Result<TcpListener> {
    let ip = bind_address
        .parse::<std::net::IpAddr>()
        .map_err(|err| anyhow::anyhow!("invalid proxy bind address {bind_address:?}: {err}"))?;
    let addr = std::net::SocketAddr::new(ip, port);
    TcpListener::bind(addr)
        .await
        .map_err(|err| anyhow::anyhow!("failed to bind proxy listener on {addr}: {err}"))
}

pub async fn serve_listener(
    listener: TcpListener,
    monitor: Option<MonitorHandle>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let local_addr = listener.local_addr()?;
    let policy = crate::inbound_auth::InboundPolicy::from_config(local_addr.ip())?;
    crate::upstream_url::check_upstream_urls()?;
    create_logger("server").info(
        "server listening",
        Some(serde_json::Map::from_iter([
            ("port".to_string(), json!(local_addr.port())),
            (
                "bindAddress".to_string(),
                json!(local_addr.ip().to_string()),
            ),
            (
                "logDir".to_string(),
                json!(
                    crate::paths::log_file()
                        .parent()
                        .map(|path| path.display().to_string())
                ),
            ),
        ])),
    );
    let app = crate::inbound_auth::guard(
        app_with_monitor(Arc::new(Registry::with_default_alias()), monitor),
        policy,
    );
    serve_connections(listener, app, shutdown, HEADER_READ_TIMEOUT).await;
    Ok(())
}

async fn serve_connections(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    header_read_timeout: Duration,
) {
    let permits = Arc::new(Semaphore::new(MAX_OPEN_CONNECTIONS));
    let graceful = GracefulShutdown::new();
    let mut builder = Builder::new(TokioExecutor::new()).http1_only();
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    tokio::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            accepted = accept_connection(&listener, &permits) => accepted,
        };
        let Some((stream, peer, permit)) = accepted else {
            continue;
        };
        let service = app.clone().map_request(move |mut request: Request<_>| {
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));
            request
        });
        let connection = builder
            .serve_connection(TokioIo::new(stream), TowerToHyperService::new(service))
            .into_owned();
        let connection = graceful.watch(connection);
        tokio::spawn(async move {
            let _ = connection.await;
            drop(permit);
        });
    }
    drop(listener);
    graceful.shutdown().await;
}

async fn accept_connection(
    listener: &TcpListener,
    permits: &Arc<Semaphore>,
) -> Option<(TcpStream, std::net::SocketAddr, OwnedSemaphorePermit)> {
    let permit = permits.clone().acquire_owned().await.ok()?;
    match listener.accept().await {
        Ok((stream, peer)) => Some((stream, peer, permit)),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            None
        }
        Err(_) => {
            tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
            None
        }
    }
}

#[cfg(test)]
mod listener_tests {
    use super::serve_connections;
    use axum::Router;
    use std::time::{Duration, Instant};
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn silent_connection_is_closed_after_the_header_read_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(serve_connections(
            listener,
            Router::new(),
            std::future::pending(),
            Duration::from_millis(200),
        ));

        let mut socket = TcpStream::connect(address).await.unwrap();
        let started = Instant::now();
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut received))
            .await
            .expect("a silent connection must be closed")
            .ok();
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
