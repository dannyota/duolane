use super::request::{
    ResponsesContentPart, ResponsesInputItem, ResponsesRequest, ResponsesToolChoice,
    ResponsesToolChoiceMode, is_compact_messages_request,
};
use crate::anthropic::schema::MessagesRequest;

pub(super) fn insert_base(
    req: &MessagesRequest,
    out: &mut ResponsesRequest,
    lite: bool,
    base: Option<&str>,
) {
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return;
    };
    if !lite
        || is_compact_messages_request(req)
        || matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::None))
        )
    {
        return;
    }
    let forced = req
        .extra
        .get("tool_choice")
        .filter(|choice| choice.get("type").and_then(serde_json::Value::as_str) == Some("tool"))
        .and_then(|choice| choice.get("name"))
        .and_then(serde_json::Value::as_str);
    if !out.input.iter().any(|item| matches!(item, ResponsesInputItem::AdditionalTools { tools, .. } if tools.iter().any(|tool| forced.is_none_or(|name| tool.get("name").and_then(serde_json::Value::as_str) == Some(name))))) {
        return;
    }
    let index = usize::from(matches!(
        out.input.first(),
        Some(ResponsesInputItem::AdditionalTools { .. })
    ));
    out.input.insert(
        index,
        ResponsesInputItem::Message {
            role: "developer".into(),
            content: vec![ResponsesContentPart::InputText {
                text: base.to_owned(),
            }],
        },
    );
}
