use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncReadExt;

use crate::provider::RequestContext;
use crate::request_identity::ConversationIdentity;

use super::auth::token_store::StoredAuth;
use super::client::*;
use super::translate::reasoning_signature::ReasoningAccount;
use super::translate::request::ResponsesRequest;

pub(super) fn test_continuation(
    owner: Option<ConversationIdentity>,
    turn_id: Option<u64>,
    previous_response_id: Option<&str>,
    origin_socket_id: Option<u64>,
    disabled_reason: Option<&str>,
) -> super::continuation::ContinuationReservation {
    super::continuation::ContinuationReservation::new(
        super::continuation::ContinuationCandidate {
            turn_id,
            previous_response_id: previous_response_id.map(str::to_string),
            input_delta: None,
            input_delta_count: 1,
            disabled_reason: disabled_reason.map(str::to_string),
        },
        owner,
        origin_socket_id,
    )
}

pub(super) fn http_test_auth() -> StoredAuth {
    StoredAuth {
        access: "test".into(),
        refresh: String::new(),
        account_id: Some("acct".into()),
        expires: u64::MAX,
    }
}

pub(super) fn http_test_context() -> RequestContext {
    RequestContext {
        req_id: "http-body-test".into(),
        session_id: None,
        traffic: None,
        monitor: None,
        passthrough: None,
    }
}

pub(super) fn http_test_client(base_url: String, body_idle_timeout_ms: u64) -> CodexHttpClient {
    CodexHttpClient::new_for_test(
        crate::tls::client_builder().no_proxy().build().unwrap(),
        base_url,
        100,
        body_idle_timeout_ms,
    )
}

pub(super) fn buffered_test_request() -> ResponsesRequest {
    ResponsesRequest {
        model: "gpt-5.6-sol".into(),
        instructions: None,
        input: vec![],
        tools: None,
        tool_choice: None,
        store: false,
        stream: true,
        parallel_tool_calls: true,
        include: None,
        client_metadata: None,
        service_tier: None,
        prompt_cache_key: None,
        text: super::translate::request::ResponsesText {
            verbosity: None,
            format: None,
        },
        reasoning: None,
        reasoning_account: ReasoningAccount::new(Some("acct")),
    }
}

pub(super) fn buffered_request_with_texts(texts: &[&str]) -> ResponsesRequest {
    let mut request = buffered_test_request();
    request.input = texts
        .iter()
        .map(
            |text| super::translate::request::ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![super::translate::request::ResponsesContentPart::InputText {
                    text: (*text).to_string(),
                }],
            },
        )
        .collect();
    request
}

pub(super) async fn next_websocket_json(
    websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> serde_json::Value {
    loop {
        match websocket.next().await {
            Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(payload))) => {
                websocket
                    .send(tokio_tungstenite::tungstenite::Message::Pong(payload))
                    .await
                    .unwrap();
            }
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                return serde_json::from_str(&text).unwrap();
            }
            other => panic!("unexpected WebSocket request frame: {other:?}"),
        }
    }
}

pub(super) async fn send_completed_websocket_response(
    websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    response_id: &str,
) {
    websocket
        .send(tokio_tungstenite::tungstenite::Message::text(
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": response_id,
                    "status": "completed",
                    "output": []
                }
            })
            .to_string(),
        ))
        .await
        .unwrap();
}

pub(super) async fn send_nested_previous_response_missing(
    websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) {
    websocket
        .send(tokio_tungstenite::tungstenite::Message::text(
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "code": "previous_response_not_found",
                        "message": "Previous response not found"
                    }
                }
            })
            .to_string(),
        ))
        .await
        .unwrap();
}

pub(super) fn authenticated_http_test_client(base_url: String) -> CodexHttpClient {
    let client = http_test_client(base_url, 100);
    client.auth_manager().set_test_auth(http_test_auth());
    client
}

pub(super) async fn read_http_request(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request ended before its body was complete");
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            return request;
        }
    }
}

pub(super) fn loopback_tls() -> (tokio_rustls::TlsAcceptor, reqwest::Client) {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let server_config = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key.into())
    .unwrap();
    let client = crate::tls::client_builder()
        .no_proxy()
        .tls_certs_only([reqwest::Certificate::from_der(cert.cert.der()).unwrap()])
        .build()
        .unwrap();
    (
        tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config)),
        client,
    )
}
