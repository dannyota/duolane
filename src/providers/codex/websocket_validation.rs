use super::*;
use futures_util::FutureExt;

pub(super) async fn check_pooled_websocket<S>(
    entry: &PoolEntry,
    websocket: &mut WebSocketStream<S>,
    timeout_ms: u64,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if entry.needs_validation() {
        return validate_pooled_websocket(websocket, timeout_ms).await;
    }
    loop {
        match tokio::task::unconstrained(websocket.next()).now_or_never() {
            None => return Ok(()),
            Some(Some(Ok(Message::Ping(payload)))) => {
                tokio::task::unconstrained(websocket.send(Message::Pong(payload)))
                    .now_or_never()
                    .ok_or_else(|| "pong send would block during pooled validation".to_string())?
                    .map_err(|error| error.to_string())?
            }
            Some(Some(Ok(_))) => {
                return Err("unexpected frame during pooled validation".to_string());
            }
            Some(Some(Err(error))) => return Err(error.to_string()),
            Some(None) => return Err("connection closed during pooled validation".to_string()),
        }
    }
}

pub(super) async fn validate_pooled_websocket<S>(
    websocket: &mut WebSocketStream<S>,
    timeout_ms: u64,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let nonce = next_monotonic_nonzero(&POOLED_VALIDATION_SEQUENCE, "pooled validation")
        .to_be_bytes()
        .to_vec();
    websocket
        .send(Message::Ping(nonce.clone().into()))
        .await
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        loop {
            match websocket.next().await {
                Some(Ok(Message::Pong(payload))) if payload == nonce => return Ok(()),
                Some(Ok(Message::Ping(payload))) => websocket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|error| error.to_string())?,
                Some(Ok(Message::Pong(_))) => {
                    return Err("unexpected Pong during pooled validation".to_string());
                }
                Some(Ok(_)) => {
                    return Err("unexpected frame during pooled validation".to_string());
                }
                Some(Err(error)) => return Err(error.to_string()),
                None => return Err("connection closed during pooled validation".to_string()),
            }
        }
    })
    .await
    .map_err(|_| "validation timeout".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::codex::client_test_support::buffered_request_with_texts;
    use crate::providers::codex::continuation::continuation_candidate_for_owner;
    use crate::request_identity::ConversationIdentity;
    use tokio_tungstenite::tungstenite::protocol::Role;

    #[tokio::test]
    async fn closed_fresh_socket_is_rejected_when_budget_is_exhausted() {
        let _pool_guard = lock_codex_websocket_pool_for_tests().await;
        clear_codex_websocket_pool_for_tests();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut socket = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let (dummy, _peer) = tokio::io::duplex(64);
        let io: BoxedWebSocketIo = Box::new(dummy);
        let entry = Arc::new(PoolEntry::new(
            WebSocketStream::from_raw_socket(io, Role::Client, None).await,
        ));
        let owner = ConversationIdentity::Main("budget-closed-socket".into());
        let request = buffered_request_with_texts(&["one"]);
        let reservation = continuation_candidate_for_owner(Some(&owner), &request, false);
        assert!(pool_insert_for_turn(&reservation, entry.clone()));
        server.send(Message::text("response")).await.unwrap();
        assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
        server.send(Message::Close(None)).await.unwrap();
        assert!(socket.get_ref().peek(&mut [0; 1]).await.unwrap() > 0);
        while tokio::task::coop::has_budget_remaining() {
            tokio::task::consume_budget().await;
        }
        assert!(
            check_pooled_websocket(&entry, &mut socket, 1000)
                .await
                .is_err()
        );
        clear_codex_websocket_pool_for_tests();
    }
}
