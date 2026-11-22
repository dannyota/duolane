use std::time::Instant;

use crate::anthropic::sse::parse_sse_events;
use crate::traffic::TrafficCapture;

use super::auth::token_store::StoredAuth;
use super::client::*;
use super::client_body::CodexHttpBody;

#[derive(Default)]
pub(super) struct HttpSseDecoder {
    frame: Vec<u8>,
    line_start: usize,
    skip_lf: bool,
}

#[derive(Default)]
pub(super) struct BufferedHttpSseEvidence {
    frame: Vec<u8>,
    line_nonempty: bool,
    discard: bool,
    skip_lf: bool,
    pub(super) hosted: bool,
    pub(super) terminal: bool,
}

impl BufferedHttpSseEvidence {
    pub(super) fn push(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.skip_lf = byte == b'\r';
                if !self.line_nonempty {
                    if !self.discard
                        && let Ok(Some(event)) = decode_http_sse_frame(&self.frame)
                        && let Some(payload) = event.payload
                    {
                        self.hosted |= payload_confirms_hosted(&payload);
                        self.terminal |= super::events::event_is_terminal(&payload);
                    }
                    self.frame.clear();
                    self.discard = false;
                } else {
                    self.append(b'\n');
                }
                self.line_nonempty = false;
            } else {
                self.line_nonempty = true;
                self.append(byte);
            }
        }
    }

    pub(super) fn finish_hosted(&mut self) {
        if self.discard {
            return;
        }
        for event in parse_sse_events(&self.frame) {
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) {
                self.hosted |= payload_confirms_hosted(&payload);
            }
        }
    }

    fn append(&mut self, byte: u8) {
        if !self.discard {
            if self.frame.len() >= MAX_UPSTREAM_FRAME_BYTES {
                self.frame.clear();
                self.discard = true;
            } else {
                self.frame.push(byte);
            }
        }
    }
}

pub(super) fn payload_confirms_hosted(payload: &serde_json::Value) -> bool {
    super::events::event_confirms_hosted_tool_execution(payload)
        || payload
            .pointer("/response/output")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|output| {
                output.iter().any(|item| {
                    item.get("type").and_then(serde_json::Value::as_str) == Some("web_search_call")
                })
            })
}

pub(super) fn http_payload_hosted(payload: &serde_json::Value, tracked: bool) -> bool {
    if tracked {
        payload_confirms_hosted(payload)
    } else {
        super::events::event_confirms_hosted_tool_execution(payload)
    }
}

pub(super) fn record_live_stream_downstream_capture(
    ctx: &crate::provider::RequestContext,
    chunk: &[u8],
) {
    let Some(traffic) = ctx.traffic.as_ref() else {
        return;
    };
    for event in parse_sse_events(chunk) {
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            continue;
        };
        traffic.write_json_event(
            "050-downstream-event",
            &serde_json::json!({
                "event": event.event.as_deref().unwrap_or("message"),
                "data": data,
            }),
        );
    }
}

pub(super) fn translated_content_ready(chunk: &[u8]) -> bool {
    parse_sse_events(chunk).into_iter().any(|event| {
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            return false;
        };
        match event.event.as_deref() {
            Some("content_block_delta") => payload
                .get("delta")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|delta| {
                    delta.iter().any(|(key, value)| {
                        key != "type" && value.as_str().is_some_and(|text| !text.is_empty())
                    })
                }),
            Some("content_block_start") => payload.get("content_block").is_some_and(|block| {
                match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("text") => block
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| !text.is_empty()),
                    Some("thinking") => block
                        .get("thinking")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| !text.is_empty()),
                    Some("tool_use" | "server_tool_use" | "web_search_tool_result") => true,
                    _ => false,
                }
            }),
            _ => false,
        }
    })
}

pub(super) fn trace_http_failure(
    payload: &serde_json::Value,
    headers: &[(String, String)],
    budget: &super::recovery::RecoveryBudget,
) -> Option<CodexError> {
    if let Some(failure) = serde_json::to_vec(payload)
        .ok()
        .and_then(|body| explicit_permanent_buffered_error(&body))
    {
        budget.mark_final_only();
        let context_window = failure.context_window;
        let mut error = codex_event_failure_error(failure);
        if context_window {
            error.detail = Some(format!("{} (context_length_exceeded)", error.message));
        }
        return Some(error);
    }
    if super::events::usage_limit_from_event_with_headers(payload, headers).is_some() {
        budget.body_uncertain();
        budget.set_kind(super::sources::FailureKind::UsageLimit);
    } else if super::events::classify_event_failure(payload)
        .is_some_and(|failure| failure.retryable())
    {
        budget.body_uncertain();
    }
    None
}

pub(super) struct DecodedHttpSseEvent {
    pub(super) event: Option<String>,
    pub(super) payload: Option<serde_json::Value>,
}

