use super::login::keychain_service;
use super::*;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use jiff::tz::TimeZone;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Mock {
    responses: HashMap<String, (u16, String)>,
    hits: Mutex<Vec<(String, bool)>>,
}

async fn usage_handler(State(mock): State<Arc<Mock>>, headers: HeaderMap) -> impl IntoResponse {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let token = header("authorization")
        .strip_prefix("Bearer ")
        .unwrap_or("")
        .to_string();
    let headers_ok =
        header("anthropic-beta") == "oauth-2025-04-20" && header("accept") == "application/json";
    mock.hits.lock().unwrap().push((token.clone(), headers_ok));
    match mock.responses.get(&token) {
        Some((status, body)) => (StatusCode::from_u16(*status).unwrap(), body.clone()),
        None => (StatusCode::UNAUTHORIZED, String::new()),
    }
}

async fn spawn_mock(responses: &[(&str, u16, String)]) -> (String, Arc<Mock>) {
    let mock = Arc::new(Mock {
        responses: responses
            .iter()
            .map(|(token, status, body)| (token.to_string(), (*status, body.clone())))
            .collect(),
        hits: Mutex::new(Vec::new()),
    });
    let app = axum::Router::new()
        .route("/api/oauth/usage", get(usage_handler))
        .with_state(Arc::clone(&mock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api/oauth/usage", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, mock)
}

struct FakeReader {
    logins: HashMap<PathBuf, (String, u64)>,
    calls: AtomicUsize,
}

impl LoginReader for FakeReader {
    fn read(&self, configured: &Path, _dir: &File) -> Result<Login, Reason> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.logins.get(configured) {
            Some((token, expires_at)) => Ok(Login {
                token: Token(token.clone()),
                expires_at: *expires_at,
            }),
            None => Err(Reason::LoginNotFound),
        }
    }
}

fn target(name: &str) -> Target {
    Target {
        name: name.to_string(),
        location: Ok((
            PathBuf::from(format!("/accounts/{name}")),
            tempfile::tempfile().unwrap(),
        )),
    }
}

fn query(logins: &[(&str, &str, u64)], endpoint: String) -> Query {
    Query {
        reader: Arc::new(FakeReader {
            logins: logins
                .iter()
                .map(|(name, token, expires_at)| {
                    (
                        PathBuf::from(format!("/accounts/{name}")),
                        (token.to_string(), *expires_at),
                    )
                })
                .collect(),
            calls: AtomicUsize::new(0),
        }),
        client: build_client(Vec::new()),
        endpoint,
    }
}

fn future() -> u64 {
    now_millis() + 3_600_000
}

fn fixed_now() -> Zoned {
    "2025-11-24T10:00:00Z"
        .parse::<Timestamp>()
        .unwrap()
        .to_zoned(TimeZone::UTC)
}

fn body(five: &str, week: &str) -> String {
    format!(
        r#"{{"five_hour":{{"utilization":{five},"resets_at":"2025-11-24T15:59:00.000000+00:00","extra":1}},"seven_day":{{"utilization":{week},"resets_at":"2025-11-27T19:00:00Z"}},"other":null}}"#
    )
}

#[test]
fn keychain_service_ignores_a_trailing_slash_and_hashes_the_spelling() {
    assert_eq!(
        keychain_service(Path::new("/Users/example/.claude-work")),
        "Claude Code-credentials-dd1118a7"
    );
    assert_eq!(
        keychain_service(Path::new("/Users/example/.claude-work/")),
        "Claude Code-credentials-dd1118a7"
    );
    assert_ne!(
        keychain_service(Path::new("/Users/example/.claude-other")),
        keychain_service(Path::new("/Users/example/.claude-work"))
    );
}

#[test]
fn file_login_must_be_private_and_small() {
    let root = tempfile::tempdir_in(crate::paths::config_dir()).unwrap();
    let dir = File::open(root.path()).unwrap();
    let path = root.path().join(".credentials.json");
    let json = format!(
        r#"{{"claudeAiOauth":{{"accessToken":"toy-token-1","expiresAt":{},"scopes":["a"]}},"other":1}}"#,
        future()
    );
    let read = || FileReader.read(Path::new("/unused"), &dir);

    assert_eq!(read().unwrap_err(), Reason::LoginNotFound);

    std::fs::write(&path, &json).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(read().unwrap_err(), Reason::LoginUnreadable);

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let login = read().unwrap();
    assert!(login.expires_at > now_millis());
    assert!(login.token.expose() == "toy-token-1");
    assert!(!format!("{login:?}").contains("toy-token-1"));

    let padded = format!(
        r#"{{"pad":"{}","claudeAiOauth":{{"accessToken":"toy-token-1","expiresAt":1}}}}"#,
        "x".repeat(64 * 1024)
    );
    std::fs::write(&path, padded).unwrap();
    assert_eq!(read().unwrap_err(), Reason::LoginUnreadable);

    std::fs::write(&path, "{}").unwrap();
    assert_eq!(read().unwrap_err(), Reason::LoginUnreadable);
}

#[test]
fn keychain_child_is_bounded_and_its_status_is_classified() {
    let root = tempfile::tempdir_in(crate::paths::config_dir()).unwrap();
    let dir = File::open(root.path()).unwrap();
    let configured = Path::new("/Users/example/.claude-work");
    let json = format!(
        r#"{{"claudeAiOauth":{{"accessToken":"toy-token-1","expiresAt":{}}}}}"#,
        future()
    );
    let run = |name: &str, script: &str, deadline: Duration| {
        let program = root.path().join(name);
        std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        KeychainReader { program, deadline }.read(configured, &dir)
    };
    let patient = Duration::from_secs(20);

    let accepted = format!(
        "[ \"$1\" = find-generic-password ] && [ \"$2\" = -s ] && [ \"$3\" = \"Claude Code-credentials-dd1118a7\" ] && [ \"$4\" = -w ] && [ \"$PATH\" = /usr/bin:/bin ] && [ -z \"$HOME\" ] || exit 99\nprintf '%s\\n' '{json}'"
    );
    assert!(run("ok", &accepted, patient).unwrap().token.expose() == "toy-token-1");
    assert_eq!(
        run("missing", "exit 44", patient).unwrap_err(),
        Reason::LoginNotFound
    );
    assert_eq!(
        run("denied", "exit 128", patient).unwrap_err(),
        Reason::KeychainDenied
    );
    assert_eq!(
        run("cancelled", "exit 51", patient).unwrap_err(),
        Reason::KeychainDenied
    );
    assert_eq!(
        run("failed", "exit 1", patient).unwrap_err(),
        Reason::LoginUnreadable
    );
    assert_eq!(
        run("garbage", "echo not-json", patient).unwrap_err(),
        Reason::LoginUnreadable
    );
    assert_eq!(
        run("flood", "head -c 70000 /dev/zero", patient).unwrap_err(),
        Reason::LoginUnreadable
    );

    let started = std::time::Instant::now();
    assert_eq!(
        run("hang", "exec sleep 5", Duration::from_millis(200)).unwrap_err(),
        Reason::KeychainTimedOut
    );
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn usage_lines_follow_pool_order_with_isolated_failures() {
    let (endpoint, mock) = spawn_mock(&[
        ("toy-token-1", 200, body("0.4", "17.6")),
        ("toy-token-2", 200, body("5.0", "51.2")),
        ("toy-token-3", 500, "upstream detail".to_string()),
    ])
    .await;
    let query = query(
        &[
            ("share-sec", "toy-token-1", future()),
            ("share-sec2", "toy-token-2", future()),
            ("acc3", "toy-token-3", future()),
            ("acc4", "toy-token-4", 1),
        ],
        endpoint,
    );
    let targets = ["share-sec", "share-sec2", "acc3", "acc4"]
        .map(target)
        .into();

    let rows = query.collect(targets).await;
    let lines = render(&rows, Some("share-sec"), &fixed_now());

    assert!(
        lines
            == vec![
                "* share-sec    5h   0% (resets 15:59)   week  18% (resets Nov 27 19:00)",
                "  share-sec2   5h   5% (resets 15:59)   week  51% (resets Nov 27 19:00)",
                "  acc3         usage request failed: HTTP 500",
                "  acc4         login expired; open Claude Code with this account",
            ]
    );
    assert!(lines.iter().all(|line| !line.contains("toy-token")));
    let hits = mock.hits.lock().unwrap();
    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|(_, headers_ok)| *headers_ok));
    assert!(hits.iter().all(|(token, _)| token != "toy-token-4"));
}

