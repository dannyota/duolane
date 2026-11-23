#[path = "support/hermetic.rs"]
mod hermetic;

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post},
};
use duolane::{
    config::AliasProvider,
    providers::codex::source_startup::StartupInput,
    registry::Registry,
    server::{AppFeatures, app_with_features},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

fn native_requests() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "/v1/responses",
            "application/json",
            r#"{"model":"gpt-5.4","input":"hello"}"#,
        ),
        (
            "/v1/images/generations",
            "application/json",
            r#"{"prompt":"draw a fox"}"#,
        ),
        (
            "/v1/images/edits",
            "application/json",
            r#"{"prompt":"edit","images":[{"image_url":"data:image/png;base64,iVBORw0KGgo="}]}"#,
        ),
        (
            "/v1/audio/transcriptions",
            "multipart/form-data; boundary=x",
            "--x\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\ntoy audio\r\n--x--\r\n",
        ),
    ]
}

async fn request(router: &Router, case: (&str, &str, &str)) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::post(case.0)
                .header("content-type", case.1)
                .body(Body::from(case.2.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn native_routes_require_selected_current_chatgpt_login() {
    let dir = tempfile::tempdir().unwrap();
    let auth = dir.path().join("auth.json");
    std::fs::write(&auth, serde_json::to_vec(&json!({"tokens":{"access_token":"toy-access","refresh_token":"toy-refresh","account_id":"toy-account"}})).unwrap()).unwrap();
    let direct_calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let upstream = Router::new().route("/v1/models", get(|| async { Json(json!({"data":[]})) })).route("/v1/responses", post({
        let calls = direct_calls.clone();
        move || { let calls = calls.clone(); async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Json(json!({"id":"resp_1","object":"response","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1}}))
        }}
    })).fallback({
        let calls = direct_calls.clone();
        move || { let calls = calls.clone(); async move { calls.fetch_add(1, Ordering::SeqCst); StatusCode::BAD_REQUEST } }
    });
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy_listener.local_addr().unwrap());
    let proxy_calls = Arc::new(AtomicUsize::new(0));
    let proxy = tokio::spawn({
        let calls = proxy_calls.clone();
        async move {
            while let Ok((mut stream, _)) = proxy_listener.accept().await {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut bytes = [0; 2048];
                    let _ = stream.read(&mut bytes).await;
                    let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                });
            }
        }
    });
    unsafe {
        std::env::set_var("DUOLANE_CODEX_AUTH_FILE", &auth);
        std::env::set_var("DUOLANE_CODEX_BASE_URL", format!("{base}/responses"));
        std::env::set_var(
            "DUOLANE_CODEX_IMAGES_BASE_URL",
            "https://chatgpt.com/backend-api/codex",
        );
        std::env::set_var("DUOLANE_CODEX_TRANSPORT", "http");
        for key in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::set_var(key, &proxy_url);
        }
        for key in ["NO_PROXY", "no_proxy"] {
            std::env::set_var(key, "127.0.0.1,localhost");
        }
        std::env::remove_var("REQUEST_METHOD");
    }
    let features = AppFeatures {
        responses_api: true,
        images_api: true,
        transcriptions_api: true,
    };
    for order in ["api-key", "chatgpt"] {
        let registry = Registry::for_serve_with_input(
            AliasProvider::Anthropic,
            StartupInput {
                config_dir: dir.path().to_owned(),
                order: Some(order.into()),
                key: Some("toy-key".into()),
                base_url: Some(base.clone().into()),
            },
        )
        .await
        .unwrap();
        let router = app_with_features(Arc::new(registry), None, features);
        for (index, case) in native_requests().into_iter().enumerate() {
            let before = proxy_calls.load(Ordering::SeqCst);
            let response = request(&router, case).await;
            if order == "api-key" {
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                let bytes = axum::body::to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                let message = body["error"]["message"].as_str().unwrap();
                assert_eq!(
                    message,
                    "This route needs a ChatGPT login and chatgpt in duolane codex auth order"
                );
                assert_eq!(direct_calls.load(Ordering::SeqCst), 0);
                assert_eq!(proxy_calls.load(Ordering::SeqCst), 0);
            } else if index == 0 {
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(direct_calls.load(Ordering::SeqCst), 1);
            } else {
                assert!(proxy_calls.load(Ordering::SeqCst) > before);
            }
        }
        if order == "chatgpt" {
            std::fs::remove_file(&auth).unwrap();
            let response = request(&router, native_requests()[0]).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("duolane codex auth order")
            );
            assert_eq!(direct_calls.load(Ordering::SeqCst), 1);
        }
    }
    server.abort();
    proxy.abort();
}
