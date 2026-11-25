#![cfg(unix)]

#[path = "support/hermetic.rs"]
mod hermetic;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use duolane::intercept::{InterceptListener, intercept_dir};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls::pki_types::PrivatePkcs8KeyDer;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SETUP_PORT: u16 = 40123;
const ORIGINAL: &str = "{\n  \"model\": \"opus\",\n  \"env\": {\n    \"FOO\": \"bar\"\n  },\n  \"permissions\": {\n    \"allow\": [\n      \"Bash(ls)\"\n    ]\n  }\n}\n";
const INDENTED: &str = "{\n    \"env\": {\n        \"CLAUDE_CODE_GZIP_REQUEST_BODIES\": \"0\"\n    },\n    \"permissions\": {\n        \"allow\": [\n            \"Bash(ls)\"\n        ]\n    }\n}\n";
const FAKE_CLAUDE: &str = "#!/bin/sh\nfor argument in \"$@\"; do printf 'arg:%s\\n' \"$argument\"; done\necho env:\nenv\n";
const PAIR: [&str; 3] = ["ca.pem", "leaf.pem", "leaf.key"];
const VARIABLES: [&str; 8] = [
    "HTTPS_PROXY",
    "https_proxy",
    "NODE_EXTRA_CA_CERTS",
    "NO_PROXY",
    "no_proxy",
    "CLAUDE_CODE_GZIP_REQUEST_BODIES",
    "CLAUDE_STREAM_FIRST_BYTE_TIMEOUT_MS",
    "CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS",
];

struct Sandbox {
    root: TempDir,
    config: PathBuf,
}

impl Sandbox {
    fn new(config: Option<PathBuf>) -> Self {
        let root = TempDir::new().unwrap();
        for dir in ["home", "state", "claude", "project", "bin", "codex"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let script = root.path().join("bin/claude");
        std::fs::write(&script, FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = config.unwrap_or_else(|| root.path().join("config"));
        Self { root, config }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn settings(&self) -> PathBuf {
        self.path("claude/settings.json")
    }

    fn intercept(&self) -> PathBuf {
        self.config.join("intercept")
    }

    fn command(&self, port: u16) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_duolane"));
        command
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path("bin").display()),
            )
            .env("HOME", self.path("home"))
            .env("DUOLANE_CONFIG_DIR", &self.config)
            .env("DUOLANE_STATE_DIR", self.path("state"))
            .env("CLAUDE_CONFIG_DIR", self.path("claude"))
            .env("CODEX_HOME", self.path("codex"))
            .env("DUOLANE_INTERCEPT_PORT", port.to_string())
            .current_dir(self.path("project"))
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(SETUP_PORT).args(args).output().unwrap()
    }

    fn password(&self) -> String {
        std::fs::read_to_string(self.intercept().join("proxy-password")).unwrap()
    }

    fn document(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.settings()).unwrap()).unwrap()
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.intercept().join(name)).unwrap()
    }

    fn pair(&self) -> Vec<Vec<u8>> {
        PAIR.iter().map(|name| self.read(name)).collect()
    }
}

#[test]
fn intercept_config_permissions_under_shared_umask() {
    let sandbox = Sandbox::new(None);
    let port = free_port();
    let mut command = sandbox.command(port);
    command.args(["serve", "--intercept", "--no-monitor", "--port", "0"]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o002));
            Ok(())
        });
    }
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut server = Server(command.spawn().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if sandbox.intercept().join("ca.pem").exists()
            && std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
        {
            break;
        }
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "server exited before bootstrap"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "server bootstrap timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        std::fs::metadata(&sandbox.config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let mut wrapper = sandbox.command(port);
    let result = wrapper
        .env_remove("CLAUDE_CONFIG_DIR")
        .args(["claude", "-p", "hi"])
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", text(&result.stderr));
}

