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
        match websocket.next().now_or_never() {
            None => return Ok(()),
            Some(Some(Ok(Message::Ping(payload)))) => websocket
                .send(Message::Pong(payload))
                .now_or_never()
                .ok_or_else(|| "pong send would block during pooled validation".to_string())?
                .map_err(|error| error.to_string())?,
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
