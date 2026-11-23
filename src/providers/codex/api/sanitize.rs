use serde_json::{Map, Value};

type Object = Map<String, Value>;

fn strings(source: &Value, target: &mut Object, keys: &[&str]) {
    for &key in keys {
        if let Some(value) = source.get(key).and_then(Value::as_str) {
            target.insert(key.into(), Value::String(value.into()));
        }
    }
}

fn numbers(source: &Value, target: &mut Object, keys: &[&str]) {
    for &key in keys {
        if let Some(value) = source.get(key).and_then(Value::as_u64) {
            target.insert(key.into(), value.into());
        }
    }
}

fn status(source: &Value, target: &mut Object) -> Result<(), ()> {
    match source.get("status") {
        None => Ok(()),
        Some(Value::String(value))
            if matches!(
                value.as_str(),
                "queued" | "in_progress" | "completed" | "incomplete" | "failed"
            ) =>
        {
            target.insert("status".into(), value.clone().into());
            Ok(())
        }
        Some(_) => Err(()),
    }
}

fn annotation(value: &Value) -> Option<Value> {
    if value.get("type")?.as_str()? != "url_citation" {
        return None;
    }
    let mut out = Object::new();
    out.insert("type".into(), "url_citation".into());
    strings(value, &mut out, &["url", "title"]);
    numbers(value, &mut out, &["start_index", "end_index"]);
    Some(out.into())
}

fn part(value: &Value) -> Option<Value> {
    let kind = value.get("type")?.as_str()?;
    if !matches!(kind, "output_text" | "summary_text") {
        return None;
    }
    let mut out = Object::new();
    out.insert("type".into(), kind.into());
    strings(value, &mut out, &["text"]);
    if let Some(items) = value.get("annotations").and_then(Value::as_array) {
        out.insert(
            "annotations".into(),
            items
                .iter()
                .filter_map(annotation)
                .collect::<Vec<_>>()
                .into(),
        );
    }
    Some(out.into())
}

fn action(value: &Value) -> Option<Value> {
    value.as_object()?;
    let mut out = Object::new();
    if let Some(kind @ ("search" | "open_page" | "find")) =
        value.get("type").and_then(Value::as_str)
    {
        out.insert("type".into(), kind.into());
    }
    strings(value, &mut out, &["query", "url", "pattern"]);
    if let Some(queries) = value.get("queries").and_then(Value::as_array) {
        out.insert(
            "queries".into(),
            queries
                .iter()
                .filter_map(Value::as_str)
                .map(Value::from)
                .collect::<Vec<_>>()
                .into(),
        );
    }
    if let Some(sources) = value.get("sources").and_then(Value::as_array) {
        let sources: Vec<Value> = sources
            .iter()
            .filter_map(|source| {
                if source.get("type")?.as_str()? != "url" {
                    return None;
                }
                let url = source.get("url")?.as_str()?;
                let mut result = serde_json::json!({"type":"url", "url":url});
                if let Some(title) = source.get("title").and_then(Value::as_str) {
                    result["title"] = title.into();
                }
                Some(result)
            })
            .collect();
        out.insert("sources".into(), sources.into());
    }
    Some(out.into())
}

fn item(value: &Value) -> Result<Option<Value>, ()> {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };
    if !matches!(
        kind,
        "message" | "reasoning" | "function_call" | "web_search_call"
    ) {
        return Ok(None);
    }
    let mut out = Object::new();
    out.insert("type".into(), kind.into());
    strings(value, &mut out, &["id"]);
    status(value, &mut out)?;
    match kind {
        "message" => {
            if value.get("role").and_then(Value::as_str) == Some("assistant") {
                out.insert("role".into(), "assistant".into());
            }
            if let Some(content) = value.get("content").and_then(Value::as_array) {
                out.insert(
                    "content".into(),
                    content.iter().filter_map(part).collect::<Vec<_>>().into(),
                );
            }
        }
        "reasoning" => {
            strings(value, &mut out, &["encrypted_content"]);
            if let Some(summary) = value.get("summary").and_then(Value::as_array) {
                out.insert(
                    "summary".into(),
                    summary.iter().filter_map(part).collect::<Vec<_>>().into(),
                );
            }
        }
        "function_call" => strings(value, &mut out, &["call_id", "name", "arguments"]),
        "web_search_call" => {
            if let Some(action) = value.get("action").and_then(action) {
                out.insert("action".into(), action);
            }
        }
        _ => return Ok(None),
    }
    Ok(Some(out.into()))
}

fn usage(value: &Value) -> Option<Value> {
    value.as_object()?;
    let mut out = Object::new();
    numbers(
        value,
        &mut out,
        &["input_tokens", "output_tokens", "total_tokens"],
    );
    for (name, key) in [
        ("input_tokens_details", "cached_tokens"),
        ("output_tokens_details", "reasoning_tokens"),
    ] {
        if let Some(details) = value.get(name).filter(|v| v.is_object()) {
            let mut projected = Object::new();
            numbers(details, &mut projected, &[key]);
            out.insert(name.into(), projected.into());
        }
    }
    Some(out.into())
}

fn response(value: &Value) -> Result<Value, ()> {
    value.as_object().ok_or(())?;
    let mut out = Object::new();
    strings(value, &mut out, &["id", "model"]);
    status(value, &mut out)?;
    for key in ["created_at", "created"] {
        if let Some(n) = value
            .get(key)
            .and_then(Value::as_number)
            .filter(|n| n.as_f64().is_some_and(|v| v >= 0.0))
        {
            out.insert(key.into(), Value::Number(n.clone()));
        }
    }
    if let Some(usage) = value.get("usage").and_then(usage) {
        out.insert("usage".into(), usage);
    }
    if let Some(output) = value.get("output").and_then(Value::as_array) {
        let items = output.iter().map(item).collect::<Result<Vec<_>, _>>()?;
        out.insert(
            "output".into(),
            items.into_iter().flatten().collect::<Vec<_>>().into(),
        );
    }
    if value
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        == Some("max_output_tokens")
    {
        out.insert(
            "incomplete_details".into(),
            serde_json::json!({"reason":"max_output_tokens"}),
        );
    }
    Ok(out.into())
}

