use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

use crate::anthropic::schema::MessagesRequest;
use crate::provider::RequestContext;

use super::client::CodexHttpClient;
use super::translate::reasoning_signature::ReasoningAccount;
use super::*;

pub(super) fn live_test_request(text: &str) -> translate::request::ResponsesRequest {
    translate::request::ResponsesRequest {
        model: "gpt-5.6-sol".to_string(),
        instructions: None,
        input: vec![translate::request::ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![translate::request::ResponsesContentPart::InputText {
                text: text.to_string(),
            }],
        }],
        tools: None,
        tool_choice: None,
        store: false,
        stream: true,
        parallel_tool_calls: true,
        include: None,
        client_metadata: None,
        service_tier: None,
        prompt_cache_key: None,
        text: translate::request::ResponsesText {
            verbosity: None,
            format: None,
        },
        reasoning: None,
        reasoning_account: ReasoningAccount::new(Some("acct")),
    }
}

pub(super) fn live_test_context(session_id: &str) -> RequestContext {
    RequestContext {
        req_id: format!("request-{session_id}"),
        session_id: Some(session_id.to_string()),
        traffic: None,
        monitor: None,
        passthrough: None,
    }
}

pub(super) fn authenticated_live_test_client(base_url: String) -> Arc<CodexHttpClient> {
    let client = CodexHttpClient::new_for_test(
        crate::tls::client_builder().no_proxy().build().unwrap(),
        base_url,
        1_000,
        1_000,
    );
    client
        .auth_manager()
        .set_test_auth(auth::token_store::StoredAuth {
            access: "test".to_string(),
            refresh: String::new(),
            expires: u64::MAX,
            account_id: Some("acct".to_string()),
        });
    Arc::new(client)
}

pub(super) async fn next_live_websocket_request(
    websocket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
) -> serde_json::Value {
    loop {
        match websocket.next().await {
            Some(Ok(Message::Ping(payload))) => {
                websocket.send(Message::Pong(payload)).await.unwrap();
            }
            Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
            other => panic!("unexpected WebSocket request frame: {other:?}"),
        }
    }
}

pub(super) async fn emit_live_event(
    websocket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    event: &serde_json::Value,
) {
    websocket
        .send(Message::text(event.to_string()))
        .await
        .unwrap();
}

pub(super) fn upstream_sse(events: &[serde_json::Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend_from_slice(format!("data: {event}\n\n").as_bytes());
    }
    bytes
}

pub(super) fn request_with_tools(tools: serde_json::Value) -> MessagesRequest {
    serde_json::from_value(serde_json::json!({
        "model": "gpt-5.6-luna",
        "messages": [{"role":"user", "content":"find it"}],
        "tools": tools
    }))
    .unwrap()
}
