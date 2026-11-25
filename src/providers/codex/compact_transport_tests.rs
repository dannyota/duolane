use std::sync::Arc;

use http_body_util::BodyExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use super::provider_test_support::*;
use super::*;

#[test]
fn summary_transport_preserves_normal_and_explicit_websocket() {
    const CHILD: &str = "TEST_SUMMARY_TRANSPORT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        for transport in ["auto", "websocket"] {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "providers::codex::compact_transport_tests::summary_transport_preserves_normal_and_explicit_websocket"])
                .env(CHILD, transport)
                .status().unwrap();
            assert!(status.success());
        }
        return;
    }
    let transport = std::env::var(CHILD).unwrap();
    let explicit = transport == "websocket";
    let dir = crate::paths::config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        format!(r#"{{"codex":{{"transport":"{transport}","serverCompaction":false}}}}"#),
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        for streaming in [false, true] {
            for (summary, gzip) in [(true, false), (true, true), (false, false)] {
                let expect_http = summary && !explicit;
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut prefix = [0; 1];
                    assert_eq!(socket.peek(&mut prefix).await.unwrap(), 1);
                    assert_eq!(prefix[0], if expect_http { b'P' } else { b'G' });
                    let events = [
                        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_up"}}),
                        serde_json::json!({"type":"response.output_text.delta","output_index":0,"delta":"ok"}),
                        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}),
                        serde_json::json!({"type":"response.completed","response":{"id":"resp_up","status":"completed","usage":{"input_tokens":5,"output_tokens":2}}}),
                    ];
                    if expect_http {
                        let request = super::client_test_support::read_http_request(&mut socket).await;
                        let end = request.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
                        let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        assert_eq!(headers.contains("content-encoding: gzip"), gzip);
                        let body = upstream_sse(&events);
                        let header = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                        socket.write_all(header.as_bytes()).await.unwrap();
                        socket.write_all(&body).await.unwrap();
                    } else {
                        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                        let _ = next_live_websocket_request(&mut ws).await;
                        for event in events { emit_live_event(&mut ws, &event).await; }
                    }
                    assert!(tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept()).await.is_err());
                });
                let mut client = super::client_test_support::authenticated_http_test_client(format!("http://{addr}/responses"));
                if gzip { client.request_compression = config::CodexRequestCompression::Gzip; }
                let provider = CodexProvider::with_client(Arc::new(client));
                let text = if summary { "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools. Your task is to create a detailed summary of the conversation so far" } else { "hello" };
                let body = serde_json::from_value(serde_json::json!({"model":"gpt-5.6-sol", "max_tokens":32, "stream":streaming, "messages":[{"role":"user", "content":text}, {"role":"system", "content":"context"}]})).unwrap();
                let response = tokio::time::timeout(std::time::Duration::from_secs(8), provider.handle_messages_inner(body, live_test_context("summary-transport"), None)).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert!(response.into_body().collect().await.is_ok());
                server.await.unwrap();
            }
        }
    });
}

#[test]
fn summary_http_resets_same_owner_websocket_continuation() {
    const CHILD: &str = "TEST_SUMMARY_CONTINUATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "providers::codex::compact_transport_tests::summary_http_resets_same_owner_websocket_continuation"])
            .env(CHILD, "1")
            .status().unwrap();
        assert!(status.success());
        return;
    }
    unsafe { std::env::set_var("DUOLANE_CODEX_PREVIOUS_RESPONSE_ID", "1") };
    let dir = crate::paths::config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        r#"{"codex":{"transport":"auto","serverCompaction":false,"previousResponseId":true}}"#,
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let _registry = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool = websocket::lock_codex_websocket_pool_for_tests().await;
        for streaming in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let first = next_live_websocket_request(&mut ws).await;
                assert!(first.get("previous_response_id").is_none());
                send_test_completion(&mut ws, "resp_first").await;
                let second = next_live_websocket_request(&mut ws).await;
                assert!(second.get("previous_response_id").is_some_and(|id| id == "resp_first"));
                send_test_completion(&mut ws, "resp_second").await;
                let (mut http, _) = listener.accept().await.unwrap();
                let mut prefix = [0; 1];
                assert_eq!(http.peek(&mut prefix).await.unwrap(), 1);
                assert_eq!(prefix[0], b'P');
                let request = super::client_test_support::read_http_request(&mut http).await;
                let end = request.windows(4).position(|v| v == b"\r\n\r\n").unwrap() + 4;
                let body: serde_json::Value = serde_json::from_slice(&request[end..]).unwrap();
                assert!(body.get("previous_response_id").is_none());
                assert_eq!(body["input"].as_array().unwrap().len(), 5);
                let body = upstream_sse(&test_completion("resp_summary"));
                let header = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                http.write_all(header.as_bytes()).await.unwrap();
                http.write_all(&body).await.unwrap();
                let (socket, _) = listener.accept().await.unwrap();
                let mut next_ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let next = next_live_websocket_request(&mut next_ws).await;
                assert!(next.get("previous_response_id").is_none());
                send_test_completion(&mut next_ws, "resp_next").await;
                drop(ws);
            });
            let provider = CodexProvider::with_client(authenticated_live_test_client(format!("http://{addr}/responses")));
            let owner = ConversationIdentity::Main(format!("summary-continuation-{streaming}"));
            let context = live_test_context("summary-continuation");
            let summary = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools. Your task is to create a detailed summary of the conversation so far";
            let mut messages = vec![serde_json::json!({"role":"user","content":"one"})];
            for (index, text) in ["two", summary, "after"].into_iter().enumerate() {
                let body = serde_json::from_value(serde_json::json!({"model":"gpt-5.6-sol","max_tokens":32,"stream":if index < 2 { true } else { streaming },"messages":messages})).unwrap();
                let response = tokio::time::timeout(std::time::Duration::from_secs(8), provider.handle_messages_inner(body, context.clone(), Some(owner.clone()))).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert!(response.into_body().collect().await.is_ok());
                if index == 2 {
                    assert!(websocket::pooled_socket_id_for_tests(&owner).is_none());
                }
                messages.push(serde_json::json!({"role":"assistant","content":"ok"}));
                messages.push(serde_json::json!({"role":"user","content":text}));
            }
            let body = serde_json::from_value(serde_json::json!({"model":"gpt-5.6-sol","max_tokens":32,"stream":streaming,"messages":messages})).unwrap();
            let response = tokio::time::timeout(std::time::Duration::from_secs(8), provider.handle_messages_inner(body, context, Some(owner.clone()))).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.into_body().collect().await.is_ok());
            server.await.unwrap();
            websocket::invalidate_codex_websocket_pool_owner(&owner);
            continuation::clear_continuation_for_owner(Some(&owner));
        }
    });
}

