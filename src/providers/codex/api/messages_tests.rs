use serde_json::json;

#[test]
fn messages_request_preserves_api_wire_fields() {
    use crate::anthropic::schema::MessagesRequest;
    use crate::providers::codex::translate::model_allowlist::resolve_tier_model_alias;
    use crate::providers::codex::translate::reasoning_signature::ReasoningAccount;
    use crate::providers::codex::translate::request::{TranslateOptions, translate_request};

    let body: MessagesRequest = serde_json::from_value(json!({
        "model":"original", "max_tokens":321, "messages":[{"role":"user","content":"hello"}],
        "output_config":{"effort":"high"},
        "tools":[{"name":"lookup","defer_loading":true,"input_schema":{"type":"object","properties":{}}},
                 {"type":"web_search_20250305","name":"web_search"}],
        "tool_choice":{"type":"auto","disable_parallel_tool_use":true}
    })).unwrap();
    let resolved = resolve_tier_model_alias("gpt-6-astra-ultrafast");
    let account = ReasoningAccount::for_api_key(b"toy-key");
    let wire =
        super::messages::build_request(&body, &resolved, Some("session".into()), &account).unwrap();
    assert_eq!(wire["model"], "gpt-6-astra");
    assert_eq!(wire["max_output_tokens"], 321);
    assert_eq!(wire["store"], false);
    assert_eq!(wire["parallel_tool_calls"], false);
    assert_eq!(wire["service_tier"], "priority");
    assert_eq!(wire["prompt_cache_key"], "session");
    assert!(wire.get("previous_response_id").is_none());
    assert!(wire.get("client_metadata").is_none());
    assert!(
        wire["include"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "reasoning.encrypted_content")
    );
    assert!(
        wire["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "lookup")
    );
    assert!(
        wire["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["type"] == "web_search")
    );
    let chatgpt = translate_request(
        &body,
        TranslateOptions {
            session_id: None,
            service_tier: resolved.service_tier.clone(),
            model: resolved.model.clone(),
            use_responses_lite: false,
            reasoning_account: account,
        },
    )
    .unwrap();
    assert_eq!(
        chatgpt.service_tier,
        Some(crate::providers::codex::translate::request::ServiceTier::Ultrafast)
    );
    assert!(
        serde_json::to_value(chatgpt)
            .unwrap()
            .get("max_output_tokens")
            .is_none()
    );
}

#[test]
fn messages_request_does_not_replay_other_key_reasoning() {
    use crate::anthropic::schema::MessagesRequest;
    use crate::providers::codex::translate::model_allowlist::resolve_tier_model_alias;
    use crate::providers::codex::translate::reasoning_signature::{
        ReasoningAccount, ReasoningReplay,
    };
    let first = ReasoningAccount::for_api_key(b"toy-key");
    let other = ReasoningAccount::for_api_key(b"other-key");
    let signature = first
        .sign(&ReasoningReplay {
            id: "rs_test".into(),
            encrypted_content: "opaque".into(),
        })
        .unwrap();
    let body: MessagesRequest = serde_json::from_value(json!({"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"plan","signature":signature}]}]})).unwrap();
    let model = resolve_tier_model_alias("gpt-5.5");
    let same = super::messages::build_request(&body, &model, None, &first).unwrap();
    let different = super::messages::build_request(&body, &model, None, &other).unwrap();
    assert!(
        same["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["type"] == "reasoning")
    );
    assert!(
        !different["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["type"] == "reasoning")
    );
    assert!(different["input"].to_string().contains("plan"));
    assert!(different.get("max_output_tokens").is_none());
}
