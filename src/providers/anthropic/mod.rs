use async_trait::async_trait;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::MessagesRequest;
use crate::logging::create_logger;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::providers::codex::translate::reasoning_signature::{
    contains_codex_signature, is_proxy_reasoning_signature,
};
use crate::providers::translate_shared::wrap_reasoning;
use crate::registry::ANTHROPIC_STYLE_ALIASES;

#[derive(Deserialize)]
struct RequestFields<'a> {
    #[serde(borrow)]
    messages: Option<&'a RawValue>,
    #[serde(borrow)]
    tools: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct MessageFields<'a> {
    #[serde(borrow)]
    role: Option<&'a RawValue>,
    #[serde(borrow)]
    content: Option<&'a RawValue>,
}

#[derive(Default, Deserialize)]
struct BlockFields<'a> {
    #[serde(borrow, rename = "type")]
    kind: Option<&'a RawValue>,
    #[serde(borrow)]
    thinking: Option<&'a RawValue>,
    #[serde(borrow)]
    signature: Option<&'a RawValue>,
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
    fields: BlockFields<'a>,
}

struct ScannedMessage<'a> {
    assistant: bool,
    blocks: Vec<ScannedBlock<'a>>,
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

fn sanitize_anthropic_request(raw: &[u8], req_id: &str) -> Option<Vec<u8>> {
    let body = std::str::from_utf8(raw).ok()?;
    let request: RequestFields = serde_json::from_str(body).ok()?;
    let messages: Vec<ScannedMessage> = elements(request.messages)
        .into_iter()
        .map(scan_message)
        .collect();

    detect_hosted_web_search_regression(request.tools, &messages, req_id);

    let mut edits = Vec::new();
    for message in messages.iter().filter(|message| message.assistant) {
        plan_message_edits(body, &message.blocks, &mut edits)?;
    }
    if edits.is_empty() {
        return None;
    }
    splice(raw, &edits)
}

fn elements(raw: Option<&RawValue>) -> Vec<&RawValue> {
    raw.and_then(|raw| serde_json::from_str(raw.get()).ok())
        .unwrap_or_default()
}

fn json_string(raw: Option<&RawValue>) -> Option<String> {
    serde_json::from_str(raw?.get()).ok()
}

fn scan_message(raw: &RawValue) -> ScannedMessage<'_> {
    let fields: Option<MessageFields> = serde_json::from_str(raw.get()).ok();
    let role = fields.as_ref().and_then(|fields| json_string(fields.role));
    let blocks = elements(fields.and_then(|fields| fields.content))
        .into_iter()
        .map(|raw| {
            let fields: BlockFields = serde_json::from_str(raw.get()).unwrap_or_default();
            ScannedBlock {
                raw,
                kind: json_string(fields.kind),
                fields,
            }
        })
        .collect();
    ScannedMessage {
        assistant: role.as_deref() == Some("assistant"),
        blocks,
    }
}

fn anthropic_conversion(block: &ScannedBlock) -> Conversion {
    if block.kind.as_deref() != Some("thinking") {
        return Conversion::Keep;
    }
    let signature = json_string(block.fields.signature).unwrap_or_default();
    if !signature.is_empty() && !is_proxy_reasoning_signature(&signature) {
        return Conversion::Keep;
    }
    let thinking = json_string(block.fields.thinking).unwrap_or_default();
    if thinking.is_empty() {
        Conversion::Remove
    } else {
        Conversion::Text(wrap_reasoning(&thinking))
    }
}

fn plan_message_edits(body: &str, blocks: &[ScannedBlock], edits: &mut Vec<Edit>) -> Option<()> {
    let mut conversions: Vec<Conversion> = blocks.iter().map(anthropic_conversion).collect();
    if conversions
        .iter()
        .all(|conversion| matches!(conversion, Conversion::Keep))
    {
        return Some(());
    }
    if conversions
        .iter()
        .all(|conversion| matches!(conversion, Conversion::Remove))
    {
        conversions[0] = Conversion::Text(wrap_reasoning(""));
    }
    let spans = blocks
        .iter()
        .map(|block| span(body, block.raw))
        .collect::<Option<Vec<_>>>()?;
    let mut index = 0;
    while index < spans.len() {
        let (start, end) = spans[index];
        match &conversions[index] {
            Conversion::Keep => {}
            Conversion::Text(text) => edits.push(Edit {
                start,
                end,
                replacement: serde_json::to_vec(&TextBlock { kind: "text", text }).ok()?,
            }),
            Conversion::Remove => {
                let last = (index..spans.len())
                    .take_while(|&next| matches!(conversions[next], Conversion::Remove))
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
    let hosted_tool = elements(tools).into_iter().any(|tool| {
        let fields: BlockFields = serde_json::from_str(tool.get()).unwrap_or_default();
        json_string(fields.kind).as_deref() == Some("web_search_20250305")
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
}

impl AnthropicProvider {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build anthropic passthrough client");
        Self {
            client,
            base_url: crate::config::anthropic_base_url(),
        }
    }

    async fn relay(&self, ctx: RequestContext) -> Response {
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
            Some(bytes) => axum::body::Bytes::from(bytes),
            None => passthrough.raw_body,
        };
        if contains_codex_signature(&outgoing) {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "request still carries encrypted Codex reasoning; refusing to forward it to Anthropic",
            );
        }

        if let Some(monitor) = monitor.as_ref() {
            monitor.upstream_started(&req_id);
        }

        let upstream = self
            .client
            .post(&url)
            .headers(headers)
            .body(outgoing)
            .send()
            .await;

        match upstream {
            Ok(upstream) => {
                let status = upstream.status();
                let mut out_headers =
                    axum::http::HeaderMap::with_capacity(upstream.headers().len());
                for (name, value) in upstream.headers() {
                    if is_stripped_response_header(name.as_str()) {
                        continue;
                    }
                    out_headers.append(name.clone(), value.clone());
                }
                let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
                *response.status_mut() = status;
                *response.headers_mut() = out_headers;
                response
            }
            Err(err) => json_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                format!("anthropic upstream request failed: {err}"),
            ),
        }
    }
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
        self.relay(ctx).await
    }

    async fn handle_count_tokens(&self, _body: MessagesRequest, ctx: RequestContext) -> Response {
        self.relay(ctx).await
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
            let out = sanitize_anthropic_request(&raw, "req");
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

        let out = sanitize_anthropic_request(raw.as_bytes(), "req").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{head}{replacement}{between}{tail}")
        );

        let untouched = format!(
            "{head}{}{between}{tail}",
            converted.replace(SAME_ACCOUNT, "EqQBsig")
        );
        assert!(sanitize_anthropic_request(untouched.as_bytes(), "req").is_none());
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
        let a = sanitize_anthropic_request(&raw, "r").unwrap();
        let b = sanitize_anthropic_request(&raw, "r").unwrap();
        assert_eq!(
            a, b,
            "rewrite must be byte-stable to preserve the cache prefix"
        );
    }

    #[test]
    fn non_json_body_is_forwarded_verbatim() {
        assert!(sanitize_anthropic_request(b"not json", "req5").is_none());
    }
}
