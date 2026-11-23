use std::convert::Infallible;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, header};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::pki_types::CertificateDer;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use super::certs::Pair;
use super::relay::Relay;
use super::renewal;
use super::tunnel::{self, Refusal};
use super::{INTERCEPTED_HOST, INTERCEPTED_PORT, PROXY_USER, dispatch, set_keepalive};
use crate::anthropic::json_error;
use crate::auth::constant_time_eq;

const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_secs(1);
const MAX_UNAUTHENTICATED: usize = 64;
const MAIN_SERVER_RESERVE: usize = 128;
const MAX_FREE_DESCRIPTORS: u64 = 2880;
const MAX_INTERCEPTED: usize = 512;
const MAX_TUNNELS: usize = 2048;

fn pool_sizes(soft_limit: Option<u64>) -> (usize, usize) {
    match soft_limit {
        None => (MAX_INTERCEPTED, MAX_TUNNELS),
        Some(limit) => {
            let rest = limit.saturating_sub((MAX_UNAUTHENTICATED + MAIN_SERVER_RESERVE) as u64);
            let free = (u128::from(rest) * 3 / 4).min(u128::from(MAX_FREE_DESCRIPTORS)) as u64;
            let budget = rest - free;
            (
                (budget / 6).min(MAX_INTERCEPTED as u64) as usize,
                (budget / 3).min(MAX_TUNNELS as u64) as usize,
            )
        }
    }
}

pub struct InterceptListener {
    listener: TcpListener,
    credentials: String,
    tls: TlsAcceptor,
    relay: Relay,
    ca: CertificateDer<'static>,
    leaf: CertificateDer<'static>,
    notices: Vec<&'static str>,
}

struct Shared {
    credentials: String,
    tls: TlsAcceptor,
    relay: Relay,
    app: Router,
    intercepted: Arc<Semaphore>,
    tunnels: Arc<Semaphore>,
}

enum Route {
    Intercept,
    Tunnel(TcpStream),
}

struct Handoff {
    upgrade: OnUpgrade,
    route: Route,
    _permit: OwnedSemaphorePermit,
}

type Slot<T> = Arc<Mutex<Option<T>>>;

impl InterceptListener {
    pub async fn bind(port: u16) -> anyhow::Result<Self> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = TcpListener::bind(address).await.map_err(|error| {
            anyhow::anyhow!("failed to bind the intercept listener on {address}: {error}")
        })?;
        let (password, loaded) =
            tokio::task::spawn_blocking(|| super::prepare(time::OffsetDateTime::now_utc()))
                .await??;
        renewal::report_replaced(&loaded);
        let due = renewal::due(
            std::slice::from_ref(&loaded.ca),
            &loaded.pair.leaf,
            time::OffsetDateTime::now_utc(),
        );
        let notices = renewal::notices(due, loaded.replaced);
        let leaf = loaded.pair.leaf.clone();
        let config = server_config(loaded.pair)?;
        Ok(Self {
            listener,
            credentials: format!("{PROXY_USER}:{password}"),
            tls: TlsAcceptor::from(Arc::new(config)),
            relay: Relay::new()?,
            ca: loaded.ca,
            leaf,
            notices,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn notices(&self) -> &[&'static str] {
        &self.notices
    }

    pub async fn serve(self, app: Router, shutdown: impl Future<Output = ()>) {
        let Self {
            listener,
            credentials,
            tls,
            relay,
            ca,
            leaf,
            notices: _,
        } = self;
        let watching = tokio::spawn(renewal::watch(ca, leaf));
        #[cfg(unix)]
        let soft_limit = rustix::process::getrlimit(rustix::process::Resource::Nofile).current;
        #[cfg(not(unix))]
        let soft_limit = None;
        let (intercepted, tunnels) = pool_sizes(soft_limit);
        let shared = Arc::new(Shared {
            credentials,
            tls,
            relay,
            app,
            intercepted: Arc::new(Semaphore::new(intercepted)),
            tunnels: Arc::new(Semaphore::new(tunnels)),
        });
        let unauthenticated = Arc::new(Semaphore::new(MAX_UNAUTHENTICATED));
        let (stop, stopped) = watch::channel(false);
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                Some(_) = connections.join_next() => {}
                accepted = accept(&listener, &unauthenticated) => {
                    if let Some((stream, permit)) = accepted {
                        connections.spawn(connection(stream, shared.clone(), permit, stopped.clone()));
                    }
                }
            }
        }
        drop(listener);
        watching.abort();
        let _ = stop.send(true);
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        connections.shutdown().await;
    }
}

fn server_config(pair: Pair) -> anyhow::Result<rustls::ServerConfig> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![pair.leaf], pair.key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

