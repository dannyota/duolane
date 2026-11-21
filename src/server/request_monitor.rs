use super::error_capture;
use crate::{
    logging::{Logger, is_credential_key},
    monitor::MonitorHandle,
    provider::MAX_BUFFERED_RESPONSE_BYTES,
    providers::codex::native::NativeResponseOutcome,
    request_identity::{CLAUDE_AGENT_HEADER, CLAUDE_PARENT_AGENT_HEADER},
};
use axum::{body::Body, http::StatusCode, response::Response};
use http_body_util::{BodyExt, StreamBody};
use serde_json::{Map, Value, json};
use std::time::Instant;

const REQUEST_ID_HEADER: &str = "request-id";

fn stamp_request_id(headers: &mut http::HeaderMap, req_id: &str) {
    if !headers.contains_key(REQUEST_ID_HEADER)
        && let Ok(value) = http::HeaderValue::from_str(req_id)
    {
        headers.insert(REQUEST_ID_HEADER, value);
    }
}

pub(super) fn with_request_id(mut response: Response, req_id: &str) -> Response {
    stamp_request_id(response.headers_mut(), req_id);
    response
}

pub(super) fn monitor_response_body(response: Response, guard: RequestMonitorGuard) -> Response {
    let status = response.status();
    let outcome = response
        .extensions()
        .get::<NativeResponseOutcome>()
        .cloned();
    let (mut parts, body) = response.into_parts();
    stamp_request_id(&mut parts.headers, &guard.req_id);
    let stream = futures_util::stream::unfold(
        (body, guard, outcome),
        move |(mut body, mut guard, outcome)| async move {
            match body.frame().await {
                Some(Ok(frame)) => Some((Ok(frame), (body, guard, outcome))),
                Some(Err(err)) => {
                    guard.failed(status, err.to_string());
                    Some((Err(err), (body, guard, outcome)))
                }
                None => {
                    if let Some(message) = outcome.as_ref().and_then(NativeResponseOutcome::failure)
                    {
                        guard.failed(status, message);
                    } else if status.is_success() {
                        guard.completed(status);
                    } else {
                        guard.failed(status, format!("HTTP {}", status.as_u16()));
                    }
                    None
                }
            }
        },
    );
    Response::from_parts(parts, Body::new(StreamBody::new(stream)))
}

pub(super) struct RequestLogContext<'a> {
    pub(super) req_id: &'a str,
    pub(super) provider: Option<&'a str>,
    pub(super) model: Option<&'a str>,
    pub(super) count_tokens: bool,
    pub(super) status: StatusCode,
    pub(super) started_at: Instant,
}

pub(super) fn log_request_completed(log: &Logger, ctx: RequestLogContext<'_>) {
    log.info(
        "request_completed",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(ctx.req_id)),
            ("provider".to_string(), json!(ctx.provider)),
            ("model".to_string(), json!(ctx.model)),
            ("countTokens".to_string(), json!(ctx.count_tokens)),
            ("status".to_string(), json!(ctx.status.as_u16())),
            (
                "ms".to_string(),
                json!(ctx.started_at.elapsed().as_millis()),
            ),
        ])),
    );
}

pub(super) struct FailedResponseLogContext<'a> {
    pub(super) req_id: &'a str,
    pub(super) provider: Option<&'a str>,
    pub(super) model: Option<&'a str>,
    pub(super) count_tokens: bool,
    pub(super) started_at: Instant,
}

pub(super) struct FailedResponseDetails {
    pub(super) message: String,
}

