#![cfg(unix)]

#[path = "support/hermetic.rs"]
mod hermetic;

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

const FAKE_CLAUDE: &str = r#"#!/bin/sh
[ -n "$DUOLANE_OPENAI_API_KEY" ] && exit 4
case "$*" in
"auth login --claudeai") exit 0 ;;
"auth logout")
    [ "$LOGOUT_FAIL" = 1 ] && exit 1
    if [ -n "$WAIT_DIR" ]; then
        : >"$WAIT_DIR/started"
        while [ ! -e "$WAIT_DIR/release" ]; do /bin/sleep 0.05; done
    fi
    printf '%s' "$CLAUDE_CONFIG_DIR" >"$LOGOUT_FILE"
    ;;
"auth status --json")
    if [ -n "$STATUS_READY" ]; then
        printf '%s' "$$" >"$STATUS_READY"
        exec /bin/sleep 3
    fi
    if [ "$STATUS_HANG" = 1 ]; then
        printf '{"loggedIn":true,"email":"late@example.com"}'
        exec 1>&-
        /bin/sleep 20
        exit 0
    fi
    [ -n "$ANTHROPIC_API_KEY" ] && exit 3
    case "$CLAUDE_CONFIG_DIR" in
    */zzz) printf '{"loggedIn":true,"email":"third@example.com"}' ;;
    */backup) printf '{"loggedIn":true,"email":"second@example.com","orgId":"org-2","subscriptionType":"max","loginExpiresAt":"2999-01-01T00:00:00Z"}' ;;
    */copy | "") printf '{"loggedIn":true,"email":"First@example.com","orgId":"org-1","subscriptionType":"team"}' ;;
    */evil) printf '{"loggedIn":true,"email":"evil\\u001b[31m@example.com","subscriptionType":"pro"}' ;;
    */big) /usr/bin/yes | /usr/bin/head -c 70000 ;;
    */badexit) printf '{"loggedIn":true,"email":"bad@example.com"}'; exit 2 ;;
    *)
        printf '{"loggedIn":false}'
        exit 1
        ;;
    esac
    ;;
*) exit 2 ;;
esac
"#;

struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let fake = bin.join("claude");
        std::fs::write(&fake, FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn config(&self) -> PathBuf {
        self.root.path().join("config")
    }

    fn profile(&self, name: &str) -> PathBuf {
        self.config().join("claude-accounts").join(name)
    }

    fn logout_file(&self) -> PathBuf {
        self.root.path().join("logout")
    }

    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_duolane"))
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", self.root.path().join("bin"))
            .env("DUOLANE_CONFIG_DIR", self.config())
            .env("DUOLANE_STATE_DIR", self.root.path().join("state"))
            .env("LOGOUT_FILE", self.logout_file())
            .env("DUOLANE_OPENAI_API_KEY", "toy-key")
            .envs(extra.iter().copied())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn login(&self, name: &str) -> Output {
        let output = self.run(&["accounts", "login", name], &[]);
        assert!(output.status.success());
        output
    }
}

#[test]
fn guarded_profile_commands_reject_recreated_and_existing_profiles() {
    use std::os::unix::fs::MetadataExt;
    let sandbox = Sandbox::new();
    sandbox.login("backup");
    let old = std::fs::metadata(sandbox.profile("backup")).unwrap();
    let birth = old
        .created()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let identity = format!(
        "{}:{}:{}:{}",
        old.dev(),
        old.ino(),
        birth.as_secs(),
        birth.subsec_nanos()
    );
    std::fs::rename(sandbox.profile("backup"), sandbox.profile("old")).unwrap();
    sandbox.login("backup");
    let stale = sandbox.run(
        &[
            "accounts", "remove", "backup", "--yes", "--expect", &identity,
        ],
        &[],
    );
    assert!(!stale.status.success());
    assert!(sandbox.profile("backup").exists());
    assert!(!sandbox.logout_file().exists());
    let absent = sandbox.run(&["accounts", "login", "backup", "--expect-absent"], &[]);
    assert!(!absent.status.success());
    let current = std::fs::metadata(sandbox.profile("backup")).unwrap();
    let birth = current
        .created()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let identity = format!(
        "{}:{}:{}:{}",
        current.dev(),
        current.ino(),
        birth.as_secs(),
        birth.subsec_nanos()
    );
    let matching = sandbox.run(
        &[
            "accounts", "remove", "backup", "--yes", "--expect", &identity,
        ],
        &[],
    );
    assert!(matching.status.success());
    assert!(!sandbox.profile("backup").exists());
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn row<'a>(table: &'a str, name: &str) -> &'a str {
    table
        .lines()
        .find(|line| line.split_whitespace().any(|word| word == name))
        .unwrap()
}

