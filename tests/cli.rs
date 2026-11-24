#[path = "support/hermetic.rs"]
mod hermetic;

use assert_cmd::Command;
use predicates::str::contains;
use std::env;
#[cfg(unix)]
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn version_aliases_print_expected_version() -> Result<(), Box<dyn std::error::Error>> {
    let expected = format!("duolane {}", env!("CARGO_PKG_VERSION"));

    for arg in ["--version", "-v", "version"] {
        let mut cmd = Command::cargo_bin("duolane")?;
        cmd.arg(arg)
            .assert()
            .success()
            .stdout(contains(expected.clone()));
    }
    Ok(())
}

#[test]
fn models_prints_codex_provider() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("duolane")?;
    cmd.arg("models");
    let out = String::from_utf8(cmd.output()?.stdout)?;
    assert!(out.contains("codex:"));
    Ok(())
}

#[test]
fn models_lists_codex_when_api_key_order_is_selected() -> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    for (order, chatgpt) in [("api-key", false), ("chatgpt,api-key", true)] {
        let output = Command::cargo_bin("duolane")?
            .arg("models")
            .env("DUOLANE_CONFIG_DIR", dir.path())
            .env("CODEX_HOME", dir.path())
            .env("DUOLANE_CODEX_AUTH_ORDER", order)
            .output()?;
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout)?;
        assert!(output.contains("codex (api key): models come from your OpenAI account; a running proxy lists them at /v1/models"));
        assert_eq!(output.contains("codex: "), chatgpt);
        assert!(!output.contains("gpt-6.1-sol"));
    }
    Ok(())
}

#[test]
fn help_describes_visible_commands_and_hides_demo() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("duolane")?;
    cmd.arg("--help");
    let output = cmd.output()?;
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout)?;
    for description in [
        "Print version information",
        "Start the proxy server and monitor",
        "List supported provider models",
        "Manage Codex authentication",
    ] {
        assert!(stdout.contains(description), "missing: {description}");
    }
    assert!(!stdout.contains("demo"));
    assert!(!stdout.contains("mock data and no proxy server"));
    Ok(())
}

#[test]
fn removed_backend_commands_are_not_offered() -> Result<(), Box<dyn std::error::Error>> {
    let help = Command::cargo_bin("duolane")?.arg("--help").output()?;
    let help = String::from_utf8(help.stdout)?.to_lowercase();
    for command in ["kimi", "grok", "cursor"] {
        assert!(!help.contains(command), "help offers {command}");
        Command::cargo_bin("duolane")?
            .args([command, "auth", "status"])
            .assert()
            .failure()
            .code(2);
    }
    Ok(())
}

#[test]
fn invalid_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("duolane")?
        .arg("definitely-not-a-command")
        .assert()
        .failure()
        .code(2);
    Ok(())
}

#[test]
fn unsupported_provider_auth_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("duolane")?;
    cmd.args(["codex", "auth", "device"]);
    let output = cmd.output()?;
    assert_eq!(output.status.code(), Some(2));
    let out = String::from_utf8(output.stderr)?;
    assert!(out.contains("Codex auth is provided by the Codex CLI"));
    Ok(())
}

#[test]
fn provider_logout_without_auth_is_success() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("duolane")?;
    cmd.args(["codex", "auth", "logout"]);
    cmd.env("DUOLANE_CONFIG_DIR", temp.path());
    cmd.env("HOME", temp.path());
    cmd.env("USERPROFILE", temp.path());
    cmd.assert().success();
    Ok(())
}