pub(super) async fn record_failed_response(
    log: &Logger,
    ctx: FailedResponseLogContext<'_>,
    response: Response,
) -> (Response, Option<FailedResponseDetails>) {
    if response.status().is_success() {
        return (response, None);
    }

    let status = response.status();
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BUFFERED_RESPONSE_BYTES).await {
        Ok(bytes) => bytes,
        Err(err) => {
            log.info(
                "request_failed",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), json!(ctx.req_id)),
                    ("provider".to_string(), json!(ctx.provider)),
                    ("model".to_string(), json!(ctx.model)),
                    ("countTokens".to_string(), json!(ctx.count_tokens)),
                    ("status".to_string(), json!(status.as_u16())),
                    (
                        "ms".to_string(),
                        json!(ctx.started_at.elapsed().as_millis()),
                    ),
                    ("bodyReadError".to_string(), json!(err.to_string())),
                ])),
            );
            return (Response::from_parts(parts, Body::empty()), None);
        }
    };

    let response_body = response_body_value(&bytes);
    let message = error_message_from_response(&response_body)
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
    let document = json!({
        "reqId": ctx.req_id,
        "provider": ctx.provider,
        "model": ctx.model,
        "countTokens": ctx.count_tokens,
        "status": status.as_u16(),
        "elapsedMs": ctx.started_at.elapsed().as_millis(),
        "message": message,
        "response": response_body,
    });
    let error_file = error_capture::write(ctx.req_id, document).await;

    let mut fields = serde_json::Map::from_iter([
        ("reqId".to_string(), json!(ctx.req_id)),
        ("provider".to_string(), json!(ctx.provider)),
        ("model".to_string(), json!(ctx.model)),
        ("countTokens".to_string(), json!(ctx.count_tokens)),
        ("status".to_string(), json!(status.as_u16())),
        (
            "ms".to_string(),
            json!(ctx.started_at.elapsed().as_millis()),
        ),
        ("message".to_string(), json!(message)),
    ]);
    if let Some(path) = error_file.as_ref() {
        fields.insert("errorFile".to_string(), json!(path.display().to_string()));
    }
    log.info("request_failed", Some(fields));

    (
        Response::from_parts(parts, Body::from(bytes)),
        Some(FailedResponseDetails { message }),
    )
}

fn response_body_value(bytes: &[u8]) -> Value {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => json!({ "json": value }),
        Err(_) => json!({ "text": String::from_utf8_lossy(bytes) }),
    }
}

