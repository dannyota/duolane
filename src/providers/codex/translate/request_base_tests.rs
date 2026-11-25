use super::super::request::{TranslateOptions, translate_request, translate_request_with_base};
use crate::anthropic::schema::MessagesRequest;
use serde_json::{Value, json};

fn options() -> TranslateOptions {
    TranslateOptions {
        session_id: None,
        service_tier: None,
        model: "gpt-5.5".into(),
        use_responses_lite: false,
        reasoning_account: Default::default(),
    }
}

fn request() -> MessagesRequest {
    serde_json::from_value(json!({
        "model": "gpt-5.5",
        "system": "CLIENT SYSTEM",
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
    }))
    .unwrap()
}

fn wire(req: &MessagesRequest, base: bool) -> Value {
    let out = if base {
        translate_request_with_base(req, options(), Some("SYNTHETIC BASE"))
    } else {
        translate_request(req, options())
    };
    serde_json::to_value(out.unwrap()).unwrap()
}

#[test]
fn full_base_precedes_system_and_history() {
    let req = request();
    let before = wire(&req, false);
    let after = wire(&req, true);
    let input = after["input"].as_array().unwrap();
    assert_eq!(input.len(), before["input"].as_array().unwrap().len() + 2);
    assert!(
        input[0]
            == json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"SYNTHETIC BASE"}]})
    );
    assert!(
        input[1]
            == json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"CLIENT SYSTEM"}]})
    );
    assert!(&input[2..] == before["input"].as_array().unwrap());
    assert!(after.get("instructions").is_none());
}

#[test]
fn full_base_without_system_precedes_history() {
    let mut req = request();
    req.extra.remove("system");
    let before = wire(&req, false);
    let after = wire(&req, true);
    let input = after["input"].as_array().unwrap();
    assert_eq!(input.len(), before["input"].as_array().unwrap().len() + 1);
    assert_eq!(input[0]["content"][0]["text"], "SYNTHETIC BASE");
    assert!(&input[1..] == before["input"].as_array().unwrap());
    assert!(after.get("instructions").is_none());
}

#[test]
fn full_base_skips_ineligible_requests() {
    let base = request();
    let mut cases = Vec::new();
    let mut web = base.clone();
    web.extra["tools"] = json!([{"type":"web_search_20250305", "name":"web_search"}]);
    web.extra.insert(
        "tool_choice".into(),
        json!({"type":"tool", "name":"web_search"}),
    );
    cases.push(web);
    let mut forced_web = base.clone();
    forced_web.extra["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"web_search_20250305", "name":"web_search"}));
    forced_web.extra.insert(
        "tool_choice".into(),
        json!({"type":"tool", "name":"web_search"}),
    );
    cases.push(forced_web);
    let mut none = base.clone();
    none.extra
        .insert("tool_choice".into(), json!({"type":"none"}));
    cases.push(none);
    let mut compact = base.clone();
    compact.extra.insert(
        "system".into(),
        json!("You are a helpful AI assistant tasked with summarizing conversations"),
    );
    cases.push(compact);
    let mut no_tools = base.clone();
    no_tools.extra.remove("tools");
    cases.push(no_tools);
    let mut placeholder = base.clone();
    placeholder.extra.insert("tools".into(), json!([{"name":"DeferredToolPlaceholder","defer_loading":true,"input_schema":{"type":"object"}}]));
    cases.push(placeholder);
    let mut forced = base.clone();
    forced.extra.insert(
        "tool_choice".into(),
        json!({"type":"tool","name":"missing"}),
    );
    cases.push(forced);
    for req in cases {
        let before = wire(&req, false);
        let after = wire(&req, true);
        assert!(before.get("instructions").is_some());
        assert!(before == after);
    }
}

#[test]
fn full_base_with_function_and_hosted_search() {
    let mut req = request();
    req.extra["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"web_search_20250305", "name":"web_search"}));
    let after = wire(&req, true);
    assert_eq!(after["input"][0]["content"][0]["text"], "SYNTHETIC BASE");
    assert!(after.get("instructions").is_none());
}

#[test]
fn full_base_requires_forced_tool_to_match() {
    let mut req = request();
    req.extra
        .insert("tool_choice".into(), json!({"type":"tool","name":"Bash"}));
    let after = wire(&req, true);
    assert_eq!(after["input"][0]["content"][0]["text"], "SYNTHETIC BASE");
    assert!(after.get("instructions").is_none());
}
