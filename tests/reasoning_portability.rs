#[path = "support/hermetic.rs"]
mod hermetic;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use duolane::providers::translate_shared::wrap_reasoning;
use duolane::{registry::Registry, server::app};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tower::util::ServiceExt;

const CODEX_TURN: &str = concat!(
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc-secret\"}}\n\n",
    "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"delta\":\"plan\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
    "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"message\",\"id\":\"msg_up\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"codex answer\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"message\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
);

async fn spawn_upstream(
    content_type: &'static str,
    reply: String,
    captured: Arc<Mutex<Vec<String>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().fallback(move |body: String| {
        let captured = captured.clone();
        let reply = reply.clone();
        async move {
            captured.lock().unwrap().push(body);
            http::Response::builder()
                .status(StatusCode::OK)
                .header("content-type", content_type)
                .body(Body::from(reply))
                .unwrap()
        }
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

fn write_codex_auth(path: &Path, account_id: &str) {
    let auth = json!({"tokens": {
        "access_token": "toy-access",
        "refresh_token": "toy-refresh",
        "account_id": account_id
    }});
    std::fs::write(path, serde_json::to_vec(&auth).unwrap()).unwrap();
}

async fn send(body: Value, expected: StatusCode) -> Value {
    send_raw(body.to_string(), expected).await
}

async fn send_raw(body: String, expected: StatusCode) -> Value {
    let response = app(Arc::new(Registry::with_default_alias()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-claude-code-session-id", "portability-session")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), expected);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

fn assistant(reply: &Value) -> Value {
    json!({"role": "assistant", "content": reply["content"]})
}

#[tokio::test]
async fn reasoning_survives_codex_to_claude_to_codex() {
    let home = tempfile::TempDir::new().unwrap();
    let auth_file = home.path().join("auth.json");
    write_codex_auth(&auth_file, "acct_a");
    let codex_requests = Arc::new(Mutex::new(Vec::new()));
    let claude_requests = Arc::new(Mutex::new(Vec::new()));
    let claude_reply = json!({
        "id": "msg_claude",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-8",
        "content": [
            {"type": "thinking", "thinking": "opus thought", "signature": "EqQBanthropic"},
            {"type": "text", "text": "claude answer"}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let codex = spawn_upstream(
        "text/event-stream",
        CODEX_TURN.to_string(),
        codex_requests.clone(),
    )
    .await;
    let claude = spawn_upstream(
        "application/json",
        claude_reply.to_string(),
        claude_requests.clone(),
    )
    .await;
    unsafe {
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("DUOLANE_CODEX_AUTH_FILE", &auth_file);
        std::env::set_var("DUOLANE_CODEX_BASE_URL", &codex);
        std::env::set_var("DUOLANE_CODEX_TRANSPORT", "http");
        std::env::set_var("DUOLANE_ANTHROPIC_BASE_URL", &claude);
    }

    let codex_reply = send(
        json!({
            "model": "gpt-5.5",
            "max_tokens": 64,
            "messages": [user("one")]
        }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        codex_reply["content"][0]["signature"],
        "duolane:codex:v1:c1383c10f6fe17fb:cnNfMQ:enc-secret"
    );

    let mut history = vec![user("one"), assistant(&codex_reply), user("two")];
    let claude_answer = send(
        json!({
            "model": "claude-opus-4-8",
            "max_tokens": 64,
            "messages": history
        }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(claude_answer, claude_reply);
    let sent_to_claude = claude_requests.lock().unwrap()[0].clone();
    assert!(!sent_to_claude.contains("duolane:"), "{sent_to_claude}");
    assert!(!sent_to_claude.contains("enc-secret"), "{sent_to_claude}");
    let sent: Value = serde_json::from_str(&sent_to_claude).unwrap();
    assert_eq!(
        sent["messages"][1]["content"][0],
        json!({"type": "text", "text": wrap_reasoning("plan")})
    );

    let mention = r#"the marker duolane:codex: and "signature" : "duolane:codex:v1:x" as text"#;
    let forwarded = [
        json!({"model": "claude-opus-4-8", "max_tokens": 64, "messages": [user(mention)]}),
        json!({"model": "claude-opus-4-8", "max_tokens": 64, "messages": [
            {"role": "assistant", "content": [{
                "type": "tool_use", "id": "toolu_1", "name": "Write",
                "input": {"signature": "duolane:codex:v1:c1383c10f6fe17fb:cnNfMQ:enc-secret"}
            }]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
            ]}
        ]}),
    ];
    for body in forwarded.map(|body| body.to_string()) {
        send_raw(body.clone(), StatusCode::OK).await;
        assert_eq!(claude_requests.lock().unwrap().last(), Some(&body));
    }

    let missed_block = json!({"role": "user", "content": codex_reply["content"]});
    let refused = [
        json!({"model": "claude-opus-4-8", "max_tokens": 64, "messages": [missed_block]})
            .to_string(),
        r#"{"model":"claude-opus-4-8","max_tokens":64,"messages":[{"role":"user","content":[{"type":"thinking","thinking":"plan","signature":"duolane:codex:v1:c1383c10f6fe17fb:cnNfMQ:enc-secret"}]}]}"#.to_string(),
    ];
    for body in refused {
        let reply = send_raw(body, StatusCode::BAD_REQUEST).await;
        assert_eq!(reply["error"]["type"], "invalid_request_error");
        assert!(!reply.to_string().contains("enc-secret"), "{reply}");
    }
    assert_eq!(claude_requests.lock().unwrap().len(), 3);

    history.extend([assistant(&claude_answer), user("three")]);
    let next_codex_turn = json!({"model": "gpt-5.5", "max_tokens": 64, "messages": history});
    send(next_codex_turn.clone(), StatusCode::OK).await;
    write_codex_auth(&auth_file, "acct_b");
    send(next_codex_turn, StatusCode::OK).await;

    let codex_inputs: Vec<Value> = codex_requests
        .lock()
        .unwrap()
        .iter()
        .map(|body| serde_json::from_str::<Value>(body).unwrap()["input"].clone())
        .collect();
    let opus_text = json!({"type": "output_text", "text": wrap_reasoning("opus thought")});
    let same_account = &codex_inputs[1];
    assert_eq!(
        same_account[1],
        json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc-secret"})
    );
    assert_eq!(same_account[4]["content"][0], opus_text);
    let other_account = &codex_inputs[2];
    assert!(!other_account.to_string().contains("enc-secret"));
    assert_eq!(
        other_account[1]["content"][0],
        json!({"type": "output_text", "text": wrap_reasoning("plan")})
    );
    assert_eq!(other_account[3]["content"][0], opus_text);
}