#[tokio::test]
async fn usage_response_is_lenient_about_windows_and_bounded_in_size() {
    let oversized = format!(r#"{{"pad":"{}"}}"#, "x".repeat(70 * 1024));
    let (endpoint, _) = spawn_mock(&[
        (
            "toy-token-1",
            200,
            r#"{"seven_day":{"utilization":20,"resets_at":"2025-11-24T15:59:00Z"}}"#.to_string(),
        ),
        (
            "toy-token-2",
            200,
            r#"{"five_hour":{"utilization":"high","resets_at":"soon"},"seven_day":null}"#
                .to_string(),
        ),
        ("toy-token-3", 200, oversized),
        ("toy-token-4", 200, "[1]".to_string()),
    ])
    .await;
    let query = query(
        &[
            ("a", "toy-token-1", future()),
            ("b", "toy-token-2", future()),
            ("c", "toy-token-3", future()),
            ("d", "toy-token-4", future()),
        ],
        endpoint,
    );

    let rows = query.collect(["a", "b", "c", "d"].map(target).into()).await;

    assert!(
        render(&rows, None, &fixed_now())
            == vec![
                "  a   5h    ?   week  20% (resets 15:59)",
                "  b   5h    ? (resets ?)   week    ?",
                "  c   usage response invalid",
                "  d   usage response invalid",
            ]
    );
}

#[tokio::test]
async fn usage_never_follows_a_redirect() {
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let location = format!("http://{}/stolen", sink.local_addr().unwrap());
    let app = axum::Router::new().fallback(move || {
        let location = location.clone();
        async move { (StatusCode::FOUND, [("location", location)]) }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/oauth/usage", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let query = query(&[("a", "toy-token-1", future())], endpoint);

    let rows = query.collect(vec![target("a")]).await;

    assert!(render(&rows, None, &fixed_now()) == vec!["  a   usage request failed: HTTP 302"]);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), sink.accept())
            .await
            .is_err()
    );
}

