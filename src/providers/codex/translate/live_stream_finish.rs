use super::live_stream::*;
use super::reducer::CodexUsage;
use super::reducer::STOP_END_TURN;
use super::reducer::STOP_MAX_TOKENS;
use super::reducer::STOP_TOOL_USE;
use super::reducer::map_codex_usage_to_anthropic;
use crate::providers::codex::events::is_standard_max_output_tokens_incomplete;
use crate::traffic::TrafficCapture;

impl LiveStreamTranslator {
    pub(super) fn finish(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        if self
            .blocks_by_output_index
            .values()
            .any(|block| matches!(block, LiveBlock::Tool { .. }))
        {
            return Err("upstream stream ended with an open Codex tool call".to_string());
        }
        self.close_thinking(traffic, out);
        self.close_open_blocks(traffic, out);
        self.emit_web_searches(traffic, out);
        self.ensure_message_start(traffic, out);
        let mut usage = payload.get("response").map(parse_codex_usage);
        if let Some(usage) = usage.as_mut() {
            usage.rate_limits = self.rate_limits.clone().map(Box::new);
        }
        let stop_reason = if is_standard_max_output_tokens_incomplete(payload) {
            STOP_MAX_TOKENS
        } else if self.saw_tool_use {
            STOP_TOOL_USE
        } else {
            STOP_END_TURN
        };
        self.emit_finish(stop_reason, usage, traffic, out);
        Ok(())
    }

    pub(super) fn emit_finish(
        &mut self,
        stop_reason: &str,
        usage: Option<CodexUsage>,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let mapped = map_codex_usage_to_anthropic(&usage, Some(self.web_search_requests));
        self.emit(
            traffic,
            out,
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": mapped,
            }),
        );
        self.emit(
            traffic,
            out,
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        );
        self.finished = true;
    }

    pub(super) fn close_open_blocks(
        &mut self,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        self.close_thinking(traffic, out);
        let open: Vec<usize> = self.blocks_by_output_index.keys().copied().collect();
        for output_index in open {
            let Some(state) = self.blocks_by_output_index.remove(&output_index) else {
                continue;
            };
            let index = match state {
                LiveBlock::Text {
                    index,
                    text,
                    deferred: true,
                } => {
                    self.deferred_text.push((index, text));
                    continue;
                }
                LiveBlock::Text {
                    index: Some(index), ..
                } => index,
                _ => continue,
            };
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({
                    "type": "content_block_stop",
                    "index": index,
                }),
            );
        }
    }

    pub(super) fn emit_signature_only_reasoning(
        &mut self,
        output_index: usize,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let Some((_, signature)) = self.reasoning_by_output_index.take(output_index) else {
            return;
        };
        self.semantic_output_started = true;
        let index = self.anthropic_index;
        self.anthropic_index += 1;
        self.ensure_message_start(traffic, out);
        self.emit(
            traffic,
            out,
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "signature_delta", "signature": signature}
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({
                "type": "content_block_stop",
                "index": index,
            }),
        );
    }

    pub(super) fn close_thinking(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        let Some(thinking) = self.thinking.take() else {
            return;
        };
        if let Some((_, signature)) = self.reasoning_by_output_index.take(thinking.output_index) {
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": thinking.anthropic_index,
                    "delta": {"type": "signature_delta", "signature": signature}
                }),
            );
        }
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({
                "type": "content_block_stop",
                "index": thinking.anthropic_index,
            }),
        );
    }
}
