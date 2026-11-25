#[path = "support/hermetic.rs"]
mod hermetic;

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use duolane::{config::AliasProvider, registry::Registry, server::app};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tower::ServiceExt;

#[tokio::test]
async fn account_retry_keeps_selected_template_after_catalog_changes() {
    let dir = tempfile::tempdir().unwrap();
    let auth = dir.path().join("auth.json");
    let catalog = dir.path().join("models_cache.json");
    let auth_json = |account: &str| {
        json!({"tokens":{"access_token":"toy-access","refresh_token":"toy-refresh","account_id":account}}).to_string()
    };
    let catalog_json = |template: &str| {
        json!({"models":[{"slug":"gpt-99-sol","supported_in_api":true,"visibility":"list","use_responses_lite":true,"model_messages":{"instructions_template":template}}]}).to_string()
    };
    std::fs::write(&auth, auth_json("first-account")).unwrap();
    std::fs::write(&catalog, catalog_json("SYNTHETIC BASE ONE")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let valid = Arc::new(AtomicBool::new(true));
    let router = Router::new().fallback(post({
        let calls = calls.clone(); let valid = valid.clone();
        move |Json(body): Json<Value>| { let calls = calls.clone(); let valid = valid.clone(); let auth = auth.clone(); let catalog = catalog.clone(); async move {
            valid.fetch_and(body.to_string().contains("SYNTHETIC BASE ONE") && !body.to_string().contains("SYNTHETIC BASE TWO"), Ordering::SeqCst);
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                std::fs::write(auth, json!({"tokens":{"access_token":"toy-access-new","refresh_token":"toy-refresh-new","account_id":"second-account"}}).to_string()).unwrap();
                std::fs::write(catalog, json!({"models":[{"slug":"gpt-99-sol","supported_in_api":true,"visibility":"list","use_responses_lite":true,"model_messages":{"instructions_template":"SYNTHETIC BASE TWO"}}]}).to_string()).unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
                return (StatusCode::UNAUTHORIZED, "{}");
            }
            (StatusCode::OK, "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n")
        }}
    }));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    unsafe {
        std::env::set_var("DUOLANE_CODEX_AUTH_FILE", dir.path().join("auth.json"));
        std::env::set_var(
            "DUOLANE_CODEX_BASE_URL",
            format!("http://{address}/responses"),
        );
        std::env::set_var("DUOLANE_CODEX_TRANSPORT", "http");
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    }
    let request = Request::builder().method("POST").uri("/v1/messages").header("content-type", "application/json").body(Body::from(json!({"model":"gpt-99-sol","messages":[{"role":"user","content":"hello"}],"tools":[{"name":"Read","input_schema":{"type":"object"}}]}).to_string())).unwrap();
    let response = app(Arc::new(Registry::new(AliasProvider::Anthropic)))
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(valid.load(Ordering::SeqCst));
    server.abort();
}
