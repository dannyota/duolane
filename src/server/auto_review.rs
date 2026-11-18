use serde_json::Value;

const CLAUDE_AUTO_REVIEW_SYSTEM_PREFIX: &str =
    "You are a security monitor for autonomous AI coding agents.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AutoReviewRoute {
    pub(super) requested_model: String,
    pub(super) override_model: String,
}

fn is_claude_auto_review_request(body: &crate::anthropic::schema::MessagesRequest) -> bool {
    if body.stream {
        return false;
    }

    let has_tools = body
        .extra
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if has_tools {
        return false;
    }

    body.extra
        .get("system")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with(CLAUDE_AUTO_REVIEW_SYSTEM_PREFIX))
            })
        })
}

pub(super) fn apply_auto_review_model(
    body: &mut crate::anthropic::schema::MessagesRequest,
    count_tokens: bool,
    configured_model: Option<&str>,
    original_provider: &str,
) -> Option<AutoReviewRoute> {
    if count_tokens || !is_claude_auto_review_request(body) {
        return None;
    }

    let override_model = configured_model
        .filter(|model| !model.is_empty())
        .or_else(|| {
            (original_provider == "codex").then(|| {
                crate::providers::codex::translate::model_allowlist::alias_target("haiku")
                    .expect("haiku is a codex alias")
            })
        })?;
    let route = AutoReviewRoute {
        requested_model: body.model.clone()?,
        override_model: override_model.to_string(),
    };
    body.model = Some(route.override_model.clone());
    Some(route)
}

#[cfg(test)]
mod auto_review_tests {
    use super::{apply_auto_review_model, is_claude_auto_review_request};
    use crate::anthropic::schema::MessagesRequest;
    use crate::request_identity::{CLAUDE_AGENT_HEADER, CLAUDE_PARENT_AGENT_HEADER};
    use crate::server::request_monitor::headers_to_record;
    use http::{HeaderMap, HeaderValue};
    use serde_json::json;

