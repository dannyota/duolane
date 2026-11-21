use std::collections::HashMap;

use crate::anthropic::sse::encode_sse_event;
use crate::providers::codex::count_tokens::count_translated_tokens;
use crate::providers::codex::events::{classify_event_failure, is_function_call_item};
use crate::traffic::TrafficCapture;

use super::reasoning_signature::ReasoningSigner;
use super::reducer::{CodexRateLimits, CodexUsage, parse_codex_rate_limits};
use super::request::ResponsesRequest;

pub(super) const BUFFERED_READ_REPAIR_TRAILING_WHITESPACE_BYTES: usize = 1_024;
pub(in crate::providers::codex) const BUFFERED_TOOL_MAX_ARGS_BYTES: usize = 5_000_000;

pub(super) enum LiveBlock {
    Text {
        index: Option<usize>,
        text: String,
        deferred: bool,
    },
    Tool {
        index: usize,
        call_id: String,
        name: String,
        args_accum: String,
        had_delta: bool,
        buffer_until_done: bool,
        emitted_args: bool,
    },
}

pub(super) struct LiveWebSearch {
    pub(super) index: Option<usize>,
    pub(super) result_index: Option<usize>,
    pub(super) id: String,
    pub(super) query: String,
}

#[derive(Clone)]
pub(super) struct LiveWebSearchResult {
    pub(super) title: String,
    pub(super) url: String,
}

#[derive(Clone, Copy)]
pub(super) struct LiveThinking {
    pub(super) output_index: usize,
    pub(super) anthropic_index: usize,
}

pub struct LiveStreamTranslator {
    message_id: String,
    model: String,
    message_started: bool,
    http_tool_commits: bool,
    pub(super) blocks_by_output_index: HashMap<usize, LiveBlock>,
    pub(super) item_id_to_output_index: HashMap<String, usize>,
    pub(super) anthropic_index: usize,
    pub(super) thinking: Option<LiveThinking>,
    pub(super) reasoning_by_output_index: ReasoningSigner,
    pub(super) saw_tool_use: bool,
    pub(super) web_search_requests: usize,
    pub(super) web_searches: Vec<LiveWebSearch>,
    pub(super) web_search_results: Vec<LiveWebSearchResult>,
    pub(super) deferred_text: Vec<(Option<usize>, String)>,
    pub(super) semantic_output_started: bool,
    estimated_input_tokens: u64,
    pub(super) finished: bool,
    pub(super) rate_limits: Option<CodexRateLimits>,
}

impl LiveStreamTranslator {
    pub fn new(message_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_estimated_input_tokens(message_id, model, 0)
    }

