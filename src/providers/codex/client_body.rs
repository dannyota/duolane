use std::io::{self, Write};

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

pub(super) type CodexHttpBodyCache = OnceCell<CodexHttpBody>;

#[derive(Clone)]
pub(super) struct CodexHttpBody {
    body: Bytes,
    encoding: CodexRequestCompression,
    capture_json: Option<Bytes>,
}

impl CodexHttpClient {
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
