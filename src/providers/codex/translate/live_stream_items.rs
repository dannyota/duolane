use super::live_stream::*;
use super::read_rewrite::sanitize_read_args;
use super::reducer::STOP_TOOL_USE;
use crate::providers::codex::client_http_tools::STALLED_READ_FINISH_FIELD;
use crate::traffic::TrafficCapture;

impl LiveStreamTranslator {
    pub(super) fn commit_http_tool_call(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let item = &payload["item"];
        let (Some(call_id), Some(name), Some(args)) = (
            item["call_id"].as_str(),
            item["name"].as_str(),
            item["arguments"].as_str(),
        ) else {
            return Err("Codex HTTP tool commit is missing required fields".to_owned());
        };
        let args = sanitize_read_args(name, args, Some(call_id));
        self.close_thinking(traffic, out);
        self.close_visible_text(traffic, out);
        self.ensure_message_start(traffic, out);
        self.saw_tool_use = true;
        self.semantic_output_started = true;
        let index = self.next_content_index();
        self.emit(
            traffic,
            out,
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": call_id, "name": name, "input": {}}
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": args}
            }),
        );
        self.emit(
            traffic,
            out,
            "content_block_stop",
            &serde_json::json!({"type": "content_block_stop", "index": index}),
        );
        if payload[STALLED_READ_FINISH_FIELD].as_bool() == Some(true) {
            self.close_open_blocks(traffic, out);
            self.emit_web_searches(traffic, out)?;
            self.emit_finish(STOP_TOOL_USE, None, traffic, out);
        }
        Ok(())
    }

    fn close_visible_text(&mut self, traffic: Option<&TrafficCapture>, out: &mut Vec<u8>) {
        let mut indexes = Vec::new();
        self.blocks_by_output_index.retain(|_, block| {
            if let LiveBlock::Text {
                index: Some(index),
                deferred: false,
                ..
            } = block
            {
                indexes.push(*index);
                false
            } else {
                true
            }
        });
        indexes.sort_unstable();
        for index in indexes {
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": index}),
            );
        }
    }

    pub(super) fn output_item_added(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let Some(item) = payload.get("item") else {
            return;
        };
        let output_index = output_index(payload);
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match item_type {
            "reasoning" => {
                self.reasoning_by_output_index.capture(output_index, item);
            }
            "message" => {
                self.close_thinking(traffic, out);
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    self.item_id_to_output_index
                        .insert(id.to_string(), output_index);
                }
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
            "function_call" => {
                self.close_thinking(traffic, out);
                self.saw_tool_use = true;
                let index = self.anthropic_index;
                self.anthropic_index += 1;
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.blocks_by_output_index.insert(
                    output_index,
                    LiveBlock::Tool {
                        index,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args_accum: String::new(),
                        had_delta: false,
                        buffer_until_done: name == "Read",
                        emitted_args: false,
                    },
                );
                self.ensure_message_start(traffic, out);
                self.emit(
                    traffic,
                    out,
                    "content_block_start",
                    &serde_json::json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": {}
                        }
                    }),
                );
            }
            "web_search_call" => {
                self.web_search_requests += 1;
            }
            _ => {}
        }
    }

    pub(super) fn output_item_done(
        &mut self,
        payload: &serde_json::Value,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) {
        let output_index = output_index(payload);
        if let Some(item) = payload
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(|v| v.as_str())
            .filter(|item_type| *item_type == "reasoning")
            .and_then(|_| payload.get("item"))
        {
            self.reasoning_by_output_index.capture(output_index, item);
            let had_active_summary = self
                .thinking
                .is_some_and(|thinking| thinking.output_index == output_index);
            self.close_thinking(traffic, out);
            if !had_active_summary {
                self.emit_signature_only_reasoning(output_index, traffic, out);
            }
            return;
        }

        if payload
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(|v| v.as_str())
            == Some("web_search_call")
        {
            self.close_thinking(traffic, out);
            self.semantic_output_started = true;
            let item = &payload["item"];
            let index = self.reserve_content_index(true);
            let result_index = self.reserve_content_index(true);
            let raw_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            self.web_searches.push(LiveWebSearch {
                index,
                result_index,
                id: super::web_search_compat::server_tool_use_id_from_codex_web_search_id(raw_id),
                query: web_search_query(item),
            });
            return;
        }

        let Some(mut state) = self.blocks_by_output_index.remove(&output_index) else {
            return;
        };

        match &mut state {
            LiveBlock::Text {
                index,
                text,
                deferred,
            } => {
                if *deferred {
                    self.deferred_text.push((*index, std::mem::take(text)));
                } else if let Some(index) = *index {
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
            LiveBlock::Tool {
                index,
                name,
                call_id,
                args_accum,
                had_delta,
                buffer_until_done,
                emitted_args,
                ..
            } => {
                self.semantic_output_started = true;
                if let Some(final_args) = payload
                    .get("item")
                    .and_then(|item| item.get("arguments"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    && (args_accum.is_empty() || (!*had_delta && !*emitted_args))
                {
                    *args_accum = final_args.to_string();
                }
                if !args_accum.is_empty() {
                    *args_accum = sanitize_read_args(name, args_accum, Some(call_id.as_str()));
                    if *buffer_until_done || !*emitted_args {
                        *emitted_args = true;
                        self.emit(
                            traffic,
                            out,
                            "content_block_delta",
                            &serde_json::json!({
                                "type": "content_block_delta",
                                "index": index,
                                "delta": {
                                    "type": "input_json_delta",
                                    "partial_json": args_accum
                                }
                            }),
                        );
                    }
                }
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
    }

    pub(super) fn web_search_annotation(&mut self, payload: &serde_json::Value) {
        let Some(annotation) = payload.get("annotation") else {
            return;
        };
        if annotation.get("type").and_then(|v| v.as_str()) != Some("url_citation") {
            return;
        }
        let Some(url) = annotation.get("url").and_then(|v| v.as_str()) else {
            return;
        };
        if self
            .web_search_results
            .iter()
            .any(|result| result.url == url)
        {
            return;
        }
        let title = annotation
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|title| !title.is_empty())
            .unwrap_or(url);
        self.web_search_results.push(LiveWebSearchResult {
            title: title.to_string(),
            url: url.to_string(),
        });
    }

    pub(super) fn emit_web_searches(
        &mut self,
        traffic: Option<&TrafficCapture>,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        self.check_web_output_limit(out.len())?;
        let searches = std::mem::take(&mut self.web_searches);
        for search in searches {
            let index = search.index.unwrap_or_else(|| self.next_content_index());
            let result_index = search
                .result_index
                .unwrap_or_else(|| self.next_content_index());
            self.ensure_message_start(traffic, out);
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {
                        "type": "server_tool_use",
                        "id": search.id,
                        "name": "web_search",
                        "input": {}
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": serde_json::to_string(&serde_json::json!({"query": search.query})).unwrap_or_default()
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": index}),
            );
            let results: Vec<_> = self
                .web_search_results
                .iter()
                .map(|result| {
                    serde_json::json!({
                        "type": "web_search_result",
                        "title": result.title,
                        "url": result.url,
                    })
                })
                .collect();
            self.emit(
                traffic,
                out,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": result_index,
                    "content_block": {
                        "type": "web_search_tool_result",
                        "tool_use_id": search.id,
                        "content": results
                    }
                }),
            );
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": result_index}),
            );
        }

        for (index, text) in std::mem::take(&mut self.deferred_text) {
            let index = index.unwrap_or_else(|| self.next_content_index());
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
            if !text.is_empty() {
                self.emit(
                    traffic,
                    out,
                    "content_block_delta",
                    &serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                );
            }
            self.emit(
                traffic,
                out,
                "content_block_stop",
                &serde_json::json!({"type": "content_block_stop", "index": index}),
            );
        }
        Ok(())
    }
}
