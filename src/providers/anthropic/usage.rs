use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;

use crate::monitor::MonitorHandle;

const MAX_BUFFER: usize = 1024 * 1024;

pub(super) fn observe(
    stream: impl Stream<Item = std::io::Result<Bytes>> + Send + 'static,
    monitor: Option<MonitorHandle>,
    request_id: String,
    is_sse: bool,
) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    let observer = monitor.map(|monitor| Observer {
        monitor,
        request_id,
        is_sse,
        buffer: Vec::new(),
        stopped: false,
        started: false,
        input: None,
        cache_creation: None,
        cache_read: None,
        output: None,
    });
    futures_util::stream::unfold(
        (Box::pin(stream), observer),
        |(mut stream, mut observer)| async move {
            match stream.next().await {
                Some(result) => {
                    if let (Some(observer), Ok(chunk)) = (&mut observer, &result) {
                        observer.chunk(chunk);
                    }
                    if result.is_err() {
                        observer = None;
                    }
                    Some((result, (stream, observer)))
                }
                None => {
                    if let Some(observer) = &mut observer {
                        observer.finish();
                    }
                    None
                }
            }
        },
    )
}

struct Observer {
    monitor: MonitorHandle,
    request_id: String,
    is_sse: bool,
    buffer: Vec<u8>,
    stopped: bool,
    started: bool,
    input: Option<u64>,
    cache_creation: Option<u64>,
    cache_read: Option<u64>,
    output: Option<u64>,
}

impl Observer {
    fn chunk(&mut self, chunk: &[u8]) {
        if self.stopped {
            return;
        }
        if self.is_sse && !chunk.is_empty() && !self.started {
            self.monitor.generation_started(&self.request_id);
            self.started = true;
        }
        if self.is_sse {
            for part in chunk.split_inclusive(|byte| *byte == b'\n') {
                if !self.append(part) {
                    return;
                }
                if part.ends_with(b"\n") {
                    let line = std::mem::take(&mut self.buffer);
                    self.line(&line);
                }
            }
            self.monitor.stream_progress(
                &self.request_id,
                chunk.len() as u64,
                1,
                self.total_input(),
                self.output,
            );
        } else {
            self.append(chunk);
        }
    }

    fn append(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > MAX_BUFFER.saturating_sub(self.buffer.len()) {
            self.buffer.clear();
            self.stopped = true;
            return false;
        }
        self.buffer.extend_from_slice(bytes);
        true
    }

    fn line(&mut self, line: &[u8]) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            return;
        };
        let usage = match value.get("type").and_then(Value::as_str) {
            Some("message_start") => value.pointer("/message/usage"),
            Some("message_delta") => value.get("usage"),
            _ => None,
        };
        if let Some(usage) = usage {
            self.update(usage);
        }
    }

    fn update(&mut self, usage: &Value) {
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input = Some(value);
        }
        if let Some(value) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.cache_creation = Some(value);
        }
        if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.cache_read = Some(value);
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output = Some(value);
        }
    }

    fn total_input(&self) -> Option<u64> {
        if self.input.is_none() && self.cache_creation.is_none() && self.cache_read.is_none() {
            return None;
        }
        Some(
            self.input
                .unwrap_or(0)
                .saturating_add(self.cache_creation.unwrap_or(0))
                .saturating_add(self.cache_read.unwrap_or(0)),
        )
    }

    fn finish(&mut self) {
        if self.stopped || self.is_sse {
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&self.buffer)
            && let Some(usage) = value.get("usage")
        {
            self.update(usage);
            self.monitor
                .usage_updated(&self.request_id, self.total_input(), self.output);
        }
    }
}
