use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::MessagesRequest;
use crate::logging::create_logger;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::providers::codex::translate::reasoning_signature::{
    is_codex_signature, is_proxy_reasoning_signature,
};
use crate::providers::translate_shared::wrap_reasoning;
use crate::registry::ANTHROPIC_STYLE_ALIASES;

mod usage;

const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const UNPARSEABLE_BODY: &str =
    "the Claude route accepts only a JSON object with unique keys and a messages array";
const UNPARSEABLE_MESSAGE: &str = "the Claude route accepts only messages that are JSON objects with unique keys, a string role, and string or array content";
const UNPARSEABLE_BLOCK: &str = "the Claude route accepts only content blocks that are JSON objects with unique keys and string type, thinking, and signature fields";
const CODEX_SIGNATURE_LEFT: &str =
    "a content block still carries a Codex reasoning signature, which must not reach Anthropic";
const UNREWRITABLE_BLOCK: &str = "the Claude route could not rewrite a content block";

struct ObjectFields<'a>(HashMap<String, &'a RawValue>);

impl<'de> Deserialize<'de> for ObjectFields<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(UniqueKeys)
    }
}

struct UniqueKeys;

impl<'de> Visitor<'de> for UniqueKeys {
    type Value = ObjectFields<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON object with unique keys")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut fields: HashMap<String, &'de RawValue> = HashMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if fields.insert(key, map.next_value()?).is_some() {
                return Err(A::Error::custom("duplicate key"));
            }
        }
        Ok(ObjectFields(fields))
    }
}

impl<'a> ObjectFields<'a> {
    fn parse(json: &'a str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }

    fn raw(&self, key: &str) -> Option<&'a RawValue> {
        self.0.get(key).copied()
    }

    fn string(&self, key: &str) -> Option<Option<String>> {
        match self.raw(key) {
            Some(raw) => serde_json::from_str(raw.get()).ok(),
            None => Some(None),
        }
    }
}

#[derive(Serialize)]
struct TextBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

struct ScannedBlock<'a> {
    raw: &'a RawValue,
    kind: Option<String>,
    thinking: Option<String>,
    signature: Option<String>,
}

struct ScannedMessage<'a> {
    assistant: bool,
    blocks: Vec<ScannedBlock<'a>>,
}

struct ScannedRequest<'a> {
    tools: Option<&'a RawValue>,
    messages: Vec<ScannedMessage<'a>>,
}

enum Conversion {
    Keep,
    Text(String),
    Remove,
}

struct Edit {
    start: usize,
    end: usize,
    replacement: Vec<u8>,
}

fn sanitize_anthropic_request(raw: &[u8], req_id: &str) -> Result<Option<Vec<u8>>, &'static str> {
    let body = std::str::from_utf8(raw).map_err(|_| UNPARSEABLE_BODY)?;
    let request = scan_request(body)?;

    detect_hosted_web_search_regression(request.tools, &request.messages, req_id);

    let mut edits = Vec::new();
    for message in &request.messages {
        let conversions = message_conversions(message);
        let codex_signature_left =
            message
                .blocks
                .iter()
                .zip(&conversions)
                .any(|(block, conversion)| {
                    matches!(conversion, Conversion::Keep)
                        && block.signature.as_deref().is_some_and(is_codex_signature)
                });
        if codex_signature_left {
            return Err(CODEX_SIGNATURE_LEFT);
        }
        plan_message_edits(body, &message.blocks, &conversions, &mut edits)
            .ok_or(UNREWRITABLE_BLOCK)?;
    }
    if edits.is_empty() {
        return Ok(None);
    }
    splice(raw, &edits).map(Some).ok_or(UNREWRITABLE_BLOCK)
}

fn scan_request(body: &str) -> Result<ScannedRequest<'_>, &'static str> {
    let fields = ObjectFields::parse(body).ok_or(UNPARSEABLE_BODY)?;
    let messages: Vec<&RawValue> = fields
        .raw("messages")
        .and_then(|raw| serde_json::from_str(raw.get()).ok())
        .ok_or(UNPARSEABLE_BODY)?;
    Ok(ScannedRequest {
        tools: fields.raw("tools"),
        messages: messages
            .into_iter()
            .map(scan_message)
            .collect::<Result<_, _>>()?,
    })
}

