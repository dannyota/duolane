pub fn rerun_without_credentials(test_name: &str) -> bool {
    if std::env::var("DUOLANE_TEST_ISOLATED_CODEX").as_deref() != Ok(test_name) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env_clear()
            .env("DUOLANE_TEST_ISOLATED_CODEX", test_name)
            .env("DUOLANE_CODEX_BASE_URL", "http://127.0.0.1:1/responses")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("isolated Codex routing test timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "isolated Codex routing test failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "isolated subprocess must execute exactly the requested test"
        );
        return true;
    }
    false
}
