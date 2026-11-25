#[cfg(test)]
use serde_json::Value;

use crate::anthropic::schema::MessagesRequest;
use crate::config;
#[cfg(test)]
use crate::providers::translate_shared::wrap_reasoning;
use crate::providers::translate_shared::{flatten_system_text, parallel_tool_calls, read_effort};

use super::reasoning_signature::ReasoningAccount;
use super::request_input::*;
pub(crate) use super::request_options::*;
pub use super::request_tools::*;
pub use super::request_types::*;

pub struct TranslateOptions {
    pub session_id: Option<String>,
    pub service_tier: Option<ServiceTier>,
    pub model: String,
    pub use_responses_lite: bool,
    pub reasoning_account: ReasoningAccount,
}

pub fn translate_request(
    req: &MessagesRequest,
    opts: TranslateOptions,
) -> Result<ResponsesRequest, anyhow::Error> {
    translate_request_inner(req, opts, true)
}

pub fn translate_request_with_base(
    req: &MessagesRequest,
    opts: TranslateOptions,
    base: Option<&str>,
) -> Result<ResponsesRequest, anyhow::Error> {
    let lite = opts.use_responses_lite;
    let mut out = translate_request(req, opts)?;
    super::request_base::insert_base(req, &mut out, lite, base);
    Ok(out)
}

fn translate_request_inner(
    req: &MessagesRequest,
    opts: TranslateOptions,
    apply_codex_config: bool,
) -> Result<ResponsesRequest, anyhow::Error> {
    let instructions = flatten_system_text(req.extra.get("system"));
    let is_compact = is_compact_messages_request(req);
    let input = build_input(req, &opts.reasoning_account);
    let tools = read_tools(req)?;
    let tool_choice = map_tool_choice(req)?;
    let parallel_tool_calls = parallel_tool_calls(req).unwrap_or(true);

    let mut text = ResponsesText {
        verbosity: Some("low".to_string()),
        format: None,
    };

    if let Some(fmt) = read_output_format(req) {
        text.format = Some(fmt);
    }

    let mut out = ResponsesRequest {
        model: opts.model,
        instructions,
        input,
        store: false,
        stream: true,
        parallel_tool_calls,
        tool_choice,
        text,
        tools: None,
        include: None,
        client_metadata: None,
        service_tier: None,
        prompt_cache_key: None,
        reasoning: None,
        reasoning_account: opts.reasoning_account,
    };

    if opts.use_responses_lite {
        out.client_metadata = Some(std::collections::HashMap::from([(
            "ws_request_header_x_openai_internal_codex_responses_lite".to_string(),
            "true".to_string(),
        )]));
        out.parallel_tool_calls = false;

        let mut prefix = Vec::new();
        if let Some(ref tools) = tools
            && !tools.is_empty()
        {
            let loads = tool_reference_loads(req);
            let (prefix_tools, loaded_tools) = split_loaded_deferred_tools(req, tools, &loads)?;
            if !prefix_tools.is_empty() {
                prefix.push(ResponsesInputItem::AdditionalTools {
                    id: None,
                    role: "developer".to_string(),
                    tools: prefix_tools,
                });
            }
            append_loaded_tools(&mut out.input, &loads, loaded_tools);
        }
        if let Some(instructions) = out.instructions.take()
            && !instructions.is_empty()
        {
            prefix.push(ResponsesInputItem::Message {
                role: "developer".to_string(),
                content: vec![ResponsesContentPart::InputText { text: instructions }],
            });
        }
        if !prefix.is_empty() {
            prefix.extend(out.input);
            out.input = prefix;
        }
    } else if let Some(tools) = tools
        && !tools.is_empty()
    {
        let deferred = deferred_tool_names(req);
        let tools: Vec<ResponsesTool> = tools
            .into_iter()
            .filter(|tool| !is_deferred_placeholder(tool, &deferred))
            .collect();
        if !tools.is_empty() {
            out.tools = Some(tools);
        }
    }

    if matches!(
        out.tool_choice,
        Some(ResponsesToolChoice::WebSearch { .. } | ResponsesToolChoice::AllowedTools { .. })
    ) {
        let has_web_search = out.tools.as_ref().is_some_and(|t| {
            t.iter()
                .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
        });
        if !has_web_search {
            out.tool_choice = Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto));
        }
    }

    if let Some(sid) = opts.session_id {
        out.prompt_cache_key = Some(sid);
    }

    if apply_codex_config {
        let service_tier = resolve_service_tier(opts.service_tier, &out.model)?;
        if let Some(ref tier) = service_tier {
            out.service_tier = Some(tier.clone());
        }
    }

    let effort = read_effort(req)?;
    let codex_effort = to_codex_effort(effort);
    let mut resolved_effort = if apply_codex_config {
        if let Some(effort) = req.auto_review_effort.as_deref() {
            resolve_effort_override(codex_effort, Some(effort))?
        } else {
            resolve_effort(codex_effort)?
        }
    } else {
        codex_effort
    };
    if apply_codex_config && is_compact && req.auto_review_effort.is_none() {
        resolved_effort = apply_compact_effort_cap(resolved_effort, compact_effort_cap());
    }
    let wants_reasoning = reasoning_requested(resolved_effort.as_ref());
    if resolved_effort.is_some() || opts.use_responses_lite {
        let summary = if wants_reasoning
            && (!apply_codex_config
                || reasoning_summary_requested(config::codex_reasoning_summary().as_deref()))
        {
            Some("auto".to_string())
        } else {
            None
        };
        out.reasoning = Some(ResponsesReasoning {
            effort: resolved_effort.clone(),
            summary,
            context: opts.use_responses_lite.then_some("all_turns".to_string()),
        });
    }
    if wants_reasoning {
        out.include = Some(vec!["reasoning.encrypted_content".to_string()]);
    }

    Ok(out)
}

