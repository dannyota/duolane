use std::io::{self, Write};
use std::sync::atomic::AtomicBool;

use bytes::Bytes;
use flate2::{Compression, write::GzEncoder};
use http::header::CONTENT_ENCODING;
use serde::Serialize;
use tokio::sync::{OnceCell, Semaphore};

use crate::anthropic::MAX_ANTHROPIC_REQUEST_BYTES;
use crate::config::CodexRequestCompression;
use crate::traffic::TrafficCapture;

use super::client::{CodexError, CodexHttpClient};
use super::client_capture::write_codex_http_request_capture;

static GZIP_SLOTS: Semaphore = Semaphore::const_new(2);

#[derive(Default)]
pub(super) struct CodexHttpBodyCache {
    body: OnceCell<CodexHttpBody>,
    pub(super) websocket_sent: std::sync::Arc<AtomicBool>,
    pub(super) size_http_selected: AtomicBool,
    measured_over_limit: OnceCell<bool>,
    #[cfg(test)]
    pub(super) size_measurements: std::sync::atomic::AtomicUsize,
}

impl CodexHttpBodyCache {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn with_websocket_sent(websocket_sent: std::sync::Arc<AtomicBool>) -> Self {
        Self {
            websocket_sent,
            ..Self::default()
        }
    }
}

impl std::ops::Deref for CodexHttpBodyCache {
    type Target = OnceCell<CodexHttpBody>;
    fn deref(&self) -> &Self::Target {
        &self.body
    }
}

#[derive(Clone)]
pub(super) struct CodexHttpBody {
    body: Bytes,
    encoding: CodexRequestCompression,
    capture_json: Option<Bytes>,
}

pub(super) fn record_size_fallback(missing_socket: &mut bool) {
    if std::mem::take(missing_socket) {
        super::websocket_pool::HTTP_FALLBACK_COUNT
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl CodexHttpClient {
    pub(super) async fn auto_size_http_selected(
        &self,
        body: &super::translate::request::ResponsesRequest,
        ctx: &crate::provider::RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
        cache: &CodexHttpBodyCache,
    ) -> Result<bool, CodexError> {
        Ok(cache
            .size_http_selected
            .load(std::sync::atomic::Ordering::Acquire)
            || self
                .auto_large_full_context(body, ctx, continuation, cache)
                .await?)
    }

    pub(super) async fn auto_large_full_context(
        &self,
        body: &super::translate::request::ResponsesRequest,
        _ctx: &crate::provider::RequestContext,
        continuation: Option<&super::continuation::ContinuationReservation>,
        _cache: &CodexHttpBodyCache,
    ) -> Result<bool, CodexError> {
        if !self.auto_http_fallback_enabled
            || self.request_compression != CodexRequestCompression::Gzip
            || self.auto_ws_max_bytes == 0
            || continuation.is_some_and(|c| c.candidate().previous_response_id.is_some())
        {
            return Ok(false);
        }
        let over_limit = _cache
            .measured_over_limit
            .get_or_try_init(|| async {
                #[cfg(test)]
                _cache
                    .size_measurements
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut counter = JsonCounter(0);
                serde_json::to_writer(&mut counter, body)
                    .map_err(|error| json_error("Failed to serialize request", error))?;
                Ok::<bool, CodexError>(counter.0 > self.auto_ws_max_bytes)
            })
            .await?;
        Ok(*over_limit)
    }

    pub(super) async fn prepare_http_body(
        &self,
        value: &impl Serialize,
        capture: bool,
        serialization_error: &str,
    ) -> Result<CodexHttpBody, CodexError> {
        if self.request_compression == CodexRequestCompression::Off {
            let json = serde_json::to_vec(value)
                .map_err(|error| json_error(serialization_error, error))?;
            return Ok(CodexHttpBody {
                body: Bytes::from(json),
                encoding: CodexRequestCompression::Off,
                capture_json: None,
            });
        }

        let permit = GZIP_SLOTS.acquire().await.map_err(compression_error)?;
        let json = serialize_gzip_input(value, MAX_ANTHROPIC_REQUEST_BYTES, serialization_error)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&json)?;
            Ok(CodexHttpBody {
                body: Bytes::from(encoder.finish()?),
                encoding: CodexRequestCompression::Gzip,
                capture_json: capture.then_some(json),
            })
        })
        .await
        .map_err(compression_error)?
        .map_err(compression_error::<io::Error>)
    }
}

struct JsonCounter(usize);

impl Write for JsonCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BoundedJson {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedJson {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.limit - self.bytes.len() {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let len = self.bytes.len() + buf.len();
        let capacity = self.bytes.capacity();
        if len > capacity {
            let next = capacity.saturating_mul(2).max(len).min(self.limit);
            self.bytes.reserve_exact(next - self.bytes.len());
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_gzip_input(
    value: &impl Serialize,
    limit: usize,
    serialization_error: &str,
) -> Result<Bytes, CodexError> {
    let mut json = BoundedJson {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut json, value).map_err(|error| {
        if error.io_error_kind() == Some(io::ErrorKind::FileTooLarge) {
            CodexError::gzip_input_limit()
        } else {
            json_error(serialization_error, error)
        }
    })?;
    Ok(json.bytes.into())
}

fn json_error(message: &str, error: serde_json::Error) -> CodexError {
    CodexError {
        detail: Some(error.to_string()),
        ..CodexError::new(500, message.to_string())
    }
}

impl CodexHttpBody {
    pub(super) fn apply(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.encoding == CodexRequestCompression::Gzip {
            request = request.header(CONTENT_ENCODING, "gzip");
        }
        request.body(self.body.clone())
    }

    pub(super) fn capture(&self, traffic: &TrafficCapture, url: &str, headers: &http::HeaderMap) {
        let json = match self.encoding {
            CodexRequestCompression::Off => Some(&self.body),
            CodexRequestCompression::Gzip => self.capture_json.as_ref(),
        };
        if let Some(json) = json {
            write_codex_http_request_capture(traffic, url, headers, json);
        }
    }
}

fn compression_error<E: std::error::Error>(_: E) -> CodexError {
    CodexError::new(500, "Failed to gzip Codex HTTP request".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn under_threshold_does_not_prepare_gzip_or_enforce_http_body_limit() {
        let mut client = super::super::client_test_support::http_test_client(
            "http://127.0.0.1:1".to_string(),
            100,
        );
        client.request_compression = CodexRequestCompression::Gzip;
        client.auto_ws_max_bytes = 128 * 1024 * 1024;
        let request = super::super::client_test_support::buffered_request_with_texts(&[
            &"x".repeat(65 * 1024 * 1024)
        ]);
        let cache = CodexHttpBodyCache::new();
        assert!(
            !client
                .auto_large_full_context(
                    &request,
                    &super::super::client_test_support::http_test_context(),
                    None,
                    &cache
                )
                .await
                .unwrap()
        );
        assert!(cache.get().is_none());
    }

    #[test]
    fn gzip_serialization_rejects_writes_over_the_input_limit() {
        let value = [0, 0, 0];
        let json = serialize_gzip_input(&value, 7, "test serialization failed").unwrap();
        assert!(
            json == b"[0,0,0]".as_slice(),
            "JSON changed at its size limit"
        );
        let error = serialize_gzip_input(&value, 6, "test serialization failed").unwrap_err();
        assert!(error.is_gzip_input_limit());

        let mut writer = BoundedJson {
            bytes: Vec::new(),
            limit: 7,
        };
        assert!(serde_json::to_writer(&mut writer, &[0, 0, 0, 0]).is_err());
        assert!(writer.bytes.capacity() <= writer.limit);
    }
}
