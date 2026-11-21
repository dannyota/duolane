use crate::anthropic::schema::MessagesRequest;
use crate::config;

use super::compaction::{CompactionAttempt, abort_compaction_attempt, activate_compaction};
use super::continuation::{
    ContinuationReservation, abort_continuation_for_owner, record_continuation_for_owner,
};
use super::error_response::*;
use super::live_response::*;
use super::translate::reasoning_signature::ReasoningAccount;
use super::translate::reducer::finish_metadata_from_upstream;
use super::*;

pub(super) fn tail_is_successful_tool_result(body: &MessagesRequest) -> bool {
    use std::collections::HashSet;

    let [.., assistant, user] = body.messages.as_slice() else {
        return false;
    };
    if assistant.role != "assistant" || user.role != "user" {
        return false;
    }
    let (Some(calls), Some(results)) = (assistant.content.as_array(), user.content.as_array())
    else {
        return false;
    };
    let mut pending = HashSet::new();
    for block in calls {
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            let Some(id) = block
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|id| !id.is_empty())
            else {
                return false;
            };
            if !pending.insert(id) {
                return false;
            }
        }
    }
    if pending.is_empty() {
        return false;
    }
    for block in results {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text")
                if block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .is_some_and(|text| text.trim().is_empty()) => {}
            Some("tool_result") => {
                if !matches!(
                    block.get("is_error"),
                    None | Some(serde_json::Value::Bool(false))
                ) {
                    return false;
                }
                let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) else {
                    return false;
                };
                if !pending.remove(id) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    pending.is_empty()
}

pub(super) fn terminal_status_is_completed(payload: &serde_json::Value) -> bool {
    payload.pointer("/response/status").and_then(|v| v.as_str()) == Some("completed")
}

pub(super) fn buffered_terminal_is_completed(upstream_sse: &[u8]) -> bool {
    crate::anthropic::sse::parse_sse_events(upstream_sse)
        .into_iter()
        .filter_map(|event| serde_json::from_str::<serde_json::Value>(&event.data).ok())
        .rfind(events::event_is_terminal)
        .is_some_and(|payload| {
            is_codex_success_terminal_event(&payload) && terminal_status_is_completed(&payload)
        })
}

pub(super) fn accept_empty_completion(successful_tool_tail: bool, empty_attempts: u32) -> bool {
    successful_tool_tail && empty_attempts >= 1
}

pub(super) fn empty_live_completion_error() -> client::CodexError {
    client::CodexError {
        status: 503,
        message: "Codex completed without producing output".to_string(),
        detail: Some(EMPTY_CODEX_COMPLETION_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: client::CodexErrorOrigin::WebSocket,
    }
}

pub(super) fn is_empty_completion_error(error: &client::CodexError) -> bool {
    error.detail.as_deref() == Some(EMPTY_CODEX_COMPLETION_DETAIL)
}

pub(super) fn empty_completion_exhausted_fields(
    req_id: &str,
    configured_transport: &str,
    model: &str,
    attempts: u32,
    reason: &str,
    elapsed_ms: u128,
) -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        ("reqId".to_string(), serde_json::json!(req_id)),
        (
            "configuredTransport".to_string(),
            serde_json::json!(configured_transport),
        ),
        ("model".to_string(), serde_json::json!(model)),
        ("attempts".to_string(), serde_json::json!(attempts)),
        ("reason".to_string(), serde_json::json!(reason)),
        ("ms".to_string(), serde_json::json!(elapsed_ms)),
    ])
}

pub(super) fn empty_buffered_completion_error() -> client::CodexError {
    client::CodexError {
        status: 503,
        message: "Codex completed without producing output".to_string(),
        detail: Some(EMPTY_CODEX_COMPLETION_DETAIL.to_string()),
        retry_after: None,
        usage_limit: None,
        origin: match config::codex_transport() {
            config::CodexTransport::Http => client::CodexErrorOrigin::BufferedHttp,
            _ => client::CodexErrorOrigin::BufferedWebSocket,
        },
    }
}

