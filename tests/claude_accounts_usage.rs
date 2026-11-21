#[path = "support/hermetic.rs"]
mod hermetic;

#[cfg(unix)]
mod unix {
    use assert_cmd::Command;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn usage_reports_unusable_account_directories_before_any_login_is_read() {
        let root = TempDir::new().unwrap();
        for dir in ["config", "state", "home", "unsafe"] {
            fs::create_dir(root.path().join(dir)).unwrap();
        }
        fs::set_permissions(
            root.path().join("unsafe"),
            fs::Permissions::from_mode(0o770),
        )
        .unwrap();
        let accounts = format!(
            "{{{:?}:{:?},{:?}:{:?}}}",
            "missing",
            root.path().join("missing"),
            "unsafe",
            root.path().join("unsafe"),
        );
        let store = root.path().join("config/claude-accounts.json");
        fs::write(&store, accounts).unwrap();
        fs::set_permissions(&store, fs::Permissions::from_mode(0o600)).unwrap();

        let output = Command::cargo_bin("duolane")
            .unwrap()
            .args(["claude-accounts", "usage"])
            .env("HOME", root.path().join("home"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_DATA_HOME", root.path().join("home"))
            .env("DUOLANE_CONFIG_DIR", root.path().join("config"))
            .env("DUOLANE_STATE_DIR", root.path().join("state"))
            .env("CODEX_HOME", root.path().join("home"))
            .env_remove("DUOLANE_INTERCEPT_PORT")
            .env_remove("DUOLANE_INTERCEPT")
            .env_remove("HTTPS_PROXY")
            .env_remove("https_proxy")
            .env_remove("ALL_PROXY")
            .env_remove("all_proxy")
            .timeout(std::time::Duration::from_secs(10))
            .output()
            .unwrap();

        assert!(output.status.success());
        assert!(output.stdout == b"  missing   login not found\n  unsafe    login unreadable\n");
        assert!(output.stderr.is_empty());
    }
}