pub(super) struct HttpEventStreamState {
    pub(super) resp: reqwest::Response,
    pub(super) response_headers: Vec<(String, String)>,
    pub(super) started_at: Instant,
    pub(super) http_body: CodexHttpBody,
    pub(super) auth: StoredAuth,
    pub(super) auth_refresh_attempted: bool,
    pub(super) use_responses_lite: bool,
    pub(super) retries: u32,
    pub(super) tracked: bool,
}

impl HttpSseDecoder {
    pub(super) fn push(
        &mut self,
        input: &[u8],
        events: &mut Vec<DecodedHttpSseEvent>,
    ) -> Result<(), CodexError> {
        for &byte in input {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(events)?,
                b'\r' => {
                    self.end_line(events)?;
                    self.skip_lf = true;
                }
                _ => self.push_byte(byte)?,
            }
        }
        Ok(())
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

    pub(super) fn pending_hosted(&self) -> bool {
        parse_sse_events(&self.frame)
            .into_iter()
            .filter_map(|event| serde_json::from_str(&event.data).ok())
            .any(|payload| payload_confirms_hosted(&payload))
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

pub(super) fn record_live_stream_progress(ctx: &crate::provider::RequestContext, chunk: &[u8]) {
    if let Some(monitor) = ctx.monitor.as_ref() {
        let (input_tokens, output_tokens) = crate::anthropic::sse::usage_from_anthropic_sse(chunk);
        monitor.stream_progress(
            &ctx.req_id,
            chunk.len() as u64,
            super::request_state::count_sse_events(chunk),
            input_tokens,
            output_tokens,
        );
    }
}

pub(super) fn append_live_metadata_payload(
    buffer: &mut Vec<u8>,
    payload: &serde_json::Value,
    is_http: bool,
) {
    if is_http
        && payload["type"] == "response.output_item.done"
        && super::events::is_function_call_item(payload)
        && let Some(index) = payload["output_index"].as_u64()
    {
        let item = &payload["item"];
        append_upstream_sse_payload(
            buffer,
            &serde_json::json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": {"type": "function_call", "call_id": item["call_id"], "name": item["name"]}
            }),
        );
    }
    append_upstream_sse_payload(buffer, payload);
}

fn append_upstream_sse_payload(buffer: &mut Vec<u8>, payload: &serde_json::Value) {
    let text = payload.to_string();
    for line in text.lines() {
        buffer.extend_from_slice(b"data: ");
        buffer.extend_from_slice(line.as_bytes());
        buffer.push(b'\n');
    }
    buffer.push(b'\n');
}

pub(super) fn translate_live_stream_payload(
    translator: &mut super::translate::live_stream::LiveStreamTranslator,
    payload: &serde_json::Value,
    traffic: Option<&crate::traffic::TrafficCapture>,
) -> Result<(Vec<u8>, bool), String> {
    let chunk = translator.accept(payload, traffic)?;
    let terminal = super::events::event_is_terminal(payload) || translator.is_finished();
    Ok((chunk, terminal))
}

pub(super) fn single_live_stream_response(chunk: Vec<u8>) -> axum::response::Response {
    event_stream_response(futures_util::stream::once(async move {
        Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(chunk))
    }))
}

pub(super) fn event_stream_response<S>(stream: S) -> axum::response::Response
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + 'static,
{
    let headers = [
        (http::header::CONTENT_TYPE, "text/event-stream"),
        (http::header::CACHE_CONTROL, "no-cache"),
        (http::header::CONNECTION, "keep-alive"),
    ];
    axum::response::IntoResponse::into_response((headers, axum::body::Body::from_stream(stream)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_sse_decoder_handles_fragmented_crlf_and_done_marker() {
        let mut decoder = HttpSseDecoder::default();
        let mut events = Vec::new();
        decoder
            .push(b"event: response.output_text.delta\r", &mut events)
            .unwrap();
        assert!(events.is_empty());
        decoder
            .push(
                b"\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\r\n\r\n",
                &mut events,
            )
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

        events.clear();
        decoder.push(b"data: [DONE]\n\n", &mut events).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].payload.is_none());
        decoder.finish().unwrap();
    }

    #[test]
    fn buffered_evidence_resynchronizes_after_invalid_and_oversized_frames() {
        let mut evidence = BufferedHttpSseEvidence::default();
        evidence.push(b"data: \xff\r\n\r\n");
        evidence.push(&vec![b'x'; MAX_UPSTREAM_FRAME_BYTES + 1]);
        evidence.push(b"\r\n\r");
        evidence.push(b"\ndata: {\"type\":\"response.web_search_call.completed\"}\r\n\r");
        assert!(evidence.hosted);
        assert!(!evidence.terminal);
        evidence.push(b"\ndata: {\"type\":\"response.completed\"}\r\n");
        assert!(!evidence.terminal);
        evidence.push(b"\r\n");
        assert!(evidence.terminal);
    }

    #[test]
    fn exhausted_recovery_is_not_retryable() {
        let error = CodexError::recovery_exhausted();
        assert!(!retryable_http_stream_error(&error));
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
