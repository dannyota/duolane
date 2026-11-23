#[path = "support/hermetic.rs"]
mod hermetic;

use axum::{
    Json, Router,
    body::Body,
    http::{Method, Request, StatusCode},
    routing::{get, post},
};
use duolane::{
    config::AliasProvider,
    monitor::MonitorHandle,
    providers::codex::source_startup::StartupInput,
    registry::Registry,
    server::{app, app_with_monitor},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tower::ServiceExt;

#[tokio::test]
async fn chatgpt_quota_falls_back_and_marks_source() {
    let dir = tempfile::tempdir().unwrap();
    let auth = dir.path().join("auth.json");
    std::fs::write(&auth, serde_json::to_vec(&json!({"tokens":{"access_token":"toy-access","refresh_token":"toy-refresh","account_id":"toy-account"}})).unwrap()).unwrap();
    let chat_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let chat_url = format!("http://{}/responses", chat_listener.local_addr().unwrap());
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let chat_success = Arc::new(AtomicBool::new(false));
    let valid = Arc::new(AtomicBool::new(true));
    let chat_router = Router::new().fallback({
        let calls = chat_calls.clone(); let valid = valid.clone(); let success = chat_success.clone();
        move |headers: http::HeaderMap| { let calls = calls.clone(); let valid = valid.clone(); let success = success.clone(); async move {
            calls.fetch_add(1, Ordering::SeqCst);
            valid.fetch_and(headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer toy-access"), Ordering::SeqCst);
            if success.load(Ordering::SeqCst) {
                return ([("content-type", "text/event-stream")], "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n");
            }
            ([("content-type", "text/event-stream")], "data: {\"type\":\"error\",\"status_code\":429,\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"limit\",\"resets_in_seconds\":90}}\n\n")
        }}
    });
    let chat_server =
        tokio::spawn(async move { axum::serve(chat_listener, chat_router).await.unwrap() });
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}/v1", api_listener.local_addr().unwrap());
    let api_calls = Arc::new(AtomicUsize::new(0));
    let reject = Arc::new(AtomicUsize::new(0));
    let api_router = Router::new().route("/v1/models", get(|| async { Json(json!({"data":[{"id":"gpt-99-sol"}]})) })).route("/v1/responses", post({
        let calls = api_calls.clone(); let valid = valid.clone(); let reject = reject.clone();
        move |headers: http::HeaderMap, Json(body): Json<Value>| { let calls = calls.clone(); let valid = valid.clone(); let reject = reject.clone(); async move {
            calls.fetch_add(1, Ordering::SeqCst);
            valid.fetch_and(headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer toy-key") && body["model"] == "gpt-99-sol", Ordering::SeqCst);
            if reject.load(Ordering::SeqCst) == 1 {
                return (StatusCode::BAD_REQUEST, "{\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"upstream marker\"}}");
            }
            if reject.load(Ordering::SeqCst) == 2 {
                return (StatusCode::UNAUTHORIZED, "{\"error\":{\"message\":\"rejected\"}}");
            }
            (StatusCode::OK, "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n")
        }}
    }));
    let api_server =
        tokio::spawn(async move { axum::serve(api_listener, api_router).await.unwrap() });
    unsafe {
        std::env::set_var("DUOLANE_CODEX_AUTH_FILE", &auth);
        std::env::set_var("DUOLANE_CODEX_BASE_URL", &chat_url);
        std::env::set_var("DUOLANE_CODEX_TRANSPORT", "http");
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    }
    let registry = Registry::for_serve_with_input(
        AliasProvider::Anthropic,
        StartupInput {
            config_dir: dir.path().to_owned(),
            order: Some("chatgpt,api-key".into()),
            key: Some("toy-key".into()),
            base_url: Some(api_url.clone().into()),
        },
    )
    .await
    .unwrap();
    let monitor = MonitorHandle::new(10);
    let router = app_with_monitor(Arc::new(registry), Some(monitor.clone()));
    for _ in 0..2 {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"model":"sol","messages":[{"role":"user","content":"hello"}]}).to_string(),
            ))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let snapshot: duolane::monitor::snapshot::MonitorSnapshot = monitor.snapshot().into();
        let value = serde_json::to_value(snapshot).unwrap();
        assert_eq!(
            value["recent"][0]["codex_source"],
            "api key (from chatgpt: usage limit)"
        );
        assert!(
            monitor.snapshot().recent[0]
                .model
                .as_deref()
                .unwrap()
                .ends_with(" → gpt-99-sol")
        );
    }
    assert_eq!(chat_calls.load(Ordering::SeqCst), 1);
    assert_eq!(api_calls.load(Ordering::SeqCst), 2);
    assert!(valid.load(Ordering::SeqCst));
    reject.store(1, Ordering::SeqCst);
    let registry = Registry::for_serve_with_input(
        AliasProvider::Anthropic,
        StartupInput {
            config_dir: dir.path().to_owned(),
            order: Some("api-key,chatgpt".into()),
            key: Some("toy-key".into()),
            base_url: Some(api_url.clone().into()),
        },
    )
    .await
    .unwrap();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model":"sol","messages":[{"role":"user","content":"hello"}]}).to_string(),
        ))
        .unwrap();
    let response = app(Arc::new(registry)).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("prompt is too long")
    );
    assert!(
        !error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("upstream marker")
    );
    assert_eq!(api_calls.load(Ordering::SeqCst), 3);
    assert_eq!(chat_calls.load(Ordering::SeqCst), 1);
    reject.store(2, Ordering::SeqCst);
    chat_success.store(true, Ordering::SeqCst);
    let registry = Registry::for_serve_with_input(
        AliasProvider::Anthropic,
        StartupInput {
            config_dir: dir.path().to_owned(),
            order: Some("api-key,chatgpt".into()),
            key: Some("toy-key".into()),
            base_url: Some(api_url.clone().into()),
        },
    )
    .await
    .unwrap();
    let router = app(Arc::new(registry));
    for _ in 0..2 {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"model":"sol","messages":[{"role":"user","content":"hello"}]}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
    }
    assert_eq!(api_calls.load(Ordering::SeqCst), 4);
    assert_eq!(chat_calls.load(Ordering::SeqCst), 3);
    let registry = Registry::for_serve_with_input(
        AliasProvider::Anthropic,
        StartupInput {
            config_dir: dir.path().to_owned(),
            order: None,
            key: Some("toy-key".into()),
            base_url: Some(api_url.clone().into()),
        },
    )
    .await
    .unwrap();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model":"sol","messages":[{"role":"user","content":"hello"}]}).to_string(),
        ))
        .unwrap();
    let legacy_monitor = MonitorHandle::new(10);
    assert_eq!(
        app_with_monitor(Arc::new(registry), Some(legacy_monitor.clone()))
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(api_calls.load(Ordering::SeqCst), 4);
    assert_eq!(chat_calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        legacy_monitor.snapshot().recent[0].codex_source.as_deref(),
        Some("chatgpt")
    );
    unsafe {
        std::env::set_var("DUOLANE_CODEX_MODEL", "gpt-5.5");
    }
    let registry = Registry::for_serve_with_input(
        AliasProvider::Anthropic,
        StartupInput {
            config_dir: dir.path().to_owned(),
            order: Some("chatgpt,api-key".into()),
            key: None,
            base_url: Some(api_url.clone().into()),
        },
    )
    .await
    .unwrap();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model":"gpt-99-sol","messages":[{"role":"user","content":"hello"}]})
                .to_string(),
        ))
        .unwrap();
    let response = app(Arc::new(registry)).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(chat_calls.load(Ordering::SeqCst), 4);
    chat_server.abort();
    api_server.abort();
}

