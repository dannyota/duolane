use super::model_allowlist::service_tier_for_model;
use super::request::*;
use crate::anthropic::schema::MessagesRequest;
use crate::config;
use crate::providers::translate_shared::flatten_system_text;
use serde_json::Value;

pub(crate) fn to_codex_effort(effort: Option<&str>) -> Option<Effort> {
    match effort {
        Some("max") => Some(Effort::Max),
        Some("xhigh") => Some(Effort::Xhigh),
        Some("low") => Some(Effort::Low),
        Some("medium") => Some(Effort::Medium),
        Some("high") => Some(Effort::High),
        _ => None,
    }
}

pub(super) fn resolve_effort(effort: Option<Effort>) -> Result<Option<Effort>, anyhow::Error> {
    resolve_effort_override(effort, config::codex_effort().as_deref())
}

pub(crate) fn resolve_effort_override(
    effort: Option<Effort>,
    override_effort: Option<&str>,
) -> Result<Option<Effort>, anyhow::Error> {
    if let Some(val) = override_effort {
        let valid = ["none", "low", "medium", "high", "xhigh", "max"];
        if !valid.contains(&val) {
            anyhow::bail!(
                "Invalid effort override: \"{val}\". Must be one of: none, low, medium, high, xhigh, max"
            );
        }
        return Ok(Some(match val {
            "max" => Effort::Max,
            "xhigh" => Effort::Xhigh,
            "high" => Effort::High,
            "medium" => Effort::Medium,
            "low" => Effort::Low,
            _ => Effort::None,
        }));
    }
    Ok(effort)
}

pub(super) fn reasoning_summary_requested(summary: Option<&str>) -> bool {
    !matches!(summary, Some("off" | "none"))
}

pub(super) fn reasoning_requested(effort: Option<&Effort>) -> bool {
    effort.is_some_and(|effort| *effort != Effort::None)
}

const COMPACT_SYSTEM_MARKER: &str =
    "You are a helpful AI assistant tasked with summarizing conversations";

const COMPACT_MESSAGE_PREFIX: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";

const COMPACT_MESSAGE_TASK: &str =
    "Your task is to create a detailed summary of the conversation so far";

pub(crate) fn is_compact_request(instructions: Option<&str>) -> bool {
    instructions.is_some_and(|text| text.contains(COMPACT_SYSTEM_MARKER))
}

pub(crate) fn is_compact_message_text(text: &str) -> bool {
    text.contains(COMPACT_MESSAGE_PREFIX) && text.contains(COMPACT_MESSAGE_TASK)
}

fn is_compact_message_content(content: &Value) -> bool {
    match content {
        Value::String(text) => is_compact_message_text(text),
        Value::Array(blocks) => blocks.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("text")
                && block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(is_compact_message_text)
        }),
        _ => false,
    }
}

pub(crate) fn is_compact_messages_request(request: &MessagesRequest) -> bool {
    is_compact_request(flatten_system_text(request.extra.get("system")).as_deref())
        || request
            .messages
            .iter()
            .rev()
            .find(|message| message.role != "system")
            .is_some_and(|message| {
                message.role == "user" && is_compact_message_content(&message.content)
            })
}

pub(super) fn compact_effort_cap() -> Option<Effort> {
    compact_effort_cap_from(std::env::var("DUOLANE_COMPACT_EFFORT").ok().as_deref())
}

pub(super) fn apply_compact_effort_cap(
    resolved: Option<Effort>,
    cap: Option<Effort>,
) -> Option<Effort> {
    match (resolved, cap) {
        (resolved, None) => resolved,
        (Some(effort), Some(cap)) if effort <= cap => Some(effort),
        (_, Some(cap)) => Some(cap),
    }
}

fn compact_effort_cap_from(raw: Option<&str>) -> Option<Effort> {
    match raw {
        None | Some("") => Some(Effort::Low),
        Some("off") => None,
        Some("none") => Some(Effort::None),
        Some(other) => to_codex_effort(Some(other)).or(Some(Effort::Low)),
    }
}

const VALID_SERVICE_TIERS: &[&str] = &["fast", "priority", "ultrafast", "flex"];

fn normalize_service_tier(tier: &str) -> Result<ServiceTier, anyhow::Error> {
    match tier {
        "fast" | "priority" => Ok(ServiceTier::Priority),
        "ultrafast" => Ok(ServiceTier::Ultrafast),
        "flex" => Ok(ServiceTier::Flex),
        _ => anyhow::bail!(
            "Invalid service tier override: \"{tier}\". Must be one of: {}",
            VALID_SERVICE_TIERS.join(", ")
        ),
    }
}

pub(super) fn resolve_service_tier(
    model_tier: Option<ServiceTier>,
    model: &str,
) -> Result<Option<ServiceTier>, anyhow::Error> {
    pick_service_tier(config::codex_service_tier().as_deref(), model_tier, model)
}

