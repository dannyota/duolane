use serde_json::json;

pub(super) fn sse(type_name: &str, payload: serde_json::Value) -> String {
    let mut obj = payload.as_object().cloned().unwrap_or_default();
    obj.insert("type".into(), json!(type_name));
    format!("data: {}\n\n", serde_json::to_string(&obj).unwrap())
}
