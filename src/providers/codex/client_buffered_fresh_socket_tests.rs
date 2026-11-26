use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

async fn complete(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>, id: &str) {
    ws.send(Message::text(
        serde_json::json!({"type":"response.created", "response":{"id":id}}).to_string(),
    ))
    .await
    .unwrap();
    super::super::super::client_test_support::send_completed_websocket_response(ws, id).await;
}

async fn check_ping_after(elapsed: std::time::Duration, expect_ping: bool) {
    let _pool_guard = super::super::super::websocket::lock_codex_websocket_pool_for_tests().await;
    super::super::super::websocket::clear_codex_websocket_pool_for_tests();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        let _ = next_websocket_json(&mut ws).await;
        complete(&mut ws, "first").await;
        let frame = ws.next().await.unwrap().unwrap();
        assert_eq!(matches!(frame, Message::Ping(_)), expect_ping);
        if let Message::Ping(payload) = frame {
            ws.send(Message::Pong(payload)).await.unwrap();
            let _ = next_websocket_json(&mut ws).await;
        } else {
            assert!(matches!(frame, Message::Text(_)));
        }
        complete(&mut ws, "second").await;
    });
    let client = authenticated_http_test_client(format!("http://{addr}/responses"));
    let context = http_test_context();
    let request = buffered_request_with_texts(&["one"]);
    let owner = ConversationIdentity::Main("fresh-socket-ping".into());
    let reservation = super::super::super::continuation::continuation_candidate_for_owner(
        Some(&owner),
        &request,
        false,
    );
    let first = client
        .post_codex_with_transport(
            &request,
            &context,
            Some(&reservation),
            crate::config::CodexTransport::WebSocket,
            &Default::default(),
        )
        .await
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(elapsed).await;
    tokio::time::resume();
    let second = client
        .post_codex_with_transport(
            &request,
            &context,
            Some(&reservation),
            crate::config::CodexTransport::WebSocket,
            &Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(first.socket_id, second.socket_id);
    server.await.unwrap();
    super::super::super::websocket::clear_codex_websocket_pool_for_tests();
}

#[tokio::test]
async fn recently_returned_socket_is_reused_without_ping() {
    check_ping_after(std::time::Duration::from_secs(9), false).await;
}

#[tokio::test]
async fn socket_returned_ten_seconds_ago_is_pinged() {
    check_ping_after(std::time::Duration::from_secs(10), true).await;
}

#[tokio::test]
async fn recently_closed_socket_reconnects_without_ping_or_request() {
    let _pool_guard = super::super::super::websocket::lock_codex_websocket_pool_for_tests().await;
    super::super::super::websocket::clear_codex_websocket_pool_for_tests();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        let _ = next_websocket_json(&mut ws).await;
        complete(&mut ws, "first").await;
        ws.send(Message::Close(None)).await.unwrap();
        closed_tx.send(()).unwrap();
        let old_socket = tokio::spawn(async move {
            assert!(matches!(
                ws.next().await,
                None | Some(Err(_)) | Some(Ok(Message::Close(_)))
            ));
        });
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        let _ = next_websocket_json(&mut ws).await;
        complete(&mut ws, "reconnected").await;
        old_socket.await.unwrap();
    });
    let client = authenticated_http_test_client(format!("http://{addr}/responses"));
    let context = http_test_context();
    let request = buffered_request_with_texts(&["one"]);
    let owner = ConversationIdentity::Main("fresh-socket-closed-frame".into());
    let reservation = super::super::super::continuation::continuation_candidate_for_owner(
        Some(&owner),
        &request,
        false,
    );
    let first = client
        .post_codex_with_transport(
            &request,
            &context,
            Some(&reservation),
            crate::config::CodexTransport::WebSocket,
            &Default::default(),
        )
        .await
        .unwrap();
    closed_rx.await.unwrap();
    let second = client
        .post_codex_with_transport(
            &request,
            &context,
            Some(&reservation),
            crate::config::CodexTransport::WebSocket,
            &Default::default(),
        )
        .await
        .unwrap();
    assert_ne!(first.socket_id, second.socket_id);
    assert!(String::from_utf8_lossy(&second.body).contains("reconnected"));
    server.await.unwrap();
    super::super::super::websocket::clear_codex_websocket_pool_for_tests();
}

#[tokio::test]
async fn closed_recent_socket_retries_on_new_socket() {
    let _pool_guard = super::super::super::websocket::lock_codex_websocket_pool_for_tests().await;
    super::super::super::websocket::clear_codex_websocket_pool_for_tests();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        let _ = next_websocket_json(&mut ws).await;
        complete(&mut ws, "first").await;
        drop(ws);
        closed_tx.send(()).unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut retry = tokio_tungstenite::accept_async(socket).await.unwrap();
        let _ = next_websocket_json(&mut retry).await;
        complete(&mut retry, "retry").await;
    });
    let client = authenticated_http_test_client(format!("http://{addr}/responses"));
    let context = http_test_context();
    let request = buffered_request_with_texts(&["one"]);
    let owner = ConversationIdentity::Main("fresh-socket-closed".into());
    let reservation = super::super::super::continuation::continuation_candidate_for_owner(
        Some(&owner),
        &request,
        false,
    );
    let first = client
        .post_codex_with_transport(
            &request,
            &context,
            Some(&reservation),
            crate::config::CodexTransport::WebSocket,
            &Default::default(),
        )
        .await
        .unwrap();
    closed_rx.await.unwrap();
    let retry = client
        .post_codex_with_transport(
            &request,
            &context,
            Some(&reservation),
            crate::config::CodexTransport::WebSocket,
            &Default::default(),
        )
        .await
        .unwrap();
    assert_ne!(first.socket_id, retry.socket_id);
    assert!(String::from_utf8_lossy(&retry.body).contains("retry"));
    server.await.unwrap();
    super::super::super::websocket::clear_codex_websocket_pool_for_tests();
}
