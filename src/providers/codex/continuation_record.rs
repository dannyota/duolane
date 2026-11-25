use super::translate::request::{ResponsesContentPart, ResponsesInputItem};
use serde_json::Value;

pub(super) fn may_record(stored: Option<&[Value]>, echoed: &[ResponsesInputItem]) -> bool {
    let Some(stored) = stored else { return false };
    stored.len() == echoed.len()
        && stored.iter().zip(echoed).all(|(stored, echoed)| {
            match (stored.get("type").and_then(Value::as_str), echoed) {
                (Some("message"), ResponsesInputItem::Message { role, content })
                    if role == "assistant" && stored.get("role").and_then(Value::as_str) == Some("assistant") =>
                {
                    let stored_text = stored.get("content").and_then(Value::as_array).and_then(|parts| {
                        parts.iter().try_fold(String::new(), |mut text, part| {
                            if part.get("type").and_then(Value::as_str) != Some("output_text")
                                || !part.get("annotations").is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
                            {
                                return None;
                            }
                            text.push_str(part.get("text")?.as_str()?);
                            Some(text)
                        })
                    });
                    let echoed_text = content.iter().try_fold(String::new(), |mut joined, part| {
                        let ResponsesContentPart::OutputText { text } = part else { return None };
                        joined.push_str(text);
                        Some(joined)
                    });
                    stored_text.is_some() && stored_text == echoed_text
                }
                (Some("reasoning"), ResponsesInputItem::Reasoning { id, encrypted_content, .. }) => {
                    stored.get("id").and_then(Value::as_str) == Some(id.as_str())
                        && stored.get("encrypted_content").and_then(Value::as_str) == Some(encrypted_content.as_str())
                }
                (Some("function_call"), ResponsesInputItem::FunctionCall { call_id, name, arguments }) => {
                    stored.get("namespace").is_none()
                        && stored.get("call_id").and_then(Value::as_str) == Some(call_id.as_str())
                        && stored.get("name").and_then(Value::as_str) == Some(name.as_str())
                        && stored.get("arguments").and_then(Value::as_str).is_some_and(|a| {
                            a == arguments || matches!((serde_json::from_str::<Value>(a), serde_json::from_str::<Value>(arguments)), (Ok(a), Ok(b)) if a == b)
                        })
                }
                _ => false,
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(value: Value) -> ResponsesInputItem {
        serde_json::from_value(value).unwrap()
    }

    fn message(text: &str) -> Value {
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})
    }

    fn reasoning(content: &str) -> Value {
        json!({"type":"reasoning","id":"r1","encrypted_content":content,"summary":[]})
    }

    fn call(arguments: &str) -> Value {
        json!({"type":"function_call","call_id":"c1","name":"Edit","arguments":arguments})
    }

    fn matches(stored: Value, echoed: Value) -> bool {
        may_record(Some(&[stored]), &[input(echoed)])
    }

    #[test]
    fn equal_items_are_recordable() {
        let stored = vec![message("hi"), reasoning("encrypted"), call("{}")];
        let echoed = stored.iter().cloned().map(input).collect::<Vec<_>>();
        assert!(may_record(Some(&stored), &echoed));
    }

    #[test]
    fn reordered_arguments_are_recordable() {
        assert!(matches(
            call("{\"a\":1,\"b\":2}"),
            call("{\"b\":2,\"a\":1}")
        ));
    }

    #[test]
    fn phase_and_summary_are_ignored() {
        let mut msg = message("hi");
        msg["phase"] = json!("final_answer");
        let mut reason = reasoning("encrypted");
        reason["summary"] = json!([{"type":"summary_text","text":"summary"}]);
        assert!(matches(msg, message("hi")));
        assert!(matches(reason, reasoning("encrypted")));
    }

    macro_rules! reject {
        ($name:ident, $stored:expr, $echoed:expr) => {
            #[test]
            fn $name() {
                assert!(!matches($stored, $echoed));
            }
        };
    }

    reject!(different_text, message("one"), message("two"));
    reject!(
        different_encrypted_content,
        reasoning("one"),
        reasoning("two")
    );
    reject!(
        different_argument_value,
        call("{\"a\":1}"),
        call("{\"a\":2}")
    );
    reject!(
        removed_argument_key,
        call("{\"a\":1,\"b\":2}"),
        call("{\"a\":1}")
    );
    reject!(
        namespace,
        json!({"type":"function_call","call_id":"c1","name":"Edit","arguments":"{}","namespace":"x"}),
        call("{}")
    );
    reject!(
        custom_tool_call,
        json!({"type":"custom_tool_call"}),
        call("{}")
    );
    reject!(
        web_search_call,
        json!({"type":"web_search_call"}),
        call("{}")
    );
    reject!(
        refusal_part,
        json!({"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"no"}]}),
        message("no")
    );
    reject!(
        annotations,
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi","annotations":[{"type":"citation"}]}]}),
        message("hi")
    );
    reject!(unknown_type, json!({"type":"unknown"}), message("hi"));

    #[test]
    fn missing_stored_list() {
        assert!(!may_record(None, &[]));
    }

    #[test]
    fn extra_stored_item() {
        assert!(!may_record(Some(&[message("hi")]), &[]));
    }

    #[test]
    fn extra_echoed_item() {
        assert!(!may_record(Some(&[]), &[input(message("hi"))]));
    }
}
