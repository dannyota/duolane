use super::request::*;
use crate::providers::translate_shared::unsupported_block_marker;
use base64::Engine;
use serde_json::Value;

enum RenderedToolResultPart {
    Text(String),
    Image(String),
}

pub(super) struct RenderedToolResult {
    parts: Vec<RenderedToolResultPart>,
}

impl RenderedToolResult {
    fn has_images(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, RenderedToolResultPart::Image(_)))
    }

    pub(super) fn joined_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                RenderedToolResultPart::Text(text) => Some(text.as_str()),
                RenderedToolResultPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn prepend_text(&mut self, text: String) {
        self.parts.insert(0, RenderedToolResultPart::Text(text));
    }

    pub(super) fn push_text(&mut self, text: String) {
        self.parts.push(RenderedToolResultPart::Text(text));
    }
}

pub(super) fn render_tool_result(content: &Value) -> RenderedToolResult {
    let parts = match content {
        Value::String(text) => vec![RenderedToolResultPart::Text(text.clone())],
        Value::Array(blocks) => blocks.iter().map(render_tool_result_block).collect(),
        _ => Vec::new(),
    };
    RenderedToolResult { parts }
}

fn render_tool_result_block(block: &Value) -> RenderedToolResultPart {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .map(|text| RenderedToolResultPart::Text(text.to_string()))
            .unwrap_or_else(|| {
                RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block))
            }),
        Some("image") => render_tool_result_image(block),
        Some("tool_reference") => match block.get("tool_name").and_then(Value::as_str) {
            Some(name) => RenderedToolResultPart::Text(format!("Tool loaded: {name}")),
            None => RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block)),
        },
        Some(other) => RenderedToolResultPart::Text(unsupported_block_marker(other)),
        None => RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block)),
    }
}

fn render_tool_result_image(block: &Value) -> RenderedToolResultPart {
    let Some(source) = block.get("source").and_then(Value::as_object) else {
        return RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block));
    };
    match source.get("type").and_then(Value::as_str) {
        Some("url") if source.get("url").and_then(Value::as_str).is_some() => {
            RenderedToolResultPart::Text("[image omitted: url]".to_string())
        }
        Some("base64") => {
            let media_type = source.get("media_type").and_then(Value::as_str);
            let data = source.get("data").and_then(Value::as_str);
            match media_type
                .zip(data)
                .and_then(|(media_type, data)| validated_image_data_url(media_type, data))
            {
                Some(image_url) => RenderedToolResultPart::Image(image_url),
                None => {
                    RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block))
                }
            }
        }
        _ => RenderedToolResultPart::Text(unsupported_tool_result_block_to_string(block)),
    }
}

fn validated_image_data_url(media_type: &str, data: &str) -> Option<String> {
    if !matches!(
        media_type,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        return None;
    }

    let compact: String = data
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    if compact.is_empty() {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&compact)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&compact))
        .ok()?;
    let canonical = base64::engine::general_purpose::STANDARD.encode(decoded);
    Some(format!("data:{media_type};base64,{canonical}"))
}

pub(super) fn function_call_output(rendered: RenderedToolResult) -> ResponsesFunctionCallOutput {
    if !rendered.has_images() {
        return ResponsesFunctionCallOutput::Text(rendered.joined_text());
    }

    ResponsesFunctionCallOutput::ContentItems(
        rendered
            .parts
            .into_iter()
            .map(|part| match part {
                RenderedToolResultPart::Text(text) => {
                    ResponsesFunctionCallOutputContentPart::InputText { text }
                }
                RenderedToolResultPart::Image(image_url) => {
                    ResponsesFunctionCallOutputContentPart::InputImage {
                        image_url,
                        detail: None,
                    }
                }
            })
            .collect(),
    )
}

fn unsupported_tool_result_block_to_string(block: &Value) -> String {
    let kind = block.get("type").and_then(Value::as_str);
    unsupported_block_marker(kind.unwrap_or("unknown"))
}

#[cfg(test)]
mod tests {
    use super::super::request_test_support::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn translate_tool_result_preserves_image_then_text_order() {
        let rendered = render_tool_result(&json!([
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": PNG_BASE64
            }},
            {"type": "text", "text": "caption"}
        ]));

        assert_eq!(
            serde_json::to_value(function_call_output(rendered)).unwrap(),
            json!([
                {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_BASE64}")},
                {"type": "input_text", "text": "caption"}
            ])
        );
    }

    #[test]
    fn unsupported_tool_result_images_become_in_place_text_placeholders() {
        let rendered = render_tool_result(&json!([
            {"type": "text", "text": "before"},
            {"type": "image", "source": {
                "type": "url",
                "url": "https://example.invalid/a.png"
            }},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "text/plain",
                "data": "aGVsbG8="
            }},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": "not base64"
            }},
            {"type": "text", "text": "after"}
        ]));

        assert_eq!(
            serde_json::to_value(function_call_output(rendered)).unwrap(),
            json!(
                "before\n[image omitted: url]\n[unsupported content block omitted: image]\n[unsupported content block omitted: image]\nafter"
            )
        );
    }

    #[test]
    fn supported_tool_result_image_media_types_pass_validation() {
        for media_type in ["image/jpeg", "image/png", "image/gif", "image/webp"] {
            assert_eq!(
                validated_image_data_url(media_type, "YQ"),
                Some(format!("data:{media_type};base64,YQ=="))
            );
        }
        assert!(validated_image_data_url("image/svg+xml", "YQ==").is_none());
        assert!(validated_image_data_url("image/png", "").is_none());
    }

    #[test]
    fn text_only_tool_result_keeps_string_wire_format() {
        let rendered = render_tool_result(&json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ]));

        assert_eq!(
            serde_json::to_value(function_call_output(rendered)).unwrap(),
            json!("first\nsecond")
        );
    }

    #[test]
    fn malformed_tool_result_blocks_still_become_text_placeholders() {
        let rendered = render_tool_result(&json!([
            {"type": "text"},
            {"type": "image"},
            {}
        ]));

        assert_eq!(
            rendered.joined_text(),
            "[unsupported content block omitted: text]\n[unsupported content block omitted: image]\n[unsupported content block omitted: unknown]"
        );
        assert!(!rendered.has_images());
    }
}
