use crate::anthropic::schema::MessagesRequest;
use crate::logging::create_logger;
use crate::provider::RequestContext;
#[cfg(test)]
use crate::request_identity::ConversationIdentity;

use super::compaction::{CompactionAttempt, abort_compaction_attempt};
#[cfg(test)]
use super::compaction::{begin_compaction, store_compaction};
#[cfg(test)]
use super::continuation::continuation_candidate_for_owner;
use super::continuation::{ContinuationReservation, abort_continuation_for_owner};
#[cfg(test)]
use super::translate::model_allowlist::resolve_model_request_with_config_override;
use super::translate::model_allowlist::{ResolvedModel, full_lane_web_search_model};
#[cfg(test)]
use super::translate::reasoning_signature::ReasoningAccount;
use super::translate::request::has_hosted_web_search;
#[cfg(test)]
use super::translate::request::{TranslateOptions, translate_request};
#[cfg(test)]
use super::*;

pub(super) fn apply_model_lane_for_request(
    resolved: &mut ResolvedModel,
    body: &MessagesRequest,
    full_lane: bool,
) -> bool {
    if has_hosted_web_search(body) {
        resolved.model = full_lane_web_search_model(&resolved.model).to_string();
        return false;
    }
    resolved.uses_responses_lite(full_lane)
}

pub(super) fn count_sse_events(bytes: &[u8]) -> u64 {
    String::from_utf8_lossy(bytes).matches("event:").count() as u64
}

pub(super) fn log_compaction_event(
    event: &str,
    ctx: &RequestContext,
    input_items: usize,
    error: Option<&str>,
) {
    let mut fields = serde_json::Map::new();
    fields.insert("reqId".into(), serde_json::json!(ctx.req_id));
    fields.insert("inputItems".into(), serde_json::json!(input_items));
    if let Some(error) = error {
        fields.insert("error".into(), serde_json::json!(error));
        create_logger("codex").warn(event, Some(fields));
    } else {
        create_logger("codex").info(event, Some(fields));
    }
}

pub(super) fn abort_request_state(
    session_id: Option<&str>,
    continuation: &ContinuationReservation,
    compaction_attempt: Option<CompactionAttempt>,
) {
    abort_compaction_attempt(session_id, compaction_attempt);
    abort_continuation_for_owner(continuation);
}

pub(super) struct LiveRequestStateCleanup {
    continuation: ContinuationReservation,
    session_id: Option<String>,
    compaction_attempt: Option<CompactionAttempt>,
    armed: bool,
}

impl LiveRequestStateCleanup {
    pub(super) fn new(
        continuation: ContinuationReservation,
        session_id: Option<String>,
        compaction_attempt: Option<CompactionAttempt>,
    ) -> Self {
        Self {
            continuation,
            session_id,
            compaction_attempt,
            armed: true,
        }
    }

