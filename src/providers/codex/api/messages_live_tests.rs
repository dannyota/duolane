use super::messages_tests::test_context;
use serde_json::json;

async fn live_fixture(
    wire: &'static str,
    hosted: bool,
) -> (
    super::ApiClient,
    crate::providers::codex::sources::SourceAttempt,
    crate::providers::codex::sources::ReplayGate,
    tokio::task::JoinHandle<()>,
    crate::anthropic::schema::MessagesRequest,
) {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        auth::api_key::OpenAiKey,
        sources::{ReplayGate, SourceAttempt},
    };
    use axum::{Router, routing::post};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/responses", post(move || async move { wire })),
        )
        .await
        .unwrap()
    });
    let api = super::ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(hosted);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let body = serde_json::from_value(if hosted { json!({"model":"client","stream":true,"messages":[{"role":"user","content":"hello"}],"tools":[{"type":"web_search_20250305","name":"web_search"}]}) } else { json!({"model":"client","stream":true,"messages":[{"role":"user","content":"hello"}]}) }).unwrap();
    (api, attempt, gate, server, body)
}

#[tokio::test]
async fn live_valid_tool_emitted_once() {
    use crate::providers::codex::{
        api::messages_live::stream_once, translate::model_allowlist::resolve_tier_model_alias,
    };
    let wire = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"it\",\"call_id\":\"call\",\"name\":\"lookup\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"item_id\":\"it\",\"delta\":\"{}\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"it\",\"call_id\":\"call\",\"name\":\"lookup\",\"arguments\":\"{}\",\"status\":\"completed\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
    );
    let (api, attempt, _, server, body) = live_fixture(wire, false).await;
    let response =
        match stream_once(&body, &resolve_tier_model_alias("gpt-5.5"), &api, &attempt).await {
            Ok(r) => r,
            Err(_) => panic!("expected stream"),
        };
    let bytes = axum::body::to_bytes(response.into_body(), 100_000)
        .await
        .unwrap();
    let output = std::str::from_utf8(&bytes).unwrap();
    assert_eq!(output.matches("\"type\":\"tool_use\"").count(), 1);
    assert_eq!(output.matches("\"id\":\"call\"").count(), 1);
    assert!(output.contains("event: message_stop"));
    assert!(!output.contains("event: error"));
    server.abort();
}

#[tokio::test]
async fn live_inconsistent_terminal_is_error_after_commit() {
    use crate::providers::codex::{
        api::messages_live::stream_once, translate::model_allowlist::resolve_tier_model_alias,
    };
    let wire = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"in_progress\"}}\n\n"
    );
    let (api, attempt, gate, server, body) = live_fixture(wire, false).await;
    let response =
        match stream_once(&body, &resolve_tier_model_alias("gpt-5.5"), &api, &attempt).await {
            Ok(r) => r,
            Err(_) => panic!("expected stream"),
        };
    assert!(gate.committed());
    let bytes = axum::body::to_bytes(response.into_body(), 100_000)
        .await
        .unwrap();
    let output = std::str::from_utf8(&bytes).unwrap();
    assert!(output.contains("event: error"));
    assert!(!output.contains("event: message_stop"));
    server.abort();
}

#[tokio::test]
async fn live_hosted_deferred_text_obeys_event_cap_before_commit() {
    use crate::providers::codex::{
        api::messages_live::stream_once_with_limit,
        translate::model_allowlist::resolve_tier_model_alias,
    };
    let wire = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"message\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"deferred text beyond the budget\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"more deferred text\"}\n\n"
    );
    let (api, attempt, gate, server, body) = live_fixture(wire, true).await;
    let error = match stream_once_with_limit(
        &body,
        &resolve_tier_model_alias("gpt-5.5"),
        &api,
        &attempt,
        400,
    )
    .await
    {
        Ok(_) => panic!("expected limit failure"),
        Err(e) => e,
    };
    assert_eq!(error.kind, super::ApiFailureKind::Other);
    assert!(gate.hosted());
    assert!(!gate.committed());
    server.abort();
}

