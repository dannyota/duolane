use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;
#[test]
fn claude_accounts_cwd_changes_are_refused() {
    assert!(
        Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .stdin(std::process::Stdio::null())
            .args([
                "--exact",
                "--ignored",
                "claude_accounts::unix::tests::claude_accounts_cwd_probe",
            ])
            .status()
            .unwrap()
            .success()
    );
}
#[test]
#[ignore]
fn claude_accounts_cwd_probe() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let work = root.join("work");
    let moved = root.join("moved");
    let profile_dir = root.join("profile");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&profile_dir).unwrap();
    std::fs::set_permissions(&profile_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::env::set_current_dir(&work).unwrap();
    let mut profile = Profile::test_profile(&profile_dir);
    let default = root.join(".claude");
    assert!(
        link_launch(&profile, &default, &work, |_| {
            std::fs::rename(&work, &moved)?;
            Ok(true)
        })
        .is_err()
    );
    assert!(!profile_dir.join("projects").exists());
    std::fs::rename(&moved, &work).unwrap();
    link_launch(&profile, &default, &work, |_| Ok(true)).unwrap();
    profile.cwd = Some(work.clone());
    assert!(profile.recheck().is_ok());
    std::fs::rename(&work, &moved).unwrap();
    assert!(profile.recheck().is_err());
    std::env::set_current_dir(&root).unwrap();
}
#[test]
#[ignore]
fn claude_accounts_lock_child_probe() {
    println!("ready");
    std::io::stdout().flush().unwrap();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf).unwrap();
}
#[test]
fn claude_accounts_login_child_retains_lock() {
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    let dir = File::open(root.path()).unwrap();
    let guard = store::try_lock(&dir).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .args([
            "--exact",
            "--ignored",
            "--nocapture",
            "claude_accounts::unix::tests::claude_accounts_lock_child_probe",
        ]);
    inherit_login_lock(&mut command, &guard).unwrap();
    let mut child = command.spawn().unwrap();
    drop(command);
    drop(guard);
    let mut line = String::new();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    while !line.contains("ready") {
        line.clear();
        assert_ne!(out.read_line(&mut line).unwrap(), 0);
    }
    let blocked = store::try_lock(&dir).is_err();
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    assert!(blocked);
    assert!(store::try_lock(&dir).is_ok());
}
#[test]
#[ignore]
fn claude_accounts_login_probe() {
    unsafe {
        std::env::set_var(
            "DUOLANE_CONFIG_DIR",
            std::env::var_os("TEST_CONFIG").unwrap(),
        );
    }
    assert_eq!(login(None).is_ok(), std::env::var("SUCCESS").unwrap() == "1");
}
#[test]
fn claude_accounts_login_unlocks_after_descendant_survives() {
    use std::process::Stdio;
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, "#!/bin/sh\n/bin/sleep 30 </dev/null >/dev/null 2>&1 &\nprintf '%s' $! > \"$PID_FILE\"\nexit \"$EXIT_CODE\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    for success in [true, false] {
        let config = root.path().join(if success { "ok" } else { "failed" });
        let pid_file = root.path().join("pid");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .env("HOME", root.path())
            .env("PATH", &bin)
            .env("TEST_CONFIG", &config)
            .env("PID_FILE", &pid_file)
            .env("EXIT_CODE", if success { "0" } else { "1" })
            .env("SUCCESS", if success { "1" } else { "0" })
            .stdin(Stdio::piped())
            .args([
                "--exact",
                "--ignored",
                "claude_accounts::unix::tests::claude_accounts_login_probe",
            ])
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"backup\n").unwrap();
        let status = child.wait().unwrap();
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let path = config.join("claude-accounts/backup");
        let dir = File::open(&path).unwrap();
        let unlocked = store::try_lock(&dir).is_ok();
        let alive = Command::new("/bin/kill")
            .args(["-0", &pid])
            .status()
            .unwrap()
            .success();
        let killed = Command::new("/bin/kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success();
        assert!(status.success() && alive && killed && unlocked);
        assert_eq!(path.join(".duolane-ready").exists(), success);
    }
}
#[test]
#[ignore]
fn claude_accounts_launch_probe() {
    unsafe {
        std::env::set_var("HOME", std::env::var_os("TEST_HOME").unwrap());
        std::env::set_var(
            "DUOLANE_CONFIG_DIR",
            std::env::var_os("TEST_CONFIG").unwrap(),
        );
        std::env::set_var("DUOLANE_STATE_DIR", std::env::var_os("TEST_STATE").unwrap());
    }
    let args: Vec<OsString> = ["TEST_ARG", "TEST_ARG2"]
        .into_iter()
        .filter_map(std::env::var_os)
        .collect();
    let result = prepare_launch(&args);
    match std::env::var("EXPECTED").unwrap().as_str() {
        "none" => assert!(matches!(result, Ok(None))),
        "some" => assert!(matches!(result, Ok(Some(_)))),
        reason => assert_eq!(
            result.err().map(|error| error.to_string()).as_deref(),
            Some(reason)
        ),
    }
}
#[test]
fn claude_accounts_launch_precedence_and_rejections() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let config = root.join("config");
    let state = root.join("state");
    std::fs::create_dir(&config).unwrap();
    let run = |expected: &str, extra: &[(&str, &str)]| {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.env_clear()
            .stdin(std::process::Stdio::null())
            .current_dir(&root)
            .env("TEST_HOME", &root)
            .env("TEST_CONFIG", &config)
            .env("TEST_STATE", &state)
            .env("EXPECTED", expected)
            .envs(extra.iter().copied())
            .args([
                "--exact",
                "--ignored",
                "claude_accounts::unix::tests::claude_accounts_launch_probe",
            ]);
        assert!(cmd.status().unwrap().success());
    };
    run("none", &[]);
    let path = config.join("claude-accounts/backup");
    let dir = store::checked_dir(&path, true).unwrap().unwrap();
    store::private_replace(&dir, ".duolane-ready", b"ready\n").unwrap();
    let selector = store::checked_dir(&config.join("claude-accounts"), false)
        .unwrap()
        .unwrap();
    store::private_replace(&selector, ".selected", b"default\n").unwrap();
    run("none", &[]);
    store::private_replace(&selector, ".selected", b"backup\n").unwrap();
    run("none", &[("CLAUDE_CONFIG_DIR", "explicit")]);
    run(
        "worktree arguments are unsupported",
        &[("TEST_ARG", "--worktree=foo")],
    );
    run(
        "worktree arguments are unsupported",
        &[("TEST_ARG", "-wfoo")],
    );
    run(
        "CLAUDE_CODE_PROJECT_DIR_NAME is unsupported",
        &[("CLAUDE_CODE_PROJECT_DIR_NAME", "foo")],
    );
    run(
        "authentication override set: ANTHROPIC_API_KEY",
        &[("ANTHROPIC_API_KEY", "toy")],
    );
    run("project sharing requires terminal confirmation", &[]);
    assert!(!path.join("projects").exists());
    transcripts::link_project(&dir, &root.join(".claude"), &root, |_| Ok(true)).unwrap();
    run("some", &[]);
    run("some", &[("TEST_ARG", "--resume")]);
    run("some", &[("TEST_ARG", "--"), ("TEST_ARG2", "-wfoo")]);
}
#[test]
#[ignore]
fn claude_accounts_resolve_probe() {
    unsafe {
        std::env::set_var(
            "DUOLANE_CONFIG_DIR",
            std::env::var_os("TEST_CONFIG").unwrap(),
        );
        std::env::set_var("DUOLANE_STATE_DIR", std::env::var_os("TEST_STATE").unwrap());
    }
    let expected = std::env::var("EXPECTED").unwrap();
    let result = selected_profile();
    match expected.as_str() {
        "none" => assert!(matches!(result, Ok(None))),
        "some" => assert!(matches!(result, Ok(Some(_)))),
        "error" => assert!(result.is_err()),
        _ => panic!("invalid expectation"),
    }
}
#[test]
fn claude_accounts_resolution_precedence_and_lock() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    let other = root.path().join("other");
    let state = root.path().join("state");
    std::fs::create_dir(&other).unwrap();
    let run = |config: &Path, expected: &str, explicit: bool| {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.env_clear()
            .env("HOME", root.path())
            .env("TEST_CONFIG", config)
            .env("TEST_STATE", &state)
            .env("EXPECTED", expected)
            .args([
                "--exact",
                "--ignored",
                "claude_accounts::unix::tests::claude_accounts_resolve_probe",
            ]);
        if explicit {
            cmd.env("CLAUDE_CONFIG_DIR", "explicit");
        }
        assert!(cmd.status().unwrap().success());
    };
    run(&config, "none", false);
    assert!(!config.exists() && !state.exists());
    std::fs::create_dir(&config).unwrap();
    let alias = root.path().join("alias");
    symlink(&config, &alias).unwrap();
    run(&alias, "none", false);
    run(&config, "none", false);
    let profile_path = config.join("claude-accounts/backup");
    let dir = store::checked_dir(&profile_path, true).unwrap().unwrap();
    store::private_replace(&dir, ".duolane-ready", b"ready\n").unwrap();
    let selector_dir = config.join("claude-accounts");
    let selector = store::checked_dir(&selector_dir, false).unwrap().unwrap();
    store::private_replace(&selector, ".selected", b"backup\n").unwrap();
    run(&alias, "some", false);
    run(&config, "some", false);
    #[cfg(target_os = "macos")]
    {
        let case_alias = root.path().join("CONFIG");
        if case_alias.exists() {
            run(&case_alias, "some", false);
        }
    }
    symlink("missing", config.join("claude-accounts/bad")).unwrap();
    store::private_replace(&selector, ".selected", b"bad\n").unwrap();
    run(&config, "error", false);
    store::private_replace(&selector, ".selected", b"backup\n").unwrap();
    run(&other, "none", false);
    let lock = store::try_lock(&dir).unwrap();
    run(&config, "error", false);
    drop(lock);
    store::private_replace(&selector, ".selected", &[b'x'; 65]).unwrap();
    run(&config, "error", false);
    run(&config, "none", true);
    std::fs::remove_file(selector_dir.join(".selected")).unwrap();
    symlink("missing", selector_dir.join(".selected")).unwrap();
    run(&config, "error", false);
    std::fs::remove_file(selector_dir.join(".selected")).unwrap();
    std::fs::write(selector_dir.join(".selected"), b"stale\n").unwrap();
    std::fs::set_permissions(
        selector_dir.join(".selected"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    run(&config, "error", false);
    store::private_replace(&selector, ".selected", b"backup\n").unwrap();
    std::fs::remove_dir_all(&config).unwrap();
    run(&config, "none", false);
}