#[cfg(unix)]
struct ChildGuard(Child);

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn wait_for_service(
    child: &mut ChildGuard,
    port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                if let Some(status) = child.0.try_wait()? {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.0.stderr.take() {
                        pipe.read_to_string(&mut stderr)?;
                    }
                    return Err(format!("service exited with {status}: {stderr}").into());
                }
                let _ = error;
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(unix)]
fn send_signal(child: &ChildGuard, signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("kill")
        .args([signal, &child.0.id().to_string()])
        .status()?;
    if !status.success() {
        return Err(format!("kill {signal} failed with {status}").into());
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_exit(
    child: &mut ChildGuard,
    timeout: Duration,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("plain service did not exit after the second signal".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn plain_service_exits_on_second_signal(signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let upstream = TcpListener::bind("127.0.0.1:0")?;
    let upstream_url = format!("http://{}", upstream.local_addr()?);
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let fixture = thread::spawn(move || {
        let (stream, _) = upstream.accept().unwrap();
        accepted_tx.send(()).unwrap();
        let _ = release_rx.recv();
        drop(stream);
    });

    let config = TempDir::new()?;
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_duolane"))
        .args(["serve", "--no-monitor", "--port", &port.to_string()])
        .env("DUOLANE_CONFIG_DIR", config.path())
        .env("DUOLANE_ALIAS_PROVIDER", "anthropic")
        .env("DUOLANE_ANTHROPIC_BASE_URL", upstream_url)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard(child);
    let mut downstream = wait_for_service(&mut child, port)?;
    let body = br#"{"model":"claude-haiku-5-5","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#;
    write!(
        downstream,
        "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )?;
    downstream.write_all(body)?;
    accepted_rx.recv_timeout(Duration::from_secs(20))?;

    send_signal(&child, signal)?;
    thread::sleep(Duration::from_millis(200));
    assert!(child.0.try_wait()?.is_none());
    send_signal(&child, signal)?;
    let status = wait_for_exit(&mut child, Duration::from_secs(2));
    let _ = release_tx.send(());
    fixture.join().unwrap();

    assert_eq!(status?.code(), Some(130));
    Ok(())
}

#[cfg(unix)]
#[test]
fn plain_service_exits_on_second_ctrl_c() -> Result<(), Box<dyn std::error::Error>> {
    plain_service_exits_on_second_signal("-INT")
}

#[cfg(unix)]
#[test]
fn plain_service_exits_on_second_sigterm() -> Result<(), Box<dyn std::error::Error>> {
    plain_service_exits_on_second_signal("-TERM")
}

#[cfg(unix)]
#[test]
fn agents_setup_and_remove_preserve_custom_files_and_symlinks()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = TempDir::new()?;
    let dir = temp.path().join("agents");
    let run = |args: &[&str]| -> Result<String, Box<dyn std::error::Error>> {
        let output = Command::cargo_bin("duolane")?
            .args(args)
            .env("CLAUDE_CONFIG_DIR", temp.path())
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    };
    run(&["agents", "remove"])?;
    run(&["agents", "setup"])?;
    for (family, label) in [
        ("terra", "Terra"),
        ("sol", "Sol"),
        ("astra", "Astra"),
        ("luna", "Luna"),
    ] {
        let path = dir.join(format!("{family}.md"));
        let expected = format!(
            "---\nname: {family}\ndescription: General-purpose {label} agent.\nmodel: {family}\n---\n\nFollow the task brief and project instructions.\n"
        );
        assert_eq!(std::fs::read_to_string(&path)?, expected);
        assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o644);
    }
    let output = run(&["agents", "setup"])?;
    assert_eq!(output.matches("unchanged").count(), 4);
    let terra = dir.join("terra.md");
    std::fs::write(&terra, "custom")?;
    assert!(run(&["agents", "setup"])?.contains("kept (differs; use --force)"));
    assert_eq!(std::fs::read_to_string(&terra)?, "custom");
    run(&["agents", "setup", "--force"])?;
    assert_ne!(std::fs::read_to_string(&terra)?, "custom");
    let target = temp.path().join("target");
    std::fs::write(&target, "untouched")?;
    let sol = dir.join("sol.md");
    std::fs::remove_file(&sol)?;
    symlink(&target, &sol)?;
    assert!(run(&["agents", "setup", "--force"])?.contains("symlink"));
    assert_eq!(std::fs::read_to_string(&target)?, "untouched");
    std::fs::write(&terra, "custom")?;
    let output = run(&["agents", "remove"])?;
    assert!(output.contains("kept"));
    assert!(output.contains("symlink"));
    assert_eq!(std::fs::read_to_string(&terra)?, "custom");
    assert!(std::fs::symlink_metadata(&sol)?.file_type().is_symlink());
    assert!(!dir.join("astra.md").exists());
    assert!(!dir.join("luna.md").exists());
    Ok(())
}

#[test]
fn claude_accounts_help() {
    let output = Command::cargo_bin("duolane")
        .unwrap()
        .env_clear()
        .args(["accounts", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("login") && help.contains("use") && !help.contains("run"));
    assert!(help.contains("Show each profile's account, plan, and login"));
    assert!(help.contains("Sign out of a named Claude profile and delete it"));
    for (command, description) in [
        ("login", "Sign in to a named Claude profile"),
        (
            "use",
            "Choose the profile for the next duolane claude launch",
        ),
    ] {
        assert!(help.contains(description));
        let output = Command::cargo_bin("duolane")
            .unwrap()
            .env_clear()
            .args(["accounts", command, "--help"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains("[NAME]"));
    }
}

#[cfg(unix)]
#[test]
fn claude_accounts_named_login_without_stdin() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(bin.join("claude"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.path().join("config");
    let run = |name: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_duolane"))
            .env_clear()
            .env("HOME", root.path())
            .env("PATH", &bin)
            .env("DUOLANE_CONFIG_DIR", &config)
            .env("DUOLANE_STATE_DIR", root.path().join("state"))
            .args(["accounts", "login", name])
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert!(run("backup").status.success());
    assert!(
        config
            .join("claude-accounts/backup/.duolane-ready")
            .exists()
    );
    assert!(!run("default").status.success());
    assert!(!run("Bad!").status.success());
}

#[cfg(unix)]
#[test]
fn claude_accounts_named_selection_without_stdin() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(bin.join("claude"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.path().join("config");
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_duolane"))
            .env_clear()
            .env("HOME", root.path())
            .env("PATH", &bin)
            .env("DUOLANE_CONFIG_DIR", &config)
            .env("DUOLANE_STATE_DIR", root.path().join("state"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert!(
        Command::cargo_bin("duolane")
            .unwrap()
            .env_clear()
            .env("HOME", root.path())
            .env("PATH", &bin)
            .env("DUOLANE_CONFIG_DIR", &config)
            .env("DUOLANE_STATE_DIR", root.path().join("state"))
            .args(["accounts", "login"])
            .write_stdin("backup\n")
            .output()
            .unwrap()
            .status
            .success()
    );
    let selector = config.join("claude-accounts/.selected");
    assert!(run(&["accounts", "use", "backup"]).status.success());
    assert_eq!(std::fs::read(&selector).unwrap(), b"backup\n");
    assert!(run(&["accounts", "use", "default"]).status.success());
    assert_eq!(std::fs::read(&selector).unwrap(), b"default\n");
    for (name, message) in [
        ("nope", "unknown profile"),
        ("Bad!", "invalid profile name"),
    ] {
        let output = run(&["accounts", "use", name]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
        assert_eq!(std::fs::read(&selector).unwrap(), b"default\n");
    }
    std::fs::create_dir(config.join("claude-accounts/unready")).unwrap();
    std::fs::set_permissions(
        config.join("claude-accounts/unready"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(
        config.join("claude-accounts/backup"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    for name in ["unready", "backup"] {
        let output = run(&["accounts", "use", name]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unknown profile"));
        assert_eq!(std::fs::read(&selector).unwrap(), b"default\n");
    }
    std::fs::write(&selector, b"malformed").unwrap();
    assert!(!run(&["accounts", "use", "default"]).status.success());
    assert_eq!(std::fs::read(&selector).unwrap(), b"malformed");
}

#[cfg(unix)]
#[test]
fn claude_accounts_login_registers_only_success() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\n[ \"$*\" = 'auth login --claudeai' ] && [ \"$CLAUDE_CONFIG_DIR\" = \"$EXPECTED\" ] && [ \"$FAIL\" != 1 ]\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.path().join("config");
    let run = |name: &str, fail: &str| {
        Command::cargo_bin("duolane")
            .unwrap()
            .env_clear()
            .env("PATH", &bin)
            .env("HOME", root.path())
            .env("DUOLANE_CONFIG_DIR", &config)
            .env("DUOLANE_STATE_DIR", root.path().join("state"))
            .env(
                "EXPECTED",
                root.path()
                    .canonicalize()
                    .unwrap()
                    .join("config/claude-accounts")
                    .join(name),
            )
            .env("FAIL", fail)
            .args(["accounts", "login"])
            .write_stdin(format!("{name}\n"))
            .output()
            .unwrap()
    };
    assert!(!run("failed", "1").status.success());
    assert!(
        !config
            .join("claude-accounts/failed/.duolane-ready")
            .exists()
    );
    assert!(run("backup", "0").status.success());
    assert!(
        config
            .join("claude-accounts/backup/.duolane-ready")
            .exists()
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(config.join("claude-accounts/.profile-backup.lock"))
        .unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    assert!(!run("backup", "0").status.success());
    drop(lock);
    assert!(run("backup", "0").status.success());
    let override_result = Command::cargo_bin("duolane")
        .unwrap()
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", &bin)
        .env("DUOLANE_CONFIG_DIR", &config)
        .env("ANTHROPIC_API_KEY", "toy")
        .args(["accounts", "login"])
        .write_stdin("blocked\n")
        .output()
        .unwrap();
    assert!(!override_result.status.success());
    assert!(!config.join("claude-accounts/blocked").exists());
}

#[cfg(unix)]
#[test]
fn claude_accounts_login_rejects_profile_replacement() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\n/bin/mv \"$CLAUDE_CONFIG_DIR\" \"$CLAUDE_CONFIG_DIR.old\" && /bin/mkdir -m 700 \"$CLAUDE_CONFIG_DIR\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.path().join("config");
    let output = Command::cargo_bin("duolane")
        .unwrap()
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", bin)
        .env("DUOLANE_CONFIG_DIR", &config)
        .env("DUOLANE_STATE_DIR", root.path().join("state"))
        .args(["accounts", "login"])
        .write_stdin("backup\n")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        !config
            .join("claude-accounts/backup/.duolane-ready")
            .exists()
    );
    assert!(
        !config
            .join("claude-accounts/backup.old/.duolane-ready")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn claude_accounts_login_rejects_permissions_widened_by_claude() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\n/bin/chmod 755 \"$CLAUDE_CONFIG_DIR\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.path().join("config");
    let output = Command::cargo_bin("duolane")
        .unwrap()
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", &bin)
        .env("DUOLANE_CONFIG_DIR", &config)
        .env("DUOLANE_STATE_DIR", root.path().join("state"))
        .args(["accounts", "login"])
        .write_stdin("backup\n")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        !config
            .join("claude-accounts/backup/.duolane-ready")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn claude_accounts_selects_default_and_named() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = TempDir::new().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(bin.join("claude"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.path().join("config");
    let state = root.path().join("state");
    let run = |args: &[&str], input: &str| {
        Command::cargo_bin("duolane")
            .unwrap()
            .env_clear()
            .env("HOME", root.path())
            .env("PATH", &bin)
            .env("DUOLANE_CONFIG_DIR", &config)
            .env("DUOLANE_STATE_DIR", &state)
            .args(args)
            .write_stdin(input)
            .output()
            .unwrap()
    };
    assert!(run(&["accounts", "login"], "backup\n").status.success());
    assert!(run(&["accounts", "use"], "2\n").status.success());
    symlink("absent", config.join("claude-accounts/unsafeprofile")).unwrap();
    let recovery = run(&["accounts", "use"], "1\n");
    assert!(recovery.status.success());
    assert!(
        String::from_utf8_lossy(&recovery.stderr).contains("skipping unsafe profile unsafeprofile")
    );
    assert!(run(&["accounts", "use"], "2\n").status.success());
    let selector = config.join("claude-accounts/.selected");
    assert_eq!(std::fs::read(&selector).unwrap(), b"backup\n");
    assert!(!run(&["accounts", "use"], "unknown\n").status.success());
    assert_eq!(std::fs::read(&selector).unwrap(), b"backup\n");
    assert!(run(&["accounts", "login"], "other\n").status.success());
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(config.join("claude-accounts/.profile-backup.lock"))
        .unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    assert!(!run(&["accounts", "use", "backup"], "").status.success());
    assert!(run(&["accounts", "use", "other"], "").status.success());
    assert!(run(&["accounts", "use", "default"], "").status.success());
    drop(lock);
    assert_eq!(std::fs::read(&selector).unwrap(), b"default\n");
    std::fs::write(&selector, b"stale\n").unwrap();
    assert!(run(&["accounts", "use"], "1\n").status.success());
    assert_eq!(std::fs::read(&selector).unwrap(), b"default\n");
    std::fs::write(&selector, b"malformed").unwrap();
    assert!(!run(&["accounts", "use"], "1\n").status.success());
    std::fs::remove_file(&selector).unwrap();
    symlink("elsewhere", &selector).unwrap();
    assert!(!run(&["accounts", "use"], "1\n").status.success());
}

#[cfg(unix)]
#[test]
fn claude_accounts_selection_follows_config_alias() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = TempDir::new().unwrap();
    let config = root.path().join("config");
    let alias = root.path().join("alias");
    let bin = root.path().join("bin");
    std::fs::create_dir(&config).unwrap();
    std::fs::create_dir(&bin).unwrap();
    symlink(&config, &alias).unwrap();
    std::fs::write(bin.join("claude"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let run = |dir: &std::path::Path, args: &[&str], input: &str| {
        Command::cargo_bin("duolane")
            .unwrap()
            .env_clear()
            .env("HOME", root.path())
            .env("PATH", &bin)
            .env("DUOLANE_CONFIG_DIR", dir)
            .env("DUOLANE_STATE_DIR", root.path().join("state"))
            .args(args)
            .write_stdin(input)
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(run(&config, &["accounts", "login"], "backup\n"));
    assert!(run(&alias, &["accounts", "use"], "2\n"));
    assert_eq!(
        std::fs::read(config.join("claude-accounts/.selected")).unwrap(),
        b"backup\n"
    );
    assert!(run(&config, &["accounts", "use"], "1\n"));
    assert_eq!(
        std::fs::read(alias.join("claude-accounts/.selected")).unwrap(),
        b"default\n"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn codex_api_auth_cli_keeps_key_and_order_safe() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let dir = temp.path().canonicalize()?;
    let run = |args: &[&str],
               input: Option<&[u8]>,
               extra: &[(&str, &str)]|
     -> Result<std::process::Output, Box<dyn std::error::Error>> {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_duolane"));
        command
            .args(args)
            .env("DUOLANE_CONFIG_DIR", &dir)
            .env_remove("DUOLANE_CODEX_AUTH_ORDER")
            .env_remove("DUOLANE_OPENAI_API_KEY")
            .env("DUOLANE_OPENAI_BASE_URL", "invalid")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra {
            command.env(key, value);
        }
        let mut child = command.spawn()?;
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input)?;
        } else {
            drop(child.stdin.take());
        }
        Ok(child.wait_with_output()?)
    };
    let result = run(
        &["codex", "auth", "key", "set", "--no-verify"],
        Some(b"toy-credential\n"),
        &[],
    )?;
    assert!(result.status.success());
    for (status_code, size, should_succeed) in [
        (200, 2, true),
        (401, 2, false),
        (200, 1024 * 1024 + 1, false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let count = socket.read(&mut request).unwrap();
            let expected = if should_succeed {
                b"Bearer other-toy".as_slice()
            } else {
                b"Bearer failed-toy".as_slice()
            };
            let correct_key = request[..count]
                .windows(expected.len())
                .any(|part| part == expected);
            let header = format!(
                "HTTP/1.1 {status_code} response\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
            );
            let _ = socket.write_all(header.as_bytes());
            let _ = socket.write_all(&vec![b'x'; size]);
            correct_key
        });
        let result = run(
            &["codex", "auth", "key", "set"],
            Some(if should_succeed {
                b"other-toy\n"
            } else {
                b"failed-toy\n"
            }),
            &[
                ("DUOLANE_OPENAI_BASE_URL", &url),
                ("DUOLANE_OPENAI_API_KEY", "env-toy"),
            ],
        )?;
        assert!(server.join().unwrap());
        assert_eq!(result.status.success(), should_succeed);
        let stored = std::fs::read(dir.join("credentials/openai-api-key"))?;
        assert!(stored == b"other-toy\n");
        let entered = if should_succeed {
            b"other-toy".as_slice()
        } else {
            b"failed-toy".as_slice()
        };
        assert!(!result.stdout.windows(entered.len()).any(|v| v == entered));
        assert!(!result.stderr.windows(entered.len()).any(|v| v == entered));
    }
    let order = run(&["codex", "auth", "order"], None, &[])?;
    assert!(order.status.success());
    assert!(String::from_utf8_lossy(&order.stdout).contains("chatgpt"));
    let status = run(&["codex", "auth", "status"], None, &[])?;
    assert!(!status.status.success());
    assert!(!status.stdout.windows(9).any(|v| v == b"other-toy"));
    assert!(!status.stderr.windows(9).any(|v| v == b"other-toy"));
    let change = run(&["codex", "auth", "order", "api-key"], None, &[])?;
    assert!(change.status.success());
    assert!(String::from_utf8_lossy(&change.stdout).contains("Restart"));
    let status = run(&["codex", "auth", "status"], None, &[])?;
    assert!(status.status.success());
    assert!(!status.stdout.windows(9).any(|v| v == b"other-toy"));
    assert!(!status.stderr.windows(9).any(|v| v == b"other-toy"));
    let removed = run(
        &["codex", "auth", "key", "remove"],
        None,
        &[("DUOLANE_OPENAI_API_KEY", "toy-env")],
    )?;
    assert!(removed.status.success());
    assert!(String::from_utf8_lossy(&removed.stdout).contains("environment key remains"));
    assert!(!removed.stdout.windows(7).any(|v| v == b"toy-env"));
    let reset = run(&["codex", "auth", "order", "--reset"], None, &[])?;
    assert!(reset.status.success());
    assert!(String::from_utf8_lossy(&reset.stdout).contains("chatgpt"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn codex_api_auth_cli_rejects_non_unicode_key_without_panic()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStringExt;
    let temp = TempDir::new()?;
    let secret = b"toy-nonunicode-\xff";
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_duolane"))
        .args(["codex", "auth", "status"])
        .env("DUOLANE_CONFIG_DIR", temp.path())
        .env(
            "DUOLANE_OPENAI_API_KEY",
            std::ffi::OsString::from_vec(secret.to_vec()),
        )
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        output
            .stdout
            .windows(b"No Codex credentials".len())
            .any(|s| s == b"No Codex credentials")
    );
    assert!(
        output
            .stdout
            .windows(b"invalid OpenAI API key".len())
            .any(|s| s == b"invalid OpenAI API key")
    );
    let prefix = b"toy-nonunicode-";
    assert!(!output.stdout.windows(prefix.len()).any(|s| s == prefix));
    assert!(!output.stderr.windows(prefix.len()).any(|s| s == prefix));
    assert!(
        !output
            .stderr
            .windows(b"panicked".len())
            .any(|s| s == b"panicked")
    );
    Ok(())
}

#[test]
fn codex_api_auth_cli_rejects_positional_key_without_disclosure()
-> Result<(), Box<dyn std::error::Error>> {
    let secret = b"positional-toy";
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_duolane"))
        .args(["codex", "auth", "key", "set", "positional-toy"])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(!output.stdout.windows(secret.len()).any(|s| s == secret));
    assert!(!output.stderr.windows(secret.len()).any(|s| s == secret));
    Ok(())
}

#[cfg(unix)]
#[test]
fn serve_rejects_invalid_order_before_binding() -> Result<(), Box<dyn std::error::Error>> {
    let config = TempDir::new()?;
    let occupied = TcpListener::bind("127.0.0.1:0")?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_duolane"))
        .args([
            "serve",
            "--no-monitor",
            "--port",
            &occupied.local_addr()?.port().to_string(),
        ])
        .env("DUOLANE_CONFIG_DIR", config.path())
        .env("DUOLANE_CODEX_AUTH_ORDER", "invalid")
        .env("DUOLANE_OPENAI_API_KEY", "toy-key")
        .env("DUOLANE_OPENAI_BASE_URL", "http://127.0.0.1:1/v1")
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("DUOLANE_CODEX_AUTH_ORDER"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn serve_fetches_models_only_once_when_api_key_is_selected()
-> Result<(), Box<dyn std::error::Error>> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    let upstream = TcpListener::bind("127.0.0.1:0")?;
    upstream.set_nonblocking(true)?;
    let base = format!("http://{}/v1", upstream.local_addr()?);
    let calls = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let seen = calls.clone();
    let stopped = done.clone();
    let fixture = thread::spawn(move || {
        while !stopped.load(Ordering::SeqCst) {
            match upstream.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0; 4096];
                    let _ = stream.read(&mut request);
                    seen.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(_) => break,
            }
        }
    });
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        for (order, expected) in [(None, 0), (Some("api-key"), 1)] {
            let config = TempDir::new()?;
            let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_duolane"));
            command.env_remove("DUOLANE_CODEX_AUTH_ORDER");
            if let Some(order) = order {
                command.env("DUOLANE_CODEX_AUTH_ORDER", order);
            }
            let child = command
                .args(["serve", "--no-monitor", "--port", &port.to_string()])
                .env("DUOLANE_CONFIG_DIR", config.path())
                .env("CODEX_HOME", config.path())
                .env("DUOLANE_OPENAI_API_KEY", "toy-key")
                .env("DUOLANE_OPENAI_BASE_URL", &base)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut child = ChildGuard(child);
            let mut stream = wait_for_service(&mut child, port)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.write_all(
                b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            assert!(response.starts_with("HTTP/1.1 200 "));
            assert_eq!(calls.load(Ordering::SeqCst), expected);
            child.0.kill()?;
            child.0.wait()?;
            let mut stderr = String::new();
            child
                .0
                .stderr
                .take()
                .unwrap()
                .take(65536)
                .read_to_string(&mut stderr)?;
            assert_eq!(stderr.contains("codex login"), order.is_none());
            let mut stdout = String::new();
            child
                .0
                .stdout
                .take()
                .unwrap()
                .take(65536)
                .read_to_string(&mut stdout)?;
            assert_eq!(stdout.contains("codex login"), order.is_none());
        }
        Ok(())
    })();
    done.store(true, Ordering::SeqCst);
    fixture.join().unwrap();
    result
}
