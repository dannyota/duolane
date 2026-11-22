use serde_json::json;

#[test]
fn standalone_search_request_forces_one_call_and_preserves_filters() {
    let body = serde_json::from_value(json!({"model":"client","max_tokens":42,"messages":[{"role":"user","content":"find it"}],"tools":[{"type":"web_search_20250305","name":"web_search","allowed_domains":["a.example"],"blocked_domains":["b.example"]}],"tool_choice":{"type":"tool","name":"web_search"}})).unwrap();
    let model =
        crate::providers::codex::translate::model_allowlist::resolve_tier_model_alias("gpt-5.5");
    let request = super::search::build_request(&body, None, &model).unwrap().0;
    assert!(request["tool_choice"]["type"] == "web_search");
    assert!(request["max_tool_calls"] == 1);
    assert!(request["tools"][0]["filters"]["allowed_domains"] == json!(["a.example"]));
    assert!(request["tools"][0]["filters"]["blocked_domains"] == json!(["b.example"]));
    assert!(request["include"] == json!(["web_search_call.action.sources"]));
}

#[tokio::test]
async fn standalone_search_commits_one_call_and_vetoes_ambiguous_failures() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, search},
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
        let n = counter.fetch_add(1, Ordering::SeqCst);
        async move {
            match n {
                0 => concat!("data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"sources\":[{\"type\":\"url\",\"url\":\"https://example.com\",\"title\":\"Example\"}]}}}\n\n", "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"web_search_call\",\"id\":\"ws1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"sources\":[{\"type\":\"url\",\"url\":\"https://example.com\"}]}}]}}\n\n"),
                1 => concat!("data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws1\",\"status\":\"completed\"}}\n\n", "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n"),
                _ => "data: {\"type\":\"response.web_search_call.searching\",\"item_id\":\"ws2\"}\n\n",
            }
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let body = serde_json::from_value(json!({"model":"client","messages":[{"role":"user","content":"find it"}],"tools":[{"type":"web_search_20250305","name":"web_search"}],"tool_choice":{"type":"tool","name":"web_search"}})).unwrap();
    let model = resolve_tier_model_alias("gpt-5.5");
    let context = super::messages_tests::test_context();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &context, gate.clone());
    let response = match search::handle(&body, None, &model, &api, &attempt).await {
        Ok(response) => response,
        Err(_) => panic!("search failed"),
    };
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value["content"][0]["type"] == "server_tool_use");
    assert!(value["content"][1]["content"][0]["title"] == "Example");
    assert!(gate.hosted() && gate.committed());
    for _ in 0..2 {
        let gate = ReplayGate::new(false);
        let attempt = SourceAttempt::new(AuthSource::ApiKey, &context, gate.clone());
        let failure = search::handle(&body, None, &model, &api, &attempt)
            .await
            .unwrap_err();
        assert_eq!(failure.kind, FailureKind::Final);
        assert!(gate.ambiguous_hosted());
        assert!(!gate.committed());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    server.abort();
}

#[test]
fn standalone_search_uses_only_unambiguous_selected_filters() {
    let mut body: crate::anthropic::schema::MessagesRequest = serde_json::from_value(json!({"messages":[{"role":"user","content":"find"}],"tools":[{"type":"web_search_20250305","name":"first","allowed_domains":["first.example"]},{"type":"web_search_20250305","name":"chosen","blocked_domains":["chosen.example"]}],"tool_choice":{"type":"tool","name":"chosen"}})).unwrap();
    let model =
        crate::providers::codex::translate::model_allowlist::resolve_tier_model_alias("gpt-5.5");
    let (request, _, _) = super::search::build_request(&body, None, &model).unwrap();
    assert!(request["tools"][0]["filters"]["blocked_domains"] == json!(["chosen.example"]));
    assert!(
        request["tools"][0]["filters"]
            .get("allowed_domains")
            .is_none()
    );
    body.extra["tool_choice"]["name"] = json!("missing");
    assert!(super::search::build_request(&body, None, &model).is_err());
    body.extra["tool_choice"]["name"] = json!("first");
    body.extra["tools"][1]["name"] = json!("first");
    assert!(super::search::build_request(&body, None, &model).is_err());
}

#[test]
fn standalone_search_deduplicates_before_enriching_and_bounds_results() {
    let long_title = "x".repeat(1000);
    let mut titles =
        std::collections::HashMap::from([("https://example.com".to_owned(), long_title.clone())]);
    let sources = vec![json!({"type":"url","url":"https://example.com"}); 40];
    let result = super::search::enrich_sources(&sources, &mut titles, 1200).unwrap();
    assert_eq!(result.len(), 1);
    assert!(result[0]["title"] == long_title);
    assert!(super::search::enrich_sources(&sources, &mut titles, 100).is_err());
}

#[tokio::test]
async fn standalone_search_bounds_filtered_raw_events() {
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        api::{ApiClient, search},
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
    let terminal = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"web_search_call\",\"id\":\"ws1\",\"status\":\"completed\"}]}}\n\n";
    let payload = format!(
        "{}{}",
        "data: {\"type\":\"unknown_extension\"}\n\n".repeat(20),
        terminal
    );
    let low = terminal.len() + 10;
    let high = payload.len() + 1;
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            let payload = payload.clone();
            async move { payload }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = ApiClient::new(url, OpenAiKey::parse(b"toy-key").unwrap()).unwrap();
    let body = serde_json::from_value(json!({"messages":[{"role":"user","content":"find"}],"tools":[{"type":"web_search_20250305","name":"web_search"}],"tool_choice":{"type":"tool","name":"web_search"}})).unwrap();
    let context = super::messages_tests::test_context();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &context, gate.clone());
    let failure = search::handle_with_raw_limit(
        &body,
        None,
        &resolve_tier_model_alias("gpt-5.5"),
        &api,
        &attempt,
        low,
    )
    .await
    .unwrap_err();
    assert_eq!(failure.kind, FailureKind::Final);
    assert_eq!(failure.send, super::SendEvidence::OutcomeUnknown);
    assert!(gate.ambiguous_hosted());
    assert!(!gate.committed());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(AuthSource::ApiKey, &context, gate.clone());
    let response = search::handle_with_raw_limit(
        &body,
        None,
        &resolve_tier_model_alias("gpt-5.5"),
        &api,
        &attempt,
        high,
    )
    .await;
    assert!(response.is_ok());
    assert!(gate.hosted() && gate.committed());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}
