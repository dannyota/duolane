use std::time::Instant;

use crate::anthropic::sse::parse_sse_events;
use crate::traffic::TrafficCapture;

use super::auth::token_store::StoredAuth;
use super::client::*;

#[derive(Default)]
pub(super) struct HttpSseDecoder {
    frame: Vec<u8>,
    line_start: usize,
    skip_lf: bool,
}

pub(super) struct DecodedHttpSseEvent {
    pub(super) event: Option<String>,
    pub(super) payload: Option<serde_json::Value>,
}

pub(super) struct HttpEventStreamState {
    pub(super) resp: reqwest::Response,
    pub(super) response_headers: Vec<(String, String)>,
    pub(super) started_at: Instant,
    pub(super) body_json: String,
    pub(super) auth: StoredAuth,
    pub(super) auth_refresh_attempted: bool,
    pub(super) use_responses_lite: bool,
    pub(super) retries: u32,
}

impl HttpSseDecoder {
    pub(super) fn push(&mut self, input: &[u8]) -> Result<Vec<DecodedHttpSseEvent>, CodexError> {
        let mut events = Vec::new();
        for &byte in input {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.end_line(&mut events)?;
                    self.skip_lf = true;
                }
                _ => self.push_byte(byte)?,
            }
        }
        Ok(events)
    }

    pub(super) fn finish(&self) -> Result<(), CodexError> {
        if self.frame.is_empty() {
            Ok(())
        } else {
            Err(http_sse_error(
                "Codex SSE stream ended with an incomplete frame",
            ))
        }
    }

    fn push_byte(&mut self, byte: u8) -> Result<(), CodexError> {
        if self.frame.len() >= MAX_UPSTREAM_FRAME_BYTES {
            return Err(http_sse_error("Codex SSE frame exceeds the size limit"));
        }
        self.frame.push(byte);
        Ok(())
    }

    fn end_line(&mut self, events: &mut Vec<DecodedHttpSseEvent>) -> Result<(), CodexError> {
        if self.frame.len() == self.line_start {
            if !self.frame.is_empty()
                && let Some(event) = decode_http_sse_frame(&self.frame)?
            {
                events.push(event);
            }
            self.frame.clear();
            self.line_start = 0;
            return Ok(());
        }
        self.push_byte(b'\n')?;
        self.line_start = self.frame.len();
        Ok(())
    }
}

fn decode_http_sse_frame(frame: &[u8]) -> Result<Option<DecodedHttpSseEvent>, CodexError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|_| http_sse_error("Codex SSE frame contains invalid UTF-8"))?;
    let mut event = None;
    let mut data = Vec::new();
    for line in frame.lines() {
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    let data = data.join("\n");
    if data == "[DONE]" {
        return Ok(Some(DecodedHttpSseEvent {
            event,
            payload: None,
        }));
    }
    let payload = serde_json::from_str(&data)
        .map_err(|_| http_sse_error("Codex SSE frame contains invalid JSON"))?;
    Ok(Some(DecodedHttpSseEvent {
        event,
        payload: Some(payload),
    }))
}

pub(super) fn http_sse_error(message: &str) -> CodexError {
    CodexError {
        status: 0,
        message: message.to_string(),
        detail: Some("http_response_sse".to_string()),
        retry_after: None,
        usage_limit: None,
        origin: CodexErrorOrigin::Http,
    }
}

pub(super) async fn forward_codex_events(
    mut source: tokio::sync::mpsc::Receiver<Result<serde_json::Value, CodexError>>,
    tx: tokio::sync::mpsc::Sender<Result<serde_json::Value, CodexError>>,
) {
    while let Some(item) = source.recv().await {
        if tx.send(item).await.is_err() {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn log_http_stream_end(
    log: &crate::logging::Logger,
    message: &str,
    req_id: &str,
    started_at: Instant,
    body_bytes: u64,
    body_chunks: u64,
    event_count: u64,
    error: Option<&str>,
) {
    let mut fields = serde_json::Map::from_iter([
        ("reqId".to_string(), serde_json::json!(req_id)),
        ("transport".to_string(), serde_json::json!("http")),
        ("bodyBytes".to_string(), serde_json::json!(body_bytes)),
        ("bodyChunks".to_string(), serde_json::json!(body_chunks)),
        ("eventCount".to_string(), serde_json::json!(event_count)),
        (
            "ms".to_string(),
            serde_json::json!(started_at.elapsed().as_millis()),
        ),
    ]);
    if let Some(error) = error {
        fields.insert("error".to_string(), serde_json::json!(error));
    }
    match message {
        "codex_http_stream_completed" => log.info(message, Some(fields)),
        _ => log.warn(message, Some(fields)),
    }
}

pub(super) fn write_codex_http_sse_event_capture(
    traffic: &TrafficCapture,
    event: Option<&str>,
    payload: &serde_json::Value,
) {
    let mut payload = payload.clone();
    if let Some(event) = event
        && let Some(object) = payload.as_object_mut()
    {
        object
            .entry("_sse_event")
            .or_insert_with(|| serde_json::json!(event));
    }
    traffic.write_json_event("040-upstream-event", &payload);
}

pub(super) fn write_codex_sse_event_capture(traffic: &TrafficCapture, body: &[u8]) {
    for event in parse_sse_events(body) {
        if event.data == "[DONE]" {
            traffic.write_json_event(
                "040-upstream-event",
                &serde_json::json!({
                    "event": event.event,
                    "data": "[DONE]",
                }),
            );
            continue;
        }

        match serde_json::from_str::<serde_json::Value>(&event.data) {
            Ok(mut value) => {
                if let Some(name) = event.event
                    && let Some(obj) = value.as_object_mut()
                {
                    obj.entry("_sse_event").or_insert(serde_json::json!(name));
                }
                traffic.write_json_event("040-upstream-event", &value);
            }
            Err(_) => {
                traffic.write_json_event(
                    "040-upstream-event",
                    &serde_json::json!({
                        "event": event.event,
                        "unparseable": true,
                        "data": event.data,
                    }),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_sse_decoder_handles_fragmented_crlf_and_done_marker() {
        let mut decoder = HttpSseDecoder::default();
        assert!(
            decoder
                .push(b"event: response.output_text.delta\r")
                .unwrap()
                .is_empty()
        );
        let events = decoder
            .push(b"\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\r\n\r\n")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event.as_deref(),
            Some("response.output_text.delta")
        );
        assert_eq!(
            events[0]
                .payload
                .as_ref()
                .and_then(|payload| payload.get("delta"))
                .and_then(|value| value.as_str()),
            Some("ok")
        );

        let done = decoder.push(b"data: [DONE]\n\n").unwrap();
        assert_eq!(done.len(), 1);
        assert!(done[0].payload.is_none());
        decoder.finish().unwrap();
    }

    #[test]
    fn http_sse_size_limit_is_not_retryable() {
        assert!(!retryable_http_stream_error(&http_sse_error(
            "Codex SSE frame exceeds the size limit",
        )));
        assert!(retryable_http_stream_error(&http_sse_error(
            "Codex SSE frame contains invalid JSON",
        )));
        assert!(retryable_http_stream_error(&http_sse_error(
            "Codex SSE frame contains invalid UTF-8",
        )));
    }
}
