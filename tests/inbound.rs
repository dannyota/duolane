#[path = "support/hermetic.rs"]
mod hermetic;

use std::{
    net::{IpAddr, SocketAddr},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    routing::get,
};
use duolane::{
    auth::constant_time_eq,
    inbound_auth::{InboundPolicy, guard},
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Notify};
use tower::ServiceExt;

const TOKEN: &str = "toy-inbound-token-0123456789abcdef";
const HOST: &str = "127.0.0.1:18765";
const PASS: StatusCode = StatusCode::OK;

type TokenCase<'a> = (
    &'a str,
    &'a str,
    Option<&'a str>,
    Option<&'a str>,
    StatusCode,
);

fn policy(bind: &str, token: Option<&str>, hosts: &[&str]) -> anyhow::Result<InboundPolicy> {
    InboundPolicy::new(
        bind.parse::<IpAddr>().unwrap(),
        token.map(str::to_string),
        hosts.iter().map(|host| host.to_string()).collect(),
    )
}

async fn through_guard(
    policy: &InboundPolicy,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let router = Router::new().fallback(|| async { Json(json!({"passed": true})) });
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = guard(router, policy.clone())
        .oneshot(request.body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or_default();
    (status, headers, body)
}

#[tokio::test]
async fn inbound_guard_admits_claude_code_and_rejects_foreign_hosts_and_origins() {
    let loopback = policy("127.0.0.1", None, &[]).unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/claude-code-requests.json")).unwrap();
    for recorded in fixture["requests"].as_array().unwrap() {
        let method = recorded["method"].as_str().unwrap();
        let mut path = recorded["path"].as_str().unwrap().to_string();
        for (index, key) in recorded["query_keys"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let separator = if index == 0 { '?' } else { '&' };
            path.push_str(&format!("{separator}{}=true", key.as_str().unwrap()));
        }
        for host in [HOST, "localhost:18765"] {
            let headers = recorded["header_names"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| match name.as_str().unwrap() {
                    "host" => ("host", host),
                    "content-type" => ("content-type", recorded["content_type"].as_str().unwrap()),
                    "content-length" => ("content-length", "2"),
                    name => (name, "toy"),
                })
                .collect::<Vec<_>>();
            let (status, _, _) = through_guard(&loopback, method, &path, &headers).await;
            assert_eq!(status, PASS, "{method} {path} {host}");
        }
    }

    let deny = StatusCode::FORBIDDEN;
    let alternate = policy("127.0.0.2", None, &[]).unwrap();
    let proxied = policy("127.0.0.1", Some(TOKEN), &["proxy.example"]).unwrap();
    let hosts: &[(&InboundPolicy, &[&str], StatusCode)] = &[
        (&loopback, &["[::1]:18765"], PASS),
        (&proxied, &["PROXY.example:8443"], PASS),
        (&loopback, &["LOCALHOST:18765"], PASS),
        (&alternate, &["127.0.0.2:18765"], PASS),
        (&loopback, &["127.0.0.2:18765"], deny),
        (&loopback, &["evil.example"], deny),
        (&loopback, &["127.0.0.1.evil.example:18765"], deny),
        (&loopback, &["localhost.:18765"], deny),
        (&loopback, &["user@127.0.0.1:18765"], deny),
        (&loopback, &[], deny),
        (&loopback, &[HOST, HOST], deny),
    ];
    let origins: &[(&str, &str, Option<&str>, StatusCode)] = &[
        ("GET", "/v1/models", Some("null"), deny),
        ("GET", "/v1/models", Some("http://127.0.0.1:18765"), deny),
        (
            "OPTIONS",
            "/v1/messages",
            Some("https://evil.example"),
            deny,
        ),
        ("OPTIONS", "/v1/messages", None, PASS),
        ("GET", "/healthz", Some("null"), PASS),
    ];
    let mut cases = Vec::new();
    for (policy, values, expected) in hosts {
        let mut headers: Vec<_> = values.iter().map(|value| ("host", *value)).collect();
        headers.push(("x-duolane-key", TOKEN));
        cases.push((*policy, "GET", "/v1/models", headers, *expected));
    }
    let remote_monitor = vec![("host", "proxy.example"), ("x-duolane-key", TOKEN)];
    cases.push((&proxied, "GET", "/monitor", remote_monitor, deny));
    for (method, path, origin, expected) in origins {
        let mut headers = vec![("host", HOST), ("access-control-request-method", "POST")];
        headers.extend(origin.map(|origin| ("origin", origin)));
        cases.push((&loopback, *method, *path, headers, *expected));
    }
    for (policy, method, path, headers, expected) in cases {
        let (status, response_headers, body) = through_guard(policy, method, path, &headers).await;
        assert_eq!(status, expected, "{method} {path} {headers:?}");
        assert!(
            response_headers
                .keys()
                .all(|name| !name.as_str().starts_with("access-control-"))
        );
        if status == deny {
            assert_eq!(body["error"]["type"], "permission_error");
            assert!(!body.to_string().contains("evil"));
        }
    }
}

#[tokio::test]
async fn inbound_guard_requires_the_route_media_type_on_post() {
    const JSON: &str = "Application/JSON; charset=utf-8";
    const MULTIPART: &str = "multipart/form-data; boundary=x";
    let deny = StatusCode::UNSUPPORTED_MEDIA_TYPE;
    let loopback = policy("127.0.0.1", None, &[]).unwrap();
    let mut cases = Vec::new();
    for path in [
        "/v1/messages",
        "/v1/messages/count_tokens",
        "/v1/responses",
        "/v1/chat/completions",
        "/v1/images/generations",
    ] {
        cases.extend([("POST", path, None, deny), ("POST", path, Some(JSON), PASS)]);
    }
    cases.extend([
        ("POST", "/v1/messages", Some("text/plain"), deny),
        (
            "POST",
            "/v1/messages",
            Some("application/x-www-form-urlencoded"),
            deny,
        ),
        ("POST", "/v1/messages", Some(MULTIPART), deny),
        ("POST", "/v1/audio/transcriptions", Some(MULTIPART), PASS),
        ("POST", "/v1/audio/transcriptions", Some(JSON), deny),
        ("POST", "/v1/images/edits", Some(JSON), PASS),
        ("POST", "/v1/images/edits", Some(MULTIPART), PASS),
        ("POST", "/v1/images/edits", Some("text/plain"), deny),
        ("GET", "/v1/models", None, PASS),
        ("HEAD", "/v1/messages", None, PASS),
    ]);
    for (method, path, content_type, expected) in cases {
        let mut headers = vec![("host", HOST)];
        headers.extend(content_type.map(|value| ("content-type", value)));
        let (status, _, body) = through_guard(&loopback, method, path, &headers).await;
        assert_eq!(status, expected, "{method} {path} {content_type:?}");
        if status == deny {
            assert_eq!(body["error"]["type"], "invalid_request_error");
        }
    }
}

#[test]
fn inbound_startup_rejects_unsafe_token_and_bind_settings() {
    let short = &TOKEN[..31];
    assert!(policy("127.0.0.1", None, &[]).unwrap().is_loopback());
    assert!(policy("::ffff:127.0.0.1", None, &[]).unwrap().is_loopback());
    assert!(policy("127.0.0.1", Some(&TOKEN[..32]), &["proxy.example"]).is_ok());
    assert!(!policy("0.0.0.0", Some(TOKEN), &[]).unwrap().is_loopback());
    for host in [
        "proxy.example:8443",
        "https://proxy.example",
        "proxy.example/v1",
        "user@proxy.example",
        "proxy.example.",
        "::1",
    ] {
        let error = policy("127.0.0.1", Some(TOKEN), &[host]).unwrap_err();
        assert!(error.to_string().contains(host), "{host}");
    }

    let broken = tempfile::TempDir::new().unwrap();
    let broken_file = broken.path().join("config.json");
    std::fs::write(&broken_file, format!(r#"{{"port":"{TOKEN}"}}"#)).unwrap();
    let broken_dir = broken.path().to_string_lossy().into_owned();
    let broken_file = broken_file.to_string_lossy().into_owned();
    for (name, value, message) in [
        (
            "DUOLANE_CONFIG_DIR",
            broken_dir.as_str(),
            broken_file.as_str(),
        ),
        ("DUOLANE_AUTH_TOKEN", short, "at least 32 characters"),
        (
            "DUOLANE_BIND_ADDRESS",
            "192.0.2.1",
            "without an inbound token",
        ),
        (
            "DUOLANE_ALLOWED_HOSTS",
            "proxy.example",
            "require an inbound token",
        ),
    ] {
        let output = exited(duolane(
            &["serve", "--no-monitor", "--port", "0"],
            &[(name, value)],
        ));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{name}");
        assert!(stderr.contains(message), "{name}: {stderr}");
        assert!(!stderr.contains(short));
    }
}

#[tokio::test]
async fn inbound_token_guards_every_route_and_never_reaches_upstream() {
    let captured = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
    let upstream = serve(Router::new().fallback({
        let captured = captured.clone();
        move |headers: HeaderMap| {
            captured.lock().unwrap().push(headers);
            async { Json(json!({"type": "message"})) }
        }
    }))
    .await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _proxy = Running(
        duolane(
            &["serve", "--no-monitor", "--port", &port.to_string()],
            &[
                ("DUOLANE_AUTH_TOKEN", TOKEN),
                ("DUOLANE_ALIAS_PROVIDER", "anthropic"),
                ("DUOLANE_ANTHROPIC_BASE_URL", &format!("http://{upstream}")),
            ],
        )
        .spawn()
        .unwrap(),
    );
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    wait_until(async || {
        let response = client.get(format!("{base}/healthz")).send().await;
        response.is_ok_and(|response| response.status() == PASS)
    })
    .await;

    let deny = StatusCode::UNAUTHORIZED;
    let bearer = format!("Bearer {TOKEN}");
    let login = "Bearer toy-claude-login";
    let cases: &[TokenCase] = &[
        ("GET", "/healthz", None, None, PASS),
        ("GET", "/v1/models", None, None, deny),
        ("GET", "/monitor", None, None, deny),
        ("POST", "/v1/messages", None, None, deny),
        ("POST", "/v1/messages/count_tokens", None, None, deny),
        ("GET", "/unknown", None, None, deny),
        ("GET", "/v1/models", Some("wrong"), None, deny),
        ("GET", "/v1/models", Some(TOKEN), None, PASS),
        ("GET", "/v1/models", None, Some(&bearer), PASS),
        ("GET", "/monitor", Some(TOKEN), None, PASS),
        ("POST", "/v1/messages", Some(TOKEN), Some(login), PASS),
        ("POST", "/v1/messages", None, Some(&bearer), PASS),
    ];
    let body = json!({"model": "claude-haiku-5-5", "max_tokens": 8, "messages": []});
    for (method, path, key, authorization, expected) in cases {
        let mut request = client.request(method.parse().unwrap(), format!("{base}{path}"));
        if *method == "POST" {
            request = request.json(&body);
        }
        if let Some(key) = key {
            request = request.header("x-duolane-key", *key);
        }
        if let Some(authorization) = authorization {
            request = request.header("authorization", *authorization);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), *expected, "{method} {path} {key:?}");
        if *expected == deny {
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["error"]["type"], "authentication_error");
        }
    }
    let smuggled = client
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .header("x-duolane-key", TOKEN)
        .header("authorization", login)
        .header("authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(smuggled.status(), StatusCode::BAD_REQUEST);
    let error: Value = smuggled.json().await.unwrap();
    assert_eq!(error["error"]["type"], "invalid_request_error");
    let captured = captured.lock().unwrap().clone();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0]["authorization"], login);
    assert!(!captured[1].contains_key("authorization"));
    for value in captured.iter().flat_map(HeaderMap::values) {
        assert!(!String::from_utf8_lossy(value.as_bytes()).contains(TOKEN));
    }

    let hits = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let monitor = get({
        let hits = hits.clone();
        let release = release.clone();
        move || async move {
            hits.fetch_add(1, Ordering::SeqCst);
            release.notified().await;
        }
    });
    let protected = policy("127.0.0.1", Some(TOKEN), &[]).unwrap();
    let gated = serve(guard(Router::new().route("/monitor", monitor), protected)).await;
    let url = format!("http://{gated}/");
    let monitor_env = [("DUOLANE_AUTH_TOKEN", TOKEN)];
    let _monitor = Running(
        duolane(&["monitor", "--url", &url], &monitor_env)
            .spawn()
            .unwrap(),
    );
    wait_until(async || hits.load(Ordering::SeqCst) > 0).await;
    release.notify_waiters();
}

#[test]
fn token_comparison_matches_only_identical_input() {
    assert!(constant_time_eq(TOKEN, TOKEN));
    assert!(!constant_time_eq(
        "toy-inbound-token-0123456789abcdeX",
        TOKEN
    ));
    assert!(!constant_time_eq(&TOKEN[..20], TOKEN));
    assert!(!constant_time_eq(&format!("{TOKEN}0"), TOKEN));
    assert!(!constant_time_eq(&TOKEN.repeat(2), TOKEN));
    assert!(!constant_time_eq("", TOKEN));
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn duolane(args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_duolane"));
    command.args(args).env_clear();
    for name in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "DUOLANE_CONFIG_DIR",
        "DUOLANE_STATE_DIR",
        "CODEX_HOME",
        "TMPDIR",
        "SYSTEMROOT",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn exited(mut command: Command) -> Output {
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    child.wait_with_output().unwrap()
}

async fn serve(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let service = router.into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, service).await.unwrap();
    });
    address
}

async fn wait_until(mut ready: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready().await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}
