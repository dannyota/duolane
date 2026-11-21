use super::*;
use crate::monitor::{EndpointKind, MonitorHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn relay_chunks(content_type: &str, chunks: Vec<Vec<u8>>) -> (Vec<u8>, MonitorHandle) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let content_type = content_type.to_owned();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).await;
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ntransfer-encoding: chunked\r\n\r\n").as_bytes()).await.unwrap();
        for chunk in chunks {
            socket
                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .await
                .unwrap();
            socket.write_all(&chunk).await.unwrap();
            socket.write_all(b"\r\n").await.unwrap();
            tokio::task::yield_now().await;
        }
        socket.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let monitor = MonitorHandle::new(10);
    monitor.request_started("req", Some("session".into()), None, EndpointKind::Messages);
    let ctx = RequestContext {
        req_id: "req".into(),
        session_id: None,
        traffic: None,
        monitor: Some(monitor.clone()),
        passthrough: Some(crate::provider::Passthrough {
            raw_body: Bytes::from_static(br#"{"messages":[]}"#),
            headers: axum::http::HeaderMap::new(),
            path_and_query: "/v1/messages".into(),
        }),
    };
    let provider = AnthropicProvider::with_timeouts(
        format!("http://{addr}"),
        Duration::from_secs(5),
        Duration::from_secs(5),
    );
    let response = provider.relay(ctx, false).await;
    let bytes = axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    monitor.request_completed("req", 200, None, None);
    (bytes.to_vec(), monitor)
}

#[tokio::test]
async fn sse_usage_is_reported_without_changing_bytes() {
    let first = br#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":4,"cache_read_input_tokens":3,"output_tokens":0}}}

event: message_delta
data: {"type":"message_delta","usage":{"output_tokens":8}}

"#;
    let (body, monitor) = relay_chunks(
        "text/event-stream",
        vec![first[..45].to_vec(), first[45..].to_vec()],
    )
    .await;
    assert!(body == first);
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].input_tokens, Some(17));
    assert_eq!(snapshot.recent[0].output_tokens, Some(8));
}

#[tokio::test]
async fn sse_dispatches_only_complete_events_with_all_line_endings() {
    let body = b"data: {\"type\":\"message_delta\",\rdata: \"usage\":{\"output_tokens\":8}}\r\rdata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":99}}\n";
    let (received, monitor) = relay_chunks("text/event-stream", vec![body.to_vec()]).await;
    assert!(received == body);
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].output_tokens, Some(8));
    assert_eq!(snapshot.recent[0].stream_chunks, 1);
}

#[tokio::test]
async fn json_usage_is_reported() {
    let body = br#"{"usage":{"input_tokens":10,"cache_read_input_tokens":3,"output_tokens":8}}"#;
    let (received, monitor) = relay_chunks("application/json", vec![body.to_vec()]).await;
    assert!(received == body);
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].input_tokens, Some(13));
    assert_eq!(snapshot.recent[0].output_tokens, Some(8));
}

#[tokio::test]
async fn oversized_json_is_forwarded_without_usage() {
    let mut body = vec![b' '; 1024 * 1024 + 1];
    body.extend_from_slice(br#"{"usage":{"input_tokens":1,"output_tokens":1}}"#);
    let (received, monitor) = relay_chunks("application/json", vec![body.clone()]).await;
    assert!(received == body);
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].input_tokens, None);
    assert_eq!(snapshot.recent[0].output_tokens, None);
}

#[tokio::test]
async fn leading_bom_is_ignored_for_sse_and_json() {
    let event =
        b"\xef\xbb\xbfdata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":8}}\n\n";
    let (received, monitor) = relay_chunks(
        "text/event-stream",
        vec![event[..1].to_vec(), event[1..].to_vec()],
    )
    .await;
    assert!(received == event);
    assert_eq!(monitor.snapshot().recent[0].output_tokens, Some(8));
    let body = b"\xef\xbb\xbf{\"usage\":{\"output_tokens\":9}}";
    let (received, monitor) = relay_chunks("application/json", vec![body.to_vec()]).await;
    assert!(received == body);
    assert_eq!(monitor.snapshot().recent[0].output_tokens, Some(9));
}