    pub fn with_estimated_input_tokens(
        message_id: impl Into<String>,
        model: impl Into<String>,
        estimated_input_tokens: u64,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            message_started: false,
            http_tool_commits: false,
            blocks_by_output_index: HashMap::new(),
            item_id_to_output_index: HashMap::new(),
            anthropic_index: 0,
            thinking: None,
            reasoning_by_output_index: ReasoningSigner::default(),
            saw_tool_use: false,
            web_search_requests: 0,
            web_searches: Vec::new(),
            web_search_results: Vec::new(),
            deferred_text: Vec::new(),
            semantic_output_started: false,
            estimated_input_tokens,
            finished: false,
            rate_limits: None,
        }
    }

    pub fn for_request(
        message_id: impl Into<String>,
        model: impl Into<String>,
        request: &ResponsesRequest,
    ) -> Self {
        Self {
            reasoning_by_output_index: ReasoningSigner::new(request.reasoning_account.clone()),
            ..Self::with_estimated_input_tokens(message_id, model, count_translated_tokens(request))
        }
    }

    pub(in crate::providers::codex) fn with_http_tool_commits(mut self, enabled: bool) -> Self {
        self.http_tool_commits = enabled;
        self
    }

    pub(super) fn next_content_index(&mut self) -> usize {
        let index = self.anthropic_index;
        self.anthropic_index += 1;
        index
    }

    pub(super) fn reserve_content_index(&mut self, deferred: bool) -> Option<usize> {
        (!deferred || !self.http_tool_commits).then(|| self.next_content_index())
    }

    pub fn accept(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
    ) -> Result<Vec<u8>, String> {
        if self.finished {
            return Ok(Vec::new());
        }

        let kind = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mut out = Vec::new();

        if let Some(failure) = classify_event_failure(payload) {
            return Err(failure.message);
        }

        match kind {
            "codex.rate_limits" => {
                self.rate_limits = parse_codex_rate_limits(payload).or(self.rate_limits.take());
                self.emit_ping(traffic, &mut out);
            }
            "keepalive" | "response.created" | "response.in_progress" => {
                self.emit_ping(traffic, &mut out);
            }
            "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed" => {}
            "response.output_item.added" => {
                self.output_item_added(payload, traffic, &mut out);
            }
            "response.reasoning_summary_part.added" => {
                let output_index = output_index(payload);
                if let Some(thinking) = self
                    .thinking
                    .filter(|thinking| thinking.output_index == output_index)
                {
                    self.emit(
                        traffic,
                        &mut out,
                        "content_block_delta",
                        &serde_json::json!({
                            "type": "content_block_delta",
                            "index": thinking.anthropic_index,
                            "delta": {"type": "thinking_delta", "thinking": "\n\n"}
                        }),
                    );
                }
            }
            "response.reasoning_summary_text.delta" => {
                self.reasoning_delta(payload, traffic, &mut out);
            }
            "response.output_text.delta" => {
                self.text_delta(payload, traffic, &mut out);
            }
            "response.output_text.annotation.added" => {
                self.web_search_annotation(payload);
            }
            "response.function_call_arguments.delta" => {
                self.tool_delta(payload, traffic, &mut out)?;
            }
            "response.function_call_arguments.done" => {
                self.tool_arguments_done(payload);
            }
            "response.output_item.done" => {
                if self.http_tool_commits && is_function_call_item(payload) {
                    self.commit_http_tool_call(payload, traffic, &mut out)?;
                } else {
                    self.output_item_done(payload, traffic, &mut out);
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.finish(payload, traffic, &mut out)?;
            }
            _ => {}
        }

        Ok(out)
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn has_semantic_output(&self) -> bool {
        self.semantic_output_started
    }

    pub fn ping_chunk(&mut self, traffic: Option<&TrafficCapture>) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.finished {
            self.emit_ping(traffic, &mut out);
        }
        out
    }

    pub fn error_chunk(
        &mut self,
        message: &str,
        error_type: &str,
        traffic: Option<&TrafficCapture>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.close_open_blocks(traffic, &mut out);
        self.ensure_message_start(traffic, &mut out);
        self.emit(
            traffic,
            &mut out,
            "error",
            &serde_json::json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "message": message,
                }
            }),
        );
        self.finished = true;
        out
    }

    pub(super) fn ensure_message_start(
        &mut self,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        self.emit(
            traffic,
            out,
            "message_start",
            &serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": self.estimated_input_tokens,
                        "output_tokens": 0
                    }
                }
            }),
        );
    }

    fn emit_ping(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        self.ensure_message_start(traffic, out);
        self.emit(traffic, out, "ping", &serde_json::json!({"type": "ping"}));
    }

    pub(super) fn emit(
        &self,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
        event: &str,
        data: &serde_json::Value,
    ) {
        if let Some(traffic) = traffic {
            traffic.write_json_event(
                "050-downstream-event",
                &serde_json::json!({
                    "event": event,
                    "data": data,
                }),
            );
        }
        out.extend_from_slice(&encode_sse_event(Some(event), &data.to_string()));
    }
}

pub(super) fn web_search_query(item: &serde_json::Value) -> String {
    let Some(action) = item.get("action") else {
        return String::new();
    };
    action
        .get("query")
        .and_then(|v| v.as_str())
        .or_else(|| {
            action
                .get("queries")
                .and_then(|v| v.as_array())
                .and_then(|queries| queries.iter().find_map(|query| query.as_str()))
        })
        .unwrap_or("")
        .to_string()
}