fn error_message_from_response(response_body: &Value) -> Option<String> {
    response_body
        .get("json")
        .and_then(|body| body.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| {
            response_body
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .map(std::string::ToString::to_string)
}

pub(super) struct RequestMonitorGuard {
    monitor: Option<MonitorHandle>,
    req_id: String,
}

impl RequestMonitorGuard {
    pub(super) fn new(monitor: Option<MonitorHandle>, req_id: String) -> Self {
        Self { monitor, req_id }
    }

    fn completed(&mut self, status: StatusCode) {
        if let Some(monitor) = self.monitor.take() {
            monitor.request_completed(&self.req_id, status.as_u16(), None, None);
        }
    }

    fn failed(&mut self, status: StatusCode, error: String) {
        if let Some(monitor) = self.monitor.take() {
            monitor.request_failed(&self.req_id, Some(status.as_u16()), error);
        }
    }
}

impl Drop for RequestMonitorGuard {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor.as_ref() {
            monitor.request_abandoned(&self.req_id, "Request future ended before completion");
        }
    }
}

pub(super) fn monitor_failed(
    monitor: Option<&MonitorHandle>,
    req_id: &str,
    status: Option<StatusCode>,
    error: impl Into<String>,
) {
    if let Some(monitor) = monitor {
        monitor.request_failed(req_id, status.map(|status| status.as_u16()), error);
    }
}

pub(super) fn headers_to_record(headers: &http::HeaderMap) -> Value {
    let mut out = Map::new();
    for (key, value) in headers {
        if let Ok(raw) = value.to_str() {
            let recorded = if is_credential_key(key.as_str())
                || matches!(
                    key.as_str(),
                    CLAUDE_AGENT_HEADER | CLAUDE_PARENT_AGENT_HEADER
                ) {
                format!("[redacted len={}]", raw.len())
            } else {
                raw.to_string()
            };
            out.insert(key.as_str().to_string(), Value::String(recorded));
        }
    }
    Value::Object(out)
}

pub(super) fn redacted_query(uri: &http::Uri) -> Value {
    let mut out = Map::new();
    let Some(query) = uri.query() else {
        return Value::Object(out);
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let key = key.into_owned();
        let value = if is_credential_key(&key) {
            Value::String(format!("[redacted len={}]", value.len()))
        } else {
            Value::String(value.into_owned())
        };
        out.insert(key, value);
    }
    Value::Object(out)
}

#[cfg(test)]
mod request_id_header_tests {
    use super::{REQUEST_ID_HEADER, RequestMonitorGuard, monitor_response_body};
    use axum::body::Body;
    use axum::response::Response;
    use http::{HeaderValue, StatusCode};

    fn guard(req_id: &str) -> RequestMonitorGuard {
        RequestMonitorGuard::new(None, req_id.to_string())
    }

    #[tokio::test]
    async fn completed_stream_retains_anthropic_usage() {
        use crate::monitor::{EndpointKind, MonitorHandle};
        use crate::providers::anthropic::observe_usage_for_test;
        use bytes::Bytes;

        let monitor = MonitorHandle::new(10);
        monitor.request_started("req", Some("session".into()), None, EndpointKind::Messages);
        let input = Bytes::from_static(b"data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":12,\"output_tokens\":8}}\n\n");
        let stream = futures_util::stream::iter(vec![Ok(input)]);
        let stream = observe_usage_for_test(stream, Some(monitor.clone()), "req".into(), true);
        let response = Response::new(Body::from_stream(stream));
        let guard = RequestMonitorGuard::new(Some(monitor.clone()), "req".into());
        let response = monitor_response_body(response, guard);
        let _ = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.recent[0].input_tokens, Some(12));
        assert_eq!(snapshot.recent[0].output_tokens, Some(8));
    }

    #[test]
    fn stamps_the_request_id_on_a_response() {
        let response = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from("{}"))
            .unwrap();
        let stamped = monitor_response_body(response, guard("req-abc-123"));
        assert_eq!(
            stamped.headers().get(REQUEST_ID_HEADER).unwrap(),
            "req-abc-123"
        );
    }

    #[test]
    fn stamps_a_streaming_response_before_the_body() {
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from("event: message_start\n"))
            .unwrap();
        let stamped = monitor_response_body(response, guard("req-stream-1"));
        assert_eq!(
            stamped.headers().get(REQUEST_ID_HEADER).unwrap(),
            "req-stream-1"
        );
    }

    #[test]
    fn does_not_clobber_an_upstream_supplied_id() {
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(
                REQUEST_ID_HEADER,
                HeaderValue::from_static("upstream-owned"),
            )
            .body(Body::from("{}"))
            .unwrap();
        let stamped = monitor_response_body(response, guard("local-uuid"));
        assert_eq!(
            stamped.headers().get(REQUEST_ID_HEADER).unwrap(),
            "upstream-owned"
        );
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::{headers_to_record, redacted_query};
    use crate::logging::redact_value;
    use crate::traffic::redact_traffic;
    use serde_json::json;

    #[test]
    fn credential_names_redact_headers_query_and_nested_values_of_any_type() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-upstream-token", "header-secret".parse().unwrap());
        headers.insert("x-client_secret", "client-secret".parse().unwrap());
        headers.insert("x-request-tokens", "17".parse().unwrap());
        let recorded = headers_to_record(&headers);
        assert_eq!(recorded["x-upstream-token"], "[redacted len=13]");
        assert_eq!(recorded["x-client_secret"], "[redacted len=13]");
        assert_eq!(recorded["x-request-tokens"], "17");

        let uri: http::Uri = "/v1/messages?Refresh-Token=query-secret&API_KEY=key&beta=true"
            .parse()
            .unwrap();
        let query = redacted_query(&uri);
        assert_eq!(query["Refresh-Token"], "[redacted len=12]");
        assert_eq!(query["API_KEY"], "[redacted len=3]");
        assert_eq!(query["beta"], "true");

        let body = json!({
            "metadata": {
                "Session_Token": {"value": "nested-secret"},
                "items": [{"db-password": ["array-secret"], "max_tokens": 5}],
                "access": 42
            },
            "input": [{"type": "input_image", "image_url": {
                "url": "data:image/png,raw-image-bytes",
                "detail": "high"
            }}]
        });
        for redacted in [redact_traffic(&body), redact_value(body.clone())] {
            let rendered = redacted.to_string();
            for secret in ["nested-secret", "array-secret", "42"] {
                assert!(!rendered.contains(secret), "{rendered}");
            }
            assert_eq!(redacted["metadata"]["Session_Token"], "[redacted]");
            assert_eq!(redacted["metadata"]["items"][0]["max_tokens"], 5);
        }
        let traffic = redact_traffic(&body);
        let image = &traffic["input"][0]["image_url"];
        assert_eq!(image["detail"], "high");
        assert!(!image["url"].as_str().unwrap().contains("raw-image-bytes"));
    }
}