    fn request(system: &str, stream: bool, tools: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "max_tokens": 2112,
            "stream": stream,
            "system": [{"type": "text", "text": system}],
            "messages": [{"role": "user", "content": "review this Bash command"}],
            "tools": tools
        }))
        .unwrap()
    }

    #[test]
    fn detects_claude_auto_review_classifier() {
        let body = request(
            "You are a security monitor for autonomous AI coding agents.\n\n## Context",
            false,
            json!([]),
        );
        assert!(is_claude_auto_review_request(&body));
    }

    #[test]
    fn ignores_normal_streaming_and_tool_using_requests() {
        assert!(!is_claude_auto_review_request(&request(
            "You are an interactive coding agent.",
            false,
            json!([]),
        )));
        assert!(!is_claude_auto_review_request(&request(
            "You are a security monitor for autonomous AI coding agents.",
            true,
            json!([]),
        )));
        assert!(!is_claude_auto_review_request(&request(
            "You are a security monitor for autonomous AI coding agents.",
            false,
            json!([{"name": "Bash"}]),
        )));
    }

    #[test]
    fn codex_classifier_defaults_to_luna() {
        let mut classifier = request(
            "You are a security monitor for autonomous AI coding agents.",
            false,
            json!([]),
        );
        let route = apply_auto_review_model(&mut classifier, false, None, "codex")
            .expect("classifier should be routed");
        assert_eq!(route.requested_model, "gpt-5.6-sol");
        assert_eq!(route.override_model, "gpt-6-luna");
        assert_eq!(classifier.model.as_deref(), Some("gpt-6-luna"));
    }

    #[test]
    fn non_codex_classifier_keeps_requested_model_without_override() {
        let mut classifier = request(
            "You are a security monitor for autonomous AI coding agents.",
            false,
            json!([]),
        );
        classifier.model = Some("claude-sonnet-5-5".to_string());
        assert!(apply_auto_review_model(&mut classifier, false, None, "anthropic").is_none());
        assert_eq!(classifier.model.as_deref(), Some("claude-sonnet-5-5"));
    }

    #[test]
    fn configured_model_overrides_provider_default() {
        let mut classifier = request(
            "You are a security monitor for autonomous AI coding agents.",
            false,
            json!([]),
        );
        let route = apply_auto_review_model(&mut classifier, false, Some("gpt-5.6-terra"), "codex")
            .expect("configured classifier should be routed");
        assert_eq!(route.override_model, "gpt-5.6-terra");
        assert_eq!(classifier.model.as_deref(), Some("gpt-5.6-terra"));
    }

    #[test]
    fn anthropic_session_classifier_routes_by_requested_model_or_explicit_override() {
        use crate::config::AliasProvider;
        use crate::registry::Registry;
        use crate::session::{record_session_request, record_session_request_with_affinity_update};

        let registry = Registry::new(AliasProvider::Anthropic);
        let session_id = "anthropic-session-classifier-routing";
        let initial =
            record_session_request(Some(session_id), None, "anthropic", "claude-opus-5-5", 1)
                .expect("Anthropic session");
        assert_eq!(initial.affinity_provider, None);

        for (requested, configured, expected_provider, expected_model) in [
            ("claude-sonnet-4-6", None, "anthropic", "claude-sonnet-4-6"),
            ("gpt-6-luna", None, "codex", "gpt-6-luna"),
            (
                "claude-sonnet-4-6",
                Some("gpt-5.6-terra"),
                "codex",
                "gpt-5.6-terra",
            ),
            (
                "gpt-6-luna",
                Some("claude-haiku-5-5"),
                "anthropic",
                "claude-haiku-5-5",
            ),
        ] {
            let mut classifier = request(
                "You are a security monitor for autonomous AI coding agents.",
                false,
                json!([]),
            );
            classifier.model = Some(requested.to_string());
            let original = registry
                .provider_for_model(requested, initial.affinity_provider.as_ref())
                .expect("original provider");
            let route =
                apply_auto_review_model(&mut classifier, false, configured, original.name());
            let model = classifier.model.as_deref().expect("classifier model");
            let provider = if route.is_some() {
                registry.provider_for_model(model, None).expect("override")
            } else {
                original
            };
            assert_eq!(provider.name(), expected_provider);
            assert_eq!(model, expected_model);

            let after_review = record_session_request_with_affinity_update(
                Some(session_id),
                Some(&initial),
                provider.name(),
                model,
                route.is_none(),
                2,
            )
            .expect("session after review");
            assert_eq!(after_review.affinity_provider, None);
        }
    }

    #[test]
    fn traffic_headers_redact_agent_lineage_ids() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            HeaderValue::from_static("session-visible"),
        );
        headers.insert(
            CLAUDE_AGENT_HEADER,
            HeaderValue::from_static("agent-secret"),
        );
        headers.insert(
            CLAUDE_PARENT_AGENT_HEADER,
            HeaderValue::from_static("parent-secret"),
        );

        let captured = headers_to_record(&headers);
        assert_eq!(
            captured["x-claude-code-session-id"],
            json!("session-visible")
        );
        assert_eq!(captured[CLAUDE_AGENT_HEADER], json!("[redacted len=12]"));
        assert_eq!(
            captured[CLAUDE_PARENT_AGENT_HEADER],
            json!("[redacted len=13]")
        );
        let serialized = captured.to_string();
        assert!(!serialized.contains("agent-secret"));
        assert!(!serialized.contains("parent-secret"));
    }

    #[test]
    fn count_tokens_keeps_requested_model() {
        let mut classifier = request(
            "You are a security monitor for autonomous AI coding agents.",
            false,
            json!([]),
        );
        assert!(
            apply_auto_review_model(&mut classifier, true, Some("gpt-5.6-terra"), "codex")
                .is_none()
        );
        assert_eq!(classifier.model.as_deref(), Some("gpt-5.6-sol"));
    }
}
