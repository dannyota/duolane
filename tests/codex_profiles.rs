#[path = "support/hermetic.rs"]
mod hermetic;

#[cfg(unix)]
mod unix {
    use assert_cmd::Command;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use tempfile::TempDir;

    struct Fixture {
        root: TempDir,
    }
    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            for dir in ["config", "state", "home", "other"] {
                fs::create_dir(root.path().join(dir)).unwrap();
            }
            fs::write(
                root.path().join("home/auth.json"),
                b"{\"tokens\":{\"access_token\":\"toy-token\",\"account_id\":\"toy-account\"}}",
            )
            .unwrap();
            fs::set_permissions(
                root.path().join("home/auth.json"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let f = Self { root };
            f.profiles(
                &serde_json::json!({"work": fs::canonicalize(f.root.path().join("home")).unwrap()}),
            );
            f
        }
        fn profiles(&self, value: &serde_json::Value) {
            fs::write(
                self.root.path().join("config/codex-profiles.json"),
                serde_json::to_vec(value).unwrap(),
            )
            .unwrap();
        }
        fn run(&self, args: &[&str]) -> std::process::Output {
            Command::cargo_bin("duolane")
                .unwrap()
                .args(args)
                .env("DUOLANE_CONFIG_DIR", self.root.path().join("config"))
                .env("DUOLANE_STATE_DIR", self.root.path().join("state"))
                .env("CODEX_HOME", self.root.path().join("other"))
                .env_remove("DUOLANE_CODEX_AUTH_FILE")
                .timeout(std::time::Duration::from_secs(5))
                .output()
                .unwrap()
        }
        fn list(&self) -> std::process::Output {
            self.run(&["codex", "auth", "list"])
        }
        fn select(&self) -> std::process::Output {
            self.run(&["codex", "auth", "use", "work"])
        }
    }

    #[test]
    fn selection_and_list_are_private_and_replace_atomically() {
        let f = Fixture::new();
        let initial = f.list();
        assert!(initial.status.success());
        assert!(
            String::from_utf8(initial.stdout)
                .unwrap()
                .lines()
                .any(|line| line.starts_with("* default "))
        );
        assert!(f.select().status.success());
        let path = f.root.path().join("state/codex-profile");
        let old = fs::metadata(&path).unwrap();
        assert_eq!(old.permissions().mode() & 0o777, 0o600);
        assert!(f.run(&["codex", "auth", "use", "default"]).status.success());
        use std::os::unix::fs::MetadataExt;
        assert_ne!(old.ino(), fs::metadata(path).unwrap().ino());
        assert_eq!(
            fs::read_dir(f.root.path().join("state")).unwrap().count(),
            1
        );
        assert!(f.select().status.success());
        let out = f.list();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(
            text.lines()
                .any(|line| line.starts_with("* work ") && line.contains("login exists"))
        );
        assert!(!text.contains("toy-token"));
        assert!(!text.contains("toy-account"));
    }

