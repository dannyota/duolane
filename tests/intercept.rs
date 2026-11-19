#[path = "support/hermetic.rs"]
mod hermetic;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use duolane::intercept::{InterceptListener, intercept_dir};
use duolane::monitor::MonitorHandle;
use duolane::server::{bind_proxy_listener, serve_listeners};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};

const HOST: &str = "api.anthropic.com";
const LOGIN: &str = "Bearer toy-claude-login";
const TOY_KEY: &str = "toy-inbound-key";
const WAIT: Duration = Duration::from_secs(15);

type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], StatusCode);

struct Seen {
    method: Method,
    target: String,
    headers: HeaderMap,
}

#[derive(Clone, Default)]
struct Upstream {
    seen: Arc<Mutex<Vec<Seen>>>,
    release: Arc<Notify>,
}

impl Upstream {
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

struct Proxy {
    address: SocketAddr,
    upstream: SocketAddr,
    password: String,
    ca: Vec<u8>,
    _stop: oneshot::Sender<()>,
}

async fn capture(
    State(upstream): State<Upstream>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Json<Value> {
    upstream.seen.lock().unwrap().push(Seen {
        method,
        target: uri.to_string(),
        headers,
    });
    Json(json!({
        "id": "msg_toy",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-5-5",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
}

async fn slow_stream(State(upstream): State<Upstream>) -> Body {
    let release = upstream.release.clone();
    Body::from_stream(futures_util::stream::unfold(0, move |step| {
        let release = release.clone();
        async move {
            match step {
                0 => Some((Ok::<_, Infallible>(Bytes::from_static(b"first")), 1)),
                1 => {
                    release.notified().await;
                    Some((Ok(Bytes::from_static(b"second")), 2))
                }
                _ => None,
            }
        }
    }))
}

async fn start() -> (Proxy, Upstream) {
    let upstream = Upstream::default();
    let router = Router::new()
        .route("/v1/stream", get(slow_stream))
        .fallback(capture)
        .with_state(upstream.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    unsafe {
        std::env::set_var(
            "DUOLANE_ANTHROPIC_BASE_URL",
            format!("http://{upstream_address}"),
        );
        std::env::set_var("DUOLANE_CODEX_BASE_URL", "http://127.0.0.1:9/responses");
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    }
    let gateway = bind_proxy_listener("127.0.0.1", 0).await.unwrap();
    let intercept = InterceptListener::bind(0).await.unwrap();
    let address = intercept.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve_listeners(
        gateway,
        Some(intercept),
        Some(MonitorHandle::default()),
        async {
            let _ = stopped.await;
        },
    ));
    let dir = intercept_dir();
    let proxy = Proxy {
        address,
        upstream: upstream_address,
        password: std::fs::read_to_string(dir.join("proxy-password")).unwrap(),
        ca: std::fs::read(dir.join("ca.pem")).unwrap(),
        _stop: stop,
    };
    (proxy, upstream)
}

async fn connect(proxy: &Proxy, target: &str, credentials: Option<&str>) -> (u16, TcpStream) {
    let mut stream = TcpStream::connect(proxy.address).await.unwrap();
    let mut head = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(credentials) = credentials {
        let encoded = STANDARD.encode(credentials);
        head.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut received = Vec::new();
    while !received.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        let read = tokio::time::timeout(WAIT, stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, 1, "the proxy closed the connection");
        received.push(byte[0]);
    }
    let head = String::from_utf8(received).unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    if status == 407 {
        assert!(
            head.to_ascii_lowercase()
                .contains("proxy-authenticate: basic")
        );
    }
    (status, stream)
}

async fn open(proxy: &Proxy) -> SendRequest<Full<Bytes>> {
    let credentials = format!("duolane:{}", proxy.password);
    let (status, stream) = connect(proxy, &format!("{HOST}:443"), Some(&credentials)).await;
    assert_eq!(status, 200);
    let mut roots = rustls::RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(&proxy.ca) {
        roots.add(certificate.unwrap()).unwrap();
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from(HOST).unwrap(), stream)
        .await
        .unwrap();
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(connection);
    sender
}

fn request(
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Request<Full<Bytes>> {
    let mut builder = Request::builder().method(method).uri(target);
    if !headers.iter().any(|(name, _)| *name == "host") {
        builder = builder.header("host", HOST);
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

async fn send(proxy: &Proxy, request: Request<Full<Bytes>>) -> StatusCode {
    let mut sender = open(proxy).await;
    let response = tokio::time::timeout(WAIT, sender.send_request(request))
        .await
        .unwrap()
        .unwrap();
    let status = response.status();
    let _ = tokio::time::timeout(WAIT, response.into_body().collect()).await;
    status
}

#[tokio::test]
async fn connect_requires_the_proxy_password() {
    let (proxy, _) = start().await;
    let target = format!("{HOST}:443");
    assert_eq!(connect(&proxy, &target, None).await.0, 407);
    assert_eq!(connect(&proxy, &target, Some("duolane:wrong")).await.0, 407);
    let credentials = format!("duolane:{}", proxy.password);
    assert_eq!(connect(&proxy, &target, Some(&credentials)).await.0, 200);
}

#[tokio::test]
async fn connect_to_a_loopback_target_is_refused() {
    let (proxy, upstream) = start().await;
    let target = proxy.upstream.to_string();
    let credentials = format!("duolane:{}", proxy.password);
    assert_eq!(connect(&proxy, &target, Some(&credentials)).await.0, 403);
    assert_eq!(upstream.count(), 0);
}

#[tokio::test]
async fn relayed_request_keeps_path_query_and_login_and_drops_the_proxy_password() {
    let (proxy, upstream) = start().await;
    let headers = [
        ("authorization", LOGIN),
        ("proxy-authorization", "Basic dG95OnRveQ=="),
    ];
    let status = send(
        &proxy,
        request("GET", "/v1/code/sessions/toy?wait=true", &headers, ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, Method::GET);
    assert_eq!(seen[0].target, "/v1/code/sessions/toy?wait=true");
    assert_eq!(seen[0].headers["authorization"], LOGIN);
    assert!(!seen[0].headers.contains_key("proxy-authorization"));
}

#[tokio::test]
async fn relayed_response_streams_chunks_as_they_arrive() {
    let (proxy, upstream) = start().await;
    let mut sender = open(&proxy).await;
    let response = tokio::time::timeout(
        WAIT,
        sender.send_request(request("GET", "/v1/stream", &[], "")),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let first = tokio::time::timeout(WAIT, body.frame())
        .await
        .expect("the first chunk must arrive before the upstream finishes")
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap(), Bytes::from_static(b"first"));
    upstream.release.notify_one();
    let rest = tokio::time::timeout(WAIT, body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert_eq!(rest, Bytes::from_static(b"second"));
}

#[tokio::test]
async fn message_requests_take_the_model_route_without_the_inbound_token() {
    let (proxy, upstream) = start().await;
    let headers = [
        ("content-type", "application/json"),
        ("authorization", LOGIN),
        ("x-duolane-key", TOY_KEY),
    ];
    let claude = json!({
        "model": "claude-haiku-5-5",
        "max_tokens": 8,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let status = send(
        &proxy,
        request(
            "POST",
            "/v1/messages?beta=true",
            &headers,
            &claude.to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    {
        let seen = upstream.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].target, "/v1/messages?beta=true");
        assert_eq!(seen[0].headers["authorization"], LOGIN);
        assert!(!seen[0].headers.contains_key("x-duolane-key"));
    }

    let codex = json!({
        "model": "gpt-5.6-terra",
        "max_tokens": 8,
        "messages": [{"role": "user", "content": "hi"}]
    });
    send(
        &proxy,
        request(
            "POST",
            "/v1/messages?beta=true",
            &headers,
            &codex.to_string(),
        ),
    )
    .await;
    assert_eq!(upstream.count(), 1);
}

#[tokio::test]
async fn dispatcher_refuses_what_it_must_not_relay() {
    let (proxy, upstream) = start().await;
    let json_type = ("content-type", "application/json");
    let cases: [Case; 6] = [
        (
            "GET",
            "/v1/models",
            &[("host", "evil.example")],
            StatusCode::MISDIRECTED_REQUEST,
        ),
        (
            "POST",
            "//v1/messages",
            &[json_type],
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            "/v1/%6Dessages",
            &[json_type],
            StatusCode::BAD_REQUEST,
        ),
        (
            "GET",
            "/v1/code/sessions",
            &[("upgrade", "websocket"), ("connection", "upgrade")],
            StatusCode::NOT_IMPLEMENTED,
        ),
        (
            "POST",
            "/v1/messages",
            &[json_type, ("content-encoding", "gzip")],
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            "GET",
            "/v1/models",
            &[("x-duolane-key", TOY_KEY)],
            StatusCode::OK,
        ),
    ];
    for (method, target, headers, expected) in cases {
        let body = if method == "POST" { "{}" } else { "" };
        let status = send(&proxy, request(method, target, headers, body)).await;
        assert_eq!(status, expected, "{method} {target}");
    }
    let seen = upstream.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].target, "/v1/models");
    assert!(!seen[0].headers.contains_key("x-duolane-key"));
}

#[cfg(unix)]
#[tokio::test]
async fn intercept_files_are_owner_only_reused_and_never_symlinks() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |name: &str| {
        std::fs::metadata(intercept_dir().join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    let first = InterceptListener::bind(0).await.unwrap();
    assert_eq!(mode(""), 0o700);
    for name in ["ca.pem", "leaf.pem", "leaf.key", "proxy-password"] {
        assert_eq!(mode(name), 0o600, "{name}");
    }
    let password = std::fs::read(intercept_dir().join("proxy-password")).unwrap();
    let leaf = std::fs::read(intercept_dir().join("leaf.pem")).unwrap();
    drop(first);

    let second = InterceptListener::bind(0).await.unwrap();
    assert!(std::fs::read(intercept_dir().join("proxy-password")).unwrap() == password);
    assert!(std::fs::read(intercept_dir().join("leaf.pem")).unwrap() == leaf);
    drop(second);

    let path = intercept_dir().join("proxy-password");
    let decoy = intercept_dir().join("decoy");
    std::fs::rename(&path, &decoy).unwrap();
    std::os::unix::fs::symlink(&decoy, &path).unwrap();
    assert!(InterceptListener::bind(0).await.is_err());
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&decoy, &path).unwrap();
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn serve_command(gateway: u16, intercept: u16, https_proxy: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_duolane"));
    command
        .args(["serve", "--no-monitor", "--intercept", "--port"])
        .arg(gateway.to_string())
        .env_clear()
        .env("DUOLANE_INTERCEPT_PORT", intercept.to_string())
        .env("HTTPS_PROXY", https_proxy)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for name in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "DUOLANE_CONFIG_DIR",
        "DUOLANE_STATE_DIR",
        "CODEX_HOME",
        "TMPDIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

#[test]
fn serve_refuses_a_proxy_variable_that_names_the_intercept_listener() {
    let intercept = free_port();
    let looped = serve_command(
        free_port(),
        intercept,
        &format!("http://127.0.0.1:{intercept}"),
    )
    .output()
    .unwrap();
    assert!(!looped.status.success());
    assert!(String::from_utf8_lossy(&looped.stderr).contains("HTTPS_PROXY"));

    let mut command = serve_command(free_port(), intercept, "http://127.0.0.1:9");
    let _running = Running(command.stderr(Stdio::null()).spawn().unwrap());
    let started = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", intercept)).is_err() {
        assert!(
            started.elapsed() < WAIT,
            "the intercept listener never opened"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