#[tokio::test]
async fn missing_api_source_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::for_serve_with_input(
        AliasProvider::Anthropic,
        StartupInput {
            config_dir: dir.path().to_owned(),
            order: Some("api-key".into()),
            key: None,
            base_url: None,
        },
    )
    .await
    .unwrap();
    let monitor = MonitorHandle::new(10);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model":"sol","messages":[{"role":"user","content":"hello"}]}).to_string(),
        ))
        .unwrap();
    let response = app_with_monitor(Arc::new(registry), Some(monitor.clone()))
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let _ = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    assert_eq!(
        monitor.snapshot().recent[0].codex_source.as_deref(),
        Some("api key")
    );
}

#[tokio::test]
async fn messages_refreshes_api_catalog_only_after_interval() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let upstream = Router::new()
        .route(
            "/v1/models",
            get(move || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"data":[{"id":"gpt-99-sol"}]}))
                }
            }),
        )
        .route(
            "/v1/responses",
            post(|| async { (StatusCode::BAD_REQUEST, "{}") }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let registry = Registry::for_serve_with_input(
        AliasProvider::Anthropic,
        StartupInput {
            config_dir: dir.path().to_owned(),
            order: Some("api-key".into()),
            key: Some("toy-key".into()),
            base_url: Some(base.into()),
        },
    )
    .await
    .unwrap();
    let router = app(Arc::new(registry));
    tokio::time::pause();
    for after in [false, true] {
        if after {
            tokio::time::advance(std::time::Duration::from_secs(3601)).await;
        }
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"model":"gpt-99-sol","messages":[{"role":"user","content":"hello"}]})
                    .to_string(),
            ))
            .unwrap();
        let _ = router.clone().oneshot(request).await.unwrap();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), if after { 2 } else { 1 });
    }
    server.abort();
}
