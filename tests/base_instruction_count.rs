#[path = "support/hermetic.rs"]
mod hermetic;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use duolane::{
    config::AliasProvider, providers::codex::source_startup::StartupInput, registry::Registry,
    server::app,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn count_uses_startup_source_order_with_temporary_catalog() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("models_cache.json"), json!({"models":[{"slug":"gpt-99-sol","supported_in_api":true,"visibility":"list","use_responses_lite":true,"model_messages":{"instructions_template":"SYNTHETIC BASE"}}]}).to_string()).unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"codex":{"authOrder":["api-key","chatgpt"]}}"#,
    )
    .unwrap();
    unsafe {
        std::env::set_var("CODEX_HOME", dir.path());
        std::env::set_var("DUOLANE_CONFIG_DIR", dir.path());
    }
    let registry = Registry::for_serve_with_input(
        AliasProvider::Anthropic,
        StartupInput {
            config_dir: dir.path().to_owned(),
            order: None,
            key: None,
            base_url: None,
        },
    )
    .await
    .unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"codex":{"authOrder":["chatgpt","api-key"]}}"#,
    )
    .unwrap();
    let router = app(Arc::new(registry));
    let count = |router: axum::Router| async move {
        let request = Request::builder().method("POST").uri("/v1/messages/count_tokens").header("content-type", "application/json").body(Body::from(json!({"model":"gpt-99-sol","messages":[{"role":"user","content":"hello"}],"tools":[{"name":"Read","input_schema":{"type":"object"}}]}).to_string())).unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()["input_tokens"]
            .as_u64()
            .unwrap()
    };
    let enabled = count(router.clone()).await;
    let chat_registry = Registry::for_serve_with_input(
        AliasProvider::Anthropic,
        StartupInput {
            config_dir: dir.path().to_owned(),
            order: None,
            key: None,
            base_url: None,
        },
    )
    .await
    .unwrap();
    let chat_router = app(Arc::new(chat_registry));
    let chat_enabled = count(chat_router.clone()).await;
    unsafe {
        std::env::set_var("DUOLANE_CODEX_BASE_INSTRUCTIONS", "false");
    }
    let disabled = count(router).await;
    assert_eq!(enabled, disabled);
    let chat_disabled = count(chat_router).await;
    assert!(chat_enabled > chat_disabled);
}