#[test]
fn production_endpoint_ignores_the_relay_base_url_override() {
    let env = |name: &str| {
        (name == "DUOLANE_ANTHROPIC_BASE_URL").then(|| "http://127.0.0.1:1".to_string())
    };
    assert_eq!(
        production_usage_endpoint(&env),
        "https://api.anthropic.com/api/oauth/usage"
    );
    assert_eq!(
        production_usage_endpoint(&|_: &str| None),
        "https://api.anthropic.com/api/oauth/usage"
    );
}

fn proxies(https: &str, all: &str) -> Result<Vec<(Kind, String)>, Reason> {
    let https = https.to_string();
    let all = all.to_string();
    usage_proxies(
        &move |name: &str| match name {
            "HTTPS_PROXY" if !https.is_empty() => Some(https.clone()),
            "all_proxy" if !all.is_empty() => Some(all.clone()),
            _ => None,
        },
        18766,
    )
}

#[test]
fn proxies_skip_the_intercept_listener_and_keep_the_rest() {
    for alias in [
        "http://127.0.0.1:18766",
        "http://0.0.0.0:18766",
        "http://[::]:18766",
        "http://[::ffff:0.0.0.0]:18766",
        "http://[::1]:18766",
    ] {
        assert!(proxies(alias, "").unwrap().is_empty());
    }
    assert!(proxies("", "localhost:18766").unwrap().is_empty());
    assert!(proxies("", "0.0.0.0:18766").unwrap().is_empty());
    assert_eq!(
        proxies("http://0.0.0.0:3128", "").unwrap(),
        vec![(Kind::Https, "http://0.0.0.0:3128/".to_string())]
    );
    assert_eq!(
        proxies("http://127.0.0.1:3128", "").unwrap(),
        vec![(Kind::Https, "http://127.0.0.1:3128/".to_string())]
    );
    assert_eq!(
        proxies("http://proxy.example:8080", "socks5://other.example:1080").unwrap(),
        vec![
            (Kind::Https, "http://proxy.example:8080/".to_string()),
            (Kind::All, "socks5://other.example:1080".to_string())
        ]
    );
}

#[tokio::test]
async fn an_unsupported_proxy_fails_every_account_without_sending() {
    for (https, all) in [
        ("ftp://proxy.example:21", ""),
        ("", "ftp://proxy.example:21"),
        ("http://", ""),
    ] {
        assert_eq!(proxies(https, all).unwrap_err(), Reason::RequestFailed);
    }

    let (endpoint, mock) = spawn_mock(&[("toy-token-1", 200, body("1", "2"))]).await;
    let mut query = query(&[("a", "toy-token-1", future())], endpoint);
    query.client = proxies("ftp://proxy.example:21", "").and_then(build_client);

    let rows = query.collect(vec![target("a")]).await;

    assert!(render(&rows, None, &fixed_now()) == vec!["  a   usage request failed"]);
    assert!(mock.hits.lock().unwrap().is_empty());
}