fn scan_message(raw: &RawValue) -> Result<ScannedMessage<'_>, &'static str> {
    let fields = ObjectFields::parse(raw.get()).ok_or(UNPARSEABLE_MESSAGE)?;
    let role = fields.string("role").ok_or(UNPARSEABLE_MESSAGE)?;
    let content = fields.raw("content").ok_or(UNPARSEABLE_MESSAGE)?;
    let blocks: Vec<&RawValue> = if content.get().starts_with('"') {
        Vec::new()
    } else {
        serde_json::from_str(content.get()).map_err(|_| UNPARSEABLE_MESSAGE)?
    };
    Ok(ScannedMessage {
        assistant: role.as_deref() == Some("assistant"),
        blocks: blocks
            .into_iter()
            .map(scan_block)
            .collect::<Result<_, _>>()?,
    })
}

fn scan_block(raw: &RawValue) -> Result<ScannedBlock<'_>, &'static str> {
    let fields = ObjectFields::parse(raw.get()).ok_or(UNPARSEABLE_BLOCK)?;
    let string = |key| fields.string(key).ok_or(UNPARSEABLE_BLOCK);
    Ok(ScannedBlock {
        raw,
        kind: string("type")?,
        thinking: string("thinking")?,
        signature: string("signature")?,
    })
}

fn anthropic_conversion(block: &ScannedBlock) -> Conversion {
    if block.kind.as_deref() != Some("thinking") {
        return Conversion::Keep;
    }
    let signature = block.signature.as_deref().unwrap_or_default();
    if !signature.is_empty() && !is_proxy_reasoning_signature(signature) {
        return Conversion::Keep;
    }
    let thinking = block.thinking.as_deref().unwrap_or_default();
    if thinking.is_empty() {
        Conversion::Remove
    } else {
        Conversion::Text(wrap_reasoning(thinking))
    }
}

fn message_conversions(message: &ScannedMessage) -> Vec<Conversion> {
    if !message.assistant {
        return message.blocks.iter().map(|_| Conversion::Keep).collect();
    }
    let mut conversions: Vec<Conversion> =
        message.blocks.iter().map(anthropic_conversion).collect();
    if !conversions.is_empty()
        && conversions
            .iter()
            .all(|conversion| matches!(conversion, Conversion::Remove))
    {
        conversions[0] = Conversion::Text(wrap_reasoning(""));
    }
    conversions
}

fn plan_message_edits(
    body: &str,
    blocks: &[ScannedBlock],
    conversions: &[Conversion],
    edits: &mut Vec<Edit>,
) -> Option<()> {
    if conversions
        .iter()
        .all(|conversion| matches!(conversion, Conversion::Keep))
    {
        return Some(());
    }
    let spans = blocks
        .iter()
        .map(|block| span(body, block.raw))
        .collect::<Option<Vec<_>>>()?;
    let mut index = 0;
    while index < spans.len() {
        let (start, end) = spans[index];
        match conversions.get(index)? {
            Conversion::Keep => {}
            Conversion::Text(text) => edits.push(Edit {
                start,
                end,
                replacement: serde_json::to_vec(&TextBlock { kind: "text", text }).ok()?,
            }),
            Conversion::Remove => {
                let last = (index..spans.len())
                    .take_while(|&next| matches!(conversions.get(next), Some(Conversion::Remove)))
                    .last()?;
                let (start, end) = match (spans.get(last + 1), index.checked_sub(1)) {
                    (Some(&(next_start, _)), _) => (start, next_start),
                    (None, Some(previous)) => (spans[previous].1, spans[last].1),
                    (None, None) => return None,
                };
                edits.push(Edit {
                    start,
                    end,
                    replacement: Vec::new(),
                });
                index = last;
            }
        }
        index += 1;
    }
    Some(())
}

fn span(body: &str, raw: &RawValue) -> Option<(usize, usize)> {
    let start = raw
        .get()
        .as_ptr()
        .addr()
        .checked_sub(body.as_ptr().addr())?;
    let end = start.checked_add(raw.get().len())?;
    body.get(start..end).map(|_| (start, end))
}

fn splice(raw: &[u8], edits: &[Edit]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len());
    let mut cursor = 0;
    for edit in edits {
        out.extend_from_slice(raw.get(cursor..edit.start)?);
        out.extend_from_slice(&edit.replacement);
        cursor = edit.end;
    }
    out.extend_from_slice(raw.get(cursor..)?);
    Some(out)
}