#[test]
fn claude_accounts_list_shows_accounts_selection_and_duplicates() {
    let sandbox = Sandbox::new();
    for name in ["backup", "copy", "evil", "big", "signedout"] {
        sandbox.login(name);
    }
    assert!(
        sandbox
            .run(&["accounts", "use", "backup"], &[])
            .status
            .success()
    );
    let output = sandbox.run(&["accounts", "list"], &[("ANTHROPIC_API_KEY", "toy")]);
    assert!(output.status.success());
    let table = text(&output.stdout);
    assert!(row(&table, "default").contains("First@example.com"));
    assert!(row(&table, "default").contains("team"));
    assert!(!row(&table, "default").contains('●'));
    let backup = row(&table, "backup");
    assert!(backup.contains('●') && backup.contains("second@example.com"));
    assert!(backup.contains("max") && backup.contains("d left"));
    assert!(row(&table, "copy").contains("same account as default"));
    assert!(!backup.contains("same account"));
    assert!(row(&table, "signedout").contains("not signed in"));
    assert!(row(&table, "big").contains("unknown"));
    assert!(row(&table, "evil").contains("evil[31m@example.com"));
    assert!(!table.contains('\u{1b}'));
    assert!(table.contains("all accounts: 5H - used, 7D - used (0 of"));

    let state = sandbox.root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let usage = state.join("claude-usage.json");
    std::fs::write(
        &usage,
        r#"{"profiles":{"backup":{"five_hour":{"used":0.25,"resets_at":4102444800},"seven_day":{"used":0.5},"observed_at":1},"../x":{"observed_at":2}}}"#,
    )
    .unwrap();
    std::fs::set_permissions(&usage, std::fs::Permissions::from_mode(0o600)).unwrap();
    let table = text(&sandbox.run(&["accounts", "list"], &[]).stdout);
    assert!(row(&table, "backup").contains("25%") && row(&table, "backup").contains("50%"));
    assert!(table.contains("all accounts: 5H 25% used, 7D 50% used (1 of"));
    std::fs::set_permissions(&usage, std::fs::Permissions::from_mode(0o644)).unwrap();
    let table = text(&sandbox.run(&["accounts", "list"], &[]).stdout);
    assert!(!row(&table, "backup").contains("25%"));
}

