use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Method, Request, StatusCode, header};
use axum::response::Response;
use hyper::body::{Body as _, Incoming};
use serde_json::json;

use super::{
    TCP_KEEPALIVE, TCP_KEEPALIVE_INTERVAL, TCP_KEEPALIVE_RETRIES, UPSTREAM_CONNECT_TIMEOUT,
};
use crate::anthropic::json_error;
use crate::inbound_auth::PROXY_KEY_HEADER;
use crate::logging::create_logger;

const UNREACHABLE: &str = "the intercepted request could not reach the Anthropic upstream";

pub(super) struct Relay {
    client: reqwest::Client,
    base_url: String,
}

impl Relay {
    pub(super) fn new() -> anyhow::Result<Self> {
        let base_url = crate::config::anthropic_base_url();
        let mut builder = crate::tls::client_builder()
            .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .tcp_keepalive(TCP_KEEPALIVE)
            .tcp_keepalive_interval(TCP_KEEPALIVE_INTERVAL)
            .tcp_keepalive_retries(TCP_KEEPALIVE_RETRIES);
        if url::Url::parse(&base_url).is_ok_and(|url| crate::upstream_url::is_loopback_host(&url)) {
            builder = builder.no_proxy();
        }
        Ok(Self {
            client: builder.build()?,
            base_url,
        })
    }

    pub(super) async fn forward(&self, request: Request<Incoming>) -> Response {
        let (parts, body) = request.into_parts();
        let target = parts
            .uri
            .path_and_query()
            .map_or_else(|| parts.uri.path(), |target| target.as_str());
        let method = parts.method.clone();
        let mut outgoing = self
            .client
            .request(parts.method, format!("{}{target}", self.base_url))
            .headers(forwarded_headers(&parts.headers, true));
        if !body.is_end_stream() {
            outgoing = outgoing.body(reqwest::Body::wrap(body));
        }
        let Ok(upstream) = outgoing.send().await else {
            log(&method, StatusCode::BAD_GATEWAY);
            return json_error(StatusCode::BAD_GATEWAY, "api_error", UNREACHABLE);
        };
        let status = upstream.status();
        log(&method, status);
        let mut headers = forwarded_headers(upstream.headers(), false);
        if method == Method::HEAD
            && let Some(length) = upstream.headers().get(header::CONTENT_LENGTH)
        {
            headers.insert(header::CONTENT_LENGTH, length.clone());
        }
        let mut response = Response::new(Body::new(reqwest::Body::from(upstream)));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        response
    }
}

fn forwarded_headers(headers: &HeaderMap, request: bool) -> HeaderMap {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect();
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if !named.contains(name) && !dropped(name.as_str(), request) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

fn dropped(name: &str, request: bool) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "te"
            | "transfer-encoding"
            | "content-length"
            | "upgrade"
            | "proxy-authorization"
    ) || request && (name == "host" || name == PROXY_KEY_HEADER)
}

fn log(method: &Method, status: StatusCode) {
    create_logger("intercept").debug(
        "relayed",
        Some(serde_json::Map::from_iter([
            ("method".to_string(), json!(method.as_str())),
            ("status".to_string(), json!(status.as_u16())),
        ])),
    );
}