fn detect_hosted_web_search_regression(
    tools: Option<&RawValue>,
    messages: &[ScannedMessage],
    req_id: &str,
) {
    let has_assistant_history = messages.iter().any(|message| message.assistant);
    let tools: Vec<&RawValue> = tools
        .and_then(|raw| serde_json::from_str(raw.get()).ok())
        .unwrap_or_default();
    let hosted_tool = tools.into_iter().any(|tool| {
        ObjectFields::parse(tool.get())
            .and_then(|fields| fields.string("type"))
            .flatten()
            .as_deref()
            == Some("web_search_20250305")
    });
    let reconstructed_block = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .any(|block| {
            matches!(
                block.kind.as_deref(),
                Some("server_tool_use") | Some("web_search_tool_result")
            )
        });

    if (hosted_tool && has_assistant_history) || reconstructed_block {
        let mut fields = serde_json::Map::new();
        fields.insert("reqId".into(), Value::String(req_id.to_string()));
        fields.insert("hostedWebSearchTool".into(), Value::Bool(hosted_tool));
        fields.insert(
            "reconstructedSearchBlock".into(),
            Value::Bool(reconstructed_block),
        );
        create_logger("anthropic").warn("hosted_web_search_in_history", Some(fields));
    }
}

fn is_stripped_request_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "accept-encoding"
            | crate::inbound_auth::PROXY_KEY_HEADER
    )
}

fn is_stripped_response_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "content-encoding"
    )
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    header_timeout: Duration,
    idle_timeout: Duration,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self::with_timeouts(
            crate::config::anthropic_base_url(),
            RESPONSE_HEADER_TIMEOUT,
            BODY_IDLE_TIMEOUT,
        )
    }

    fn with_timeouts(base_url: String, header_timeout: Duration, idle_timeout: Duration) -> Self {
        let mut builder = crate::tls::client_builder()
            .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        if url::Url::parse(&base_url).is_ok_and(|url| crate::upstream_url::is_loopback_host(&url)) {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .expect("failed to build anthropic passthrough client");
        Self {
            client,
            base_url,
            header_timeout,
            idle_timeout,
        }
    }

    async fn relay(&self, ctx: RequestContext, count_tokens: bool) -> Response {
        let RequestContext {
            req_id,
            monitor,
            passthrough,
            ..
        } = ctx;
        let Some(passthrough) = passthrough else {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "anthropic passthrough is missing the original request",
            );
        };

        let url = format!("{}{}", self.base_url, passthrough.path_and_query);
        let mut headers = axum::http::HeaderMap::with_capacity(passthrough.headers.len());
        for (name, value) in passthrough.headers.iter() {
            if is_stripped_request_header(name.as_str()) {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }

        let outgoing = match sanitize_anthropic_request(&passthrough.raw_body, &req_id) {
            Ok(Some(bytes)) => axum::body::Bytes::from(bytes),
            Ok(None) => passthrough.raw_body,
            Err(cause) => {
                return json_error(StatusCode::BAD_REQUEST, "invalid_request_error", cause);
            }
        };

        if let Some(monitor) = monitor.as_ref() {
            monitor.upstream_started(&req_id);
        }

        let upstream = tokio::time::timeout(
            self.header_timeout,
            self.client
                .post(&url)
                .headers(headers)
                .body(outgoing)
                .send(),
        )
        .await;

        match upstream {
            Ok(Ok(upstream)) => {
                let status = upstream.status();
                let mut out_headers =
                    axum::http::HeaderMap::with_capacity(upstream.headers().len());
                for (name, value) in upstream.headers() {
                    if is_stripped_response_header(name.as_str()) {
                        continue;
                    }
                    out_headers.append(name.clone(), value.clone());
                }
                let content_type = upstream
                    .headers()
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                let is_sse = content_type
                    .split(';')
                    .next()
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
                let observe = status.is_success() && !count_tokens && monitor.is_some();
                let body = idle_limited(upstream.bytes_stream(), self.idle_timeout);
                let body = usage::observe(body, monitor.filter(|_| observe), req_id, is_sse);
                let mut response = Response::new(Body::from_stream(body));
                *response.status_mut() = status;
                *response.headers_mut() = out_headers;
                response
            }
            Ok(Err(err)) => json_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("anthropic upstream request failed: {err}"),
            ),
            Err(_) => json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "api_error",
                "anthropic upstream sent no response headers in time",
            ),
        }
    }
}

fn idle_limited(
    stream: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
    idle_timeout: Duration,
) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    futures_util::stream::unfold(Some(Box::pin(stream)), move |stream| async move {
        let mut stream = stream?;
        match tokio::time::timeout(idle_timeout, stream.next()).await {
            Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(stream))),
            Ok(Some(Err(error))) => Some((Err(std::io::Error::other(error)), None)),
            Ok(None) => None,
            Err(_) => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "anthropic upstream stream was idle too long",
                )),
                None,
            )),
        }
    })
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn supported_models(&self) -> Vec<String> {
        ANTHROPIC_STYLE_ALIASES
            .iter()
            .map(|alias| (*alias).to_string())
            .collect()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &ANTHROPIC_CLI
    }

    async fn handle_messages(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        self.relay(ctx, false).await
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        self.relay(ctx, true).await
    }
}

