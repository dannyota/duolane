use super::{ApiClient, chat_completions::handle_buffered, messages_tests::test_context};
use crate::{
    config::codex_auth::AuthSource,
    providers::codex::{
        auth::api_key::OpenAiKey,
        chat_completions::request::translate_api_request,
        sources::{ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    },
};
use axum::{Router, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn buffered_chat_preserves_denial_evidence_and_does_not_retry() {
    use crate::providers::codex::{api::SendEvidence, sources::FailureKind};
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                (StatusCode::UNAUTHORIZED, "private upstream sentinel")
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let request = translate_api_request(
        &json!({"model":"gpt-5.5","messages":[{"role":"user","content":"hello"}]}),
        &resolve_tier_model_alias("gpt-5.5"),
    )
    .unwrap();
    let failure = match handle_buffered(&request, &api, &attempt).await {
        Err(failure) => failure,
        Ok(_) => panic!("expected denial"),
    };
    assert_eq!(failure.kind, FailureKind::RejectedCredential);
    assert_eq!(failure.send, SendEvidence::DeniedBeforeExecution);
    assert!(failure.credential == Some(api.credential_id()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!gate.committed());
    server.abort();
}

#[tokio::test]
async fn buffered_chat_retries_transient_and_commits_completion() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(move || {
        let counter = counter.clone();
        async move {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                (StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "0")], "private upstream sentinel")
            } else {
                (StatusCode::OK, [("retry-after", "0")], concat!(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"))
            }
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let request = translate_api_request(
        &json!({"model":"gpt-5.5","messages":[{"role":"user","content":"hello"}]}),
        &resolve_tier_model_alias("gpt-5.5"),
    )
    .unwrap();
    let response = match handle_buffered(&request, &api, &attempt).await {
        Ok(response) => response,
        Err(_) => panic!("expected buffered success"),
    };
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value["choices"][0]["message"]["content"] == "hello");
    assert_eq!(value["usage"]["total_tokens"], 7);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(gate.committed());
    server.abort();
}

#[tokio::test]
async fn buffered_chat_rejects_empty_and_contradictory_terminal() {
    use crate::providers::codex::sources::FailureKind;
    for body in [
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.completed\",\"status\":200,\"status_code\":500,\"response\":{\"status\":\"completed\"}}\n\n"
        ),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url =
            url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
        let app = Router::new().route(
            "/v1/responses",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    body
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
        let gate = ReplayGate::new(false);
        let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
        let request = translate_api_request(
            &json!({"model":"gpt-5.5","messages":[{"role":"user","content":"hello"}]}),
            &resolve_tier_model_alias("gpt-5.5"),
        )
        .unwrap();
        let failure = match handle_buffered(&request, &api, &attempt).await {
            Err(failure) => failure,
            Ok(_) => panic!("expected final failure"),
        };
        assert_eq!(failure.kind, FailureKind::Final);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!gate.committed());
        server.abort();
    }
}

#[tokio::test]
async fn buffered_chat_retries_unexpected_eof_before_output() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(move || {
        let counter = counter.clone();
        async move {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ""
            } else {
                concat!(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
                )
            }
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let request = translate_api_request(
        &json!({"model":"gpt-5.5","messages":[{"role":"user","content":"hello"}]}),
        &resolve_tier_model_alias("gpt-5.5"),
    )
    .unwrap();
    let response = handle_buffered(&request, &api, &attempt).await;
    assert!(response.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(gate.committed());
    server.abort();
}

#[tokio::test]
async fn buffered_chat_does_not_retry_done_or_pending_hosted() {
    use crate::providers::codex::sources::FailureKind;
    for (first, hosted) in [
        (
            "data: {\"type\":\"response.web_search_call.completed\"}\n",
            true,
        ),
        ("data: [DONE]\n\n", false),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url =
            url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
        let app = Router::new().route("/v1/responses", post(move || {
            let counter = counter.clone();
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    first
                } else {
                    concat!(
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
                    )
                }
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
        let gate = ReplayGate::new(false);
        let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
        let request = translate_api_request(
            &json!({"model":"gpt-5.5","messages":[{"role":"user","content":"hello"}]}),
            &resolve_tier_model_alias("gpt-5.5"),
        )
        .unwrap();
        let failure = match handle_buffered(&request, &api, &attempt).await {
            Err(failure) => failure,
            Ok(_) => panic!("expected failure"),
        };
        assert_eq!(failure.kind, FailureKind::Final);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(gate.hosted(), hosted);
        assert!(!gate.committed());
        server.abort();
    }
}
