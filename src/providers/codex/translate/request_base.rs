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
    let forced = match &out.tool_choice {
        Some(ResponsesToolChoice::Function { name, .. }) if name.is_empty() => return,
        Some(ResponsesToolChoice::Function { name, .. }) => Some(name.as_str()),
        _ => None,
    };
    if !out.input.iter().any(|item| matches!(item, ResponsesInputItem::AdditionalTools { tools, .. } if tools.iter().any(|tool| tool.get("name").and_then(serde_json::Value::as_str).is_some_and(|name| !name.is_empty() && forced.is_none_or(|forced| forced == name))))) {
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