    pub(super) fn abort(&mut self) {
        if self.armed {
            abort_request_state(
                self.session_id.as_deref(),
                &self.continuation,
                self.compaction_attempt,
            );
            self.armed = false;
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LiveRequestStateCleanup {
    fn drop(&mut self) {
        if self.armed {
            abort_request_state(
                self.session_id.as_deref(),
                &self.continuation,
                self.compaction_attempt,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use translate::request::ServiceTier;

    use super::super::provider_test_support::*;
    use super::*;

    #[test]
    fn web_search_requests_leave_lite_lane_and_upgrade_luna() {
        let body = request_with_tools(serde_json::json!([
            {"type":"web_search_20250305", "name":"web_search"}
        ]));
        for (resolved, expected) in [
            ("gpt-5.6-luna", "gpt-5.6-sol"),
            ("gpt-5.6-sol", "gpt-5.6-sol"),
            ("gpt-5.6-terra", "gpt-5.6-terra"),
            ("gpt-6-luna", "gpt-6-sol"),
            ("gpt-6-sol", "gpt-6-sol"),
            ("gpt-6.1-terra", "gpt-6.1-terra"),
            ("gpt-5.4", "gpt-5.4"),
        ] {
            for full_lane in [false, true] {
                let mut model = resolve_model_request_with_config_override(resolved, false);
                let lite = apply_model_lane_for_request(&mut model, &body, full_lane);
                assert!(!lite, "{resolved} with web_search must use the full lane");
                assert_eq!(model.model, expected);
            }
        }
    }

    #[test]
    fn requests_without_web_search_keep_model_and_lite_lane() {
        let body = request_with_tools(serde_json::json!([
            {"name":"Bash", "input_schema":{}}
        ]));
        for (resolved, lite_expected) in [
            ("gpt-5.6-luna", true),
            ("gpt-5.6-sol", true),
            ("gpt-6-luna", true),
            ("gpt-6-sol", true),
            ("gpt-6.1-terra", true),
            ("gpt-5.4", false),
        ] {
            let mut model = resolve_model_request_with_config_override(resolved, false);
            let lite = apply_model_lane_for_request(&mut model, &body, false);
            assert_eq!(
                model.model, resolved,
                "model must not change without web_search"
            );
            assert_eq!(lite, lite_expected);
        }
    }

    #[test]
    fn full_lane_translation_preserves_parallel_tool_restrictions() {
        for model in [
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6.1-sol",
            "gpt-6.1-terra",
            "gpt-5.6-luna",
            "gpt-6-luna",
        ] {
            for full_lane in [false, true] {
                for disable_parallel in [false, true] {
                    let mut body = request_with_tools(serde_json::json!([
                        {"name":"Read", "input_schema":{"type":"object"}}
                    ]));
                    body.extra.insert("tool_choice".to_string(),
                        serde_json::json!({"type":"auto", "disable_parallel_tool_use":disable_parallel}),
                    );
                    let mut resolved = resolve_model_request_with_config_override(model, false);
                    let lite = apply_model_lane_for_request(&mut resolved, &body, full_lane);
                    let translated = translate_request(
                        &body,
                        TranslateOptions {
                            session_id: None,
                            service_tier: None,
                            model: resolved.model,
                            use_responses_lite: lite,
                            reasoning_account: ReasoningAccount::default(),
                        },
                    )
                    .unwrap();
                    assert_eq!(translated.model, model);
                    assert_eq!(translated.client_metadata.is_some(), lite);
                    assert_eq!(translated.parallel_tool_calls, !lite && !disable_parallel);
                    assert_eq!(translated.tools.is_some(), !lite);
                }
            }
        }
    }

    #[test]
    fn terra_translation_preserves_model_and_tier_across_lanes() {
        for requested in ["gpt-6.1-terra", "gpt-6.1-terra-fast"] {
            for hosted_search in [false, true] {
                let tools = if hosted_search {
                    serde_json::json!([{"type":"web_search_20250305", "name":"web_search"}])
                } else {
                    serde_json::json!([])
                };
                let mut body = request_with_tools(tools);
                body.model = Some(requested.to_string());
                let mut resolved = resolve_model_request_with_config_override(requested, false);
                let lite = apply_model_lane_for_request(&mut resolved, &body, false);
                let translated = translate_request(
                    &body,
                    TranslateOptions {
                        session_id: None,
                        service_tier: resolved.service_tier,
                        model: resolved.model,
                        use_responses_lite: lite,
                        reasoning_account: ReasoningAccount::default(),
                    },
                )
                .unwrap();
                assert_eq!(translated.model, "gpt-6.1-terra");
                assert_eq!(translated.client_metadata.is_some(), !hosted_search);
                assert_eq!(
                    translated.service_tier,
                    requested
                        .ends_with("-fast")
                        .then_some(ServiceTier::Priority)
                );
            }
        }
    }

    #[tokio::test]
    async fn stale_request_cleanup_preserves_newer_turn_and_compaction_attempt() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let session_id = "stale-live-request-cleanup";
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        let request = live_test_request("one");
        let stale_continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let stale_compaction = begin_compaction(session_id, &request);
        let stale_cleanup = LiveRequestStateCleanup::new(
            stale_continuation,
            Some(session_id.to_string()),
            Some(stale_compaction),
        );

        let newer_continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let newer_compaction = begin_compaction(session_id, &request);
        drop(stale_cleanup);

        assert!(continuation::is_current_turn_for_owner(&newer_continuation));
        assert!(store_compaction(session_id, newer_compaction, Vec::new()));
        abort_request_state(
            Some(session_id),
            &newer_continuation,
            Some(newer_compaction),
        );
    }
}