pub(super) fn output_index(payload: &serde_json::Value) -> usize {
    payload
        .get("output_index")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}

pub(super) fn parse_codex_usage(response: &serde_json::Value) -> CodexUsage {
    let usage = match response.get("usage") {
        Some(u) => u,
        None => return CodexUsage::default(),
    };
    CodexUsage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
        input_tokens_details_cached: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        output_tokens_details_reasoning: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
        rate_limits: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::anthropic::sse::parse_sse_events;

    use super::*;

    fn render(events: Vec<serde_json::Value>) -> String {
        render_with(LiveStreamTranslator::new("msg_1", "gpt-5.5"), events)
    }

    fn render_with(mut translator: LiveStreamTranslator, events: Vec<serde_json::Value>) -> String {
        let mut out = Vec::new();
        for event in events {
            out.extend(translator.accept(&event, None).unwrap());
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn emits_text_delta_before_terminal_event() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let out = translator
            .accept(
                &json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": "hello"
                }),
                None,
            )
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("message_start"));
        assert!(out.contains("content_block_start"));
        assert!(out.contains("text_delta"));
        assert!(out.contains("hello"));
        assert!(!out.contains("message_stop"));
        assert!(translator.has_semantic_output());
    }

    #[test]
    fn estimated_input_is_visible_at_start_and_provider_usage_is_exact_at_finish() {
        let mut translator =
            LiveStreamTranslator::with_estimated_input_tokens("msg_1", "gpt-5.5", 321);

        let started = translator
            .accept(
                &json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": "abcdefgh"
                }),
                None,
            )
            .unwrap();
        let started = parse_sse_events(&started)
            .into_iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|value| {
                value.get("type").and_then(serde_json::Value::as_str) == Some("message_start")
            })
            .unwrap();
        assert_eq!(
            started.pointer("/message/usage/input_tokens"),
            Some(&json!(321))
        );

        let finished = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_1",
                        "status": "completed",
                        "usage": {"input_tokens": 300, "output_tokens": 9}
                    }
                }),
                None,
            )
            .unwrap();
        let finished = parse_sse_events(&finished)
            .into_iter()
            .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
            .find(|value| {
                value.get("type").and_then(serde_json::Value::as_str) == Some("message_delta")
            })
            .unwrap();
        assert_eq!(finished.pointer("/usage/input_tokens"), Some(&json!(300)));
        assert_eq!(finished.pointer("/usage/output_tokens"), Some(&json!(9)));
    }

    #[test]
    fn finishes_text_stream() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": "msg_up"}
            }),
            json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "hello"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message"}
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "status": "completed", "incomplete_details": null, "usage": {"input_tokens": 2, "output_tokens": 1}}
            }),
        ]);
        assert!(out.contains("content_block_stop"));
        assert!(out.contains("message_delta"));
        assert!(out.contains(r#""stop_reason":"end_turn""#));
        assert!(out.contains("message_stop"));
    }

    #[test]
    fn terminal_only_completion_remains_non_semantic() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let out = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_1", "status": "completed", "incomplete_details": null, "usage": {}}
                }),
                None,
            )
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(r#""stop_reason":"end_turn""#));
        assert!(!out.contains(r#""stop_reason":"max_tokens""#));
        assert!(!translator.has_semantic_output());
    }

    #[test]
    fn max_output_tokens_incomplete_is_a_max_tokens_stop() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-luna");
        let out = translator
            .accept(
                &json!({
                    "type":"response.incomplete",
                    "response": {
                        "status":"incomplete",
                        "error":null,
                        "incomplete_details":{"reason":"max_output_tokens"},
                        "usage":{"input_tokens":2,"output_tokens":8}
                    }
                }),
                None,
            )
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(r#""stop_reason":"max_tokens""#));
        assert!(out.contains(r#""output_tokens":8"#));
        assert!(out.contains("event: message_stop"));
        assert!(translator.is_finished());
    }

    #[test]
    fn other_incomplete_reasons_fail_the_stream() {
        for reason in ["content_filter", "unknown"] {
            let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.6-luna");
            assert!(
                translator
                    .accept(
                        &json!({
                            "type":"response.incomplete",
                            "response": {
                                "status":"incomplete",
                                "incomplete_details":{"reason":reason}
                            }
                        }),
                        None,
                    )
                    .is_err(),
                "{reason}"
            );
        }
    }

    #[test]
    fn structural_tool_start_is_not_semantic_until_arguments_arrive() {
        let mut tool = LiveStreamTranslator::new("msg_tool", "gpt-5.5");
        tool.accept(
            &json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Read"}
            }),
            None,
        )
        .unwrap();
        assert!(!tool.has_semantic_output());
        tool.accept(
            &json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{}"
            }),
            None,
        )
        .unwrap();
        assert!(tool.has_semantic_output());

        let mut thinking = LiveStreamTranslator::new("msg_thinking", "gpt-5.5");
        thinking
            .accept(
                &json!({
                    "type": "response.reasoning_summary_text.delta",
                    "output_index": 0,
                    "delta": "plan"
                }),
                None,
            )
            .unwrap();
        assert!(thinking.has_semantic_output());

        let mut web_search = LiveStreamTranslator::new("msg_search", "gpt-5.5");
        web_search
            .accept(
                &json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {"type": "web_search_call", "id": "ws_1"}
                }),
                None,
            )
            .unwrap();
        assert!(!web_search.has_semantic_output());
        web_search
            .accept(
                &json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "type": "web_search_call",
                        "id": "ws_1",
                        "action": {"query": "duolane"}
                    }
                }),
                None,
            )
            .unwrap();
        assert!(web_search.has_semantic_output());
    }

    #[test]
    fn buffers_read_tool_args_until_done() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Read"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"file_path\":\"/tmp/a\",\"pages\":\"\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "Read",
                    "arguments": "{\"file_path\":\"/tmp/a\",\"pages\":\"\"}"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "usage": {}}
            }),
        ]);
        assert!(out.contains("tool_use"));
        assert!(out.contains("input_json_delta"));
        assert!(out.contains("/tmp/a"));
        assert!(!out.contains("pages"));
    }

    #[test]
    fn repairs_whitespace_stalled_read_args_as_tool_use_finish() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let mut out = Vec::new();
        out.extend(
            translator
                .accept(
                    &json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": {"type":"function_call","call_id":"call_1","name":"Read"}
                    }),
                    None,
                )
                .unwrap(),
        );
        out.extend(
            translator
                .accept(
                    &json!({
                        "type": "response.function_call_arguments.delta",
                        "output_index": 0,
                        "delta": format!("{{\"file_path\":\"/tmp/a\",\"pages\":\"\"{}", " ".repeat(1024))
                    }),
                    None,
                )
                .unwrap(),
        );
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains(r#""partial_json":"{\"file_path\":\"/tmp/a\"}""#));
        assert!(rendered.contains(r#""stop_reason":"tool_use""#));
        assert!(rendered.contains("message_stop"));
        assert!(translator.is_finished());
    }

    #[test]
    fn terminal_event_with_open_tool_call_fails_the_stream() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let mut out = Vec::new();
        for event in [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"function_call","call_id":"call_1","name":"Bash"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"command\":\"ls"
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }
        let error = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_1", "status": "completed", "usage": {}}
                }),
                None,
            )
            .unwrap_err();
        out.extend(translator.error_chunk(&error, "api_error", None));
        let rendered = String::from_utf8(out).unwrap();
        let tool_start = rendered.find(r#""type":"tool_use""#).unwrap();
        assert!(rendered.contains("event: error"), "{rendered}");
        assert!(!rendered.contains("event: message_delta"), "{rendered}");
        assert!(!rendered.contains("message_stop"), "{rendered}");
        assert!(
            !rendered[tool_start..].contains("event: content_block_stop"),
            "{rendered}"
        );
    }

    #[test]
    fn emits_web_search_results_from_citations_before_deferred_text() {
        let out = render(vec![
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "web_search_call", "id": "ws_1"}
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "action": {"query": "reasoning effort"}
                }
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {"type": "message", "id": "msg_up"}
            }),
            json!({
                "type": "response.output_text.delta",
                "output_index": 1,
                "delta": "See the official docs."
            }),
            json!({
                "type": "response.output_text.annotation.added",
                "annotation": {
                    "type": "url_citation",
                    "title": "Reasoning",
                    "url": "https://docs.example.test/guides/reasoning"
                }
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": {"type": "message"}
            }),
            json!({
                "type": "response.completed",
                "response": {"status": "completed", "usage": {}}
            }),
        ]);

        let tool = out.find("server_tool_use").unwrap();
        let result = out.find("web_search_tool_result").unwrap();
        let text = out.find("See the official docs.").unwrap();
        assert!(tool < result && result < text);
        assert!(out.contains("https://docs.example.test/guides/reasoning"));
        assert!(out.contains(r#""web_search_requests":1"#));
    }

    #[test]
    fn rate_limit_event_is_progress_telemetry() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let out = translator
            .accept(
                &json!({
                    "type": "codex.rate_limits",
                    "rate_limits": {"limit_reached": true}
                }),
                None,
            )
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("event: ping"));
        assert!(!out.contains("event: error"));

        let finish = translator
            .accept(
                &json!({
                    "type": "response.completed",
                    "response": {"id": "resp_1", "status": "completed", "usage": {"input_tokens": 5, "output_tokens": 2}}
                }),
                None,
            )
            .unwrap();
        let finish = String::from_utf8(finish).unwrap();
        assert!(
            finish.contains(r#""codex_rate_limits":{"limit_reached":true}"#),
            "{finish}"
        );
    }

    #[test]
    fn progress_events_start_message_and_emit_pings() {
        let mut translator = LiveStreamTranslator::new("msg_1", "gpt-5.5");
        let first = String::from_utf8(
            translator
                .accept(&json!({"type": "response.created"}), None)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first.matches("event: message_start").count(), 1);
        assert_eq!(first.matches("event: ping").count(), 1);

        let second = String::from_utf8(translator.ping_chunk(None)).unwrap();
        assert!(!second.contains("event: message_start"));
        assert_eq!(second.matches("event: ping").count(), 1);
    }

    fn signing_translator() -> LiveStreamTranslator {
        let mut request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {}
        }))
        .unwrap();
        request.reasoning_account =
            super::super::reasoning_signature::ReasoningAccount::new(Some("acct"));
        LiveStreamTranslator::for_request("msg_1", "gpt-5.5", &request)
    }

    #[test]
    fn live_stream_emits_signature_delta_before_thinking_stop() {
        let out = render_with(
            signing_translator(),
            vec![
                json!({
                    "type":"response.output_item.added",
                    "output_index":0,
                    "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}
                }),
                json!({
                    "type":"response.reasoning_summary_text.delta",
                    "output_index":0,
                    "delta":"plan"
                }),
                json!({
                    "type":"response.output_item.done",
                    "output_index":0,
                    "item":{"type":"reasoning","id":"rs_1"}
                }),
                json!({
                    "type":"response.completed",
                    "response":{"id":"resp_1","usage":{}}
                }),
            ],
        );
        let thinking_delta = out.find(r#""type":"thinking_delta""#).unwrap();
        let signature_delta = out.find(r#""type":"signature_delta""#).unwrap();
        let thinking_stop = out[signature_delta..]
            .find("event: content_block_stop")
            .map(|offset| signature_delta + offset)
            .unwrap();
        assert!(thinking_delta < signature_delta);
        assert!(signature_delta < thinking_stop);
        assert!(out.contains("duolane:codex:v1:def0b17f603285ef:cnNfMQ:opaque"));
    }

    #[test]
    fn signature_only_reasoning_is_semantic_output() {
        let mut translator = signing_translator();
        let mut out = Vec::new();
        for event in [
            json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}
            }),
            json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"type":"reasoning","id":"rs_1"}
            }),
        ] {
            out.extend(translator.accept(&event, None).unwrap());
        }

        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(r#""type":"signature_delta""#));
        assert!(translator.has_semantic_output());
    }
}