pub(super) fn is_empty_codex_success_completion(
    upstream_sse: &[u8],
    account: &ReasoningAccount,
) -> bool {
    use super::translate::reducer::{ReducerEvent, TERM_COMPLETED, TERM_DONE};

    let Ok(events) = super::translate::reducer::reduce_upstream_bytes(upstream_sse, account) else {
        return false;
    };
    let mut saw_success_terminal = false;
    for event in &events {
        match event {
            ReducerEvent::TextDelta { text, .. } if !text.is_empty() => return false,
            ReducerEvent::ThinkingStart { .. }
            | ReducerEvent::ToolStart { .. }
            | ReducerEvent::WebSearch { .. } => return false,
            ReducerEvent::Finish { terminal_type, .. }
                if terminal_type == TERM_COMPLETED || terminal_type == TERM_DONE =>
            {
                saw_success_terminal = true;
            }
            _ => {}
        }
    }
    saw_success_terminal
}

pub(super) fn is_codex_success_terminal_event(payload: &serde_json::Value) -> bool {
    events::event_is_success_terminal(payload)
}

pub(super) fn retryable_live_start_codex_error(err: &client::CodexError) -> bool {
    if err.origin == client::CodexErrorOrigin::WebSocketHandshake {
        if err.detail.as_deref() == Some(websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL) {
            return false;
        }
        return err.status == 0 || matches!(err.status, 429 | 500 | 502 | 503 | 504 | 529);
    }
    if err.detail.as_deref() == Some(websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL) {
        return true;
    }
    matches!(err.status, 429 | 500 | 502 | 503 | 504 | 529)
        || (err.status == 0
            && (err.detail.as_deref() == Some(client::TRANSPORT_UNEXPECTED_EOF_DETAIL)
                || retryable_live_message(codex_error_message(err))))
}

pub(super) fn is_missing_previous_response_error(err: &client::CodexError) -> bool {
    matches!(
        err.detail.as_deref(),
        Some("previous_response_not_found")
            | Some(websocket::WEBSOCKET_CONTINUATION_SOCKET_MISSING_DETAIL)
    )
}

pub(super) fn drop_live_continuation_for_retry(
    continuation: &mut Option<ContinuationReservation>,
) -> bool {
    if continuation
        .as_ref()
        .and_then(|reservation| reservation.candidate().previous_response_id.as_deref())
        .is_none()
    {
        return false;
    }

    if let Some(reservation) = continuation.as_ref() {
        *continuation = Some(reservation.full_context_retry());
    }
    true
}

