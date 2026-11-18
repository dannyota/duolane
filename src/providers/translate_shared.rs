use serde_json::Value;

use crate::anthropic::schema::MessagesRequest;

pub const REASONING_OPEN: &str = "<previous_reasoning>";
pub const REASONING_CLOSE: &str = "</previous_reasoning>";

pub fn wrap_reasoning(reasoning: &str) -> String {
    format!("{REASONING_OPEN}\n{reasoning}\n{REASONING_CLOSE}")
}

pub fn unsupported_block_marker(kind: &str) -> String {
    format!("[unsupported content block omitted: {kind}]")
}

#[derive(Debug)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
}

impl ContentBlock {
    pub fn unsupported_marker(&self) -> String {
        unsupported_block_marker(match self {
            Self::Text { .. } => "text",
            Self::Image { .. } => "image",
            Self::ToolUse { .. } => "tool_use",
            Self::ToolResult { .. } => "tool_result",
            Self::Thinking { .. } => "thinking",
        })
    }
}

#[derive(Debug)]
pub struct ImageSource {
    pub media_type: String,
    pub data: String,
    pub source_type: String,
}

pub fn flatten_system_text(system_val: Option<&Value>) -> Option<String> {
    let system = system_val?;
    let texts: Vec<String> = match system {
        Value::String(s) => vec![s.clone()],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| {
                let text = b.get("text").and_then(|v| v.as_str())?;
                if text.starts_with("x-anthropic-billing-header:") {
                    None
                } else {
                    Some(text.to_string())
                }
            })
            .collect(),
        _ => return None,
    };
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    }
}

pub fn parallel_tool_calls(req: &MessagesRequest) -> Option<bool> {
    req.extra
        .get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("disable_parallel_tool_use"))
        .and_then(Value::as_bool)
        .map(|disabled| !disabled)
}

pub fn read_effort(req: &MessagesRequest) -> Result<Option<&str>, anyhow::Error> {
    let allowed = ["low", "medium", "high", "xhigh", "max"];
    let output_config = match req.extra.get("output_config") {
        Some(Value::Object(m)) => m,
        _ => return Ok(None),
    };
    match output_config.get("effort") {
        Some(Value::String(s)) => {
            if allowed.contains(&s.as_str()) {
                Ok(Some(s.as_str()))
            } else {
                anyhow::bail!("Invalid output_config.effort: {s}")
            }
        }
        _ => Ok(None),
    }
}

pub fn normalize_content(content: &Value, missing_tool_input: Value) -> Vec<ContentBlock> {
    match content {
        Value::String(s) => {
            vec![ContentBlock::Text { text: s.clone() }]
        }
        Value::Array(arr) => arr
            .iter()
            .map(|item| parse_content_block(item, missing_tool_input.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn image_source_to_url(source: &ImageSource) -> String {
    if source.source_type == "url" {
        source.data.clone()
    } else {
        format!("data:{};base64,{}", source.media_type, source.data)
    }
}

fn parse_content_block(value: &Value, missing_tool_input: Value) -> ContentBlock {
    let Some(kind) = value.get("type").and_then(|v| v.as_str()) else {
        return unsupported_block("unknown");
    };
    match kind {
        "text" => {
            let text = value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            ContentBlock::Text { text }
        }
        "image" => {
            let Some(source) = value.get("source") else {
                return unsupported_block(kind);
            };
            let media_type = source
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png")
                .to_string();
            let source_type = source
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("base64")
                .to_string();
            if !matches!(source_type.as_str(), "base64" | "url") {
                return unsupported_block(kind);
            }
            let data = if source_type == "url" {
                source.get("url").and_then(|v| v.as_str()).unwrap_or("")
            } else {
                source.get("data").and_then(|v| v.as_str()).unwrap_or("")
            }
            .to_string();
            ContentBlock::Image {
                source: ImageSource {
                    media_type,
                    data,
                    source_type,
                },
            }
        }
        "tool_use" => {
            let id = value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input = value.get("input").cloned().unwrap_or(missing_tool_input);
            ContentBlock::ToolUse { id, name, input }
        }
        "tool_result" => {
            let tool_use_id = value
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = value
                .get("content")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            let is_error = value.get("is_error").and_then(|v| v.as_bool());
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            }
        }
        "thinking" => {
            let thinking = value
                .get("thinking")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let signature = value
                .get("signature")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            ContentBlock::Thinking {
                thinking,
                signature,
            }
        }
        other => unsupported_block(other),
    }
}

fn unsupported_block(kind: &str) -> ContentBlock {
    ContentBlock::Text {
        text: unsupported_block_marker(kind),
    }
}
