use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::Deserialize;

use crate::monitor::MonitorHandle;

const MAX_BUFFER: usize = 1024 * 1024;

pub(crate) fn observe(
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
        line_start: 0,
        bom_matched: 0,
        after_cr: false,
        has_data: false,
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

#[derive(Deserialize)]
struct Document<'a> {
    #[serde(rename = "type")]
    kind: Option<&'a str>,
    usage: Option<Usage>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<serde_json::Number>,
    cache_creation_input_tokens: Option<serde_json::Number>,
    cache_read_input_tokens: Option<serde_json::Number>,
    output_tokens: Option<serde_json::Number>,
}

fn data_value(line: &[u8]) -> Option<&[u8]> {
    if line == b"data" {
        Some(&[])
    } else {
        line.strip_prefix(b"data:")
            .map(|value| value.strip_prefix(b" ").unwrap_or(value))
    }
}

fn may_contain_usage(bytes: &[u8]) -> bool {
    bytes.windows(7).any(|window| window == b"\"usage\"")
        || bytes.windows(2).any(|window| window == b"\\u")
}

struct Observer {
    monitor: MonitorHandle,
    request_id: String,
    is_sse: bool,
    buffer: Vec<u8>,
    line_start: usize,
    bom_matched: usize,
    after_cr: bool,
    has_data: bool,
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
            let mut events = 0u64;
            for &byte in chunk {
                const BOM: &[u8] = b"\xef\xbb\xbf";
                if self.bom_matched < BOM.len() {
                    if byte == BOM[self.bom_matched] {
                        self.bom_matched += 1;
                        continue;
                    }
                    let matched = self.bom_matched;
                    self.bom_matched = BOM.len();
                    if !self.append(&BOM[..matched]) {
                        break;
                    }
                }
                if self.after_cr {
                    self.after_cr = false;
                    if byte == b'\n' {
                        continue;
                    }
                }
                if byte == b'\r' || byte == b'\n' {
                    let line = &self.buffer[self.line_start..];
                    if line.is_empty() {
                        if self.has_data {
                            events = events.saturating_add(1);
                            self.dispatch();
                        }
                        self.buffer.clear();
                        self.line_start = 0;
                        self.has_data = false;
                    } else {
                        if line == b"data" || line.starts_with(b"data:") {
                            self.has_data = true;
                        }
                        if !self.append(b"\n") {
                            break;
                        }
                        self.line_start = self.buffer.len();
                    }
                    self.after_cr = byte == b'\r';
                } else if !self.append(&[byte]) {
                    break;
                }
            }
            self.monitor.stream_progress(
                &self.request_id,
                chunk.len() as u64,
                events,
                self.total_input(),
                self.output,
            );
        } else {
            self.append(chunk);
        }
    }

    fn append(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > MAX_BUFFER.saturating_sub(self.buffer.len()) {
            self.buffer = Vec::new();
            self.stopped = true;
            return false;
        }
        self.buffer.extend_from_slice(bytes);
        true
    }

    fn dispatch(&mut self) {
        if !may_contain_usage(&self.buffer) {
            return;
        }
        let mut read = 0;
        let mut write = 0;
        while read < self.buffer.len() {
            let end = self.buffer[read..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(self.buffer.len(), |offset| read + offset);
            if let Some(value) = data_value(&self.buffer[read..end]) {
                let len = value.len();
                let start = end - len;
                self.buffer.copy_within(start..end, write);
                write += len;
                self.buffer[write] = b'\n';
                write += 1;
            }
            read = end + 1;
        }
        if let Ok(document) = serde_json::from_slice::<Document<'_>>(&self.buffer[..write]) {
            let usage = match document.kind {
                Some("message_start") => document.message.and_then(|message| message.usage),
                Some("message_delta") => document.usage,
                _ => None,
            };
            if let Some(usage) = usage {
                self.update(usage);
            }
        }
    }

    fn update(&mut self, usage: Usage) {
        let valid = |value: Option<serde_json::Number>| {
            value
                .and_then(|number| number.as_u64())
                .filter(|count| *count <= 1_000_000_000)
        };
        if let Some(value) = valid(usage.input_tokens) {
            self.input = Some(value);
        }
        if let Some(value) = valid(usage.cache_creation_input_tokens) {
            self.cache_creation = Some(value);
        }
        if let Some(value) = valid(usage.cache_read_input_tokens) {
            self.cache_read = Some(value);
        }
        if let Some(value) = valid(usage.output_tokens) {
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
        if self.stopped || self.is_sse || !may_contain_usage(&self.buffer) {
            return;
        }
        if let Ok(document) = serde_json::from_slice::<Document<'_>>(
            self.buffer
                .strip_prefix(b"\xef\xbb\xbf")
                .unwrap_or(&self.buffer),
        ) && let Some(usage) = document.usage
        {
            self.update(usage);
            self.monitor
                .usage_updated(&self.request_id, self.total_input(), self.output);
        }
    }
}
