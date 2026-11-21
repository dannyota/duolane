#[path = "support/hermetic.rs"]
mod hermetic;

#[cfg(unix)]
mod unix {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use duolane::{providers::codex::profiles, registry::Registry, server::app};
    use http_body_util::BodyExt;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn messages_use_selected_profile_and_reject_bad_selection() {
        let root = tempfile::TempDir::new().unwrap();
        let config = root.path().join("config");
        let state = root.path().join("state");
        let home = root.path().join("home");
        let other = root.path().join("other");
        for dir in [&config, &state, &home, &other] {
            std::fs::create_dir(dir).unwrap();
        }
        let auth = home.join("auth.json");
        std::fs::write(&auth, br#"{"tokens":{"access_token":"profile-toy","refresh_token":"refresh-toy","account_id":"profile-account"}}"#).unwrap();
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        let other_auth = other.join("auth.json");
        std::fs::write(&other_auth, br#"{"tokens":{"access_token":"second-toy","refresh_token":"refresh-toy","account_id":"second-account"}}"#).unwrap();
        std::fs::set_permissions(&other_auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(
            config.join("codex-profiles.json"),
            serde_json::to_vec(&serde_json::json!({"work": home, "other": other})).unwrap(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let upstream_calls = calls.clone();
        let upstream = tokio::spawn(async move {
            for expected in [
                "authorization: bearer profile-toy",
                "authorization: bearer second-toy",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 32768];
                let n = socket.read(&mut request).await.unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..n])
                        .to_ascii_lowercase()
                        .contains(expected)
                );
                upstream_calls.fetch_add(1, Ordering::SeqCst);
                socket.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            }
        });
        let claude_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let claude_addr = claude_listener.local_addr().unwrap();
        let claude_upstream = tokio::spawn(async move {
            let (mut socket, _) = claude_listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
        });
        unsafe {
            std::env::set_var(
                "DUOLANE_ANTHROPIC_BASE_URL",
                format!("http://{claude_addr}"),
            );
            std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
            std::env::set_var("DUOLANE_CONFIG_DIR", &config);
            std::env::set_var("DUOLANE_STATE_DIR", &state);
            std::env::set_var("DUOLANE_CODEX_BASE_URL", format!("http://{addr}/responses"));
            std::env::set_var("DUOLANE_CODEX_TRANSPORT", "http");
        }
        profiles::select("work").unwrap();
        let router = app(Arc::new(Registry::with_default_alias()));
        let request = || {
            Request::builder().method("POST").uri("/v1/messages").header("content-type", "application/json").body(Body::from(r#"{"model":"gpt-6.1-terra","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#)).unwrap()
        };
        let _ = router.clone().oneshot(request()).await.unwrap();
        profiles::select("other").unwrap();
        let _ = router.clone().oneshot(request()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), upstream)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(state.join("codex-profile"), b"invalid").unwrap();
        let response = router.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()["x-should-retry"], "false");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error["error"]["type"], "api_error");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("duolane codex auth use <name>")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let claude = Request::builder().method("POST").uri("/v1/messages").header("content-type", "application/json").body(Body::from(r#"{"model":"claude-opus-4-8","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#)).unwrap();
        let response = router.oneshot(claude).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(std::time::Duration::from_secs(3), claude_upstream)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
