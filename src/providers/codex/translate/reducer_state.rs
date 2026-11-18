use super::reasoning_signature::ReasoningAccount;
use super::reasoning_signature::ReasoningSigner;
use super::reducer::*;
use super::request::ResponsesInputItem;

pub type StopReason = &'static str;

pub const STOP_END_TURN: &str = "end_turn";

pub const STOP_TOOL_USE: &str = "tool_use";

pub const STOP_MAX_TOKENS: &str = "max_tokens";

pub type TerminalType = &'static str;

pub const TERM_COMPLETED: &str = "response.completed";

pub const TERM_INCOMPLETE: &str = "response.incomplete";

pub const TERM_DONE: &str = "response.done";

#[derive(Debug, Clone)]
pub enum ReducerEvent {
    ThinkingStart {
        index: usize,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    ThinkingSignature {
        index: usize,
        signature: String,
    },
    ThinkingStop {
        index: usize,
    },
    TextStart {
        index: usize,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    TextStop {
        index: usize,
    },
    ToolStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolDelta {
        index: usize,
        partial_json: String,
    },
    ToolStop {
        index: usize,
    },
    ToolProgress {
        index: usize,
    },
    Progress,
    WebSearch {
        index: usize,
        result_index: usize,
        id: String,
        query: String,
    },
    Finish {
        stop_reason: StopReason,
        terminal_type: String,
        continuation_eligible: bool,
        usage: Option<CodexUsage>,
        web_search_requests: usize,
        response_id: Option<String>,
        output_items: Vec<ResponsesInputItem>,
    },
}

#[derive(Debug, Clone)]
pub struct FinishMetadata {
    pub continuation_eligible: bool,
    pub response_id: Option<String>,
    pub output_items: Vec<ResponsesInputItem>,
}

pub(super) enum BlockState {
    Text {
        index: usize,
        text_accum: String,
    },
    Tool {
        index: usize,
        #[allow(dead_code)]
        output_index: usize,
        call_id: String,
        name: String,
        args_accum: String,
        had_delta: bool,
        buffer_until_done: bool,
        emitted_args: bool,
    },
}

#[derive(Clone, Copy)]
pub(super) struct ActiveThinking {
    pub(super) output_index: usize,
    pub(super) anthropic_index: usize,
}

fn finalize_active_thinking(
    active: ActiveThinking,
    out: &mut Vec<ReducerEvent>,
    reasoning_by_output_index: &mut ReasoningSigner,
    output_items_by_index: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
) {
    if let Some((replay, signature)) = reasoning_by_output_index.take(active.output_index) {
        out.push(ReducerEvent::ThinkingSignature {
            index: active.anthropic_index,
            signature,
        });
        output_items_by_index.insert(active.output_index, replay.into_input_item());
    }
    out.push(ReducerEvent::ThinkingStop {
        index: active.anthropic_index,
    });
}

pub(super) fn close_thinking(
    out: &mut Vec<ReducerEvent>,
    active_thinking: &mut Option<ActiveThinking>,
    reasoning_by_output_index: &mut ReasoningSigner,
    output_items_by_index: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
) {
    if let Some(active) = active_thinking.take() {
        finalize_active_thinking(
            active,
            out,
            reasoning_by_output_index,
            output_items_by_index,
        );
    }
}

pub(super) fn emit_signature_only_reasoning(
    output_index: usize,
    anthropic_index: &mut usize,
    out: &mut Vec<ReducerEvent>,
    reasoning_by_output_index: &mut ReasoningSigner,
    output_items_by_index: &mut std::collections::BTreeMap<usize, ResponsesInputItem>,
) {
    let Some((replay, signature)) = reasoning_by_output_index.take(output_index) else {
        return;
    };
    let index = *anthropic_index;
    *anthropic_index += 1;
    out.push(ReducerEvent::ThinkingStart { index });
    out.push(ReducerEvent::ThinkingSignature { index, signature });
    out.push(ReducerEvent::ThinkingStop { index });
    output_items_by_index.insert(output_index, replay.into_input_item());
}

pub fn finish_metadata_from_upstream(
    input: &[u8],
    account: &ReasoningAccount,
) -> Result<Option<FinishMetadata>, UpstreamStreamError> {
    let events = reduce_upstream_bytes(input, account)?;
    Ok(events.into_iter().rev().find_map(|event| match event {
        ReducerEvent::Finish {
            continuation_eligible,
            response_id,
            output_items,
            ..
        } => Some(FinishMetadata {
            continuation_eligible,
            response_id,
            output_items,
        }),
        _ => None,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::reducer_test_support::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn finish_metadata_extracts_continuation_state() {
        let upstream = format!(
            "{}{}{}{}",
            sse(
                "response.output_item.added",
                json!({
                    "output_index": 0,
                    "item": {"type":"message","id":"msg_up"}
                })
            ),
            sse(
                "response.output_text.delta",
                json!({
                    "output_index":0,"delta":"hello"
                })
            ),
            sse(
                "response.output_item.done",
                json!({
                    "output_index":0,"item":{"type":"message"}
                })
            ),
            sse(
                "response.completed",
                json!({
                    "response":{"id":"resp_1","usage":{}}
                })
            ),
        );
        let metadata =
            finish_metadata_from_upstream(upstream.as_bytes(), &ReasoningAccount::default())
                .unwrap()
                .unwrap();
        assert!(metadata.continuation_eligible);
        assert_eq!(metadata.response_id.as_deref(), Some("resp_1"));
        assert_eq!(metadata.output_items.len(), 1);
    }
}