#[test]
fn intercept_rejects_existing_writable_config_with_remediation() {
    let sandbox = Sandbox::new(None);
    std::fs::create_dir(&sandbox.config).unwrap();
    std::fs::set_permissions(&sandbox.config, std::fs::Permissions::from_mode(0o775)).unwrap();
    let result = sandbox.run(&["intercept", "setup"]);
    let error = text(&result.stderr);
    assert!(!result.status.success());
    assert!(
        error.contains(&format!("{:?}", sandbox.config.canonicalize().unwrap())),
        "{error}"
    );
    assert!(error.contains("0775"));
    assert!(error.contains("chmod -- go-w <directory>"));
    assert_eq!(
        std::fs::metadata(&sandbox.config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o775
    );
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn received(output: &Output) -> Vec<String> {
    text(&output.stdout)
        .lines()
        .take_while(|line| *line != "env:")
        .filter_map(|line| line.strip_prefix("arg:").map(str::to_string))
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_expired_install(dir: &Path) {
    let issued = rcgen::date_time_ymd(2020, 1, 1);
    let expired = rcgen::date_time_ymd(2020, 4, 1);
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.not_before = issued;
    ca.not_after = expired;
    let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
    let issuer = Issuer::new(ca, ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf = CertificateParams::new(vec!["api.anthropic.com".to_string()]).unwrap();
    leaf.not_before = issued;
    leaf.not_after = expired;
    let leaf_pem = leaf.signed_by(&leaf_key, &issuer).unwrap().pem();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("ca.pem"), ca_pem).unwrap();
    std::fs::write(dir.join("leaf.pem"), leaf_pem).unwrap();
    std::fs::write(dir.join("leaf.key"), leaf_key.serialize_pem()).unwrap();
    std::fs::write(dir.join("proxy-password"), "t".repeat(43)).unwrap();
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn listen() -> (u16, String) {
    let listener = InterceptListener::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(listener.serve(axum::Router::new(), std::future::pending()));
    let password = std::fs::read_to_string(intercept_dir().join("proxy-password")).unwrap();
    (port, password)
}

async fn output(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

fn hermetic_config() -> Option<PathBuf> {
    std::env::var_os("DUOLANE_CONFIG_DIR").map(PathBuf::from)
}

fn foreign_acceptor() -> tokio_rustls::TlsAcceptor {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let issuer = Issuer::new(ca, ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["api.anthropic.com".to_string()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone()],
        PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
    )
    .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

async fn read_head(stream: &mut tokio::net::TcpStream) {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(stream.read_u8().await.unwrap());
    }
}

async fn eavesdropper() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_head(&mut stream).await;
    });
    port
}

async fn impostor() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = foreign_acceptor();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let _ = acceptor.accept(stream).await;
    });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_starts_with_the_intercept_environment_and_the_lineup_before_the_owner_arguments() {
    let (port, password) = listen().await;
    let sandbox = Sandbox::new(hermetic_config());
    let catalog = json!({"models": [
        {"slug": "gpt-6.2-sol", "supported_in_api": true, "visibility": "list", "priority": 3},
        {"slug": "gpt-5.4-mini", "supported_in_api": true, "visibility": "list", "priority": 4},
        {"slug": "gpt-toy-late", "supported_in_api": true, "visibility": "list", "priority": 2, "display_name": "Toy Late"},
        {"slug": "gpt-toy-early", "supported_in_api": true, "visibility": "list", "priority": 1, "display_name": "Toy Early"}
    ]});
    std::fs::write(sandbox.path("codex/models_cache.json"), catalog.to_string()).unwrap();
    let arguments = [
        "--",
        "--model",
        "gpt-5.6-terra",
        "-p",
        "two words",
        "--help",
    ];
    let mut command = sandbox.command(port);
    command
        .arg("claude")
        .args(arguments)
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("NO_PROXY", "corp.example")
        .env("CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS", "1234");
    let started = output(command).await;
    assert!(started.status.success());
    assert!(!text(&started.stderr).contains(&password));
    let received = received(&started);
    assert_eq!(received[0], "--settings");
    let lineup: Value = serde_json::from_str(&received[1]).unwrap();
    let expected = json!({"modelPicker": {"options": [
        {"model": "terra[1m]", "label": "Terra (Codex)"},
        {"model": "sol[1m]", "label": "Sol (Codex)"},
        {"model": "astra[1m]", "label": "Astra (Codex)"},
        {"model": "luna[1m]", "label": "Luna (Codex)"},
        {"model": "gpt-toy-early", "label": "Toy Early (Codex)"},
        {"model": "gpt-toy-late", "label": "Toy Late (Codex)"},
        {"model": "gpt-6.2-sol[1m]", "label": "gpt-6.2-sol (Codex)"},
        {"model": "gpt-5.4-mini", "label": "gpt-5.4-mini (Codex)"}
    ]}});
    assert_eq!(lineup, expected);
    assert_eq!(received[2..], arguments);
    let stdout = text(&started.stdout);
    let env: HashMap<&str, &str> = stdout
        .lines()
        .skip_while(|line| *line != "env:")
        .filter_map(|line| line.split_once('='))
        .collect();
    let proxy = format!("http://duolane:{password}@127.0.0.1:{port}");
    assert!(env.get("HTTPS_PROXY") == Some(&proxy.as_str()));
    assert!(env.get("https_proxy") == Some(&proxy.as_str()));
    let ca = intercept_dir().join("ca.pem");
    assert_eq!(env.get("NODE_EXTRA_CA_CERTS"), ca.to_str().as_ref());
    assert_eq!(
        env.get("NO_PROXY"),
        Some(&"corp.example,localhost,127.0.0.1,::1")
    );
    assert_eq!(env.get("no_proxy"), Some(&"localhost,127.0.0.1,::1"));
    assert_eq!(env.get("CLAUDE_CODE_GZIP_REQUEST_BODIES"), Some(&"0"));
    assert_eq!(
        env.get("CLAUDE_STREAM_FIRST_BYTE_TIMEOUT_MS"),
        Some(&"330000")
    );
    assert_eq!(env.get("CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS"), Some(&"1234"));
    assert!(!env.contains_key("ANTHROPIC_BASE_URL"));
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_adds_no_lineup_over_the_owner_settings() {
    let (port, _) = listen().await;
    let sandbox = Sandbox::new(hermetic_config());
    let cases: [(&[&str], &str); 3] = [
        (&["--settings={}", "-p", "hi"], "--settings"),
        (&["-p", "hi", "--settings", "{}"], "--settings"),
        (&["remote-control", "--name", "x"], "not an option"),
    ];
    for (arguments, reason) in cases {
        let mut command = sandbox.command(port);
        command.arg("claude").args(arguments);
        let unchanged = output(command).await;
        assert!(unchanged.status.success());
        assert_eq!(received(&unchanged), arguments);
        assert!(text(&unchanged.stderr).contains(reason), "{arguments:?}");
    }

    let owner = json!({"modelPicker": {"options": [{"model": "opus"}]}}).to_string();
    std::fs::write(sandbox.settings(), &owner).unwrap();
    let mut command = sandbox.command(port);
    command.args(["claude", "-p", "hi"]);
    let owned = output(command).await;
    assert!(owned.status.success());
    assert_eq!(received(&owned), ["-p", "hi"]);
    assert!(text(&owned.stderr).contains("sets modelPicker"));
    assert_eq!(std::fs::read_to_string(sandbox.settings()).unwrap(), owner);
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_refuses_a_down_listener_and_another_proxy() {
    let (port, _) = listen().await;
    let sandbox = Sandbox::new(hermetic_config());
    let mut command = sandbox.command(free_port());
    command.args(["claude", "-p", "hi"]);
    let down = output(command).await;
    assert!(!down.status.success());
    assert!(text(&down.stderr).contains("is down"));
    assert!(!text(&down.stderr).contains("password was sent"));
    assert!(down.stdout.is_empty());

    let mut command = sandbox.command(eavesdropper().await);
    command.args(["claude", "-p", "hi"]);
    let closed = output(command).await;
    assert!(!closed.status.success());
    assert!(text(&closed.stderr).contains("password was sent"));
    assert!(closed.stdout.is_empty());

    let mut command = sandbox.command(port);
    command
        .args(["claude", "-p", "hi"])
        .env("HTTPS_PROXY", "http://proxy.example:3128");
    let proxied = output(command).await;
    assert!(!proxied.status.success());
    assert!(text(&proxied.stderr).contains("HTTPS_PROXY"));
    assert!(proxied.stdout.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_refuses_another_certificate_and_says_the_password_was_sent() {
    drop(InterceptListener::bind(0).await.unwrap());
    let port = impostor().await;
    let sandbox = Sandbox::new(hermetic_config());
    let mut command = sandbox.command(port);
    command.args(["claude", "-p", "hi"]);
    let refused = output(command).await;
    assert!(!refused.status.success());
    let stderr = text(&refused.stderr);
    assert!(stderr.contains("another certificate"));
    assert!(stderr.contains("password was sent"));
    assert!(refused.stdout.is_empty());
}

#[test]
fn claude_refuses_an_expired_certificate_before_it_connects() {
    let sandbox = Sandbox::new(None);
    write_expired_install(&sandbox.intercept());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let refused = sandbox
        .command(port)
        .args(["claude", "-p", "hi"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(text(&refused.stderr).contains("restart `duolane serve --intercept`"));
    assert!(refused.stdout.is_empty());
    assert!(listener.accept().is_err());
}

#[test]
fn renew_replaces_the_certificate_and_keeps_the_password_and_settings() {
    let sandbox = Sandbox::new(None);
    std::fs::write(sandbox.settings(), ORIGINAL).unwrap();
    assert!(sandbox.run(&["intercept", "setup"]).status.success());
    let before = sandbox.pair();
    let password = sandbox.password();
    let settings = std::fs::read(sandbox.settings()).unwrap();
    let renewed = sandbox
        .command(free_port())
        .args(["intercept", "renew"])
        .output()
        .unwrap();
    assert!(renewed.status.success());
    let stdout = text(&renewed.stdout);
    assert!(!stdout.contains(&password));
    for ((name, old), new) in PAIR.iter().zip(&before).zip(sandbox.pair()) {
        assert!(*old != new, "{name} did not change");
        assert!(stdout.contains(&format!("wrote {name}")));
    }
    assert!(sandbox.password() == password);
    assert!(std::fs::read(sandbox.settings()).unwrap() == settings);
    let manifest: Value = serde_json::from_slice(&sandbox.read("manifest.json")).unwrap();
    let entries = manifest["files"].as_array().unwrap();
    assert_eq!(entries.len(), 4);
    for entry in entries {
        let name = entry["name"].as_str().unwrap();
        assert!(
            entry["sha256"] == sha256(&sandbox.read(name)).as_str(),
            "{name}"
        );
    }
}

#[test]
fn renew_refuses_a_held_port_and_a_missing_install() {
    let sandbox = Sandbox::new(None);
    let missing = sandbox.run(&["intercept", "renew"]);
    assert!(!missing.status.success());
    assert!(text(&missing.stderr).contains("no intercept certificate to renew"));

    assert!(sandbox.run(&["intercept", "setup"]).status.success());
    let before = sandbox.pair();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let refused = sandbox
        .command(port)
        .args(["intercept", "renew"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(text(&refused.stderr).contains(&format!("127.0.0.1:{port}")));
    assert!(sandbox.pair() == before);
}

#[test]
fn setup_replaces_a_damaged_pair_only_when_the_port_is_free() {
    let sandbox = Sandbox::new(None);
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let setup = || {
        sandbox
            .command(port)
            .args(["intercept", "setup"])
            .output()
            .unwrap()
    };
    assert!(setup().status.success());
    std::fs::write(sandbox.intercept().join("leaf.pem"), "damaged").unwrap();
    let before = sandbox.pair();
    let manifest = sandbox.read("manifest.json");
    let settings = std::fs::read(sandbox.settings()).unwrap();
    let refused = setup();
    assert!(!refused.status.success());
    let stderr = text(&refused.stderr);
    assert!(stderr.contains(&format!("127.0.0.1:{port}")));
    assert!(stderr.contains("run `duolane intercept setup` again"));
    assert!(sandbox.pair() == before);
    assert!(sandbox.read("manifest.json") == manifest);
    assert!(std::fs::read(sandbox.settings()).unwrap() == settings);

    drop(held);
    let replaced = setup();
    assert!(replaced.status.success());
    let stdout = text(&replaced.stdout);
    assert!(stdout.contains("changed leaf.pem"));
    assert!(stdout.contains("restart `duolane serve --intercept`"));
}

#[test]
fn setup_writes_the_env_block_once_and_keeps_the_rest_of_the_file() {
    let sandbox = Sandbox::new(None);
    std::fs::write(sandbox.settings(), ORIGINAL).unwrap();
    let first = sandbox.run(&["intercept", "setup"]);
    assert!(first.status.success());
    let password = sandbox.password();
    assert!(!text(&first.stdout).contains(&password));
    let written = std::fs::read(sandbox.settings()).unwrap();
    let layout = text(&written);
    let position = |key: &str| layout.find(&format!("\"{key}\"")).unwrap();
    assert!(position("model") < position("env") && position("env") < position("permissions"));
    let document = sandbox.document();
    assert_eq!(document["model"], "opus");
    assert_eq!(document["permissions"], json!({"allow": ["Bash(ls)"]}));
    assert_eq!(document["env"]["FOO"], "bar");
    for name in VARIABLES {
        assert!(document["env"][name].is_string(), "{name}");
    }
    let proxy = format!("http://duolane:{password}@127.0.0.1:{SETUP_PORT}");
    assert!(document["env"]["HTTPS_PROXY"] == proxy.as_str());
    let mode = std::fs::metadata(sandbox.settings())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    let second = sandbox.run(&["intercept", "setup"]);
    assert!(second.status.success());
    assert!(text(&second.stdout).contains("Nothing changed."));
    assert!(std::fs::read(sandbox.settings()).unwrap() == written);
}

#[test]
fn setup_refuses_a_symlinked_settings_file_and_a_conflicting_project_value() {
    let sandbox = Sandbox::new(None);
    let target = sandbox.path("elsewhere.json");
    std::fs::write(&target, "{}\n").unwrap();
    std::os::unix::fs::symlink(&target, sandbox.settings()).unwrap();
    let linked = sandbox.run(&["intercept", "setup"]);
    assert!(!linked.status.success());
    assert!(text(&linked.stderr).contains("symlink"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "{}\n");
    std::fs::remove_file(sandbox.settings()).unwrap();

    let project = sandbox.path("project/.claude");
    std::fs::create_dir_all(&project).unwrap();
    let conflict = json!({"env": {"HTTPS_PROXY": "http://proxy.example:3128"}});
    std::fs::write(project.join("settings.json"), conflict.to_string()).unwrap();
    let conflicted = sandbox.run(&["intercept", "setup"]);
    assert!(!conflicted.status.success());
    let stderr = text(&conflicted.stderr);
    assert!(stderr.contains("HTTPS_PROXY") && stderr.contains(".claude/settings.json"));
    assert!(!sandbox.settings().exists());
}

#[test]
fn cleanup_restores_the_settings_file_and_leaves_a_changed_setting() {
    let sandbox = Sandbox::new(None);
    std::fs::write(sandbox.settings(), ORIGINAL).unwrap();
    assert!(sandbox.run(&["intercept", "setup"]).status.success());
    let cleaned = sandbox.run(&["intercept", "cleanup"]);
    assert!(cleaned.status.success());
    assert!(std::fs::read_to_string(sandbox.settings()).unwrap() == ORIGINAL);
    assert!(!sandbox.intercept().exists());

    assert!(sandbox.run(&["intercept", "setup"]).status.success());
    let edited = std::fs::read_to_string(sandbox.settings())
        .unwrap()
        .replace(
            "\"CLAUDE_CODE_GZIP_REQUEST_BODIES\": \"0\"",
            "\"CLAUDE_CODE_GZIP_REQUEST_BODIES\": \"1\"",
        );
    std::fs::write(sandbox.settings(), edited).unwrap();
    let cleaned = sandbox.run(&["intercept", "cleanup"]);
    assert!(cleaned.status.success());
    assert!(text(&cleaned.stdout).contains("left CLAUDE_CODE_GZIP_REQUEST_BODIES"));
    let env = json!({"FOO": "bar", "CLAUDE_CODE_GZIP_REQUEST_BODIES": "1"});
    assert!(sandbox.document()["env"] == env);
    assert!(!sandbox.intercept().exists());
}

#[test]
fn setup_records_the_lineup_and_cleanup_removes_it_after_a_reformat() {
    let sandbox = Sandbox::new(None);
    std::fs::write(sandbox.settings(), ORIGINAL).unwrap();
    let setup = sandbox.run(&["intercept", "setup"]);
    assert!(setup.status.success());
    assert!(text(&setup.stdout).contains("set modelPicker"));
    let layout = std::fs::read_to_string(sandbox.settings()).unwrap();
    assert!(layout.find("\"permissions\"") < layout.find("\"modelPicker\""));
    let options = &sandbox.document()["modelPicker"]["options"];
    assert!(options[0]["label"].as_str().unwrap().ends_with(" (Codex)"));
    let manifest: Value = serde_json::from_slice(&sandbox.read("manifest.json")).unwrap();
    assert_eq!(manifest["keys"][0]["name"], "modelPicker");

    let reformatted = serde_json::to_vec(&sandbox.document()).unwrap();
    std::fs::write(sandbox.settings(), reformatted).unwrap();
    let cleaned = sandbox.run(&["intercept", "cleanup"]);
    assert!(cleaned.status.success());
    assert!(text(&cleaned.stdout).contains("removed modelPicker"));
    let document = sandbox.document();
    assert!(document.get("modelPicker").is_none());
    assert!(document["env"] == json!({"FOO": "bar"}));
}

#[test]
fn setup_and_cleanup_leave_the_owner_lineup() {
    let sandbox = Sandbox::new(None);
    let owner = json!({"modelPicker": {"options": [{"model": "opus", "label": "Mine"}]}});
    std::fs::write(sandbox.settings(), owner.to_string()).unwrap();
    let setup = sandbox.run(&["intercept", "setup"]);
    assert!(setup.status.success());
    assert!(text(&setup.stdout).contains("left modelPicker: set by the owner"));
    assert_eq!(sandbox.document()["modelPicker"], owner["modelPicker"]);
    let cleaned = sandbox.run(&["intercept", "cleanup"]);
    assert!(cleaned.status.success());
    assert_eq!(sandbox.document()["modelPicker"], owner["modelPicker"]);
}

#[test]
fn cleanup_without_a_manifest_removes_only_duolane_values() {
    let sandbox = Sandbox::new(None);
    std::fs::write(sandbox.settings(), ORIGINAL).unwrap();
    assert!(sandbox.run(&["intercept", "setup"]).status.success());
    std::fs::remove_file(sandbox.intercept().join("manifest.json")).unwrap();
    let ours = format!(
        "\"HTTPS_PROXY\": \"http://duolane:{}@127.0.0.1:{SETUP_PORT}\"",
        sandbox.password()
    );
    let edited = std::fs::read_to_string(sandbox.settings())
        .unwrap()
        .replace(&ours, "\"HTTPS_PROXY\": \"http://proxy.example:3128\"");
    std::fs::write(sandbox.settings(), edited).unwrap();
    let cleaned = sandbox.run(&["intercept", "cleanup"]);
    assert!(cleaned.status.success());
    let document = sandbox.document();
    let env = json!({"FOO": "bar", "HTTPS_PROXY": "http://proxy.example:3128"});
    assert!(document["env"] == env);
    assert_eq!(document["model"], "opus");
    assert!(!sandbox.intercept().exists());
}

#[test]
fn setup_keeps_the_indentation_and_cleanup_leaves_a_value_the_owner_set() {
    let sandbox = Sandbox::new(None);
    std::fs::write(sandbox.settings(), INDENTED).unwrap();
    assert!(sandbox.run(&["intercept", "setup"]).status.success());
    let written = std::fs::read_to_string(sandbox.settings()).unwrap();
    assert!(written.contains("\n        \"HTTPS_PROXY\": "));
    assert!(
        written
            .lines()
            .all(|line| (line.len() - line.trim_start_matches(' ').len()) % 4 == 0)
    );
    assert!(sandbox.run(&["intercept", "cleanup"]).status.success());
    assert!(std::fs::read_to_string(sandbox.settings()).unwrap() == INDENTED);

    assert!(sandbox.run(&["intercept", "setup"]).status.success());
    std::fs::remove_file(sandbox.intercept().join("manifest.json")).unwrap();
    assert!(sandbox.run(&["intercept", "setup"]).status.success());
    assert!(sandbox.run(&["intercept", "cleanup"]).status.success());
    let env = &sandbox.document()["env"];
    for name in ["HTTPS_PROXY", "https_proxy", "NODE_EXTRA_CA_CERTS"] {
        assert!(env.get(name).is_none(), "{name}");
    }
    assert_eq!(env["CLAUDE_CODE_GZIP_REQUEST_BODIES"], "0");
}

#[test]
fn setup_refuses_another_settings_file_until_cleanup() {
    let sandbox = Sandbox::new(None);
    std::fs::write(sandbox.settings(), ORIGINAL).unwrap();
    assert!(sandbox.run(&["intercept", "setup"]).status.success());
    let first = std::fs::read(sandbox.settings()).unwrap();
    let other = sandbox.path("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("settings.json"), ORIGINAL).unwrap();
    let refused = sandbox
        .command(SETUP_PORT)
        .args(["intercept", "setup"])
        .env("CLAUDE_CONFIG_DIR", &other)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let stderr = text(&refused.stderr);
    assert!(stderr.contains(&sandbox.settings().display().to_string()));
    assert!(stderr.contains("duolane intercept cleanup"));
    assert!(std::fs::read(sandbox.settings()).unwrap() == first);
    assert_eq!(
        std::fs::read_to_string(other.join("settings.json")).unwrap(),
        ORIGINAL
    );
}

#[test]
fn cleanup_removes_the_files_and_fails_on_a_symlinked_settings_file() {
    let sandbox = Sandbox::new(None);
    std::fs::write(sandbox.settings(), ORIGINAL).unwrap();
    assert!(sandbox.run(&["intercept", "setup"]).status.success());
    let target = sandbox.path("elsewhere.json");
    std::fs::rename(sandbox.settings(), &target).unwrap();
    std::os::unix::fs::symlink(&target, sandbox.settings()).unwrap();
    let cleaned = sandbox.run(&["intercept", "cleanup"]);
    assert!(!cleaned.status.success());
    let stderr = text(&cleaned.stderr);
    let settings = sandbox.settings().display().to_string();
    assert_eq!(stderr.matches(&settings).count(), 1);
    assert!(stderr.contains("symlink"));
    assert!(text(&cleaned.stdout).contains("removed proxy-password"));
    assert!(!sandbox.intercept().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_accounts_switch_launches_the_selected_profile_with_session_arguments() {
    use std::os::unix::fs::symlink;
    let (port, _) = listen().await;
    let sandbox = Sandbox::new(None);
    std::fs::create_dir_all(sandbox.intercept()).unwrap();
    for file in ["ca.pem", "leaf.pem", "proxy-password"] {
        std::fs::copy(intercept_dir().join(file), sandbox.intercept().join(file)).unwrap();
    }
    let profile = sandbox.config.join("claude-accounts/backup");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700)).unwrap();
    let ready = profile.join(".duolane-ready");
    std::fs::write(&ready, b"ready\n").unwrap();
    std::fs::set_permissions(&ready, std::fs::Permissions::from_mode(0o600)).unwrap();
    let cwd = sandbox.path("project").canonicalize().unwrap();
    let key: String = cwd
        .to_str()
        .unwrap()
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                b as char
            } else {
                '-'
            }
        })
        .collect();
    let source = sandbox.path("home/.claude/projects").join(&key);
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(profile.join("projects")).unwrap();
    std::fs::create_dir_all(profile.join(".duolane-projects")).unwrap();
    symlink(
        source.canonicalize().unwrap(),
        profile.join("projects").join(&key),
    )
    .unwrap();
    let record = profile.join(".duolane-projects").join(&key);
    std::fs::write(&record, cwd.to_str().unwrap()).unwrap();
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
    for args in [
        vec!["--model", "toy"],
        vec!["--resume", "session-id", "--", "tail"],
    ] {
        let mut command = sandbox.command(port);
        command
            .env_remove("CLAUDE_CONFIG_DIR")
            .args(["accounts", "switch", "backup", "--"])
            .args(&args);
        let launched = output(command).await;
        assert!(launched.status.success(), "{}", text(&launched.stderr));
        let arguments = received(&launched);
        let expected = if args[0] == "--resume" {
            args.clone()
        } else {
            [vec!["--continue"], args.clone()].concat()
        };
        assert!(
            arguments.ends_with(
                &expected
                    .iter()
                    .map(|arg| arg.to_string())
                    .collect::<Vec<_>>()
            )
        );
        assert!(text(&launched.stdout).lines().any(|line| line
            == format!(
                "CLAUDE_CONFIG_DIR={}",
                profile.canonicalize().unwrap().display()
            )));
        let mut reset = sandbox.command(port);
        reset
            .env_remove("CLAUDE_CONFIG_DIR")
            .args(["accounts", "use", "default"]);
        assert!(output(reset).await.status.success());
    }
}