    #[test]
    fn status_uses_selected_profile() {
        let f = Fixture::new();
        assert!(f.select().status.success());
        let output = f.run(&["codex", "auth", "status"]);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("Profile: work"));
        assert!(text.contains("Account: toy-account"));
    }

    #[test]
    fn changed_path_and_unsafe_state_never_fall_back() {
        let f = Fixture::new();
        assert!(f.select().status.success());
        f.profiles(
            &serde_json::json!({"work": fs::canonicalize(f.root.path().join("other")).unwrap()}),
        );
        let out = f.list();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("codex auth use"));
        fs::remove_file(f.root.path().join("state/codex-profile")).unwrap();
        fs::set_permissions(
            f.root.path().join("state"),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let output = f.list();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("codex auth use"));
    }

    #[test]
    fn default_auth_file_may_be_a_symlink() {
        let f = Fixture::new();
        symlink(
            f.root.path().join("home/auth.json"),
            f.root.path().join("other/auth.json"),
        )
        .unwrap();
        let output = f.list();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.lines()
                .any(|line| line.starts_with("* default ") && line.contains("login exists"))
        );
    }

    #[test]
    fn rejects_bad_configurations() {
        let f = Fixture::new();
        for value in [
            serde_json::json!({"bad name":"/x"}),
            serde_json::json!({"default":"/x"}),
            serde_json::json!({"work":"relative"}),
            serde_json::json!({"work":"/x/../y"}),
            serde_json::Value::Object(
                (0..17)
                    .map(|i| (format!("p{i}"), serde_json::json!("/x")))
                    .collect(),
            ),
        ] {
            f.profiles(&value);
            assert!(!f.list().status.success());
        }
        fs::write(
            f.root.path().join("config/codex-profiles.json"),
            vec![b' '; 16385],
        )
        .unwrap();
        assert!(!f.list().status.success());
    }

    #[test]
    fn rejects_unsafe_named_files_and_home() {
        let f = Fixture::new();
        let root = f.root.path();
        let auth = root.join("home/auth.json");
        fs::set_permissions(root.join("home"), fs::Permissions::from_mode(0o770)).unwrap();
        assert!(!f.select().status.success());
        fs::set_permissions(root.join("home"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&auth, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(!f.select().status.success());
        fs::remove_file(&auth).unwrap();
        symlink(root.join("other"), &auth).unwrap();
        assert!(!f.select().status.success());
        fs::remove_file(&auth).unwrap();
        fs::write(&auth, vec![b'x'; 1024 * 1024 + 1]).unwrap();
        fs::set_permissions(&auth, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!f.select().status.success());
        fs::remove_file(&auth).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&auth)
                .status()
                .unwrap()
                .success()
        );
        let fifo = f.select();
        assert_eq!(fifo.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&fifo.stderr).contains("unsafe file type"));
        fs::remove_file(&auth).unwrap();
        symlink(root.join("home"), root.join("linked")).unwrap();
        f.profiles(&serde_json::json!({"work": fs::canonicalize(root).unwrap().join("linked")}));
        assert!(!f.select().status.success());
    }

    #[test]
    fn rejects_unsafe_profile_file_and_bad_selector() {
        let f = Fixture::new();
        let config = f.root.path().join("config/codex-profiles.json");
        fs::set_permissions(&config, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(!f.list().status.success());
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(&config, f.root.path().join("config/real")).unwrap();
        symlink(Path::new("real"), &config).unwrap();
        assert!(!f.list().status.success());
        fs::remove_file(&config).unwrap();
        fs::rename(f.root.path().join("config/real"), &config).unwrap();
        let selector = f.root.path().join("state/codex-profile");
        fs::write(&selector, "broken").unwrap();
        fs::set_permissions(&selector, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!f.list().status.success());
        fs::set_permissions(&selector, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(!f.list().status.success());
        fs::set_permissions(&selector, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(f.select().status.success());
        let mut padded = fs::read(&selector).unwrap();
        padded.resize(257, b' ');
        fs::write(&selector, padded).unwrap();
        assert!(!f.list().status.success());
        fs::remove_file(&selector).unwrap();
        symlink(Path::new("missing"), &selector).unwrap();
        assert!(!f.list().status.success());
    }
    #[test]
    fn default_fingerprint_and_unknown_selection_are_errors() {
        let f = Fixture::new();
        assert!(f.run(&["codex", "auth", "use", "default"]).status.success());
        let output = Command::cargo_bin("duolane")
            .unwrap()
            .args(["codex", "auth", "list"])
            .env("DUOLANE_CONFIG_DIR", f.root.path().join("config"))
            .env("DUOLANE_STATE_DIR", f.root.path().join("state"))
            .env(
                "DUOLANE_CODEX_AUTH_FILE",
                f.root.path().join("different-auth"),
            )
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("codex auth use"));
        assert!(f.select().status.success());
        f.profiles(&serde_json::json!({}));
        let output = f.list();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("codex auth use"));
    }

    #[test]
    fn home_ancestor_symlink_is_resolved_and_state_symlink_rejected() {
        let f = Fixture::new();
        let root = fs::canonicalize(f.root.path()).unwrap();
        symlink(&root, root.join("parent-link")).unwrap();
        f.profiles(&serde_json::json!({"work": root.join("parent-link/home")}));
        assert!(f.select().status.success());
        fs::rename(root.join("state"), root.join("real-state")).unwrap();
        symlink(root.join("real-state"), root.join("state")).unwrap();
        assert!(!f.list().status.success());
    }
    #[test]
    fn ancestor_modes_and_missing_state_directory() {
        let f = Fixture::new();
        fs::remove_dir(f.root.path().join("state")).unwrap();
        assert!(f.list().status.success());
        assert!(!f.root.path().join("state").exists());
        fs::set_permissions(f.root.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!f.select().status.success());
        fs::set_permissions(f.root.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(f.select().status.success());
        assert_eq!(
            fs::metadata(f.root.path().join("state"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn default_use_ignores_broken_profile_file() {
        let f = Fixture::new();
        fs::write(f.root.path().join("config/codex-profiles.json"), "invalid").unwrap();
        assert!(f.run(&["codex", "auth", "use", "default"]).status.success());
    }

    #[test]
    fn list_detects_access_token_not_file_presence() {
        let f = Fixture::new();
        fs::write(f.root.path().join("home/auth.json"), b"{}").unwrap();
        let output = f.list();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.lines().any(|line| line.contains("work no login")));
    }

    #[test]
    fn default_fingerprint_tracks_symlink_target() {
        let f = Fixture::new();
        let auth = f.root.path().join("other/auth.json");
        symlink(f.root.path().join("home/auth.json"), &auth).unwrap();
        assert!(f.run(&["codex", "auth", "use", "default"]).status.success());
        fs::write(f.root.path().join("other/another"), b"{}").unwrap();
        fs::remove_file(&auth).unwrap();
        symlink(f.root.path().join("other/another"), &auth).unwrap();
        let output = f.list();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("codex auth use"));
    }
    #[test]
    fn resolve_ignores_bad_profile_config_without_selector() {
        if let Some(root) = std::env::var_os("PROFILE_TEST_ROOT") {
            let root = std::path::PathBuf::from(root);
            unsafe {
                std::env::set_var("DUOLANE_CONFIG_DIR", root.join("config"));
                std::env::set_var("DUOLANE_STATE_DIR", root.join("state"));
                std::env::set_var("CODEX_HOME", root.join("other"));
            }
            let (name, _) = duolane::providers::codex::profiles::resolve_selected().unwrap();
            assert_eq!(name, "default");
            return;
        }
        let f = Fixture::new();
        fs::write(f.root.path().join("config/codex-profiles.json"), "invalid").unwrap();
        fs::remove_dir(f.root.path().join("state")).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unix::resolve_ignores_bad_profile_config_without_selector",
            ])
            .env("PROFILE_TEST_ROOT", f.root.path())
            .output()
            .unwrap();
        assert!(child.status.success());
        assert!(!f.root.path().join("state").exists());
    }
    #[test]
    fn named_home_and_state_reject_writable_ancestors() {
        let f = Fixture::new();
        let root = fs::canonicalize(f.root.path()).unwrap();
        let ancestor = root.join("unsafe-parent");
        fs::create_dir(&ancestor).unwrap();
        fs::rename(root.join("home"), ancestor.join("home")).unwrap();
        f.profiles(&serde_json::json!({"work": ancestor.join("home")}));
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(!f.select().status.success());
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(f.select().status.success());
        fs::rename(root.join("state"), ancestor.join("state")).unwrap();
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o770)).unwrap();
        let output = Command::cargo_bin("duolane")
            .unwrap()
            .args(["codex", "auth", "list"])
            .env("DUOLANE_CONFIG_DIR", root.join("config"))
            .env("DUOLANE_STATE_DIR", ancestor.join("state"))
            .env("CODEX_HOME", root.join("other"))
            .env_remove("DUOLANE_CODEX_AUTH_FILE")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("codex auth use"));
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[test]
    fn symlink_in_unsafe_directory_cannot_redirect_state() {
        let f = Fixture::new();
        let root = fs::canonicalize(f.root.path()).unwrap();
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&root, outside.join("link")).unwrap();
        let run = || {
            Command::cargo_bin("duolane")
                .unwrap()
                .args(["codex", "auth", "list"])
                .env("DUOLANE_CONFIG_DIR", root.join("config"))
                .env("DUOLANE_STATE_DIR", outside.join("link/state"))
                .env("CODEX_HOME", root.join("other"))
                .env_remove("DUOLANE_CODEX_AUTH_FILE")
                .output()
                .unwrap()
        };
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o770)).unwrap();
        let out = run();
        assert!(!out.status.success());
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(run().status.success());
    }

    #[test]
    fn named_fingerprint_detects_parent_symlink_retargeting() {
        let f = Fixture::new();
        let root = fs::canonicalize(f.root.path()).unwrap();
        fs::create_dir(root.join("a")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        fs::rename(root.join("home"), root.join("a/home")).unwrap();
        fs::create_dir(root.join("b/home")).unwrap();
        symlink(root.join("a"), root.join("link")).unwrap();
        f.profiles(&serde_json::json!({"work": root.join("link/home")}));
        assert!(f.select().status.success());
        fs::remove_file(root.join("link")).unwrap();
        symlink(root.join("b"), root.join("link")).unwrap();
        let out = f.list();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("codex auth use"));
    }

    #[test]
    fn default_fifo_is_rejected_without_blocking() {
        let f = Fixture::new();
        let auth = f.root.path().join("other/auth.json");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&auth)
                .status()
                .unwrap()
                .success()
        );
        let out = f.list();
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("unsafe file type"));
    }
    #[test]
    fn relative_directory_symlinks_resolve_and_cycles_fail() {
        let f = Fixture::new();
        let root = fs::canonicalize(f.root.path()).unwrap();
        fs::create_dir(root.join("links")).unwrap();
        symlink("..", root.join("links/back")).unwrap();
        f.profiles(&serde_json::json!({"work": root.join("links/back/home")}));
        assert!(f.select().status.success());
        fs::remove_file(root.join("links/back")).unwrap();
        symlink("back", root.join("links/back")).unwrap();
        let output = f.select();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("too many directory symlinks"));
    }
    #[test]
    fn default_auth_read_is_bounded() {
        let f = Fixture::new();
        fs::write(
            f.root.path().join("other/auth.json"),
            vec![b' '; 1024 * 1024 + 1],
        )
        .unwrap();
        let out = f.list();
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("file exceeds size limit"));
    }
    #[test]
    fn missing_default_auth_resolves_symlinked_parents_and_targets() {
        for dangling_file_symlink in [false, true] {
            let f = Fixture::new();
            let root = fs::canonicalize(f.root.path()).unwrap();
            let real = root.join("real");
            fs::create_dir(&real).unwrap();
            symlink(&real, root.join("alias")).unwrap();
            let target = real.join("missing/deep/auth.json");
            let path = if dangling_file_symlink {
                let link = root.join("other/auth.json");
                symlink(root.join("alias/missing/deep/auth.json"), &link).unwrap();
                link
            } else {
                root.join("alias/missing/deep/auth.json")
            };
            let run = |args: &[&str]| {
                Command::cargo_bin("duolane")
                    .unwrap()
                    .args(args)
                    .env("DUOLANE_CONFIG_DIR", root.join("config"))
                    .env("DUOLANE_STATE_DIR", root.join("state"))
                    .env("DUOLANE_CODEX_AUTH_FILE", &path)
                    .output()
                    .unwrap()
            };
            assert!(run(&["codex", "auth", "use", "default"]).status.success());
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, b"{}").unwrap();
            assert!(run(&["codex", "auth", "list"]).status.success());
        }
    }
    #[test]
    fn default_auth_accepts_group_writable_directory() {
        let f = Fixture::new();
        let root = fs::canonicalize(f.root.path()).unwrap();
        let dir = root.join("shared");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
        let auth = dir.join("auth.json");
        fs::write(&auth, br#"{"tokens":{"access_token":"toy"}}"#).unwrap();
        let run = |args: &[&str]| {
            Command::cargo_bin("duolane")
                .unwrap()
                .args(args)
                .env("DUOLANE_CONFIG_DIR", root.join("config"))
                .env("DUOLANE_STATE_DIR", root.join("state"))
                .env("DUOLANE_CODEX_AUTH_FILE", &auth)
                .output()
                .unwrap()
        };
        assert!(run(&["codex", "auth", "use", "default"]).status.success());
        let out = run(&["codex", "auth", "list"]);
        assert!(out.status.success());
        assert!(
            String::from_utf8(out.stdout)
                .unwrap()
                .lines()
                .any(|line| line.starts_with("* default ") && line.contains("login exists"))
        );
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o111)).unwrap();
        let output = run(&["codex", "auth", "use", "default"]);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(output.status.success());
    }

    #[test]
    fn default_missing_parent_before_dotdot_never_loads_other_login() {
        let f = Fixture::new();
        let root = fs::canonicalize(f.root.path()).unwrap();
        let auth = root.join("auth.json");
        fs::write(&auth, br#"{"tokens":{"access_token":"toy"}}"#).unwrap();
        let missing = root.join("missing/../auth.json");
        let run = |args: &[&str], path: &Path| {
            Command::cargo_bin("duolane")
                .unwrap()
                .args(args)
                .env("DUOLANE_CONFIG_DIR", root.join("config"))
                .env("DUOLANE_STATE_DIR", root.join("state"))
                .env("DUOLANE_CODEX_AUTH_FILE", path)
                .output()
                .unwrap()
        };
        assert!(
            run(&["codex", "auth", "use", "default"], &missing)
                .status
                .success()
        );
        let selection = fs::read(root.join("state/codex-profile")).unwrap();
        let output = run(&["codex", "auth", "list"], &missing);
        assert!(output.status.success());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .any(|line| line.starts_with("* default ") && line.contains("no login"))
        );
        assert!(
            run(&["codex", "auth", "use", "default"], &auth)
                .status
                .success()
        );
        assert!(selection != fs::read(root.join("state/codex-profile")).unwrap());
    }
    #[test]
    fn default_nondirectory_before_dotdot_does_not_resolve_to_another_file() {
        let f = Fixture::new();
        let root = fs::canonicalize(f.root.path()).unwrap();
        fs::write(root.join("file"), b"{}").unwrap();
        fs::write(
            root.join("auth.json"),
            br#"{"tokens":{"access_token":"toy"}}"#,
        )
        .unwrap();
        let out = Command::cargo_bin("duolane")
            .unwrap()
            .args(["codex", "auth", "use", "default"])
            .env("DUOLANE_CONFIG_DIR", root.join("config"))
            .env("DUOLANE_STATE_DIR", root.join("state"))
            .env("DUOLANE_CODEX_AUTH_FILE", root.join("file/../auth.json"))
            .output()
            .unwrap();
        assert!(!out.status.success());
    }
    #[test]
    fn default_auth_trailing_slash_never_selects_a_file() {
        let f = Fixture::new();
        let root = fs::canonicalize(f.root.path()).unwrap();
        let file = root.join("auth.json");
        fs::write(&file, br#"{"tokens":{"access_token":"toy"}}"#).unwrap();
        let with_slash = format!("{}/", file.display());
        let with_dot = format!("{}/.", file.display());
        let link = root.join("trailing-link");
        let dot_link = root.join("dot-link");
        symlink(&with_slash, &link).unwrap();
        symlink(&with_dot, &dot_link).unwrap();
        for path in [
            Path::new(&with_slash),
            link.as_path(),
            Path::new(&with_dot),
            dot_link.as_path(),
        ] {
            let run = |args: &[&str]| {
                Command::cargo_bin("duolane")
                    .unwrap()
                    .args(args)
                    .env("DUOLANE_CONFIG_DIR", root.join("config"))
                    .env("DUOLANE_STATE_DIR", root.join("state"))
                    .env("DUOLANE_CODEX_AUTH_FILE", path)
                    .output()
                    .unwrap()
            };
            assert!(!run(&["codex", "auth", "use", "default"]).status.success());
            assert!(!run(&["codex", "auth", "list"]).status.success());
        }
    }
}