#[test]
fn full_context_reuses_socket_after_unrecordable_and_buffered_responses() {
    const CHILD: &str = "TEST_FULL_CONTEXT_REUSE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "providers::codex::compact_transport_tests::full_context_reuses_socket_after_unrecordable_and_buffered_responses"])
            .env(CHILD, "1")
            .status().unwrap();
        assert!(status.success());
        return;
    }
    unsafe { std::env::set_var("DUOLANE_CODEX_PREVIOUS_RESPONSE_ID", "1") };
    let dir = crate::paths::config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        r#"{"codex":{"transport":"websocket","serverCompaction":false,"previousResponseId":true}}"#,
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let _registry = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool = websocket::lock_codex_websocket_pool_for_tests().await;
        for buffered in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = next_live_websocket_request(&mut ws).await;
                emit_live_event(&mut ws, &serde_json::json!({"type":"response.created","response":{"id":"resp_first"}})).await;
                let mut events = test_completion("resp_first");
                if !buffered {
                    events[3]["response"]["output"].as_array_mut().unwrap().push(serde_json::json!({"type":"web_search_call","id":"ws_1","status":"completed"}));
                }
                for event in events { emit_live_event(&mut ws, &event).await; }
                let second = tokio::time::timeout(std::time::Duration::from_secs(3), next_live_websocket_request(&mut ws)).await.unwrap();
                assert!(second.get("previous_response_id").is_none());
                assert_eq!(second["input"].as_array().unwrap().len(), 3);
                send_test_completion(&mut ws, "resp_second").await;
                let third = next_live_websocket_request(&mut ws).await;
                assert!(third.get("previous_response_id").is_some());
                drop(ws);
                let (socket, _) = tokio::time::timeout(std::time::Duration::from_secs(8), listener.accept()).await.unwrap().unwrap();
                let mut retry = tokio_tungstenite::accept_async(socket).await.unwrap();
                let request = next_live_websocket_request(&mut retry).await;
                assert!(request.get("previous_response_id").is_none());
                assert_eq!(request["input"].as_array().unwrap().len(), 5);
                send_test_completion(&mut retry, "resp_retry").await;
            });
            let provider = CodexProvider::with_client(authenticated_live_test_client(format!("http://{addr}/responses")));
            let owner = ConversationIdentity::Main(format!("full-context-reuse-{buffered}"));
            let context = live_test_context("full-context-reuse");
            for (stream, messages) in [( !buffered, serde_json::json!([{"role":"user","content":"one"}])), (true, serde_json::json!([{"role":"user","content":"one"},{"role":"assistant","content":"ok"},{"role":"user","content":"two"}])), (true, serde_json::json!([{"role":"user","content":"one"},{"role":"assistant","content":"ok"},{"role":"user","content":"two"},{"role":"assistant","content":"ok"},{"role":"user","content":"three"}]))] {
                let body = serde_json::from_value(serde_json::json!({"model":"gpt-5.6-sol","max_tokens":32,"stream":stream,"messages":messages})).unwrap();
                let response = tokio::time::timeout(std::time::Duration::from_secs(8), provider.handle_messages_inner(body, context.clone(), Some(owner.clone()))).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert!(response.into_body().collect().await.is_ok());
            }
            server.await.unwrap();
            websocket::invalidate_codex_websocket_pool_owner(&owner);
            continuation::clear_continuation_for_owner(Some(&owner));
        }
    });
}

fn test_completion(id: &str) -> [serde_json::Value; 4] {
    [
        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_up"}}),
        serde_json::json!({"type":"response.output_text.delta","output_index":0,"delta":"ok"}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}),
        serde_json::json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":5,"output_tokens":2}}}),
    ]
}

async fn send_test_completion(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    id: &str,
) {
    emit_live_event(
        ws,
        &serde_json::json!({"type":"response.created","response":{"id":id}}),
    )
    .await;
    for event in test_completion(id) {
        emit_live_event(ws, &event).await;
    }
}