fn pick_service_tier(
    override_tier: Option<&str>,
    model_tier: Option<ServiceTier>,
    model: &str,
) -> Result<Option<ServiceTier>, anyhow::Error> {
    let tier = match override_tier {
        Some(value) => Some(normalize_service_tier(value)?),
        None => model_tier,
    };
    Ok(tier.map(|tier| service_tier_for_model(model, tier)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn service_tier_override_values_normalize_to_wire_tiers() {
        for (value, tier, wire) in [
            ("fast", ServiceTier::Priority, "priority"),
            ("priority", ServiceTier::Priority, "priority"),
            ("ultrafast", ServiceTier::Ultrafast, "ultrafast"),
            ("flex", ServiceTier::Flex, "flex"),
        ] {
            let normalized = normalize_service_tier(value).unwrap();
            assert_eq!(normalized, tier, "{value}");
            assert_eq!(serde_json::to_value(normalized).unwrap(), json!(wire));
        }
        for value in ["", "default", "Ultrafast", "ultra-fast", "turbo"] {
            let error = normalize_service_tier(value).unwrap_err().to_string();
            assert!(error.contains("ultrafast"), "{value}: {error}");
        }
    }

    #[test]
    fn service_tier_override_wins_and_narrows_to_the_model() {
        let cases = [
            (None, None, "gpt-6-astra", None),
            (
                None,
                Some(ServiceTier::Ultrafast),
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                None,
                Some(ServiceTier::Ultrafast),
                "gpt-6-sol",
                Some(ServiceTier::Priority),
            ),
            (
                Some("ultrafast"),
                None,
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                Some("ultrafast"),
                Some(ServiceTier::Priority),
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                Some("ultrafast"),
                None,
                "gpt-6-luna",
                Some(ServiceTier::Priority),
            ),
            (
                Some("fast"),
                Some(ServiceTier::Ultrafast),
                "gpt-6-astra",
                Some(ServiceTier::Priority),
            ),
            (
                Some("flex"),
                Some(ServiceTier::Ultrafast),
                "gpt-6-astra",
                Some(ServiceTier::Flex),
            ),
        ];
        for (override_tier, model_tier, model, expected) in cases {
            let picked = pick_service_tier(override_tier, model_tier.clone(), model).unwrap();
            assert_eq!(picked, expected, "{override_tier:?} {model_tier:?} {model}");
        }
        assert!(pick_service_tier(Some("turbo"), None, "gpt-6-astra").is_err());
    }

    #[test]
    fn translate_effort_override_max_maps_to_max() {
        let effort = resolve_effort_override(Some(Effort::Low), Some("max")).unwrap();
        assert!(matches!(effort, Some(Effort::Max)));
    }

    #[test]
    fn compact_request_detected_from_system_marker() {
        assert!(is_compact_request(Some(
            "You are a helpful AI assistant tasked with summarizing conversations."
        )));
        assert!(!is_compact_request(Some("You are Claude Code.")));
        assert!(!is_compact_request(None));
    }

    #[test]
    fn compact_request_detected_from_final_user_message() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {"role": "user", "content": "prior turn"},
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tool-1",
                            "content": "result"
                        },
                        {
                            "type": "text",
                            "text": concat!(
                                "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n",
                                "Your task is to create a detailed summary of the conversation so far, ",
                                "paying close attention to the user's explicit requests."
                            )
                        }
                    ]
                }
            ],
            "system": "You are Claude Code."
        }))
        .unwrap();

        assert!(is_compact_messages_request(&req));
    }

    #[test]
    fn compact_message_markers_must_be_in_final_user_message() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [
                {
                    "role": "user",
                    "content": concat!(
                        "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n",
                        "Your task is to create a detailed summary of the conversation so far."
                    )
                },
                {"role": "user", "content": "continue normally"}
            ],
            "system": "You are Claude Code."
        }))
        .unwrap();

        assert!(!is_compact_messages_request(&req));
    }

    #[test]
    fn compact_message_requires_both_markers() {
        let req: MessagesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "messages": [{
                "role": "user",
                "content": "Your task is to create a detailed summary of the conversation so far."
            }],
            "system": "You are Claude Code."
        }))
        .unwrap();

        assert!(!is_compact_messages_request(&req));
    }

    #[test]
    fn compact_effort_cap_parses_env_values() {
        assert!(matches!(compact_effort_cap_from(None), Some(Effort::Low)));
        assert!(matches!(
            compact_effort_cap_from(Some("")),
            Some(Effort::Low)
        ));
        assert!(compact_effort_cap_from(Some("off")).is_none());
        assert!(matches!(
            compact_effort_cap_from(Some("none")),
            Some(Effort::None)
        ));
        assert!(matches!(
            compact_effort_cap_from(Some("medium")),
            Some(Effort::Medium)
        ));
        assert!(matches!(
            compact_effort_cap_from(Some("bogus")),
            Some(Effort::Low)
        ));
    }

    #[test]
    fn compact_effort_cap_defaults_a_missing_effort() {
        assert!(matches!(
            apply_compact_effort_cap(None, Some(Effort::Low)),
            Some(Effort::Low)
        ));
        assert!(matches!(
            apply_compact_effort_cap(None, Some(Effort::None)),
            Some(Effort::None)
        ));
        assert!(matches!(
            apply_compact_effort_cap(Some(Effort::None), Some(Effort::Low)),
            Some(Effort::None)
        ));
        assert!(matches!(
            apply_compact_effort_cap(Some(Effort::Low), Some(Effort::Low)),
            Some(Effort::Low)
        ));
        assert!(matches!(
            apply_compact_effort_cap(Some(Effort::High), Some(Effort::Low)),
            Some(Effort::Low)
        ));
        assert!(apply_compact_effort_cap(None, None).is_none());
        assert!(matches!(
            apply_compact_effort_cap(Some(Effort::High), None),
            Some(Effort::High)
        ));
    }

    #[test]
    fn only_a_non_none_effort_requests_reasoning_artifacts() {
        assert!(!reasoning_requested(None));
        assert!(!reasoning_requested(Some(&Effort::None)));
        for effort in [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ] {
            assert!(reasoning_requested(Some(&effort)));
        }
    }

    #[test]
    fn reasoning_summary_override_values() {
        assert!(reasoning_summary_requested(None));
        assert!(reasoning_summary_requested(Some("auto")));
        assert!(reasoning_summary_requested(Some("detailed")));
        assert!(!reasoning_summary_requested(Some("off")));
        assert!(!reasoning_summary_requested(Some("none")));
    }
}
