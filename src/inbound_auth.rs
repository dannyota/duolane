use std::{net::IpAddr, sync::Arc};

use axum::{
    Router,
    extract::{Request, State},
    http::{
        HeaderMap, Method, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN},
    },
    middleware::Next,
    response::Response,
};

use crate::{anthropic::json_error, auth::constant_time_eq};

pub const PROXY_KEY_HEADER: &str = "x-duolane-key";
pub const MIN_TOKEN_CHARS: usize = 32;
pub const PLAIN_HTTP_WARNING: &str = "warning: duolane is listening beyond loopback over plain HTTP. Inbound traffic, including the duolane token and the Claude login, is unencrypted; reach it only through an encrypted path such as an SSH tunnel, WireGuard, or a TLS-terminating proxy.";

const JSON: &str = "application/json";
const MULTIPART: &str = "multipart/form-data";

#[derive(Debug, Clone)]
pub struct InboundPolicy {
    token: Option<Arc<str>>,
    loopback: bool,
    local_hosts: Vec<String>,
    hosts: Vec<String>,
}

impl InboundPolicy {
    pub fn new(
        bind: IpAddr,
        token: Option<String>,
        extra_hosts: Vec<String>,
    ) -> anyhow::Result<Self> {
        if token
            .as_ref()
            .is_some_and(|token| token.chars().count() < MIN_TOKEN_CHARS)
        {
            anyhow::bail!(
                "the inbound token must be at least {MIN_TOKEN_CHARS} characters; set a longer DUOLANE_AUTH_TOKEN or authToken"
            );
        }
        let loopback = bind.to_canonical().is_loopback();
        if !loopback && token.is_none() {
            anyhow::bail!(
                "refusing to listen on non-loopback address {bind} without an inbound token; set DUOLANE_AUTH_TOKEN or authToken"
            );
        }
        if !extra_hosts.is_empty() && token.is_none() {
            anyhow::bail!(
                "allowed hosts require an inbound token; set DUOLANE_AUTH_TOKEN or authToken"
            );
        }
        if let Some(host) = extra_hosts.iter().find(|host| !valid_extra_host(host)) {
            anyhow::bail!(
                "invalid allowed host {host:?}: use a bare host name or a bracketed IPv6 address, without scheme, port, path, userinfo, or trailing dot"
            );
        }
        let literal = match bind {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };
        let local_hosts = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "[::1]".to_string(),
            literal,
        ];
        let mut hosts = local_hosts.clone();
        hosts.extend(extra_hosts);
        Ok(Self {
            token: token.map(Arc::from),
            loopback,
            local_hosts,
            hosts,
        })
    }

    pub fn from_config(bind: IpAddr) -> anyhow::Result<Self> {
        crate::config::check_config_file()?;
        Self::new(
            bind,
            crate::config::auth_token(),
            crate::config::allowed_hosts(),
        )
    }

    pub fn is_loopback(&self) -> bool {
        self.loopback
    }
}

pub fn guard(router: Router, policy: InboundPolicy) -> Router {
    router.layer(axum::middleware::from_fn_with_state(
        Arc::new(policy),
        check_request,
    ))
}

async fn check_request(
    State(policy): State<Arc<InboundPolicy>>,
    mut request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }
    let monitor = request.uri().path() == "/monitor";
    let hosts = if monitor {
        &policy.local_hosts
    } else {
        &policy.hosts
    };
    if (policy.loopback || monitor) && !host_allowed(hosts, &request) {
        return json_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Request must carry exactly one Host header naming this proxy",
        );
    }
    if request.headers().contains_key(ORIGIN) {
        return json_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Requests with an Origin header are not accepted",
        );
    }
    if [AUTHORIZATION.as_str(), PROXY_KEY_HEADER]
        .iter()
        .any(|name| request.headers().get_all(*name).iter().nth(1).is_some())
    {
        return json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Request must carry at most one Authorization and one x-duolane-key header",
        );
    }
    if let Some(token) = policy.token.as_deref() {
        let key_ok = request
            .headers()
            .get(PROXY_KEY_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| constant_time_eq(value.trim(), token));
        let bearer_ok =
            bearer(request.headers()).is_some_and(|value| constant_time_eq(value, token));
        if !key_ok && !bearer_ok {
            return json_error(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                format!(
                    "missing or invalid proxy token; send `{PROXY_KEY_HEADER}: <token>` or `Authorization: Bearer <token>`"
                ),
            );
        }
        if bearer_ok {
            request.headers_mut().remove(AUTHORIZATION);
        }
    }
    let media_types = required_media_types(request.method(), request.uri().path());
    if !media_types.is_empty() && !media_type_allowed(request.headers(), media_types) {
        return json_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request_error",
            format!("Request content type must be {}", media_types.join(" or ")),
        );
    }
    next.run(request).await
}

fn host_allowed(hosts: &[String], request: &Request) -> bool {
    let mut values = request.headers().get_all(HOST).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    let allowed = |authority: &str| {
        authority_host(authority).is_some_and(|host| {
            hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host))
        })
    };
    value.to_str().is_ok_and(allowed)
        && request
            .uri()
            .authority()
            .is_none_or(|authority| allowed(authority.as_str()))
}

fn authority_host(authority: &str) -> Option<&str> {
    let (host, port) = match authority.find(']') {
        Some(end) if authority.starts_with('[') => authority.split_at(end + 1),
        _ => authority.split_at(authority.find(':').unwrap_or(authority.len())),
    };
    let port_ok = port.is_empty()
        || port.strip_prefix(':').is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        });
    (port_ok && !host.is_empty() && !host.ends_with('.') && !host.contains('@')).then_some(host)
}

fn valid_extra_host(host: &str) -> bool {
    match host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        Some(ip) => ip.parse::<std::net::Ipv6Addr>().is_ok(),
        None => {
            !host.is_empty()
                && !host.ends_with('.')
                && host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        }
    }
}

fn required_media_types(method: &Method, path: &str) -> &'static [&'static str] {
    if method != Method::POST {
        return &[];
    }
    match path {
        "/v1/messages"
        | "/v1/messages/count_tokens"
        | "/v1/responses"
        | "/v1/chat/completions"
        | "/v1/images/generations" => &[JSON],
        "/v1/audio/transcriptions" => &[MULTIPART],
        "/v1/images/edits" => &[JSON, MULTIPART],
        _ => &[],
    }
}

fn media_type_allowed(headers: &HeaderMap, allowed: &[&str]) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| {
            allowed
                .iter()
                .any(|allowed| media.trim().eq_ignore_ascii_case(allowed))
        })
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (scheme, token) = headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .trim()
        .split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}
