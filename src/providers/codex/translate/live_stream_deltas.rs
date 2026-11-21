use super::live_stream::*;
use super::live_stream_read_args::*;
use super::read_rewrite::sanitize_read_args;
use super::reducer::STOP_TOOL_USE;
use crate::traffic::TrafficCapture;

impl LiveStreamTranslator {
    pub(super) fn reasoning_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let output_index = output_index(payload);
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return;
        }
        self.semantic_output_started = true;
        if self.thinking.map(|thinking| thinking.output_index) != Some(output_index) {
            self.close_thinking(traffic, out);
            let index = self.anthropic_index;
            self.anthropic_index += 1;
            self.thinking = Some(LiveThinking {
                output_index,
                anthropic_index: index,
            });
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
        }
        let index = self
            .thinking
            .expect("thinking block was started")
            .anthropic_index;
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "thinking_delta", "thinking": delta}
            }),
        );
    }

    pub(super) fn text_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        self.close_thinking(traffic, out);
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return;
        }
        self.semantic_output_started = true;

        let output_index = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .or_else(|| {
                payload
                    .get("item_id")
                    .and_then(|v| v.as_str())
                    .and_then(|id| self.item_id_to_output_index.get(id).copied())
            })
            .unwrap_or(0);

        if !self.blocks_by_output_index.contains_key(&output_index) {
            let deferred = !self.web_searches.is_empty();
            let index = self.reserve_content_index(deferred);
            self.blocks_by_output_index.insert(
                output_index,
                LiveBlock::Text {
                    index,
                    text: String::new(),
                    deferred,
                },
            );
            if !deferred && let Some(index) = index {
                self.ensure_message_start(traffic, out);
                self.emit(
                    traffic,
                    out,
                    "content_block_start",
                    &serde_json::json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                );
            }
        }

        let Some(LiveBlock::Text {
            index,
            text,
            deferred,
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return;
        };
        text.push_str(delta);
        if *deferred {
            return;
        }
        let Some(index) = *index else {
            return;
        };
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": delta}
            }),
        );
    }

    pub(super) fn tool_delta(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let Some(output_index) = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
        else {
            return Ok(());
        };
        let delta = payload.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if delta.is_empty() {
            return Ok(());
        }
        self.semantic_output_started = true;
        let mut repaired_read: Option<(usize, String)> = None;
        let Some(LiveBlock::Tool {
            index,
            call_id,
            name,
            args_accum,
            had_delta,
            buffer_until_done,
            emitted_args,
            ..
        }) = self.blocks_by_output_index.get_mut(&output_index)
        else {
            return Ok(());
        };
        args_accum.push_str(delta);
        *had_delta = true;
        if *buffer_until_done {
            if args_accum.len() > BUFFERED_TOOL_MAX_ARGS_BYTES {
                return Err(format!(
                    "Buffered {name} tool arguments exceeded safe limits"
                ));
            }
            if let Some(repaired) = repair_whitespace_stalled_read_args(name, args_accum) {
                let repaired = sanitize_read_args(name, &repaired, Some(call_id.as_str()));
                *args_accum = repaired.clone();
                *emitted_args = true;
                repaired_read = Some((*index, repaired));
            }
        } else {
            *emitted_args = true;
            let index = *index;
            self.semantic_output_started = true;
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": delta
                    }
                }),
            );
            return Ok(());
        }
        if let Some((index, repaired)) = repaired_read {
            self.blocks_by_output_index.remove(&output_index);
            self.semantic_output_started = true;
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": repaired
                    }
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
            self.ensure_message_start(traffic, out);
            self.emit_finish(STOP_TOOL_USE, None, traffic, out);
        }
        Ok(())
    }

    pub(super) fn tool_arguments_done(&mut self, payload: &serde_json::Value) {
        let Some(output_index) = payload
            .get("output_index")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
        else {
            return;
        };
        let Some(args) = payload.get("arguments").and_then(|v| v.as_str()) else {
            return;
        };
        let Some(LiveBlock::Tool { args_accum, .. }) =
            self.blocks_by_output_index.get_mut(&output_index)
        else {
            return;
        };
        if args_accum.is_empty() {
            *args_accum = args.to_string();
        }
    }
}