fn read_output_format(req: &MessagesRequest) -> Option<ResponsesTextFormat> {
    let output_config = req.extra.get("output_config")?.as_object()?;
    let format = output_config.get("format")?.as_object()?;
    let kind = format.get("type")?.as_str()?;
    match kind {
        "json_schema" => {
            let name = format
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("response")
                .to_string();
            let schema = format.get("schema")?;
            let normalized = normalize_strict_json_schema(schema);
            Some(ResponsesTextFormat::JsonSchema {
                name,
                schema: normalized,
                strict: Some(true),
            })
        }
        "json_object" => Some(ResponsesTextFormat::JsonObject),
        _ => Some(ResponsesTextFormat::Text),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::request_test_support::*;
    use super::*;

    fn opts() -> TranslateOptions {
        TranslateOptions {
            session_id: None,
            service_tier: None,
            model: "gpt-5.5".to_string(),
            use_responses_lite: false,
            reasoning_account: ReasoningAccount::default(),
        }
    }

    #[test]
    fn translate_tool_choice_preserves_wire_and_parallel_semantics() {
        for (tool_choice, expected_choice, expected_parallel) in [
            (json!({"type":"auto"}), json!("auto"), true),
            (
                json!({"type":"auto","disable_parallel_tool_use":false}),
                json!("auto"),
                true,
            ),
            (json!({"type":"none"}), json!("none"), true),
            (json!({"type":"any"}), json!("required"), true),
            (
                json!({"type":"any","disable_parallel_tool_use":true}),
                json!("required"),
                false,
            ),
            (
                json!({
                    "type":"tool",
                    "name":"test",
                    "disable_parallel_tool_use":true
                }),
                json!({"type":"function","name":"test"}),
                false,
            ),
        ] {
            let req: MessagesRequest = serde_json::from_value(json!({
                "model": "gpt-5.5",
                "messages": [{"role":"user", "content":"use the tool"}],
                "tools": [{
                    "name":"test",
                    "input_schema":{"type":"object","properties":{}}
                }],
                "tool_choice": tool_choice
            }))
            .unwrap();
            let wire = serde_json::to_value(translate_request(&req, opts()).unwrap()).unwrap();

            assert_eq!(wire["tool_choice"], expected_choice);
            assert_eq!(wire["parallel_tool_calls"], expected_parallel);
        }
    }

    #[test]
    fn responses_lite_keeps_parallel_tool_calls_disabled() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"use the tool"}],
            "tools": [{
                "name":"test",
                "input_schema":{"type":"object","properties":{}}
            }],
            "tool_choice": {
                "type":"any",
                "disable_parallel_tool_use":false
            }
        }))
        .unwrap();
        let wire = serde_json::to_value(
            translate_request(
                &req,
                TranslateOptions {
                    model: "gpt-5.6-luna".to_string(),
                    use_responses_lite: true,
                    ..opts()
                },
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(wire["tool_choice"], json!("required"));
        assert_eq!(wire["parallel_tool_calls"], false);
    }

    #[test]
    fn translate_web_search_tool_to_codex_tool() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["example.com"]
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: Some("s".into()),
                service_tier: None,
                model: "gpt-5.5".to_string(),
                use_responses_lite: false,
                reasoning_account: ReasoningAccount::default(),
            },
        )
        .unwrap();
        assert_eq!(out.prompt_cache_key.as_deref(), Some("s"));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
        let tool_choice = serde_json::to_value(out.tool_choice.as_ref().unwrap()).unwrap();
        assert_eq!(tool_choice["type"], "allowed_tools");
        assert_eq!(tool_choice["mode"], "required");
        assert_eq!(tool_choice["tools"], json!([{"type":"web_search"}]));
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        assert!(tool.external_web_access);
        assert_eq!(
            tool.filters.as_ref().unwrap().allowed_domains.as_deref(),
            Some(&["example.com".to_string()][..])
        );
        assert!(out.instructions.is_none());
    }

    #[test]
    fn automatic_filtered_web_search_keeps_native_filters() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["example.com"],
                "blocked_domains":["spam.example"]
            }],
            "tool_choice": {"type":"auto"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        assert!(tool.external_web_access);
        let filters = tool.filters.as_ref().unwrap();
        assert_eq!(
            filters.allowed_domains.as_deref(),
            Some(&["example.com".to_string()][..])
        );
        assert_eq!(
            filters.blocked_domains.as_deref(),
            Some(&["spam.example".to_string()][..])
        );
        assert!(out.instructions.is_none());
    }

    #[test]
    fn forced_filtered_web_search_keeps_native_filters() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"find it"}],
            "system": "Be brief.",
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search",
                "allowed_domains":["a.example", "b.example"],
                "blocked_domains":["spam.example"]
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
            panic!("expected web_search tool");
        };
        let filters = tool.filters.as_ref().unwrap();
        assert_eq!(
            filters.allowed_domains.as_deref(),
            Some(&["a.example".to_string(), "b.example".to_string()][..])
        );
        assert_eq!(
            filters.blocked_domains.as_deref(),
            Some(&["spam.example".to_string()][..])
        );
        assert_eq!(out.instructions.as_deref(), Some("Be brief."));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
    }

    #[test]
    fn unfiltered_web_search_adds_no_domain_instructions() {
        for tool_choice in [None, Some(json!({"type":"tool", "name":"web_search"}))] {
            let mut body = json!({
                "model": "gpt-5.5",
                "messages": [{"role":"user", "content":"find it"}],
                "tools": [{"type":"web_search_20250305", "name":"web_search"}]
            });
            if let Some(tool_choice) = tool_choice {
                body["tool_choice"] = tool_choice;
            }
            let req: MessagesRequest = serde_json::from_value(body).unwrap();
            let out = translate_request(&req, opts()).unwrap();
            let ResponsesTool::WebSearch(tool) = &out.tools.as_ref().unwrap()[0] else {
                panic!("expected web_search tool");
            };
            assert!(tool.external_web_access);
            assert!(tool.filters.is_none());
            assert!(out.instructions.is_none());
        }
    }

    #[test]
    fn responses_lite_downgrades_unregistered_web_search_tool_choice() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search"
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
                reasoning_account: ReasoningAccount::default(),
            },
        )
        .unwrap();
        assert!(out.tools.is_none());
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto))
        ));
        assert_eq!(
            serde_json::to_value(&out).unwrap()["tool_choice"],
            json!("auto")
        );
    }

    #[test]
    fn full_lane_keeps_web_search_tool_choice_registered() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [{
                "type":"web_search_20250305",
                "name":"web_search"
            }],
            "tool_choice": {"type":"tool", "name":"web_search"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                session_id: None,
                service_tier: None,
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: false,
                reasoning_account: ReasoningAccount::default(),
            },
        )
        .unwrap();
        assert!(out.tools.as_ref().is_some_and(|t| {
            t.iter()
                .any(|tool| matches!(tool, ResponsesTool::WebSearch(_)))
        }));
        assert!(matches!(
            out.tool_choice,
            Some(ResponsesToolChoice::AllowedTools { .. })
        ));
    }

    #[test]
    fn translate_artifact_tool_strips_exact_incompatible_pattern() {
        let req: MessagesRequest = serde_json::from_str(
            r#"{
                "model": "gpt-5.5",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{
                    "name": "Artifact",
                    "description": "Manage artifacts.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "field": {
                                "type": "string",
                                "pattern": "^(?!__.*__$)[^\\p{Cc}\\p{Cf}\\p{Zl}\\p{Zp}\"\\\\./[\\]]{1,200}$",
                                "minLength": 1,
                                "maxLength": 200
                            }
                        },
                        "required": ["field"],
                        "additionalProperties": false
                    }
                }]
            }"#,
        )
        .unwrap();
        let original_tools = req.extra["tools"].clone();
        assert_eq!(
            original_tools[0]["input_schema"]["properties"]["field"]["pattern"],
            r#"^(?!__.*__$)[^\p{Cc}\p{Cf}\p{Zl}\p{Zp}"\\./[\]]{1,200}$"#
        );
        let mut expected = original_tools[0]["input_schema"].clone();
        expected["properties"]["field"]
            .as_object_mut()
            .unwrap()
            .remove("pattern");

        let normal = translate_request(&req, opts()).unwrap();
        let compatible = translate_request_inner(&req, opts(), false).unwrap();
        let lite = translate_request(
            &req,
            TranslateOptions {
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        for out in [normal, compatible, lite] {
            let wire = serde_json::to_value(out).unwrap();
            let tool = if wire["tools"].is_array() {
                &wire["tools"][0]
            } else {
                &wire["input"][0]["tools"][0]
            };
            assert_eq!(
                tool,
                &json!({
                    "type": "function",
                    "name": "Artifact",
                    "description": "Manage artifacts.",
                    "parameters": expected,
                    "strict": false
                })
            );
        }
        assert_eq!(req.extra["tools"], original_tools);
    }

    #[test]
    fn translate_read_tool_adds_codex_offset_guidance() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"read it"}],
            "tools": [{
                "name": "Read",
                "description": "Reads a file from the local filesystem.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "file_path": {"type": "string", "pattern": "^/"},
                        "offset": {"type": "integer", "description": "old offset"},
                        "limit": {"type": "integer", "description": "old limit"}
                    },
                    "required": ["file_path"]
                }
            }]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let tools = out.tools.as_ref().unwrap();
        let ResponsesTool::Function(tool) = &tools[0] else {
            panic!("expected function tool");
        };
        let description = tool.description.as_deref().unwrap();
        assert!(description.contains("Codex Read guidance"));
        assert!(description.contains("zero based continuation index"));
        assert!(description.contains("guessed positions are invalid offsets"));

        let props = tool
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(props["file_path"], json!({"type": "string"}));
        assert_eq!(
            props
                .get("offset")
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some(
                "Optional continuation index. Use only after a prior Read of the same file returned content and more lines are needed. Compute as prior offset plus returned line count. Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets. Omit when unsure."
            )
        );
        assert_eq!(
            props
                .get("limit")
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some(
                "Optional number of lines to read. Omit when opening a file. Use with offset only when continuing a large file."
            )
        );
    }

    #[test]
    fn translate_non_read_tool_preserves_tool_metadata() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"search"}],
            "tools": [{
                "name": "Search",
                "description": "Find matching records.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "offset": {"type": "integer", "description": "record offset"}
                    }
                }
            }]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let tools = out.tools.as_ref().unwrap();
        let ResponsesTool::Function(tool) = &tools[0] else {
            panic!("expected function tool");
        };
        assert_eq!(tool.description.as_deref(), Some("Find matching records."));
        assert!(!tool.strict);
        assert_eq!(
            serde_json::to_value(tool).unwrap()["strict"],
            Value::Bool(false)
        );
        assert_eq!(
            tool.parameters
                .get("properties")
                .and_then(|v| v.get("offset"))
                .and_then(|v| v.get("description"))
                .and_then(Value::as_str),
            Some("record offset")
        );
    }

    #[test]
    fn translate_omits_reasoning_when_not_enabled() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(out.reasoning.is_none());
        assert!(out.include.is_none());
    }

    #[test]
    fn translate_includes_reasoning_when_enabled() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let reasoning = out.reasoning.unwrap();
        assert!(matches!(reasoning.effort, Some(Effort::Medium)));
        assert_eq!(reasoning.summary.as_deref(), Some("auto"));
        assert_eq!(
            out.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn translate_effort_max_maps_to_max() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "max"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Max)));
    }

    #[test]
    fn compact_request_downgrades_effort_to_cap() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"summarize"}],
            "system": "You are a helpful AI assistant tasked with summarizing conversations.",
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Low)));
    }

    #[test]
    fn compact_cap_never_raises_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"summarize"}],
            "system": "You are a helpful AI assistant tasked with summarizing conversations.",
            "output_config": {"effort": "low"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Low)));
    }

    #[test]
    fn non_compact_request_keeps_requested_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "You are Claude Code.",
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn max_tokens_is_not_serialized_for_codex() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "max_tokens": 4096,
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        let value = serde_json::to_value(out).unwrap();
        assert!(value.get("max_output_tokens").is_none());
    }

    #[test]
    fn translate_effort_xhigh_maps_to_xhigh() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "xhigh"}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::Xhigh)));
        assert_eq!(
            out.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn translate_user_text_and_image() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [
                {"type":"text", "text":"describe"},
                {"type":"image", "source": {"type":"base64", "media_type":"image/jpeg", "data":"xyz"}}
            ]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 1);
        if let ResponsesInputItem::Message { role, content } = &out.input[0] {
            assert_eq!(role, "user");
            assert_eq!(content.len(), 2);
        } else {
            panic!("expected Message");
        }
    }

    #[test]
    fn translate_assistant_with_text_and_tool_use() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"assistant", "content": [
                {"type":"text", "text":"answer"},
                {"type":"tool_use", "id":"tu_1", "name":"search", "input": {"q":"rust"}}
            ]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
    }

    #[test]
    fn translate_strict_json_schema_normalization() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content":"hi"}],
            "output_config": {"format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {"ok": {"type": "boolean"}, "reason": {"type": "string"}},
                    "required": ["ok"]
                }
            }}
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        if let Some(ResponsesTextFormat::JsonSchema { schema, .. }) = &out.text.format {
            let required = schema.get("required").and_then(|v| v.as_array()).unwrap();
            assert!(required.iter().any(|v| v == "ok"));
            assert!(required.iter().any(|v| v == "reason"));
        } else {
            panic!("expected JsonSchema format");
        }
    }

    fn tool_search_request(load: bool, extra_turn: bool) -> MessagesRequest {
        let mut tools = vec![
            json!({"name":"Bash","description":"run","input_schema":{"type":"object","properties":{}}}),
            json!({"name":"ToolSearch","description":"search","input_schema":{"type":"object","properties":{}}}),
            json!({"name":"DeferredToolPlaceholder","description":"never call","defer_loading":true,
                   "input_schema":{"type":"object","properties":{}}}),
        ];
        let mut messages = vec![json!({"role":"user","content":"find the mail tool"})];
        if load {
            tools.push(json!({"name":"mcp__mail__search","description":"search mail","defer_loading":true,
                              "input_schema":{"type":"object","properties":{"q":{"type":"string"}}}}));
            messages.push(
                json!({"role":"assistant","content":[{"type":"tool_use","id":"tu_search",
                "name":"ToolSearch","input":{"query":"select:mcp__mail__search"}}]}),
            );
            messages.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_search",
                 "content":[{"type":"tool_reference","tool_name":"mcp__mail__search"}]},
                {"type":"text","text":"Tool loaded."}]}));
        }
        if extra_turn {
            messages.push(
                json!({"role":"assistant","content":[{"type":"tool_use","id":"tu_bash",
                "name":"Bash","input":{"command":"ls"}}]}),
            );
            messages.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu_bash","content":"a.txt"}]}));
        }
        serde_json::from_value(json!({"model":"gpt-6-astra","messages":messages,"tools":tools}))
            .unwrap()
    }

    fn lite_wire(req: &MessagesRequest) -> Value {
        serde_json::to_value(
            translate_request(
                req,
                TranslateOptions {
                    model: "gpt-6-astra".to_string(),
                    use_responses_lite: true,
                    ..opts()
                },
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn base_instructions_follow_tools_and_precede_system() {
        let mut req = tool_search_request(false, false);
        req.extra.insert("system".into(), json!("CLIENT SYSTEM"));
        let options = || TranslateOptions {
            use_responses_lite: true,
            ..opts()
        };
        let without = translate_request(&req, options()).unwrap();
        let with = translate_request_with_base(&req, options(), Some("SYNTHETIC BASE")).unwrap();
        let before = serde_json::to_value(without).unwrap();
        let after = serde_json::to_value(with).unwrap();
        let old = before["input"].as_array().unwrap();
        let input = after["input"].as_array().unwrap();
        let template = "SYNTHETIC BASE";
        let has_extra_item = input.len() == old.len() + 1;
        assert!(has_extra_item, "base adds one input item");
        let tools_first = input[0]["type"] == "additional_tools";
        assert!(tools_first, "tools precede base instructions");
        let template_matches = input[1]["content"][0]["text"] == template;
        assert!(template_matches, "base instructions match template");
        let remaining_matches = input[2..] == old[1..];
        assert!(remaining_matches, "remaining input is unchanged");
    }

    #[test]
    fn base_instructions_require_forced_tool_to_be_callable() {
        let mut req = tool_search_request(false, false);
        req.extra.insert(
            "tool_choice".into(),
            json!({"type":"tool", "name":"missing"}),
        );
        let options = || TranslateOptions {
            use_responses_lite: true,
            ..opts()
        };
        let before = serde_json::to_value(translate_request(&req, options()).unwrap()).unwrap();
        let after = serde_json::to_value(
            translate_request_with_base(&req, options(), Some("SYNTHETIC BASE")).unwrap(),
        )
        .unwrap();
        let unchanged = before == after;
        assert!(unchanged, "request is unchanged");
        req.extra
            .insert("tool_choice".into(), json!({"type":"tool"}));
        let before = translate_request(&req, options()).unwrap();
        let after = translate_request_with_base(&req, options(), Some("SYNTHETIC BASE")).unwrap();
        let unchanged_length = before.input.len() == after.input.len();
        assert!(unchanged_length, "input length is unchanged");
        req.extra
            .insert("tool_choice".into(), json!({"type":"tool", "name":"Bash"}));
        let without = translate_request(&req, options()).unwrap();
        let with = translate_request_with_base(&req, options(), Some("SYNTHETIC BASE")).unwrap();
        let has_extra_item = with.input.len() == without.input.len() + 1;
        assert!(has_extra_item, "base adds one input item");
    }

    #[test]
    fn base_instructions_skip_ineligible_requests() {
        let base = tool_search_request(false, false);
        let mut cases = Vec::new();
        let mut no_tools = base.clone();
        no_tools.extra.remove("tools");
        cases.push((no_tools, true));
        let mut placeholder = base.clone();
        placeholder.extra.insert("tools".into(), json!([{"name":"DeferredToolPlaceholder","defer_loading":true,"input_schema":{"type":"object"}}]));
        cases.push((placeholder, true));
        let mut unnamed = base.clone();
        unnamed.extra.insert(
            "tools".into(),
            json!([{"name":"","input_schema":{"type":"object"}}]),
        );
        cases.push((unnamed, true));
        let mut none = base.clone();
        none.extra
            .insert("tool_choice".into(), json!({"type":"none"}));
        cases.push((none, true));
        let mut compact = base.clone();
        compact.extra.insert(
            "system".into(),
            json!("You are a helpful AI assistant tasked with summarizing conversations"),
        );
        cases.push((compact, true));
        cases.push((base, false));
        for (req, lite) in cases {
            let options = || TranslateOptions {
                use_responses_lite: lite,
                ..opts()
            };
            let before = serde_json::to_value(translate_request(&req, options()).unwrap()).unwrap();
            let after = serde_json::to_value(
                translate_request_with_base(&req, options(), Some("SYNTHETIC BASE")).unwrap(),
            )
            .unwrap();
            let unchanged = before == after;
            assert!(unchanged, "request is unchanged");
        }
    }

    #[test]
    fn base_instructions_apply_when_only_loaded_tools_are_callable() {
        let mut req = tool_search_request(true, false);
        req.extra["tools"]
            .as_array_mut()
            .unwrap()
            .retain(|tool| tool["name"] == "mcp__mail__search");
        let translated = translate_request_with_base(
            &req,
            TranslateOptions {
                use_responses_lite: true,
                ..opts()
            },
            Some("SYNTHETIC BASE"),
        )
        .unwrap();
        let wire = serde_json::to_value(translated).unwrap();
        let template = "SYNTHETIC BASE";
        let template_matches = wire["input"][0]["content"][0]["text"] == template;
        assert!(template_matches, "base instructions match template");
    }

    fn additional_tool_names(item: &Value) -> Vec<String> {
        item["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn loaded_deferred_tool_is_appended_after_its_tool_search_result() {
        let wire = lite_wire(&tool_search_request(true, false));
        let input = wire["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "additional_tools");
        assert_eq!(additional_tool_names(&input[0]), vec!["Bash", "ToolSearch"]);
        let output_at = input
            .iter()
            .position(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "tu_search"
            })
            .unwrap();
        assert_eq!(input[output_at]["output"], "Tool loaded: mcp__mail__search");
        assert_eq!(input[output_at + 1]["type"], "additional_tools");
        assert_eq!(input[output_at + 1]["role"], "developer");
        assert_eq!(
            additional_tool_names(&input[output_at + 1]),
            vec!["mcp__mail__search"]
        );
        assert!(!wire.to_string().contains("DeferredToolPlaceholder"));
    }

    #[test]
    fn tool_load_keeps_the_earlier_request_as_a_prefix() {
        let before = lite_wire(&tool_search_request(false, false));
        let after = lite_wire(&tool_search_request(true, false));
        let later = lite_wire(&tool_search_request(true, true));
        let before_input = before["input"].as_array().unwrap();
        let after_input = after["input"].as_array().unwrap();
        let later_input = later["input"].as_array().unwrap();
        assert_eq!(&after_input[..before_input.len()], &before_input[..]);
        assert_eq!(&later_input[..after_input.len()], &after_input[..]);
    }

    #[test]
    fn deferred_tool_without_a_visible_load_stays_in_the_prefix() {
        let mut req = tool_search_request(false, false);
        req.extra["tools"].as_array_mut().unwrap().push(json!({
            "name":"mcp__mail__search","description":"search mail","defer_loading":true,
            "input_schema":{"type":"object","properties":{}}
        }));
        let wire = lite_wire(&req);
        let input = wire["input"].as_array().unwrap();
        assert_eq!(
            additional_tool_names(&input[0]),
            vec!["Bash", "ToolSearch", "mcp__mail__search"]
        );
        assert_eq!(
            input
                .iter()
                .filter(|item| item["type"] == "additional_tools")
                .count(),
            1
        );
    }

    #[test]
    fn full_lane_keeps_loaded_tools_and_drops_the_placeholder() {
        let out = translate_request(&tool_search_request(true, false), opts()).unwrap();
        let wire = serde_json::to_value(&out).unwrap();
        let names: Vec<&str> = wire["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Bash", "ToolSearch", "mcp__mail__search"]);
        assert!(
            !wire["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "additional_tools")
        );
    }

    #[test]
    fn translate_tool_result_content() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_1",
                "content": [{"type":"text", "text":"result"}]
            }]}]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 1);
        if let ResponsesInputItem::FunctionCallOutput { call_id, output } = &out.input[0] {
            assert_eq!(call_id, "tu_1");
            assert_eq!(output.as_text(), Some("result"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_offset_error_adds_guidance() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 2952, "limit": 200}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "is_error": true,
                    "content": [{"type":"text", "text":"File has 331 lines, but offset 2952 was requested."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            let output = output.as_text().expect("text tool output");
            assert!(output.contains("[tool execution error]"));
            assert!(output.contains("File has 331 lines"));
            assert!(output.contains("Codex Read guidance:"));
            assert!(output.contains("zero based continuation index"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_unrelated_error_keeps_original_output() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 10, "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "is_error": true,
                    "content": [{"type":"text", "text":"File does not exist."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            assert_eq!(
                output.as_text(),
                Some("[tool execution error]\nFile does not exist.")
            );
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_rewritten_read_result_adds_proxy_note() {
        crate::providers::codex::translate::read_rewrite::sanitize_read_args(
            "Read",
            r#"{"file_path":"/tmp/a","offset":1300000,"limit":20}"#,
            Some("tu_rewritten_read"),
        );
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_rewritten_read",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_rewritten_read",
                    "content": [{"type":"text", "text":"1\tcontent"}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            let output = output.as_text().expect("text tool output");
            assert!(output.contains("1\tcontent"));
            assert!(output.contains("Proxy Read offset note:"));
            assert!(output.contains("1300000"));
            assert!(output.contains("/tmp/a"));
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_read_success_with_offset_words_keeps_original_output() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"assistant", "content": [{
                    "type": "tool_use",
                    "id": "tu_1",
                    "name": "Read",
                    "input": {"file_path": "/tmp/a", "offset": 10, "limit": 20}
                }]},
                {"role":"user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "content": [{"type":"text", "text":"File has 331 lines, and the requested offset is shown in this fixture."}]
                }]}
            ]
        }))
        .unwrap();
        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(out.input.len(), 2);
        if let ResponsesInputItem::FunctionCallOutput { output, .. } = &out.input[1] {
            assert_eq!(
                output.as_text(),
                Some("File has 331 lines, and the requested offset is shown in this fixture.")
            );
        } else {
            panic!("expected FunctionCallOutput");
        }
    }

    #[test]
    fn translate_tool_result_preserves_mixed_content_order() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_image",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "image", "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": PNG_BASE64
                    }},
                    {"type": "text", "text": "after"}
                ]
            }]}]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(
            serde_json::to_value(&out.input[0]).unwrap(),
            json!({
                "type": "function_call_output",
                "call_id": "tu_image",
                "output": [
                    {"type": "input_text", "text": "before"},
                    {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_BASE64}")},
                    {"type": "input_text", "text": "after"}
                ]
            })
        );
    }

    #[test]
    fn tool_result_error_prefix_precedes_image_content() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role":"user", "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_error_image",
                "is_error": true,
                "content": [{"type": "image", "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": PNG_BASE64
                }}]
            }]}]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(
            serde_json::to_value(&out.input[0]).unwrap()["output"],
            json!([
                {"type": "input_text", "text": "[tool execution error]"},
                {"type": "input_image", "image_url": format!("data:image/png;base64,{PNG_BASE64}")}
            ])
        );
    }

    #[test]
    fn unsupported_message_blocks_become_markers() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role":"user", "content": [
                    {"type": "text", "text": "summarize"},
                    {"type": "document", "source": {
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": "JVBERi0="
                    }},
                    {"type": "image", "source": {"type": "file", "file_id": "file_1"}}
                ]},
                {"role":"assistant", "content": [
                    {"type": "redacted_thinking", "data": "opaque"},
                    {"type": "thinking", "thinking": "", "signature": ""},
                    {"type": "text", "text": "done"}
                ]}
            ]
        }))
        .unwrap();

        let out = translate_request(&req, opts()).unwrap();
        assert_eq!(
            serde_json::to_value(&out.input).unwrap(),
            json!([
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "summarize"},
                    {"type": "input_text", "text": "[unsupported content block omitted: document]"},
                    {"type": "input_text", "text": "[unsupported content block omitted: image]"}
                ]},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "[unsupported content block omitted: redacted_thinking]"},
                    {"type": "output_text", "text": "done"}
                ]}
            ])
        );
    }

    #[test]
    fn luna_preserves_high_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn sol_preserves_high_effort() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "high"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-sol".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(matches!(out.reasoning.unwrap().effort, Some(Effort::High)));
    }

    #[test]
    fn responses_lite_moves_instructions_and_tools_into_input() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "be helpful",
            "tools": [{"name":"test","input_schema":{"type":"object"}}]
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(out.instructions.is_none());
        assert!(out.tools.is_none());
        assert!(!out.parallel_tool_calls);
        assert!(out.client_metadata.is_some());
        assert_eq!(out.input.len(), 3);
        assert!(matches!(
            out.input[0],
            ResponsesInputItem::AdditionalTools { .. }
        ));
        if let ResponsesInputItem::Message { role, content } = &out.input[1] {
            assert_eq!(role, "developer");
            assert!(matches!(content[0], ResponsesContentPart::InputText { .. }));
        } else {
            panic!("expected developer message");
        }
    }

    #[test]
    fn responses_lite_without_effort_uses_all_turns_context() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-haiku-4-5",
            "messages": [{"role":"user", "content":"hello"}]
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        let reasoning = out.reasoning.unwrap();
        assert!(reasoning.effort.is_none());
        assert!(reasoning.summary.is_none());
        assert_eq!(reasoning.context.as_deref(), Some("all_turns"));
        assert!(out.include.is_none());
    }

    #[test]
    fn responses_lite_reasoning_uses_all_turns_context() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-luna",
            "messages": [{"role":"user", "content":"hello"}],
            "output_config": {"effort": "medium"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.6-luna".to_string(),
                use_responses_lite: true,
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(out.reasoning.unwrap().context.as_deref(), Some("all_turns"));
    }

    #[test]
    fn translate_returns_only_expected_top_level_fields() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role":"user", "content":"hello"}],
            "system": "be helpful",
            "tools": [{"name":"test","input_schema":{"type":"object"}}],
            "tool_choice": {"type":"tool", "name":"test"}
        }))
        .unwrap();
        let out = translate_request(
            &req,
            TranslateOptions {
                model: "gpt-5.4".to_string(),
                ..opts()
            },
        )
        .unwrap();
        assert_eq!(out.model, "gpt-5.4");
        let out_value = serde_json::to_value(&out).unwrap();
        let keys: std::collections::BTreeSet<String> =
            out_value.as_object().unwrap().keys().cloned().collect();
        for key in &[
            "model",
            "input",
            "store",
            "stream",
            "parallel_tool_calls",
            "text",
        ] {
            assert!(keys.contains(*key), "missing key: {key}");
        }
    }

    #[test]
    fn thinking_reaches_codex_as_the_reasoning_table_says() {
        let replay = super::super::reasoning_signature::ReasoningReplay {
            id: "rs_1".to_string(),
            encrypted_content: "opaque".to_string(),
        };
        let account = ReasoningAccount::new(Some("acct"));
        let same = account.sign(&replay).unwrap();
        let other = ReasoningAccount::new(Some("acct_b")).sign(&replay).unwrap();
        let reasoning =
            json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque"});
        let message = |texts: &[&str]| {
            let content: Vec<Value> = texts
                .iter()
                .map(|text| json!({"type":"output_text","text":text}))
                .collect();
            json!({"type":"message","role":"assistant","content":content})
        };
        let plan = wrap_reasoning("plan");
        let cases = [
            (
                "anthropic signature",
                "plan",
                Some("EqQBsig"),
                &account,
                vec![message(&[&plan, "391"])],
            ),
            (
                "anthropic signature, empty",
                "",
                Some("EqQBsig"),
                &account,
                vec![message(&["391"])],
            ),
            (
                "codex signature, same account",
                "plan",
                Some(same.as_str()),
                &account,
                vec![reasoning.clone(), message(&["391"])],
            ),
            (
                "codex signature only, same account",
                "",
                Some(same.as_str()),
                &account,
                vec![reasoning.clone(), message(&["391"])],
            ),
            (
                "codex signature, other account",
                "plan",
                Some(other.as_str()),
                &account,
                vec![message(&[&plan, "391"])],
            ),
            (
                "codex signature, other account, empty",
                "",
                Some(other.as_str()),
                &account,
                vec![message(&["391"])],
            ),
            (
                "codex signature, unknown account",
                "plan",
                Some(same.as_str()),
                &ReasoningAccount::default(),
                vec![message(&[&plan, "391"])],
            ),
            (
                "no signature",
                "plan",
                None,
                &account,
                vec![message(&[&plan, "391"])],
            ),
            (
                "no signature, empty",
                "",
                Some(""),
                &account,
                vec![message(&["391"])],
            ),
        ];
        for (name, thinking, signature, current, expected) in cases {
            let mut block = json!({"type":"thinking","thinking":thinking});
            if let Some(signature) = signature {
                block["signature"] = json!(signature);
            }
            let req: MessagesRequest = serde_json::from_value(json!({
                "model": "gpt-5.5",
                "messages": [{"role":"assistant","content":[block, {"type":"text","text":"391"}]}]
            }))
            .unwrap();
            let opts = TranslateOptions {
                reasoning_account: current.clone(),
                ..opts()
            };
            let out = translate_request(&req, opts).unwrap();
            assert_eq!(
                serde_json::to_value(&out.input).unwrap(),
                json!(expected),
                "{name}"
            );
        }
    }
}