#[tokio::test]
async fn live_hosted_amplified_results_are_bounded() {
    use crate::providers::codex::{
        api::messages_live::stream_once_with_limits,
        translate::model_allowlist::resolve_tier_model_alias,
    };
    let wire = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws1\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws1\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws2\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws2\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws3\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws3\"}}\n\n",
        "data: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\",\"title\":\"A long enough title to cause output amplification when copied repeatedly\",\"url\":\"https://example.test/long-enough-path-for-amplification\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
    );
    let (api, attempt, gate, server, body) = live_fixture(wire, true).await;
    let result = stream_once_with_limits(
        &body,
        &resolve_tier_model_alias("gpt-5.5"),
        &api,
        &attempt,
        100_000,
        7_000,
    )
    .await;
    assert!(result.is_err());
    assert!(gate.hosted());
    assert!(!gate.committed());
    server.abort();
}

#[tokio::test]
async fn live_hosted_small_results_succeed_under_output_bound() {
    use crate::providers::codex::{
        api::messages_live::stream_once_with_limits,
        translate::model_allowlist::resolve_tier_model_alias,
    };
    let wire = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws\"}}\n\n",
        "data: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\",\"title\":\"Example\",\"url\":\"https://example.test/\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
    );
    let (api, attempt, gate, server, body) = live_fixture(wire, true).await;
    let response = match stream_once_with_limits(
        &body,
        &resolve_tier_model_alias("gpt-5.5"),
        &api,
        &attempt,
        100_000,
        7_000,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => panic!("expected response"),
    };
    let bytes = axum::body::to_bytes(response.into_body(), 100_000)
        .await
        .unwrap();
    let output = std::str::from_utf8(&bytes).unwrap();
    assert!(gate.committed());
    assert!(output.contains("web_search_tool_result"));
    assert!(output.contains("event: message_stop"));
    assert!(!output.contains("event: error"));
    server.abort();
}

#[tokio::test]
async fn handle_recovers_stream_and_delegates_buffered() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::messages::handle,
        auth::api_key::OpenAiKey,
        sources::{ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Router, http::StatusCode, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/v1/responses", post(move || { let counter = counter.clone(); async move {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 { (StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "0")], "") } else { (StatusCode::OK, [("retry-after", "0")], concat!("data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n", "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\n", "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n", "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n")) }
    }}))).await.unwrap()
    });
    let api = super::ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let model = resolve_tier_model_alias("gpt-5.5");
    for stream in [true, false] {
        let gate = ReplayGate::new(false);
        let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
        let body = serde_json::from_value(json!({"model":"client","stream":stream,"messages":[{"role":"user","content":"hello"}]})).unwrap();
        let response = match handle(&body, None, &model, &api, &attempt).await {
            Ok(response) => response,
            Err(_) => panic!("expected success"),
        };
        assert!(gate.committed());
        let bytes = axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap();
        if stream {
            assert!(
                std::str::from_utf8(&bytes)
                    .unwrap()
                    .contains("event: message_stop")
            );
        } else {
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["content"][0]["text"], "hello");
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn handle_hosted_veto_and_transient_exhaustion() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{SendEvidence, messages::handle},
        auth::api_key::OpenAiKey,
        sources::{FailureKind, ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Router, http::StatusCode, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v1/responses",
                post(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("retry-after", "0")],
                            "private sentinel",
                        )
                    }
                }),
            ),
        )
        .await
        .unwrap()
    });
    let api = super::ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let model = resolve_tier_model_alias("gpt-5.5");
    for hosted in [true, false] {
        let gate = ReplayGate::new(hosted);
        let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
        let body = serde_json::from_value(if hosted { json!({"stream":true,"messages":[{"role":"user","content":"hello"}],"tools":[{"type":"web_search_20250305","name":"web_search"}]}) } else { json!({"stream":true,"messages":[{"role":"user","content":"hello"}]}) }).unwrap();
        let failure = match handle(&body, None, &model, &api, &attempt).await {
            Ok(_) => panic!("expected failure"),
            Err(e) => e,
        };
        assert_eq!(
            failure.kind,
            if hosted {
                FailureKind::Final
            } else {
                FailureKind::TransientExhausted
            }
        );
        assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
        assert!(failure.credential == Some(api.credential_id()));
        assert_eq!(failure.error.status, 503);
        assert!(!failure.error.message.contains("private sentinel"));
        assert_eq!(gate.ambiguous_hosted(), hosted);
        assert!(!gate.committed());
        assert_eq!(calls.load(Ordering::SeqCst), if hosted { 1 } else { 5 });
    }
    server.abort();
}