async fn accept(
    listener: &TcpListener,
    permits: &Arc<Semaphore>,
) -> Option<(TcpStream, OwnedSemaphorePermit)> {
    let permit = permits.clone().acquire_owned().await.ok()?;
    match listener.accept().await {
        Ok((stream, _)) => Some((stream, permit)),
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

async fn connection(
    stream: TcpStream,
    shared: Arc<Shared>,
    permit: OwnedSemaphorePermit,
    mut stopped: watch::Receiver<bool>,
) {
    set_keepalive(&stream);
    let unauthenticated: Slot<OwnedSemaphorePermit> = Arc::new(Mutex::new(Some(permit)));
    let (sender, mut receiver) = oneshot::channel();
    let sender: Slot<oneshot::Sender<Handoff>> = Arc::new(Mutex::new(Some(sender)));
    let service = {
        let shared = shared.clone();
        service_fn(move |request| {
            let shared = shared.clone();
            let unauthenticated = unauthenticated.clone();
            let sender = sender.clone();
            async move {
                let mut response =
                    handle_connect(request, &shared, &unauthenticated, &sender).await;
                if !response.status().is_success() {
                    response
                        .headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("close"));
                }
                Ok::<_, Infallible>(response)
            }
        })
    };
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEAD_TIMEOUT);
    let connection = builder
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    tokio::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => {}
        () = stop_requested(&mut stopped) => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.as_mut().await;
        }
    }
    let Ok(handoff) = receiver.try_recv() else {
        return;
    };
    let Handoff {
        upgrade,
        route,
        _permit,
    } = handoff;
    match route {
        Route::Intercept => intercepted(upgrade, shared, stopped).await,
        Route::Tunnel(upstream) => {
            if let Ok(upgraded) = upgrade.await {
                tunnel::splice(TokioIo::new(upgraded), upstream).await;
            }
        }
    }
}

async fn handle_connect(
    mut request: Request<Incoming>,
    shared: &Shared,
    unauthenticated: &Slot<OwnedSemaphorePermit>,
    sender: &Slot<oneshot::Sender<Handoff>>,
) -> Response {
    if request.method() != Method::CONNECT {
        return json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "invalid_request_error",
            "The intercept proxy accepts only CONNECT",
        );
    }
    if !authorized(request.headers(), &shared.credentials) {
        let mut response = json_error(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED,
            "authentication_error",
            "CONNECT requires the duolane proxy password",
        );
        response.headers_mut().insert(
            header::PROXY_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"duolane\""),
        );
        return response;
    }
    let Some((host, port)) = authority_target(request.uri()) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "CONNECT needs a host:port target",
        );
    };
    let intercept = host.eq_ignore_ascii_case(INTERCEPTED_HOST) && port == INTERCEPTED_PORT;
    let pool = if intercept {
        &shared.intercepted
    } else {
        &shared.tunnels
    };
    let Ok(permit) = pool.clone().try_acquire_owned() else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "The intercept proxy has too many open connections",
        );
    };
    drop(take(unauthenticated));
    let route = if intercept {
        Route::Intercept
    } else {
        match tunnel::connect(&host, port).await {
            Ok(upstream) => Route::Tunnel(upstream),
            Err(Refusal::Local) => {
                return json_error(
                    StatusCode::FORBIDDEN,
                    "permission_error",
                    "CONNECT to a loopback or unspecified address is refused",
                );
            }
            Err(Refusal::Unreachable) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "The CONNECT target could not be reached",
                );
            }
        }
    };
    let Some(sender) = take(sender) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "This connection already carries a CONNECT",
        );
    };
    let handoff = Handoff {
        upgrade: hyper::upgrade::on(&mut request),
        route,
        _permit: permit,
    };
    if sender.send(handoff).is_err() {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "The intercept proxy is shutting down",
        );
    }
    Response::new(Body::empty())
}

async fn stop_requested(stopped: &mut watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stop| *stop).await;
}

fn take<T>(slot: &Slot<T>) -> Option<T> {
    slot.lock().ok().and_then(|mut value| value.take())
}

fn authorized(headers: &HeaderMap, credentials: &str) -> bool {
    let mut values = headers.get_all(header::PROXY_AUTHORIZATION).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    let Some((scheme, token)) = value
        .to_str()
        .ok()
        .and_then(|value| value.trim().split_once(' '))
    else {
        return false;
    };
    scheme.eq_ignore_ascii_case("basic")
        && STANDARD
            .decode(token.trim())
            .ok()
            .and_then(|decoded| String::from_utf8(decoded).ok())
            .is_some_and(|decoded| constant_time_eq(&decoded, credentials))
}

fn authority_target(uri: &Uri) -> Option<(String, u16)> {
    if uri.scheme().is_some() || uri.path_and_query().is_some() {
        return None;
    }
    let authority = uri.authority()?;
    let port = authority.port_u16()?;
    let host = authority.host();
    (!host.is_empty() && !authority.as_str().contains('@')).then(|| (host.to_string(), port))
}

async fn intercepted(upgrade: OnUpgrade, shared: Arc<Shared>, mut stopped: watch::Receiver<bool>) {
    let Ok(upgraded) = upgrade.await else {
        return;
    };
    let Ok(Ok(tls)) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, shared.tls.accept(TokioIo::new(upgraded))).await
    else {
        return;
    };
    let service = service_fn(move |request| {
        let shared = shared.clone();
        async move { Ok::<_, Infallible>(dispatch::dispatch(request, &shared.app, &shared.relay).await) }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEAD_TIMEOUT);
    let connection = builder.serve_connection(TokioIo::new(tls), service);
    tokio::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => {}
        () = stop_requested(&mut stopped) => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.as_mut().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pool_sizes;

    #[test]
    fn pools_follow_soft_open_file_limit() {
        assert_eq!(pool_sizes(None), (512, 2048));
        assert_eq!(pool_sizes(Some(u64::MAX)), (512, 2048));
        assert_eq!(pool_sizes(Some(16384)), (512, 2048));
        assert_eq!(pool_sizes(Some(4096)), (170, 341));
        assert_eq!(pool_sizes(Some(256)), (2, 5));
        assert_eq!(pool_sizes(Some(128)), (0, 0));
    }
}
