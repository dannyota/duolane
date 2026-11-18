use super::request::*;
use super::request_input::*;
use crate::anthropic::schema::MessagesRequest;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;

pub fn normalize_strict_json_schema(schema: &Value) -> Value {
    match schema {
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_strict_json_schema).collect()),
        Value::Object(map) => {
            let mut out = map.clone();
            if let Some(properties) = out.get("properties").and_then(|v| v.as_object()) {
                let keys: Vec<String> = properties.keys().cloned().collect();
                out.insert(
                    "required".into(),
                    Value::Array(keys.into_iter().map(Value::String).collect()),
                );
            }
            for (key, val) in out.clone().iter() {
                out.insert(key.clone(), normalize_strict_json_schema(val));
            }
            Value::Object(out)
        }
        _ => schema.clone(),
    }
}

pub fn has_hosted_web_search(req: &MessagesRequest) -> bool {
    req.extra
        .get("tools")
        .and_then(|v| v.as_array())
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("type").and_then(|v| v.as_str()) == Some("web_search_20250305")
            })
        })
}

const DEFERRED_TOOL_PLACEHOLDER: &str = "DeferredToolPlaceholder";

fn responses_tool_name(tool: &ResponsesTool) -> Option<&str> {
    match tool {
        ResponsesTool::Function(function) => Some(function.name.as_str()),
        ResponsesTool::WebSearch(_) => None,
    }
}

pub(super) fn deferred_tool_names(req: &MessagesRequest) -> HashSet<String> {
    req.extra
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| tool.get("defer_loading").and_then(Value::as_bool) == Some(true))
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

pub(super) fn is_deferred_placeholder(tool: &ResponsesTool, deferred: &HashSet<String>) -> bool {
    responses_tool_name(tool)
        .is_some_and(|name| name == DEFERRED_TOOL_PLACEHOLDER && deferred.contains(name))
}

pub(super) fn tool_reference_loads(req: &MessagesRequest) -> HashMap<String, Vec<String>> {
    let mut loads: HashMap<String, Vec<String>> = HashMap::new();
    for msg in req.messages.iter().filter(|msg| msg.role == "user") {
        let Some(blocks) = msg.content.as_array() else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let names = block
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|child| child.get("type").and_then(Value::as_str) == Some("tool_reference"))
                .filter_map(|child| child.get("tool_name").and_then(Value::as_str));
            loads
                .entry(id.to_string())
                .or_default()
                .extend(names.map(str::to_string));
        }
    }
    loads.retain(|_, names| !names.is_empty());
    loads
}

pub(super) fn split_loaded_deferred_tools(
    req: &MessagesRequest,
    tools: &[ResponsesTool],
    loads: &HashMap<String, Vec<String>>,
) -> Result<(Vec<Value>, HashMap<String, Value>), anyhow::Error> {
    let deferred = deferred_tool_names(req);
    let loaded: HashSet<&str> = loads.values().flatten().map(String::as_str).collect();
    let mut prefix = Vec::new();
    let mut appended = HashMap::new();
    for tool in tools {
        if is_deferred_placeholder(tool, &deferred) {
            continue;
        }
        let value = serde_json::to_value(tool)?;
        match responses_tool_name(tool) {
            Some(name) if deferred.contains(name) && loaded.contains(name) => {
                appended.insert(name.to_string(), value);
            }
            _ => prefix.push(value),
        }
    }
    Ok((prefix, appended))
}

pub(super) fn append_loaded_tools(
    input: &mut Vec<ResponsesInputItem>,
    loads: &HashMap<String, Vec<String>>,
    mut loaded_tools: HashMap<String, Value>,
) {
    if loaded_tools.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(input.len() + loads.len());
    for item in input.drain(..) {
        let names = match &item {
            ResponsesInputItem::FunctionCallOutput { call_id, .. } => loads.get(call_id),
            _ => None,
        };
        out.push(item);
        let tools: Vec<Value> = names
            .into_iter()
            .flatten()
            .filter_map(|name| loaded_tools.remove(name))
            .collect();
        if !tools.is_empty() {
            out.push(ResponsesInputItem::AdditionalTools {
                id: None,
                role: "developer".to_string(),
                tools,
            });
        }
    }
    debug_assert!(
        loaded_tools.is_empty(),
        "every loaded tool should follow the tool_result that loaded it"
    );
    *input = out;
}

