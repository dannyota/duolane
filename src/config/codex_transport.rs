use std::collections::HashMap;

use crate::paths;

use super::read_file_config;

const REQUEST_COMPRESSION_ENV: &str = "DUOLANE_CODEX_REQUEST_COMPRESSION";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexTransport {
    Http,
    WebSocket,
    Auto,
}

impl CodexTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            CodexTransport::Http => "http",
            CodexTransport::WebSocket => "websocket",
            CodexTransport::Auto => "auto",
        }
    }
}

fn parse_codex_transport(raw: &str) -> Option<CodexTransport> {
    match raw {
        "http" => Some(CodexTransport::Http),
        "websocket" => Some(CodexTransport::WebSocket),
        "auto" => Some(CodexTransport::Auto),
        _ => None,
    }
}

pub fn codex_transport() -> CodexTransport {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(raw) = env.get("DUOLANE_CODEX_TRANSPORT")
        && let Some(transport) = parse_codex_transport(raw)
    {
        return transport;
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(transport) = codex.transport.as_deref().and_then(parse_codex_transport)
    {
        return transport;
    }
    CodexTransport::Auto
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexRequestCompression {
    Off,
    Gzip,
}

impl CodexRequestCompression {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Gzip => "gzip",
        }
    }
}

fn parse_codex_request_compression(raw: &str) -> Option<CodexRequestCompression> {
    match raw {
        "off" => Some(CodexRequestCompression::Off),
        "gzip" => Some(CodexRequestCompression::Gzip),
        _ => None,
    }
}

pub fn codex_request_compression() -> CodexRequestCompression {
    std::env::var(REQUEST_COMPRESSION_ENV)
        .ok()
        .as_deref()
        .and_then(parse_codex_request_compression)
        .or_else(|| {
            read_file_config(&paths::config_dir())?
                .codex?
                .request_compression
                .as_deref()
                .and_then(parse_codex_request_compression)
        })
        .unwrap_or(CodexRequestCompression::Off)
}

const AUTO_WS_MAX_BYTES_ENV: &str = "DUOLANE_CODEX_AUTO_WS_MAX_BYTES";
pub const CODEX_DEFAULT_AUTO_WS_MAX_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn parse_auto_ws_file_value(value: &serde_json::Value) -> Option<usize> {
    value.as_u64().and_then(|value| usize::try_from(value).ok())
}

pub fn codex_auto_ws_max_bytes() -> usize {
    std::env::var(AUTO_WS_MAX_BYTES_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .or_else(|| {
            let codex = read_file_config(&paths::config_dir())?.codex?;
            codex
                .auto_websocket_max_bytes
                .as_ref()
                .and_then(parse_auto_ws_file_value)
        })
        .unwrap_or(CODEX_DEFAULT_AUTO_WS_MAX_BYTES)
}

pub(super) fn auto_ws_max_bytes_env_source(env: &HashMap<String, String>) -> Option<String> {
    env.get(AUTO_WS_MAX_BYTES_ENV)
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .map(|_| "codex.autoWebSocketMaxBytes (env)".to_string())
}

pub const CODEX_DEFAULT_HEADER_TIMEOUT_MS: u64 = 300_000;
pub const CODEX_MIN_HEADER_TIMEOUT_MS: u64 = 1_000;

pub fn codex_header_timeout_ms() -> u64 {
    let env: HashMap<_, _> = std::env::vars().collect();
    if let Some(ms) = env
        .get("DUOLANE_CODEX_HEADER_TIMEOUT_MS")
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms >= CODEX_MIN_HEADER_TIMEOUT_MS)
    {
        return ms;
    }
    let config_dir = paths::config_dir();
    if let Some(file) = read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(ms) = codex
            .header_timeout_ms
            .filter(|ms| *ms >= CODEX_MIN_HEADER_TIMEOUT_MS)
    {
        return ms;
    }
    CODEX_DEFAULT_HEADER_TIMEOUT_MS
}

pub(super) fn compression_env_source(env: &HashMap<String, String>) -> Option<String> {
    env.get(REQUEST_COMPRESSION_ENV)
        .and_then(|raw| parse_codex_request_compression(raw))
        .map(|_| "codex.requestCompression (env)".to_string())
}

pub(super) fn compression_file_source(raw: Option<&str>) -> Option<String> {
    raw.and_then(parse_codex_request_compression)
        .map(|compression| format!("codex.requestCompression: {}", compression.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_codex_transport_variants() {
        assert_eq!(parse_codex_transport("http"), Some(CodexTransport::Http));
        assert_eq!(
            parse_codex_transport("websocket"),
            Some(CodexTransport::WebSocket)
        );
        assert_eq!(parse_codex_transport("auto"), Some(CodexTransport::Auto));
        assert_eq!(parse_codex_transport(""), None);
        assert_eq!(parse_codex_transport("HTTP"), None);
        assert_eq!(parse_codex_transport("ws"), None);
    }

    #[test]
    fn codex_transport_as_str() {
        assert_eq!(CodexTransport::Http.as_str(), "http");
        assert_eq!(CodexTransport::WebSocket.as_str(), "websocket");
        assert_eq!(CodexTransport::Auto.as_str(), "auto");
    }
}
