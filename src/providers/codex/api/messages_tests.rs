use serde_json::json;

#[test]
fn messages_request_preserves_api_wire_fields() {
    use crate::anthropic::schema::MessagesRequest;
    use crate::providers::codex::translate::model_allowlist::resolve_tier_model_alias;
    use crate::providers::codex::translate::reasoning_signature::ReasoningAccount;
    use crate::providers::codex::translate::request::{TranslateOptions, translate_request};

    let body: MessagesRequest = serde_json::from_value(json!({
        "model":"original", "max_tokens":321, "messages":[{"role":"user","content":"hello"}],
        "output_config":{"effort":"high"},
        "tools":[{"name":"lookup","defer_loading":true,"input_schema":{"type":"object","properties":{}}},
                 {"type":"web_search_20250305","name":"web_search"}],
        "tool_choice":{"type":"auto","disable_parallel_tool_use":true}
    })).unwrap();
    let resolved = resolve_tier_model_alias("gpt-6-astra-ultrafast");
    let account = ReasoningAccount::for_api_key(b"toy-key");
    let wire =
        super::messages::build_request(&body, &resolved, Some("session".into()), &account).unwrap();
    assert_eq!(wire["model"], "gpt-6-astra");
    assert_eq!(wire["max_output_tokens"], 321);
    assert_eq!(wire["store"], false);
    assert_eq!(wire["parallel_tool_calls"], false);
    assert_eq!(wire["service_tier"], "priority");
    assert_eq!(wire["prompt_cache_key"], "session");
    assert!(wire.get("previous_response_id").is_none());
    assert!(wire.get("client_metadata").is_none());
    assert!(
        wire["include"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "reasoning.encrypted_content")
    );
    assert!(
        wire["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "lookup")
    );
    assert!(
        wire["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["type"] == "web_search")
    );
    let chatgpt = translate_request(
        &body,
        TranslateOptions {
            session_id: None,
            service_tier: resolved.service_tier.clone(),
            model: resolved.model.clone(),
            use_responses_lite: false,
            reasoning_account: account,
        },
    )
    .unwrap();
    assert_eq!(
        chatgpt.service_tier,
        Some(crate::providers::codex::translate::request::ServiceTier::Ultrafast)
    );
    assert!(
        serde_json::to_value(chatgpt)
            .unwrap()
            .get("max_output_tokens")
            .is_none()
    );
}

#[test]
fn messages_request_does_not_replay_other_key_reasoning() {
    use crate::anthropic::schema::MessagesRequest;
    use crate::providers::codex::translate::model_allowlist::resolve_tier_model_alias;
    use crate::providers::codex::translate::reasoning_signature::{
        ReasoningAccount, ReasoningReplay,
    };
    let first = ReasoningAccount::for_api_key(b"toy-key");
    let other = ReasoningAccount::for_api_key(b"other-key");
    let signature = first
        .sign(&ReasoningReplay {
            id: "rs_test".into(),
            encrypted_content: "opaque".into(),
        })
        .unwrap();
    let body: MessagesRequest = serde_json::from_value(json!({"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"plan","signature":signature}]}]})).unwrap();
    let model = resolve_tier_model_alias("gpt-5.5");
    let same = super::messages::build_request(&body, &model, None, &first).unwrap();
    let different = super::messages::build_request(&body, &model, None, &other).unwrap();
    assert!(
        same["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["type"] == "reasoning")
    );
    assert!(
        !different["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["type"] == "reasoning")
    );
    assert!(different["input"].to_string().contains("plan"));
    assert!(different.get("max_output_tokens").is_none());
}

#[tokio::test]
async fn buffered_messages_success_and_transient_retry() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, messages::handle_buffered},
        auth::api_key::OpenAiKey,
        sources::{ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Json, Router, http::StatusCode, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(move |Json(body): Json<serde_json::Value>| {
        let counter = counter.clone();
        async move {
            assert_eq!(body["max_output_tokens"], 42);
            assert_eq!(body["model"], "gpt-5.5");
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                (StatusCode::SERVICE_UNAVAILABLE, [("retry-after", "0")], "private upstream sentinel")
            } else {
                (StatusCode::OK, [("retry-after", "0")], concat!(
                    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"))
            }
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let body = serde_json::from_value(json!({"model":"client-model","max_tokens":42,"messages":[{"role":"user","content":"hello"}]})).unwrap();
    let response =
        match handle_buffered(&body, &resolve_tier_model_alias("gpt-5.5"), &api, &attempt).await {
            Ok(r) => r,
            Err(_) => panic!("expected success"),
        };
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["content"][0]["text"], "hello");
    assert_eq!(value["usage"]["input_tokens"], 5);
    assert_eq!(value["usage"]["output_tokens"], 2);
    assert!(gate.committed());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn buffered_hosted_eof_vetoes_replay() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, SendEvidence, messages::handle_buffered},
        auth::api_key::OpenAiKey,
        sources::{FailureKind, ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Json, Router, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(move |Json(body): Json<serde_json::Value>| {
        let counter = counter.clone();
        async move {
            assert!(body["tools"].as_array().unwrap().iter().any(|tool| tool["type"] == "web_search"));
            counter.fetch_add(1, Ordering::SeqCst);
            "data: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n\n"
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(true);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let body = serde_json::from_value(json!({"model":"client-model","messages":[{"role":"user","content":"hello"}],"tools":[{"type":"web_search_20250305","name":"web_search"}]})).unwrap();
    let failure =
        match handle_buffered(&body, &resolve_tier_model_alias("gpt-5.5"), &api, &attempt).await {
            Err(e) => e,
            Ok(_) => panic!("expected failure"),
        };
    assert_eq!(failure.kind, FailureKind::Final);
    assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
    assert!(failure.credential == Some(api.credential_id()));
    assert!(gate.ambiguous_hosted());
    assert!(!gate.committed());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

pub(super) fn test_context() -> crate::provider::RequestContext {
    crate::provider::RequestContext {
        recovery_budget: Default::default(),
        req_id: String::new(),
        session_id: None,
        traffic: None,
        monitor: None,
        passthrough: None,
    }
}

#[tokio::test]
async fn buffered_function_calls_require_completed_arguments() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, messages::handle_buffered},
        auth::api_key::OpenAiKey,
        sources::{FailureKind, ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Router, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(move || {
        let counter = counter.clone();
        async move {
            let args = if counter.fetch_add(1, Ordering::SeqCst) == 0 { "{}" } else { "not-json" };
            let start = json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"item1","call_id":"call1","name":"lookup","arguments":""}});
            let done = json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"item1","call_id":"call1","name":"lookup","arguments":args,"status":"completed"}});
            let terminal = json!({"type":"response.completed","response":{"status":"completed"}});
            format!("data: {start}\n\ndata: {done}\n\ndata: {terminal}\n\n")
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let body = serde_json::from_value(
        json!({"model":"client-model","messages":[{"role":"user","content":"hello"}]}),
    )
    .unwrap();
    let model = resolve_tier_model_alias("gpt-5.5");
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let response = match handle_buffered(&body, &model, &api, &attempt).await {
        Ok(r) => r,
        Err(_) => panic!("expected tool call"),
    };
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["content"][0]["type"], "tool_use");
    assert_eq!(value["content"][0]["input"], json!({}));
    assert!(gate.committed());
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let failure = match handle_buffered(&body, &model, &api, &attempt).await {
        Err(e) => e,
        Ok(_) => panic!("invalid arguments accepted"),
    };
    assert_eq!(failure.kind, FailureKind::Final);
    assert!(!gate.committed());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn buffered_empty_completion_is_final_without_retry() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, messages::handle_buffered},
        auth::api_key::OpenAiKey,
        sources::{FailureKind, ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Router, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
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
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let body =
        serde_json::from_value(json!({"messages":[{"role":"user","content":"hello"}]})).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let failure =
        match handle_buffered(&body, &resolve_tier_model_alias("gpt-5.5"), &api, &attempt).await {
            Err(e) => e,
            Ok(_) => panic!("empty stream accepted"),
        };
    assert_eq!(failure.kind, FailureKind::Final);
    assert!(!gate.committed());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn buffered_hosted_rejection_is_typed_and_safe_to_switch() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, SendEvidence, messages::handle_buffered},
        auth::api_key::OpenAiKey,
        sources::{FailureKind, ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Router, http::StatusCode, routing::post};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route(
        "/v1/responses",
        post(|| async {
            (
                StatusCode::UNAUTHORIZED,
                r#"{"error":{"message":"private upstream sentinel"}}"#,
            )
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let body = serde_json::from_value(json!({"messages":[{"role":"user","content":"hello"}],"tools":[{"type":"web_search_20250305","name":"web_search"}]})).unwrap();
    let gate = ReplayGate::new(true);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let failure =
        match handle_buffered(&body, &resolve_tier_model_alias("gpt-5.5"), &api, &attempt).await {
            Err(e) => e,
            Ok(_) => panic!("rejected request accepted"),
        };
    assert_eq!(failure.kind, FailureKind::RejectedCredential);
    assert_eq!(failure.send, SendEvidence::DeniedBeforeExecution);
    assert!(failure.credential == Some(api.credential_id()));
    assert!(!failure.error.message.contains("private upstream sentinel"));
    assert!(gate.may_switch(failure.kind, failure.send));
    server.abort();
}

#[tokio::test]
async fn buffered_unhosted_eof_retries_before_output() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, messages::handle_buffered},
        auth::api_key::OpenAiKey,
        sources::{ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Router, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(move || {
        let counter = counter.clone();
        async move {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 { "" } else { concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n") }
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let body =
        serde_json::from_value(json!({"messages":[{"role":"user","content":"hello"}]})).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let result = handle_buffered(&body, &resolve_tier_model_alias("gpt-5.5"), &api, &attempt).await;
    assert!(result.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(gate.committed());
    server.abort();
}

#[test]
fn structured_errors_prefer_permanent_codes_and_redact() {
    use super::{ApiFailure, ApiFailureKind, SendEvidence};
    for code in [
        "context_length_exceeded",
        "invalid_prompt",
        "cyber_policy",
        "bio_policy",
    ] {
        let failure = ApiFailure::upstream(
            503,
            Some(&json!({"error":{"code":code,"message":"private sentinel"}})),
            false,
            false,
        );
        assert_eq!(
            failure.kind,
            if code == "context_length_exceeded" {
                ApiFailureKind::ContextOverflow
            } else {
                ApiFailureKind::Invalid
            }
        );
        assert_eq!(failure.error.status, 400);
        assert!(!failure.error.message.contains("private sentinel"));
        if code == "context_length_exceeded" {
            assert!(failure.error.message.contains("prompt is too long"));
            assert!(crate::providers::codex::events::is_context_overflow(
                None,
                &failure.error.message
            ));
        }
    }
    let transient = ApiFailure::upstream(
        0,
        Some(&json!({"error":{"type":"server_error"}})),
        false,
        true,
    );
    assert_eq!(transient.kind, ApiFailureKind::Transient);
    let quota = ApiFailure::upstream(
        503,
        Some(&json!({"error":{"type":"insufficient_quota"}})),
        false,
        false,
    );
    assert_eq!(quota.kind, ApiFailureKind::Transient);
    assert_eq!(quota.send, SendEvidence::OutcomeUnknown);
}

#[test]
fn api_strict_tool_observer_waits_for_real_done() {
    use crate::providers::codex::client_http_tools::HttpToolState;
    let mut state = HttpToolState::default();
    let mut start = json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call1","name":"Read","id":"item1","arguments":""}});
    assert!(!state.observe_strict(&mut start).unwrap());
    let mut delta = json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":format!("{{\"file_path\":\"/toy\"}}{}", " ".repeat(1024))});
    assert!(!state.observe_strict(&mut delta).unwrap());
    assert!(
        state
            .validate_terminal(&json!({"type":"response.completed"}))
            .is_err()
    );
    let mut done = json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"call1","name":"Read","arguments":"{\"file_path\":\"/actual\"}","status":"completed"}});
    assert!(state.observe_strict(&mut done).unwrap());
    assert_eq!(done["item"]["arguments"], "{\"file_path\":\"/actual\"}");
}

#[tokio::test]
async fn buffered_unindexed_tool_does_not_collide_with_text_indices() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, messages::handle_buffered},
        auth::api_key::OpenAiKey,
        sources::{ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Router, routing::post};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let app = Router::new().route("/v1/responses", post(|| async { concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"before\"}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"it\",\"call_id\":\"call\",\"name\":\"lookup\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call\",\"name\":\"lookup\",\"arguments\":\"{}\",\"status\":\"completed\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"after\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"message\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"later\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"message\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n") }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let body =
        serde_json::from_value(json!({"messages":[{"role":"user","content":"hello"}]})).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate);
    let response =
        match handle_buffered(&body, &resolve_tier_model_alias("gpt-5.5"), &api, &attempt).await {
            Ok(r) => r,
            Err(_) => panic!("expected complete response"),
        };
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["content"][0]["text"], "beforeafter");
    assert_eq!(value["content"][1]["type"], "tool_use");
    assert_eq!(value["content"][2]["text"], "later");
    server.abort();
}

#[tokio::test]
async fn buffered_http_error_with_hosted_evidence_cannot_switch() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, SendEvidence, messages::handle_buffered},
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
    let app = Router::new().route("/v1/responses", post(move || {
        let counter = counter.clone();
        async move {
            let status = if counter.fetch_add(1, Ordering::SeqCst) == 0 { StatusCode::UNAUTHORIZED } else { StatusCode::TOO_MANY_REQUESTS };
            (status, r#"{"response":{"output":[{"type":"web_search_call"}],"error":{"code":"insufficient_quota","message":"private sentinel"}}}"#)
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let body = serde_json::from_value(json!({"messages":[{"role":"user","content":"hello"}],"tools":[{"type":"web_search_20250305","name":"web_search"}]})).unwrap();
    for _ in 0..2 {
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
        let failure = match handle_buffered(
            &body,
            &resolve_tier_model_alias("gpt-5.5"),
            &api,
            &attempt,
        )
        .await
        {
            Err(e) => e,
            Ok(_) => panic!("expected rejection"),
        };
        assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
        assert!(gate.hosted());
        assert!(!gate.may_switch(failure.kind, failure.send));
        assert!(!failure.error.message.contains("private sentinel"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[test]
fn explicit_client_error_with_server_code_is_final_and_redacted() {
    use super::{ApiFailure, ApiFailureKind, SendEvidence};
    use crate::providers::codex::sources::{FailureKind, ReplayGate};
    for status in [400, 403] {
        let failure = ApiFailure::upstream(
            status,
            Some(&json!({"error":{"code":"server_error","message":"private sentinel"}})),
            false,
            false,
        );
        assert_eq!(failure.kind, ApiFailureKind::Invalid);
        assert_eq!(failure.error.status, status);
        assert_eq!(failure.send, SendEvidence::OutcomeUnknown);
        assert!(!failure.error.message.contains("private sentinel"));
        assert!(!ReplayGate::new(false).may_switch(FailureKind::Final, failure.send));
    }
}

#[tokio::test]
async fn live_denial_is_typed_before_commit() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, ApiFailureKind, messages_live::stream_once},
        auth::api_key::OpenAiKey,
        sources::{ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Router, http::StatusCode, routing::post};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/responses", post(|| async { StatusCode::UNAUTHORIZED })),
        )
        .await
        .unwrap()
    });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let body = serde_json::from_value(
        json!({"model":"client", "stream":true, "messages":[{"role":"user","content":"hello"}]}),
    )
    .unwrap();
    let error = match stream_once(&body, &resolve_tier_model_alias("gpt-5.5"), &api, &attempt).await
    {
        Ok(_) => panic!("expected denial"),
        Err(e) => e,
    };
    assert_eq!(error.kind, ApiFailureKind::Rejected);
    assert!(!gate.committed());
    server.abort();
}

#[tokio::test]
async fn live_committed_failure_stays_in_sanitized_stream() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, messages_live::stream_once},
        auth::api_key::OpenAiKey,
        sources::{ReplayGate, SourceAttempt},
        translate::model_allowlist::resolve_tier_model_alias,
    };
    use axum::{Router, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = url::Url::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/v1/responses", post(move || { let counter = counter.clone(); async move { counter.fetch_add(1, Ordering::SeqCst); concat!("data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n", "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hello\"}\n\n", "data: {\"type\":\"error\",\"error\":{\"type\":\"server_error\",\"message\":\"private sentinel\"}}\n\n") } }))).await.unwrap()
    });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &test_context(), gate.clone());
    let body = serde_json::from_value(
        json!({"model":"client", "stream":true, "messages":[{"role":"user","content":"hello"}]}),
    )
    .unwrap();
    let response =
        match stream_once(&body, &resolve_tier_model_alias("gpt-5.5"), &api, &attempt).await {
            Ok(r) => r,
            Err(_) => panic!("expected stream"),
        };
    assert!(gate.committed());
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let output = std::str::from_utf8(&bytes).unwrap();
    assert!(output.contains("hello"));
    assert!(output.contains("event: error"));
    assert!(!output.contains("private sentinel"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}