#[tokio::test]
async fn escaped_usage_key_is_recognized() {
    let event = br#"data: {"type":"message_delta","\u0075sage":{"output_tokens":8}}

"#;
    let (received, monitor) = relay_chunks("text/event-stream", vec![event.to_vec()]).await;
    assert!(received == event);
    assert_eq!(monitor.snapshot().recent[0].output_tokens, Some(8));
    let body = br#"{"\u0075sage":{"output_tokens":9}}"#;
    let (received, monitor) = relay_chunks("application/json", vec![body.to_vec()]).await;
    assert!(received == body);
    assert_eq!(monitor.snapshot().recent[0].output_tokens, Some(9));
}

#[tokio::test]
async fn near_limit_event_reports_usage() {
    let mut event = b"data: {\"type\":\"message_delta\",\"padding\":\"".to_vec();
    event.extend(std::iter::repeat_n(b'x', 1024 * 1024 - 128));
    event.extend_from_slice(b"\",\"usage\":{\"output_tokens\":8}}\n\n");
    let (received, monitor) = relay_chunks("text/event-stream", vec![event.clone()]).await;
    assert!(received == event);
    assert_eq!(monitor.snapshot().recent[0].output_tokens, Some(8));
}

#[tokio::test]
async fn excessive_token_fields_are_ignored() {
    let event = b"data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":1000000000,\"output_tokens\":7}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":18446744073709551615,\"cache_read_input_tokens\":1000000001,\"output_tokens\":8}}\n\n";
    let (received, monitor) = relay_chunks("text/event-stream", vec![event.to_vec()]).await;
    assert!(received == event);
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].input_tokens, Some(1_000_000_000));
    assert_eq!(snapshot.recent[0].output_tokens, Some(8));
}

#[tokio::test]
async fn invalid_numeric_fields_do_not_hide_valid_usage() {
    let body = br#"{"usage":{"input_tokens":18446744073709551616,"cache_read_input_tokens":1.5,"output_tokens":8}}"#;
    let (received, monitor) = relay_chunks("application/json", vec![body.to_vec()]).await;
    assert!(received == body);
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].input_tokens, None);
    assert_eq!(snapshot.recent[0].output_tokens, Some(8));
}

#[tokio::test]
async fn bom_does_not_count_toward_sse_buffer_limit() {
    let mut body = b"\xef\xbb\xbfdata: {\"type\":\"message_delta\",\"padding\":\"".to_vec();
    let tail = b"\",\"usage\":{\"output_tokens\":8}}";
    body.extend(std::iter::repeat_n(
        b'x',
        1024 * 1024 - 1 - (body.len() - 3) - tail.len(),
    ));
    body.extend_from_slice(tail);
    body.extend_from_slice(b"\n\n");
    let (received, monitor) = relay_chunks("text/event-stream", vec![body.clone()]).await;
    assert!(received == body);
    assert_eq!(monitor.snapshot().recent[0].output_tokens, Some(8));
}

#[tokio::test]
async fn incomplete_bom_prefix_remains_part_of_first_line() {
    use futures_util::StreamExt;
    let monitor = MonitorHandle::new(10);
    monitor.request_started("req", Some("session".into()), None, EndpointKind::Messages);
    let chunks = vec![Bytes::from_static(b"\xef"), Bytes::from_static(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":8}}\n\n")];
    let input = futures_util::stream::iter(chunks.clone().into_iter().map(Ok));
    let mut stream = Box::pin(super::usage::observe(
        input,
        Some(monitor.clone()),
        "req".into(),
        true,
    ));
    for expected in chunks {
        assert!(stream.next().await.unwrap().unwrap() == expected);
    }
    assert!(stream.next().await.is_none());
    monitor.request_completed("req", 200, None, None);
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot.recent[0].input_tokens, Some(8));
    assert_eq!(snapshot.recent[0].output_tokens, None);
}
