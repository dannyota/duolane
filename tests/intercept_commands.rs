#![cfg(unix)]

#[path = "support/hermetic.rs"]
mod hermetic;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use duolane::intercept::{InterceptListener, intercept_dir};
use serde_json::{Value, json};
use tempfile::TempDir;

const SETUP_PORT: u16 = 40123;
const ORIGINAL: &str = "{\n  \"model\": \"opus\",\n  \"env\": {\n    \"FOO\": \"bar\"\n  },\n  \"permissions\": {\n    \"allow\": [\n      \"Bash(ls)\"\n    ]\n  }\n}\n";
const FAKE_CLAUDE: &str = "#!/bin/sh\nfor argument in \"$@\"; do printf 'arg:%s\\n' \"$argument\"; done\necho env:\nenv\n";
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
        for dir in ["home", "state", "claude", "project", "bin"] {
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
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
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

#[tokio::test(flavor = "multi_thread")]
async fn claude_starts_with_the_intercept_environment_and_unchanged_arguments() {
    let (port, password) = listen().await;
    let sandbox = Sandbox::new(hermetic_config());
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
    let stdout = text(&started.stdout);
    let received: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("arg:"))
        .collect();
    assert_eq!(received, arguments);
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
async fn claude_refuses_a_down_listener_and_another_proxy() {
    let (port, _) = listen().await;
    let sandbox = Sandbox::new(hermetic_config());
    let mut command = sandbox.command(free_port());
    command.args(["claude", "-p", "hi"]);
    let down = output(command).await;
    assert!(!down.status.success());
    assert!(text(&down.stderr).contains("is down"));
    assert!(down.stdout.is_empty());

    let mut command = sandbox.command(port);
    command
        .args(["claude", "-p", "hi"])
        .env("HTTPS_PROXY", "http://proxy.example:3128");
    let proxied = output(command).await;
    assert!(!proxied.status.success());
    assert!(text(&proxied.stderr).contains("HTTPS_PROXY"));
    assert!(proxied.stdout.is_empty());
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
