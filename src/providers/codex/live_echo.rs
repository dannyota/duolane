use super::translate::reasoning_signature::ReasoningAccount;
use super::translate::request::ResponsesInputItem;
use crate::anthropic::schema::{Message, MessagesRequest};
use crate::provider::MAX_BUFFERED_RESPONSE_BYTES;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct LiveEcho {
    blocks: BTreeMap<u64, (Value, String)>,
    size: usize,
    invalid: bool,
}

impl LiveEcho {
    pub(super) fn push(&mut self, chunk: &[u8]) {
        if self.invalid {
            return;
        }
        for event in crate::anthropic::sse::parse_sse_events(chunk) {
            self.size = self.size.saturating_add(event.data.len());
            if self.size > MAX_BUFFERED_RESPONSE_BYTES {
                self.blocks.clear();
                self.invalid = true;
                return;
            }
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                self.invalid = true;
                return;
            };
            let Some(kind) = value.get("type").and_then(Value::as_str) else {
                self.invalid = true;
                return;
            };
            match kind {
                "content_block_start" => {
                    let Some(index) = value.get("index").and_then(Value::as_u64) else {
                        self.invalid = true;
                        return;
                    };
                    let Some(block) = value.get("content_block") else {
                        self.invalid = true;
                        return;
                    };
                    if self
                        .blocks
                        .insert(index, (block.clone(), String::new()))
                        .is_some()
                    {
                        self.invalid = true;
                        return;
                    }
                }
                "content_block_delta" => {
                    let Some(index) = value.get("index").and_then(Value::as_u64) else {
                        self.invalid = true;
                        return;
                    };
                    let Some((block, args)) = self.blocks.get_mut(&index) else {
                        self.invalid = true;
                        return;
                    };
                    let delta = &value["delta"];
                    let (field, text) = match delta["type"].as_str() {
                        Some("text_delta") => ("text", delta["text"].as_str()),
                        Some("thinking_delta") => ("thinking", delta["thinking"].as_str()),
                        Some("signature_delta") => ("signature", delta["signature"].as_str()),
                        Some("input_json_delta") => {
                            if let Some(part) = delta["partial_json"].as_str() {
                                args.push_str(part);
                                continue;
                            } else {
                                self.invalid = true;
                                return;
                            }
                        }
                        _ => {
                            self.invalid = true;
                            return;
                        }
                    };
                    let Some(text) = text else {
                        self.invalid = true;
                        return;
                    };
                    let Some(Value::String(existing)) = block.get_mut(field) else {
                        self.invalid = true;
                        return;
                    };
                    existing.push_str(text);
                }
                _ => {}
            }
        }
    }

    pub(super) fn input(self, account: &ReasoningAccount) -> Option<Vec<ResponsesInputItem>> {
        if self.invalid {
            return None;
        }
        let mut blocks = Vec::new();
        for (_, (mut block, args)) in self.blocks {
            match block["type"].as_str()? {
                "tool_use" if !args.is_empty() => {
                    block["input"] = serde_json::from_str(&args).ok()?
                }
                "tool_use" | "text" | "thinking" => {}
                _ => return None,
            }
            blocks.push(block);
        }
        let request = MessagesRequest {
            model: None,
            max_tokens: None,
            messages: vec![Message {
                role: "assistant".into(),
                content: Value::Array(blocks),
            }],
            stream: false,
            bypass_provider_model_override: false,
            auto_review_effort: None,
            extra: Default::default(),
        };
        Some(super::translate::request_input::build_input(
            &request, account,
        ))
    }
}