fn retryable_live_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "overloaded",
        "rate limit",
        "you can retry your request",
        "temporarily unavailable",
        "timed out",
        "connection closed",
        "connection reset",
        "broken pipe",
        "epipe",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn update_continuation_from_upstream(
    session_id: Option<&str>,
    continuation: &ContinuationReservation,
    compaction_attempt: Option<CompactionAttempt>,
    request_body: &translate::request::ResponsesRequest,
    upstream_body: &[u8],
    socket_id: Option<u64>,
    compact_boundary: bool,
) {
    match finish_metadata_from_upstream(upstream_body, &request_body.reasoning_account) {
        Ok(Some(finish)) if finish.continuation_eligible => {
            if compact_boundary {
                activate_compaction(
                    session_id,
                    compaction_attempt,
                    &request_body.model,
                    &finish.output_items,
                );
            }
            record_continuation_for_owner(
                continuation,
                request_body,
                finish.response_id.as_deref(),
                socket_id,
                &finish.output_items,
            );
        }
        _ => {
            abort_compaction_attempt(session_id, compaction_attempt);
            abort_continuation_for_owner(continuation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::provider_test_support::*;
    use super::*;

    #[test]
    fn terminal_only_completed_upstream_is_empty_completion() {
        let body = upstream_sse(&[serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "status": "completed", "incomplete_details": null, "usage": {"input_tokens": 5, "output_tokens": 0}}
        })]);
        assert!(is_empty_codex_success_completion(
            &body,
            &ReasoningAccount::default()
        ));
    }

    #[test]
    fn terminal_only_done_upstream_is_empty_completion() {
        let body = upstream_sse(&[serde_json::json!({
            "type": "response.done",
            "response": {"id": "resp_1", "usage": {}}
        })]);
        assert!(is_empty_codex_success_completion(
            &body,
            &ReasoningAccount::default()
        ));
    }

    #[test]
    fn empty_message_item_is_empty_completion() {
        let body = upstream_sse(&[
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": "msg_1"}
            }),
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message"}
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "usage": {}}
            }),
        ]);
        assert!(is_empty_codex_success_completion(
            &body,
            &ReasoningAccount::default()
        ));
    }

    #[test]
    fn upstream_with_text_is_not_empty_completion() {
        let body = upstream_sse(&[
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": "msg_1"}
            }),
            serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": "hello"
            }),
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "message"}
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "usage": {}}
            }),
        ]);
        assert!(!is_empty_codex_success_completion(
            &body,
            &ReasoningAccount::default()
        ));
    }

    #[test]
    fn upstream_with_tool_call_is_not_empty_completion() {
        let body = upstream_sse(&[
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Read", "arguments": ""}
            }),
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{}"}
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "usage": {}}
            }),
        ]);
        assert!(!is_empty_codex_success_completion(
            &body,
            &ReasoningAccount::default()
        ));
    }

    #[test]
    fn upstream_without_terminal_event_is_not_empty_completion() {
        assert!(!is_empty_codex_success_completion(
            &upstream_sse(&[]),
            &ReasoningAccount::default()
        ));
    }

    fn request_with_tail(messages: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "gpt-5.6-luna",
            "messages": messages
        }))
        .unwrap()
    }

    #[test]
    fn successful_tool_result_tail_is_detected() {
        let body = request_with_tail(serde_json::json!([
            {"role":"user","content":"ping"},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"reply","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"sent"}]}
        ]));
        assert!(tail_is_successful_tool_result(&body));
    }

    #[test]
    fn error_tool_result_tail_is_excluded() {
        let body = request_with_tail(serde_json::json!([
            {"role":"user","content":"ping"},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"run","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"boom","is_error":true}]}
        ]));
        assert!(!tail_is_successful_tool_result(&body));
    }

    #[test]
    fn non_tool_tails_are_excluded() {
        let text = request_with_tail(serde_json::json!([{"role":"user","content":"ping"}]));
        assert!(!tail_is_successful_tool_result(&text));
        let assistant = request_with_tail(serde_json::json!([
            {"role":"user","content":"ping"},
            {"role":"assistant","content":[{"type":"text","text":"pong"}]}
        ]));
        assert!(!tail_is_successful_tool_result(&assistant));
        let not_last = request_with_tail(serde_json::json!([
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":"sent"},
                {"type":"text","text":"and also this"}
            ]}
        ]));
        assert!(!tail_is_successful_tool_result(&not_last));
        let empty = request_with_tail(serde_json::json!([]));
        assert!(!tail_is_successful_tool_result(&empty));
    }

    #[test]
    fn tool_tail_requires_complete_successful_matching_results() {
        let calls = serde_json::json!({"role":"assistant","content":[
            {"type":"tool_use","id":"a","name":"reply","input":{}},
            {"type":"tool_use","id":"b","name":"reply","input":{}}
        ]});
        let results = serde_json::json!([
            {"type":"tool_result","tool_use_id":"a","content":"sent"},
            {"type":"tool_result","tool_use_id":"b","content":"sent"}
        ]);
        let make = |content| {
            request_with_tail(serde_json::json!([
                calls.clone(), {"role":"user","content":content}
            ]))
        };
        assert!(tail_is_successful_tool_result(&make(results.clone())));
        let mut whitespace = results.clone();
        whitespace
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type":"text","text":" \n\t"}));
        assert!(tail_is_successful_tool_result(&make(whitespace)));
        let mut cases = vec![
            serde_json::json!([results[0]]),
            serde_json::json!([results[0], results[0]]),
        ];
        for key in ["is_error", "tool_use_id"] {
            let mut invalid = results.clone();
            invalid[0][key] = if key == "is_error" {
                serde_json::json!(true)
            } else {
                serde_json::json!("unknown")
            };
            cases.push(invalid);
        }
        for text in [
            "continue working",
            "<system-reminder>PostToolUse hook: continue</system-reminder>",
        ] {
            let mut invalid = results.clone();
            invalid
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"type":"text","text":text}));
            cases.push(invalid);
        }
        for case in cases {
            assert!(
                !tail_is_successful_tool_result(&make(case.clone())),
                "{case}"
            );
        }
        for role in ["user", "system"] {
            let mut body = make(results.clone());
            let trailing = request_with_tail(
                serde_json::json!([{"role":role,"content":"PostToolUse hook: continue"}]),
            );
            body.messages.extend(trailing.messages);
            assert!(!tail_is_successful_tool_result(&body));
        }
    }

    #[test]
    fn empty_completion_is_accepted_only_after_one_retry_and_only_with_the_tail() {
        assert!(
            !accept_empty_completion(true, 0),
            "first empty must retry once"
        );
        assert!(
            accept_empty_completion(true, 1),
            "second consecutive empty is the turn's end"
        );
        assert!(accept_empty_completion(true, 2));
        assert!(
            !accept_empty_completion(false, 0),
            "no tool tail: unchanged #70/#71 behavior"
        );
        assert!(
            !accept_empty_completion(false, 5),
            "no tool tail: never accepted"
        );
    }

    #[test]
    fn empty_completion_exhausted_fields_distinguish_the_two_give_up_reasons() {
        let max = empty_completion_exhausted_fields(
            "req-1",
            "auto",
            "gpt-6-sol",
            11,
            "max_attempts",
            157_000,
        );
        assert_eq!(max["reqId"], "req-1");
        assert_eq!(max["configuredTransport"], "auto");
        assert!(max.get("transport").is_none());
        assert_eq!(max["model"], "gpt-6-sol");
        assert_eq!(max["attempts"], 11);
        assert_eq!(max["reason"], "max_attempts");
        assert_eq!(max["ms"], 157_000);

        let budget = empty_completion_exhausted_fields(
            "req-1",
            "websocket",
            "gpt-6-sol",
            4,
            "backoff_budget",
            20_000,
        );
        assert_eq!(budget["reason"], "backoff_budget");
    }

    #[test]
    fn live_start_statusless_websocket_handshake_error_is_retryable() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket connect timeout after 15000ms".to_string(),
            detail: None,
            retry_after: None,
            usage_limit: None,
            origin: client::CodexErrorOrigin::WebSocketHandshake,
        };

        assert!(retryable_live_start_codex_error(&err));
    }

    #[test]
    fn live_start_proxy_tunnel_rejection_is_not_retryable() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket proxy tunnel was rejected".to_string(),
            detail: Some(websocket::WEBSOCKET_PROXY_TUNNEL_REJECTED_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: client::CodexErrorOrigin::WebSocketHandshake,
        };

        assert!(!retryable_live_start_codex_error(&err));
    }

    #[test]
    fn live_start_keepalive_failure_is_retryable() {
        let err = client::CodexError {
            status: 0,
            message: "WebSocket keepalive error: test write failed".to_string(),
            detail: Some(websocket::WEBSOCKET_KEEPALIVE_FAILURE_DETAIL.to_string()),
            retry_after: None,
            usage_limit: None,
            origin: client::CodexErrorOrigin::WebSocket,
        };

        assert!(retryable_live_start_codex_error(&err));
    }
}
