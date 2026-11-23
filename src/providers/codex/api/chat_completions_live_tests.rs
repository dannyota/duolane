use super::handle;
use crate::{
    config::codex_auth::AuthSource,
    providers::codex::{
        api::{ApiClient, messages_tests::test_context},
        auth::api_key::OpenAiKey,
        chat_completions::request::translate_api_request,
        sources::{FailureKind, ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    },
};
use axum::{Router, routing::post};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn live_chat_commits_only_text_and_emits_success_or_safe_error() {
    for failed in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url =
            url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
        let app = Router::new().route("/v1/responses", post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                if failed {
                    concat!("data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n", "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_request_error\",\"message\":\"private sentinel\"}}}\n\n")
                } else {
                    concat!("data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n", "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"id\":\"resp_late\",\"model\":\"late-model\",\"created_at\":1,\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n")
                }
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
        let gate = ReplayGate::new(false);
        let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
        let request = translate_api_request(&json!({"model":"gpt-5.5","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hello"}]}), &resolve_tier_model_alias("gpt-5.5")).unwrap();
        let response = match handle(&request, &api, &attempt).await {
            Ok(r) => r,
            Err(_) => panic!("expected stream"),
        };
        assert!(gate.committed());
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let events = crate::anthropic::sse::parse_sse_events(&bytes);
        assert!(events.last().is_some_and(|event| event.data == "[DONE]"));
        let chunks: Vec<_> = events
            .iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .collect();
        assert!(chunks[0]["choices"][0]["delta"]["role"] == "assistant");
        assert!(chunks[1]["choices"][0]["delta"]["content"] == "hello");
        assert!(!String::from_utf8_lossy(&bytes).contains("private sentinel"));
        if failed {
            assert!(chunks.last().unwrap().get("error").is_some());
        } else {
            assert_eq!(chunks.last().unwrap()["usage"]["total_tokens"], 7);
            for key in ["id", "model", "created"] {
                assert!(chunks[0][key] == chunks[1][key]);
                assert!(chunks[0][key] == chunks.last().unwrap()[key]);
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}

#[tokio::test]
async fn live_chat_metadata_then_failure_is_not_committed() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(|| async { concat!("data: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n\n", "data: {\"type\":\"response.failed\",\"status\":401,\"response\":{\"error\":{\"message\":\"private sentinel\"}}}\n\n") }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let request = translate_api_request(
        &json!({"model":"gpt-5.5","stream":true,"messages":[{"role":"user","content":"hello"}]}),
        &resolve_tier_model_alias("gpt-5.5"),
    )
    .unwrap();
    let failure = match handle(&request, &api, &attempt).await {
        Err(f) => f,
        Ok(_) => panic!("expected failure"),
    };
    assert_eq!(failure.failure.kind, FailureKind::RejectedCredential);
    assert!(!gate.committed());
    assert!(failure.failure.credential == Some(api.credential_id()));
    server.abort();
}

#[tokio::test]
async fn live_chat_retries_eof_before_output() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(move || {
        let counter = counter.clone();
        async move {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 { "" } else {
                concat!("data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n", "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n")
            }
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let request = translate_api_request(
        &json!({"model":"gpt-5.5","stream":true,"messages":[{"role":"user","content":"hello"}]}),
        &resolve_tier_model_alias("gpt-5.5"),
    )
    .unwrap();
    let response = match handle(&request, &api, &attempt).await {
        Ok(r) => r,
        Err(_) => panic!("expected stream"),
    };
    assert!(gate.committed());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(
        crate::anthropic::sse::parse_sse_events(&bytes)
            .last()
            .is_some_and(|event| event.data == "[DONE]")
    );
    server.abort();
}

#[tokio::test]
async fn live_chat_accepts_completed_terminal_with_text_only() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(|| async {
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}]}}\n\n"
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let request = translate_api_request(
        &json!({"model":"gpt-5.5","stream":true,"messages":[{"role":"user","content":"hello"}]}),
        &resolve_tier_model_alias("gpt-5.5"),
    )
    .unwrap();
    let response = match handle(&request, &api, &attempt).await {
        Ok(r) => r,
        Err(_) => panic!("expected terminal content"),
    };
    assert!(gate.committed());
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let frames = crate::anthropic::sse::parse_sse_events(&bytes);
    let chunks: Vec<_> = frames
        .iter()
        .filter_map(|frame| serde_json::from_str::<serde_json::Value>(&frame.data).ok())
        .collect();
    assert!(chunks[1]["choices"][0]["delta"]["content"] == "hello");
    assert!(frames.last().is_some_and(|frame| frame.data == "[DONE]"));
    server.abort();
}

#[tokio::test]
async fn live_chat_bounds_serialized_output_before_and_after_commit() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(|| async {
        let mut body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n\n".repeat(8);
        body.push_str("data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n");
        assert!(body.len() < 700);
        body
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let request = translate_api_request(
        &json!({"model":"gpt-5.5","stream":true,"messages":[{"role":"user","content":"hello"}]}),
        &resolve_tier_model_alias("gpt-5.5"),
    )
    .unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    assert!(
        super::live::once_with_output_limit(&request, &api, &attempt, 1)
            .await
            .is_err()
    );
    assert!(!gate.committed());
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let response = match super::live::once_with_output_limit(&request, &api, &attempt, 700).await {
        Ok(r) => r,
        Err(_) => panic!("expected stream"),
    };
    assert!(gate.committed());
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(bytes.len() <= 700);
    let frames = crate::anthropic::sse::parse_sse_events(&bytes);
    assert!(frames.last().is_some_and(|frame| frame.data == "[DONE]"));
    assert!(
        frames
            .iter()
            .filter_map(|frame| serde_json::from_str::<serde_json::Value>(&frame.data).ok())
            .any(|value| value.get("error").is_some())
    );
    server.abort();
}
