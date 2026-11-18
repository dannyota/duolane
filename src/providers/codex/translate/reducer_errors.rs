use super::reducer::*;

#[derive(Debug, Clone)]
pub struct UpstreamStreamError {
    pub kind: UpstreamErrorKind,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
    pub diagnostics: Option<UpstreamStreamDiagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamErrorKind {
    RateLimit,
    Overloaded,
    Transient,
    Failed,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UpstreamStreamDiagnostics {
    pub event_count: usize,
    pub last_event_type: Option<String>,
    pub saw_terminal_event: bool,
    pub open_blocks: Vec<OpenBlockDiagnostic>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenBlockDiagnostic {
    pub output_index: usize,
    pub anthropic_index: usize,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_bytes: Option<usize>,
}

pub(super) fn describe_open_blocks(
    blocks: &std::collections::HashMap<usize, BlockState>,
) -> Vec<OpenBlockDiagnostic> {
    let mut out: Vec<_> = blocks
        .iter()
        .map(|(output_index, state)| match state {
            BlockState::Text { index, text_accum } => OpenBlockDiagnostic {
                output_index: *output_index,
                anthropic_index: *index,
                kind: "text".to_string(),
                name: None,
                call_id: None,
                text_bytes: Some(text_accum.len()),
                argument_bytes: None,
            },
            BlockState::Tool {
                index,
                call_id,
                name,
                args_accum,
                ..
            } => OpenBlockDiagnostic {
                output_index: *output_index,
                anthropic_index: *index,
                kind: "tool".to_string(),
                name: Some(name.clone()),
                call_id: Some(call_id.clone()),
                text_bytes: None,
                argument_bytes: Some(args_accum.len()),
            },
        })
        .collect();
    out.sort_by_key(|block| block.output_index);
    out
}