pub struct AnthropicCli;
pub static ANTHROPIC_CLI: AnthropicCli = AnthropicCli;

impl CliHandlers for AnthropicCli {
    fn login(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "The Claude backend reuses Claude Code's own login; no separate authentication is required"
        )
    }
    fn device(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "The Claude backend reuses Claude Code's own login; no separate authentication is required"
        )
    }
    fn status(&self) -> anyhow::Result<()> {
        println!("Claude backend: transparent passthrough to api.anthropic.com");
        println!("Auth: forwarded from Claude Code (no proxy credentials stored)");
        Ok(())
    }
    fn logout(&self) -> anyhow::Result<()> {
        println!("Claude backend stores no credentials; nothing to remove");
        Ok(())
    }
}

#[cfg(test)]
mod usage_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_hop_by_hop_and_encoding_from_request() {
        assert!(is_stripped_request_header("host"));
        assert!(is_stripped_request_header("content-length"));
        assert!(is_stripped_request_header("accept-encoding"));
        assert!(is_stripped_request_header("connection"));
        assert!(is_stripped_request_header(
            crate::inbound_auth::PROXY_KEY_HEADER
        ));
        assert!(!is_stripped_request_header("authorization"));
        assert!(!is_stripped_request_header("anthropic-beta"));
        assert!(!is_stripped_request_header("anthropic-version"));
        assert!(!is_stripped_request_header("content-type"));
    }

    #[test]
    fn strips_framing_from_response() {
        assert!(is_stripped_response_header("content-length"));
        assert!(is_stripped_response_header("content-encoding"));
        assert!(is_stripped_response_header("transfer-encoding"));
        assert!(!is_stripped_response_header("content-type"));
        assert!(!is_stripped_response_header("request-id"));
        assert!(!is_stripped_response_header(
            "anthropic-ratelimit-requests-remaining"
        ));
    }

    #[test]
    fn provider_reports_name_and_models() {
        let provider = AnthropicProvider::new();
        assert_eq!(provider.name(), "anthropic");
        assert!(provider.supported_models().iter().any(|m| m == "opus"));
    }

    const SAME_ACCOUNT: &str = "duolane:codex:v1:def0b17f603285ef:cnNfMQ:gAAAAsecret";
    const OTHER_ACCOUNT: &str = "duolane:codex:v1:aa6eb7ee5d514507:cnNfMQ:gAAAAsecret";

    fn reasoning_text(text: &str) -> Value {
        serde_json::json!({"type": "text", "text": wrap_reasoning(text)})
    }

    fn thinking(text: &str, signature: Option<&str>) -> Value {
        match signature {
            Some(signature) => {
                serde_json::json!({"type": "thinking", "thinking": text, "signature": signature})
            }
            None => serde_json::json!({"type": "thinking", "thinking": text}),
        }
    }

    #[test]
    fn thinking_reaches_anthropic_as_the_reasoning_table_says() {
        let answer = serde_json::json!({"type": "text", "text": "391"});
        let cases = [
            (
                "anthropic signature",
                vec![thinking("opus", Some("EqQBsig"))],
                None,
            ),
            (
                "anthropic signature, empty",
                vec![thinking("", Some("EqQBsig"))],
                None,
            ),
            (
                "codex signature, same account",
                vec![thinking("plan", Some(SAME_ACCOUNT)), answer.clone()],
                Some(vec![reasoning_text("plan"), answer.clone()]),
            ),
            (
                "codex signature, other account",
                vec![thinking("plan", Some(OTHER_ACCOUNT))],
                Some(vec![reasoning_text("plan")]),
            ),
            (
                "empty signature",
                vec![thinking("plan", Some(""))],
                Some(vec![reasoning_text("plan")]),
            ),
            (
                "missing signature",
                vec![thinking("plan", None)],
                Some(vec![reasoning_text("plan")]),
            ),
            (
                "empty thinking with codex signature is removed",
                vec![thinking("", Some(SAME_ACCOUNT)), answer.clone()],
                Some(vec![answer.clone()]),
            ),
            (
                "empty thinking without signature is removed",
                vec![answer.clone(), thinking("", None)],
                Some(vec![answer.clone()]),
            ),
            (
                "emptied message keeps one empty reasoning text",
                vec![thinking("", Some(OTHER_ACCOUNT)), thinking("", Some(""))],
                Some(vec![reasoning_text("")]),
            ),
        ];
        for (name, blocks, expected) in cases {
            let body = serde_json::json!({"messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": blocks},
            ]});
            let raw = serde_json::to_vec(&body).unwrap();
            let out = sanitize_anthropic_request(&raw, "req").unwrap();
            let Some(expected) = expected else {
                assert!(out.is_none(), "{name}");
                continue;
            };
            let out = String::from_utf8(out.expect(name)).unwrap();
            assert!(!out.contains("duolane:"), "{name}: {out}");
            let doc: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(
                doc["messages"][1]["content"],
                Value::from(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn claude_route_rewrites_only_the_converted_block_bytes() {
        let head = "{ \"model\":\"claude-opus-4-1\",  \"max_tokens\" :64,\n \"messages\":[{\"content\":\"hi\",\"role\":\"user\"},{\"content\" : [ ";
        let converted =
            format!(r#"{{"signature":"{SAME_ACCOUNT}", "thinking":"plané","type":"thinking"}}"#);
        let between = r#" ,  {"type":"text", "text":"a"}"#;
        let removed = r#",{"thinking":"","type":"thinking"}"#;
        let tail = r#" ], "role":"assistant"}],"system":[{"text":"s","type":"text","cache_control":{"type":"ephemeral"}}] }"#;
        let raw = format!("{head}{converted}{between}{removed}{tail}");
        let replacement = "{\"type\":\"text\",\"text\":\"<previous_reasoning>\\nplan\u{e9}\\n</previous_reasoning>\"}";

        let out = sanitize_anthropic_request(raw.as_bytes(), "req")
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{head}{replacement}{between}{tail}")
        );

        let untouched = format!(
            "{head}{}{between}{tail}",
            converted.replace(SAME_ACCOUNT, "EqQBsig")
        );
        assert_eq!(
            sanitize_anthropic_request(untouched.as_bytes(), "req"),
            Ok(None)
        );
    }

    #[test]
    fn rewrite_is_deterministic() {
        let body = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "same", "signature": ""}
                ]}
            ]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        let a = sanitize_anthropic_request(&raw, "r").unwrap().unwrap();
        let b = sanitize_anthropic_request(&raw, "r").unwrap().unwrap();
        assert_eq!(
            a, b,
            "rewrite must be byte-stable to preserve the cache prefix"
        );
    }

    #[test]
    fn unparseable_or_duplicate_key_bodies_are_refused() {
        for body in [
            r#"not json"#,
            r#"{"messages":[],"messages":[]}"#,
            r#"{"messages":[{"role":"user","r\u006fle":"assistant","content":"x"}]}"#,
            r#"{"messages":[{"role":"assistant","content":[{"type":"text","text":"a","text":"b"}]}]}"#,
            r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","signature":7}]}]}"#,
        ] {
            assert!(
                sanitize_anthropic_request(body.as_bytes(), "req").is_err(),
                "{body}"
            );
        }
    }

    async fn stalled_upstream(head: &'static [u8]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            socket.write_all(head).await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        format!("http://{address}")
    }

    fn passthrough_context() -> RequestContext {
        RequestContext {
            req_id: "req-timeout".into(),
            session_id: None,
            traffic: None,
            monitor: None,
            passthrough: Some(crate::provider::Passthrough {
                raw_body: Bytes::from_static(br#"{"messages":[]}"#),
                headers: axum::http::HeaderMap::new(),
                path_and_query: "/v1/messages".into(),
            }),
        }
    }

    #[tokio::test]
    async fn claude_route_times_out_waiting_for_headers_and_between_body_bytes() {
        let short = Duration::from_millis(200);
        let silent = stalled_upstream(b"").await;
        let provider = AnthropicProvider::with_timeouts(silent, short, Duration::from_secs(60));
        let started = std::time::Instant::now();
        let response = provider.relay(passthrough_context(), false).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(started.elapsed() < Duration::from_secs(10));

        let stalling = stalled_upstream(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n",
        )
        .await;
        let provider = AnthropicProvider::with_timeouts(stalling, Duration::from_secs(60), short);
        let response = provider.relay(passthrough_context(), false).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        assert_eq!(
            body.next().await.unwrap().unwrap(),
            Bytes::from_static(b"hello")
        );
        let idle = tokio::time::timeout(Duration::from_secs(10), body.next())
            .await
            .expect("idle body must end before the test deadline");
        assert!(matches!(idle, Some(Err(_))), "{idle:?}");
    }
}