pub(super) fn event(value: &Value) -> Result<Option<Value>, ()> {
    let kind = value.get("type").and_then(Value::as_str).ok_or(())?;
    let mut out = Object::new();
    out.insert("type".into(), kind.into());
    numbers(
        value,
        &mut out,
        &[
            "sequence_number",
            "output_index",
            "content_index",
            "summary_index",
            "annotation_index",
        ],
    );
    strings(value, &mut out, &["item_id", "response_id"]);
    match kind {
        "keepalive"
        | "response.web_search_call.in_progress"
        | "response.web_search_call.searching"
        | "response.web_search_call.completed" => {}
        "response.created"
        | "response.in_progress"
        | "response.completed"
        | "response.incomplete"
        | "response.done" => {
            out.insert(
                "response".into(),
                response(value.get("response").ok_or(())?)?,
            );
        }
        "response.reasoning_summary_text.delta"
        | "response.output_text.delta"
        | "response.function_call_arguments.delta" => {
            let delta = value.get("delta").and_then(Value::as_str).ok_or(())?;
            out.insert("delta".into(), delta.into());
        }
        "response.function_call_arguments.done" => {
            let args = value.get("arguments").and_then(Value::as_str).ok_or(())?;
            out.insert("arguments".into(), args.into());
        }
        "response.output_item.added" | "response.output_item.done" => {
            let raw = value.get("item").filter(|v| v.is_object()).ok_or(())?;
            if raw.get("type").and_then(Value::as_str).is_none() {
                return Err(());
            }
            let Some(item) = item(raw)? else {
                return Ok(None);
            };
            out.insert("item".into(), item);
        }
        "response.reasoning_summary_part.added" => {
            let raw = value.get("part").filter(|v| v.is_object()).ok_or(())?;
            let Some(part) = part(raw) else {
                return Ok(None);
            };
            out.insert("part".into(), part);
        }
        "response.output_text.annotation.added" => {
            let raw = value
                .get("annotation")
                .filter(|v| v.is_object())
                .ok_or(())?;
            let Some(annotation) = annotation(raw) else {
                return Ok(None);
            };
            out.insert("annotation".into(), annotation);
        }
        _ => return Ok(None),
    }
    Ok(Some(out.into()))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::event;

    #[test]
    fn codex_api_redacts_invalid_status_without_erasing_it() {
        for invalid in [json!("toy private status"), json!(17)] {
            let item = json!({"type":"function_call","call_id":"call_1","name":"tool","arguments":"{}","status":invalid});
            assert!(event(&json!({"type":"response.output_item.done","item":item})).is_err());
            assert!(event(&json!({"type":"response.completed","response":{"status":"completed","output":[item]}})).is_err());
            assert!(
                event(&json!({"type":"response.completed","response":{"status":invalid}})).is_err()
            );
        }
        let valid = json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"tool","arguments":"{}","status":"completed"}});
        let result = event(&valid).unwrap().unwrap();
        assert!(result.pointer("/item/status").and_then(Value::as_str) == Some("completed"));
        let absent = json!({"type":"response.output_item.done","item":{"type":"function_call"}});
        assert!(
            event(&absent)
                .unwrap()
                .unwrap()
                .pointer("/item/status")
                .is_none()
        );
        let unknown = json!({"type":"response.completed","response":{"output":[{"type":"future_type","status":"toy private status"}]}});
        assert!(event(&unknown).is_ok());
    }

    #[test]
    fn codex_api_redacts_item_fields_without_losing_model_content() {
        let inputs = [
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"tool","arguments":"{\"message\":\"allowed\"}","detail":"drop_me"}}),
            json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"allowed","annotations":[{"type":"url_citation","url":"https://example.test","title":"title","start_index":0,"end_index":3,"detail":"drop_me"}],"message":"drop_me"}]}}),
            json!({"type":"response.incomplete","response":{"id":"resp_1","model":"gpt-5.5","created_at":123,"status":"incomplete","incomplete_details":{"reason":"max_output_tokens","message":"drop_me"},"usage":{"input_tokens":3,"output_tokens":4,"output_tokens_details":{"reasoning_tokens":2,"detail":"drop_me"}}}}),
        ];
        let values: Vec<Value> = inputs
            .iter()
            .map(|value| event(value).unwrap().unwrap())
            .collect();
        assert!(
            !values
                .iter()
                .any(|value| value.to_string().contains("drop_me"))
        );
        assert!(
            values[0].pointer("/item/arguments").and_then(Value::as_str)
                == Some("{\"message\":\"allowed\"}")
        );
        assert!(
            values[1]
                .pointer("/item/content/0/text")
                .and_then(Value::as_str)
                == Some("allowed")
        );
        assert!(
            values[1]
                .pointer("/item/content/0/annotations/0/url")
                .and_then(Value::as_str)
                == Some("https://example.test")
        );
        assert!(
            values[2]
                .pointer("/response/incomplete_details/reason")
                .and_then(Value::as_str)
                == Some("max_output_tokens")
        );
        assert!(
            values[2]
                .pointer("/response/usage/output_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
                == Some(2)
        );
        assert!(event(&json!({"type":"response.output_item.done","item":{"type":"extension","message":"drop_me"}})).unwrap().is_none());
    }
}