#[test]
fn claude_accounts_status_interrupt_cleans_up_child() {
    use std::time::{Duration, Instant};
    for signal in [rustix::process::Signal::HUP, rustix::process::Signal::TERM] {
        let sandbox = Sandbox::new();
        let ready = sandbox.root.path().join("status-ready");
        let mut child = Command::new(env!("CARGO_BIN_EXE_duolane"))
            .env_clear()
            .env("HOME", sandbox.root.path())
            .env("PATH", sandbox.root.path().join("bin"))
            .env("DUOLANE_CONFIG_DIR", sandbox.config())
            .env("DUOLANE_STATE_DIR", sandbox.root.path().join("state"))
            .env("STATUS_READY", &ready)
            .args(["accounts", "list"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid = std::fs::read_to_string(&ready).ok();
        let sent = if pid.is_some() && child.try_wait().unwrap().is_none() {
            rustix::process::kill_process(
                rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
                signal,
            )
            .is_ok()
        } else {
            false
        };
        let status = child.wait().unwrap();
        let alive = |pid: &str| {
            Command::new("/bin/kill")
                .args(["-0", pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let cleaned = pid.as_deref().is_some_and(|pid| !alive(pid));
        if let Some(pid) = &pid {
            let deadline = Instant::now() + Duration::from_secs(4);
            while alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(sent && !status.success() && cleaned);
    }
}

#[test]
fn claude_accounts_status_rejects_timeout_after_json() {
    let sandbox = Sandbox::new();
    let started = std::time::Instant::now();
    let output = sandbox.run(&["accounts", "list"], &[("STATUS_HANG", "1")]);
    assert!(output.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
    assert!(row(&text(&output.stdout), "default").contains("unknown"));
}

#[test]
fn claude_accounts_status_rejects_unexpected_exit() {
    let sandbox = Sandbox::new();
    sandbox.login("badexit");
    let output = sandbox.run(&["accounts", "list"], &[]);
    assert!(output.status.success());
    assert!(row(&text(&output.stdout), "badexit").contains("unknown"));
}

#[test]
fn claude_accounts_login_reports_account_and_duplicate() {
    let sandbox = Sandbox::new();
    let backup = sandbox.login("backup");
    assert!(text(&backup.stdout).contains("second@example.com"));
    assert!(!text(&backup.stderr).contains("same account"));
    let copy = sandbox.login("copy");
    assert!(text(&copy.stdout).contains("First@example.com"));
    assert!(text(&copy.stderr).contains("same account as default"));
}

#[test]
fn claude_accounts_remove_preserves_concurrent_selection() {
    let sandbox = Sandbox::new();
    sandbox.login("backup");
    sandbox.login("copy");
    assert!(
        sandbox
            .run(&["accounts", "use", "backup"], &[])
            .status
            .success()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_duolane"))
        .env_clear()
        .env("HOME", sandbox.root.path())
        .env("PATH", sandbox.root.path().join("bin"))
        .env("DUOLANE_CONFIG_DIR", sandbox.config())
        .env("DUOLANE_STATE_DIR", sandbox.root.path().join("state"))
        .env("LOGOUT_FILE", sandbox.logout_file())
        .env("WAIT_DIR", sandbox.root.path())
        .args(["accounts", "remove", "backup", "--yes"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !sandbox.root.path().join("started").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let started = sandbox.root.path().join("started").exists();
    let stable_lock = sandbox
        .config()
        .join("claude-accounts/.profile-backup.lock")
        .is_file();
    let busy = !sandbox
        .run(&["accounts", "login", "backup"], &[])
        .status
        .success();
    let selected = sandbox
        .run(&["accounts", "use", "copy"], &[])
        .status
        .success();
    std::fs::write(sandbox.root.path().join("release"), b"").unwrap();
    let removed = child.wait().unwrap().success();
    assert!(started && stable_lock && busy && selected && removed);
    assert_eq!(
        std::fs::read(sandbox.config().join("claude-accounts/.selected")).unwrap(),
        b"copy\n"
    );
}

#[test]
fn claude_accounts_remove_signs_out_and_keeps_shared_history() {
    let sandbox = Sandbox::new();
    sandbox.login("backup");
    let shared = sandbox.root.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::write(shared.join("session.jsonl"), b"history").unwrap();
    let projects = sandbox.profile("backup").join("projects");
    std::fs::create_dir(&projects).unwrap();
    symlink(&shared, projects.join("project")).unwrap();
    assert!(
        sandbox
            .run(&["accounts", "use", "backup"], &[])
            .status
            .success()
    );
    let kept = |output: Output, message: &str| {
        assert!(!output.status.success());
        assert!(text(&output.stderr).contains(message));
        assert!(sandbox.profile("backup").join(".duolane-ready").exists());
        assert!(!sandbox.logout_file().exists());
    };
    kept(
        sandbox.run(&["accounts", "remove", "backup"], &[]),
        "requires terminal confirmation",
    );
    kept(
        sandbox.run(&["accounts", "remove", "default", "--yes"], &[]),
        "invalid profile name",
    );
    kept(
        sandbox.run(&["accounts", "remove", "nope", "--yes"], &[]),
        "unknown profile",
    );
    kept(
        sandbox.run(
            &["accounts", "remove", "backup", "--yes"],
            &[("LOGOUT_FAIL", "1")],
        ),
        "Claude logout failed",
    );
    let removed = sandbox.run(&["accounts", "remove", "backup", "--yes"], &[]);
    assert!(removed.status.success());
    assert!(!sandbox.profile("backup").exists());
    assert_eq!(
        Path::new(&std::fs::read_to_string(sandbox.logout_file()).unwrap()),
        sandbox
            .config()
            .canonicalize()
            .unwrap()
            .join("claude-accounts/backup")
    );
    assert_eq!(
        std::fs::read(shared.join("session.jsonl")).unwrap(),
        b"history"
    );
    assert_eq!(
        std::fs::read(sandbox.config().join("claude-accounts/.selected")).unwrap(),
        b"default\n"
    );
}

fn selected(sandbox: &Sandbox) -> Option<Vec<u8>> {
    std::fs::read(sandbox.config().join("claude-accounts/.selected")).ok()
}

#[test]
fn claude_accounts_switch_next_skips_same_account_and_keeps_selection_on_launch_failure() {
    let sandbox = Sandbox::new();
    sandbox.login("copy");
    sandbox.login("signedout");
    sandbox.login("zzz");
    let output = sandbox.run(&["accounts", "switch", "--next"], &[]);
    assert!(!output.status.success());
    assert_eq!(selected(&sandbox).as_deref(), Some(&b"zzz\n"[..]));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("zzz is now selected"));
    assert!(stderr.contains("same Claude arguments"));
    assert!(stderr.contains("duolane claude --continue"));
    let output = sandbox.run(
        &[
            "accounts",
            "switch",
            "default",
            "--",
            "--resume",
            "private-session",
        ],
        &[],
    );
    let stderr = text(&output.stderr);
    assert!(stderr.contains("duolane claude`"));
    assert!(!stderr.contains("--continue"));
    assert!(!stderr.contains("private-session"));
}

#[test]
fn claude_accounts_switch_rejects_current_selection_and_lone_account() {
    let sandbox = Sandbox::new();
    sandbox.login("copy");
    let current = sandbox.run(&["accounts", "switch", "default"], &[]);
    assert!(text(&current.stderr).contains("already selected"));
    let lone = sandbox.run(&["accounts", "switch", "--next"], &[]);
    assert!(text(&lone.stderr).contains("no other profile"));
    let both = sandbox.run(&["accounts", "switch", "copy", "--next"], &[]);
    assert!(!both.status.success());
    assert_eq!(selected(&sandbox), None);
}

#[test]
fn claude_accounts_switch_refuses_inside_claude_code_or_config_override() {
    let sandbox = Sandbox::new();
    sandbox.login("backup");
    let inside = sandbox.run(&["accounts", "switch", "backup"], &[("CLAUDECODE", "1")]);
    assert!(text(&inside.stderr).contains("CLAUDECODE is set"));
    let config = sandbox.root.path().join("explicit");
    let overridden = sandbox.run(
        &["accounts", "switch", "backup"],
        &[("CLAUDE_CONFIG_DIR", config.to_str().unwrap())],
    );
    assert!(text(&overridden.stderr).contains("CLAUDE_CONFIG_DIR is set"));
    assert_eq!(selected(&sandbox), None);
}

#[test]
fn claude_accounts_switch_rejects_environment_before_selection() {
    let sandbox = Sandbox::new();
    sandbox.login("backup");
    let output = sandbox.run(
        &[
            "accounts",
            "switch",
            "backup",
            "--",
            "--environment=example",
        ],
        &[],
    );
    assert!(text(&output.stderr).contains("--environment is unsupported"));
    assert_eq!(selected(&sandbox), None);
}

#[test]
fn claude_accounts_switch_next_allows_unknown_status() {
    let sandbox = Sandbox::new();
    sandbox.login("badexit");
    let output = sandbox.run(&["accounts", "switch", "--next"], &[]);
    assert!(!output.status.success());
    assert_eq!(selected(&sandbox).as_deref(), Some(&b"badexit\n"[..]));
}