pub(super) fn read_tools(
    req: &MessagesRequest,
) -> Result<Option<Vec<ResponsesTool>>, anyhow::Error> {
    let Some(tools) = req.extra.get("tools") else {
        return Ok(None);
    };
    let tools_arr = match tools {
        Value::Array(a) => a,
        _ => return Ok(None),
    };
    let mut out = Vec::new();
    for tool in tools_arr {
        let tool_type = tool
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("function");
        if tool_type == "web_search_20250305" {
            let mut filters = ResponsesWebSearchFilters {
                allowed_domains: None,
                blocked_domains: None,
            };
            let allowed = tool.get("allowed_domains").and_then(|v| v.as_array());
            if allowed.is_some_and(|a| !a.is_empty()) {
                filters.allowed_domains = allowed.map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
            }
            let blocked = tool.get("blocked_domains").and_then(|v| v.as_array());
            if blocked.is_some_and(|a| !a.is_empty()) {
                filters.blocked_domains = blocked.map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
            }
            let has_filters =
                filters.allowed_domains.is_some() || filters.blocked_domains.is_some();
            out.push(ResponsesTool::WebSearch(ResponsesWebSearchTool {
                kind: "web_search".to_string(),
                external_web_access: true,
                search_content_types: vec!["text".to_string(), "image".to_string()],
                filters: if has_filters { Some(filters) } else { None },
            }));
        } else {
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let parameters = tool
                .get("input_schema")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let description = codex_tool_description(&name, description);
            let parameters = codex_tool_parameters(&name, parameters);
            out.push(ResponsesTool::Function(ResponsesFunctionTool {
                kind: "function".to_string(),
                name,
                description,
                parameters,
                strict: false,
            }));
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

fn codex_tool_description(name: &str, description: Option<String>) -> Option<String> {
    if name != "Read" {
        return description;
    }

    let base = description.unwrap_or_else(|| "Reads a file from the local filesystem.".to_string());
    Some(format!("{base}\n\n{}", read_offset_guidance()))
}

fn strip_tool_schema_patterns(schema: &mut Value) {
    let Some(schema) = schema.as_object_mut() else {
        return;
    };
    schema.remove("pattern");

    for (keyword, value) in schema {
        match keyword.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas"
            | "dependencies" => {
                if let Some(schemas) = value.as_object_mut() {
                    for schema in schemas.values_mut() {
                        strip_tool_schema_patterns(schema);
                    }
                }
            }
            "allOf" | "anyOf" | "oneOf" | "prefixItems" | "items" => {
                if let Some(schemas) = value.as_array_mut() {
                    for schema in schemas {
                        strip_tool_schema_patterns(schema);
                    }
                } else if keyword == "items" {
                    strip_tool_schema_patterns(value);
                }
            }
            "additionalProperties"
            | "additionalItems"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "contains"
            | "propertyNames"
            | "not"
            | "if"
            | "then"
            | "else"
            | "contentSchema" => strip_tool_schema_patterns(value),
            _ => {}
        }
    }
}

fn codex_tool_parameters(name: &str, mut parameters: Value) -> Value {
    strip_tool_schema_patterns(&mut parameters);
    if name != "Read" {
        return parameters;
    }

    let Some(props) = parameters
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    else {
        return parameters;
    };

    if let Some(offset) = props.get_mut("offset").and_then(Value::as_object_mut) {
        offset.insert(
            "description".to_string(),
            Value::String(
                "Optional continuation index. Use only after a prior Read of the same file returned content and more lines are needed. Compute as prior offset plus returned line count. Displayed line numbers, grep line numbers, byte counts, token counts, file sizes, and guessed positions are invalid offsets. Omit when unsure.".to_string(),
            ),
        );
    }

    if let Some(limit) = props.get_mut("limit").and_then(Value::as_object_mut) {
        limit.insert(
            "description".to_string(),
            Value::String(
                "Optional number of lines to read. Omit when opening a file. Use with offset only when continuing a large file."
                    .to_string(),
            ),
        );
    }

    parameters
}

pub(super) fn map_tool_choice(
    req: &MessagesRequest,
) -> Result<Option<ResponsesToolChoice>, anyhow::Error> {
    let choice = match req.extra.get("tool_choice") {
        Some(Value::Object(m)) => m,
        Some(Value::String(s)) => {
            return Ok(Some(match s.as_str() {
                "auto" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
                "none" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::None),
                "any" | "required" => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Required),
                _ => ResponsesToolChoice::Mode(ResponsesToolChoiceMode::Auto),
            }));
        }
        _ => return Ok(None),
    };

    let choice_type = choice
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("auto");
    match choice_type {
        "auto" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::Auto,
        ))),
        "none" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::None,
        ))),
        "any" | "required" => Ok(Some(ResponsesToolChoice::Mode(
            ResponsesToolChoiceMode::Required,
        ))),
        "tool" => {
            let name = choice.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let tools = req.extra.get("tools").and_then(|v| v.as_array());
            let is_web_search = tools.is_some_and(|t| {
                t.iter().any(|tool| {
                    (tool.get("type").and_then(|v| v.as_str()) == Some("web_search_20250305"))
                        && tool.get("name").and_then(|v| v.as_str()) == Some(name)
                })
            });
            if is_web_search {
                Ok(Some(ResponsesToolChoice::AllowedTools {
                    r#type: "allowed_tools".to_string(),
                    mode: "required".to_string(),
                    tools: vec![serde_json::json!({"type": "web_search"})],
                }))
            } else {
                Ok(Some(ResponsesToolChoice::Function {
                    r#type: "function".to_string(),
                    name: name.to_string(),
                }))
            }
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn has_hosted_web_search_detects_web_search_tool() {
        let with: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"find it"}],
            "tools": [
                {"name":"Bash", "input_schema":{}},
                {"type":"web_search_20250305", "name":"web_search"}
            ]
        }))
        .unwrap();
        assert!(has_hosted_web_search(&with));

        let without: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role":"user", "content":"run it"}],
            "tools": [{"name":"Bash", "input_schema":{}}]
        }))
        .unwrap();
        assert!(!has_hosted_web_search(&without));
    }

    #[test]
    fn tool_schema_patterns_are_removed_only_from_schema_locations() {
        let literal = json!({
            "pattern": "literal pattern",
            "properties": {"pattern": {"pattern": "also literal"}},
            "items": [{"pattern": "literal item"}]
        });
        let leaf = json!({
            "type": "string",
            "pattern": "^[a-z]+$",
            "description": "Keep metadata and literal data.",
            "default": literal,
            "examples": [literal],
            "const": literal,
            "enum": [literal],
            "x-custom": literal
        });
        let mut clean_leaf = leaf.clone();
        clean_leaf.as_object_mut().unwrap().remove("pattern");

        let mut schema = json!({"pattern": "^root$", "required": ["pattern"]});
        let mut expected = json!({"required": ["pattern"]});
        for keyword in [
            "properties",
            "patternProperties",
            "$defs",
            "definitions",
            "dependentSchemas",
            "dependencies",
        ] {
            schema[keyword] = json!({
                "pattern": leaf,
                "properties": {"properties": {"pattern": leaf}},
                "booleanSchema": false
            });
            expected[keyword] = json!({
                "pattern": clean_leaf,
                "properties": {"properties": {"pattern": clean_leaf}},
                "booleanSchema": false
            });
        }
        for keyword in ["allOf", "anyOf", "oneOf", "prefixItems", "items"] {
            schema[keyword] = json!([leaf, true, false]);
            expected[keyword] = json!([clean_leaf, true, false]);
        }
        for keyword in [
            "additionalProperties",
            "additionalItems",
            "unevaluatedProperties",
            "unevaluatedItems",
            "contains",
            "propertyNames",
            "not",
            "if",
            "then",
            "else",
            "contentSchema",
        ] {
            schema[keyword] = leaf.clone();
            expected[keyword] = clean_leaf.clone();
        }
        schema["dependencies"]["literalDependency"] = json!(["pattern"]);
        expected["dependencies"]["literalDependency"] = json!(["pattern"]);
        schema["$ref"] = json!("#/$defs/pattern");
        expected["$ref"] = json!("#/$defs/pattern");
        assert_eq!(codex_tool_parameters("Custom", schema), expected);

        let nested = json!({"items": {"properties": {"pattern": leaf}}});
        let expected = json!({"items": {"properties": {"pattern": clean_leaf}}});
        assert_eq!(codex_tool_parameters("Custom", nested), expected);
        for unchanged in [json!({}), json!(true), json!(false)] {
            assert_eq!(
                codex_tool_parameters("Custom", unchanged.clone()),
                unchanged
            );
        }
    }
}
