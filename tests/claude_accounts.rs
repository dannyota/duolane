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
    printf '%s' "$CLAUDE_CONFIG_DIR" >"$LOGOUT_FILE"
    ;;
"auth status --json")
    [ -n "$ANTHROPIC_API_KEY" ] && exit 3
    case "$CLAUDE_CONFIG_DIR" in
    */backup) printf '{"loggedIn":true,"email":"second@example.com","orgId":"org-2","subscriptionType":"max","loginExpiresAt":"2999-01-01T00:00:00Z"}' ;;
    */copy | "") printf '{"loggedIn":true,"email":"First@example.com","orgId":"org-1","subscriptionType":"team"}' ;;
    */evil) printf '{"loggedIn":true,"email":"evil\\u001b[31m@example.com","subscriptionType":"pro"}' ;;
    */big) /usr/bin/yes | /usr/bin/head -c 70000 ;;
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
