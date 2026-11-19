use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::Response;
use hyper::body::Incoming;
use tower::ServiceExt;

use super::INTERCEPTED_HOST;
use super::relay::Relay;
use crate::anthropic::json_error;

const MESSAGE_PATHS: [&str; 2] = ["/v1/messages", "/v1/messages/count_tokens"];

pub(super) async fn dispatch(request: Request<Incoming>, app: &Router, relay: &Relay) -> Response {
    if !host_allowed(request.headers()) {
        return json_error(
            StatusCode::MISDIRECTED_REQUEST,
            "invalid_request_error",
            "Requests on this connection must name api.anthropic.com in Host",
        );
    }
    let uri = request.uri();
    if uri.scheme().is_some() || uri.authority().is_some() || !uri.path().starts_with('/') {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Request target must be in origin form",
        );
    }
    let message_path = MESSAGE_PATHS.contains(&uri.path());
    if !message_path && disguises_message_path(uri.path()) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Request path must spell a message path exactly",
        );
    }
    if request.headers().contains_key(header::UPGRADE) || request.method() == Method::CONNECT {
        return json_error(
            StatusCode::NOT_IMPLEMENTED,
            "invalid_request_error",
            "Protocol upgrades and CONNECT are not supported on this connection",
        );
    }
    if message_path && request.headers().contains_key(header::CONTENT_ENCODING) {
        return json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request_error",
            "Message requests must not carry a Content-Encoding",
        );
    }
    if !message_path || request.method() != Method::POST {
        return relay.forward(request).await;
    }
    match app.clone().oneshot(request.map(Body::new)).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

fn host_allowed(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::HOST).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value.to_str().is_ok_and(|host| {
        host.strip_suffix(":443")
            .unwrap_or(host)
            .eq_ignore_ascii_case(INTERCEPTED_HOST)
    })
}

fn disguises_message_path(path: &str) -> bool {
    let decoded = percent_decode(path.as_bytes());
    let mut segments: Vec<&[u8]> = Vec::new();
    for segment in decoded.split(|byte| matches!(byte, b'/' | b'\\')) {
        match segment {
            b"" | b"." => {}
            b".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    let mut normalized = Vec::with_capacity(decoded.len());
    for segment in segments {
        normalized.push(b'/');
        normalized.extend(segment.iter().map(u8::to_ascii_lowercase));
    }
    MESSAGE_PATHS
        .iter()
        .any(|candidate| candidate.as_bytes() == normalized)
}

fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%'
            && let [high, low, after @ ..] = tail
            && let (Some(high), Some(low)) = (hex(*high), hex(*low))
        {
            decoded.push(high << 4 | low);
            rest = after;
            continue;
        }
        decoded.push(byte);
        rest = tail;
    }
    decoded
}

fn hex(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
}
