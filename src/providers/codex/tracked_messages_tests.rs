use super::sources::{CredentialId, FailureKind, ReplayGate, SourceAttempt};
use super::*;
use crate::config::codex_auth::AuthSource;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn tracked_buffered_quota_preserves_response_and_credential_proof() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = super::client_test_support::read_http_request(&mut stream).await;
        if request.starts_with(b"GET ") {
            stream.write_all(b"HTTP/1.1 426 Upgrade Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await.unwrap();
            drop(stream);
            let (next, _) = listener.accept().await.unwrap();
            stream = next;
            let _ = super::client_test_support::read_http_request(&mut stream).await;
        }
        let body = br#"{"error":{"type":"usage_limit_reached","resets_at":2000000000}}"#;
        let headers = format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
    });
    let client = Arc::new(super::client_test_support::authenticated_http_test_client(
        format!("http://{addr}/responses"),
    ));
    let provider = CodexProvider::with_client(client);
    let body = serde_json::from_value(serde_json::json!({"model":"gpt-5.6-sol", "max_tokens":64, "messages":[{"role":"user","content":"hello"}]})).unwrap();
    let gate = ReplayGate::new(false);
    let attempt = SourceAttempt::new(
        AuthSource::ChatGpt,
        &super::client_test_support::http_test_context(),
        gate.clone(),
    );
    let resolved = super::translate::model_allowlist::resolve_model_request_with_config_override(
        "gpt-5.6-sol",
        false,
    );
    let result = provider
        .tracked_messages_attempt(&body, None, resolved, &attempt)
        .await;
    let result = match result {
        Err(failure) => failure,
        Ok(_) => panic!("expected failure"),
    };
    let failure = &result.failure;
    let response = result.response.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(failure.kind == FailureKind::UsageLimit);
    assert!(failure.send == super::api::SendEvidence::DeniedBeforeExecution);
    assert!(
        failure.credential
            == Some(CredentialId::for_chatgpt_auth(
                &super::client_test_support::http_test_auth()
            ))
    );
    assert!(gate.may_switch(failure.kind, failure.send));
    assert_eq!(
        response
            .headers()
            .get("anthropic-ratelimit-unified-status")
            .and_then(|v| v.to_str().ok()),
        Some("rejected")
    );
    assert_eq!(
        response
            .headers()
            .get("anthropic-ratelimit-unified-reset")
            .and_then(|v| v.to_str().ok()),
        Some("2000000000")
    );
    server.await.unwrap();
}
