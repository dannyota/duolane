use axum::response::Response;
use serde_json::{Value, json};

use super::{ApiClient, ApiFailure, ApiFailureKind, SendEvidence, messages};
use crate::{
    anthropic::schema::MessagesRequest,
    providers::codex::{
        events,
        search::{self, SearchResponse},
        sources::{FailureKind, SourceAttempt, SourceFailure},
        translate::{model_allowlist::ResolvedModel, request::ServiceTier},
    },
    request_identity::ConversationIdentity,
};

pub(super) fn build_request(
    body: &MessagesRequest,
    identity: Option<&ConversationIdentity>,
    resolved: &ResolvedModel,
) -> anyhow::Result<(Value, String, u64)> {
    let selected = body
        .extra
        .get("tool_choice")
        .and_then(Value::as_object)
        .filter(|choice| choice.get("type").and_then(Value::as_str) == Some("tool"))
        .and_then(|choice| choice.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Invalid search tool choice"))?;
    let mut tools = body
        .extra
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| {
            tool.get("type").and_then(Value::as_str) == Some("web_search_20250305")
                && tool.get("name").and_then(Value::as_str) == Some(selected)
        });
    let selected_tool = tools
        .next()
        .ok_or_else(|| anyhow::anyhow!("Search tool not found"))?;
    if tools.next().is_some() {
        anyhow::bail!("Ambiguous search tool");
    }
    let (mut search, query) = search::build_search_request(body, &resolved.model, identity)?;
    search.settings.filters = search::search_tool_filters(selected_tool);
    let tokens = search::search_request_input_tokens(&search);
    let mut tool = json!({"type":"web_search"});
    if let Some(filters) = search.settings.filters {
        tool["filters"] = serde_json::to_value(filters)?;
    }
    let mut request = json!({
        "model": resolved.model, "input": query, "store": false, "stream": true,
        "tools": [tool], "tool_choice": {"type":"web_search"}, "max_tool_calls": 1,
        "include": ["web_search_call.action.sources"]
    });
    if let Some(tier) = &resolved.service_tier {
        request["service_tier"] = serde_json::to_value(if *tier == ServiceTier::Ultrafast {
            &ServiceTier::Priority
        } else {
            tier
        })?;
    }
    if let Some(max) = body.max_tokens {
        request["max_output_tokens"] = json!(max);
    }
    Ok((request, query, tokens))
}

fn invalid(send: SendEvidence) -> ApiFailure {
    ApiFailure::local(ApiFailureKind::Other, send, false)
}

#[allow(clippy::result_large_err)]
pub async fn handle(
    body: &MessagesRequest,
    identity: Option<&ConversationIdentity>,
    resolved: &ResolvedModel,
    api: &ApiClient,
    attempt: &SourceAttempt,
) -> Result<Response, SourceFailure> {
    handle_with_raw_limit(
        body,
        identity,
        resolved,
        api,
        attempt,
        crate::provider::MAX_BUFFERED_RESPONSE_BYTES,
    )
    .await
}

#[allow(clippy::result_large_err)]
pub(super) async fn handle_with_raw_limit(
    body: &MessagesRequest,
    identity: Option<&ConversationIdentity>,
    resolved: &ResolvedModel,
    api: &ApiClient,
    attempt: &SourceAttempt,
    raw_limit: usize,
) -> Result<Response, SourceFailure> {
    let local =
        || messages::source_failure(api, invalid(SendEvidence::NotSent), FailureKind::Final);
    if attempt.gate.hosted() || attempt.gate.ambiguous_hosted() || attempt.gate.committed() {
        return Err(local());
    }
    let (request, query, tokens) = build_request(body, identity, resolved).map_err(|_| local())?;
    let mut retries = 0;
    loop {
        let result = match api.responses(&request).await {
            Ok(events) => collect(events.with_raw_limit(raw_limit), attempt).await,
            Err(e) => Err(e),
        };
        let failure = match result {
            Ok(results) => {
                let search = SearchResponse {
                    encrypted_output: None,
                    output: String::new(),
                    results: Some(results),
                };
                let response = search::anthropic_search_response(
                    &search,
                    &query,
                    &format!("msg_{}", uuid::Uuid::new_v4().simple()),
                    body.model.as_deref().unwrap_or("opus"),
                    body.stream,
                    tokens,
                    attempt.context.traffic.as_deref(),
                );
                attempt.gate.mark_committed();
                return Ok(response);
            }
            Err(e) => e,
        };
        if failure.hosted_executed {
            attempt.gate.mark_hosted();
        }
        if failure.send == SendEvidence::OutcomeUnknown {
            attempt.gate.mark_ambiguous_hosted();
        }
        let kind = match failure.kind {
            ApiFailureKind::Quota => FailureKind::UsageLimit,
            ApiFailureKind::Rejected => FailureKind::RejectedCredential,
            _ => FailureKind::Final,
        };
        if failure.kind != ApiFailureKind::Transient
            || failure.send != SendEvidence::NotSent
            || attempt.gate.hosted()
            || attempt.gate.ambiguous_hosted()
        {
            return Err(messages::source_failure(api, failure, kind));
        }
        let backoff =
            crate::retry::compute_backoff_delay(retries, failure.error.retry_after.as_deref());
        if backoff.exceeds_budget
            || !attempt
                .context
                .recovery_budget
                .wait_retry(backoff.wait_ms, false)
                .await
        {
            return Err(messages::source_failure(
                api,
                failure,
                FailureKind::TransientExhausted,
            ));
        }
        retries += 1;
    }
}

async fn collect(
    mut stream: super::ApiEvents,
    attempt: &SourceAttempt,
) -> Result<Vec<Value>, ApiFailure> {
    let mut id: Option<String> = None;
    let mut completed = false;
    let mut results = Vec::new();
    let mut titles = std::collections::HashMap::<String, String>::new();
    let mut total = 0usize;
    let mut terminal = false;
    while let Some(event) = stream.next().await {
        let event = event?;
        total = total
            .checked_add(
                serde_json::to_vec(&event)
                    .map_err(|_| invalid(SendEvidence::OutcomeUnknown))?
                    .len(),
            )
            .ok_or_else(|| invalid(SendEvidence::OutcomeUnknown))?;
        if total > crate::provider::MAX_BUFFERED_RESPONSE_BYTES {
            return Err(invalid(SendEvidence::OutcomeUnknown));
        }
        if events::event_confirms_hosted_tool_execution(&event)
            || event
                .pointer("/response/output")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|v| v["type"] == "web_search_call"))
        {
            attempt.gate.mark_hosted();
        }
        if matches!(
            event.get("type").and_then(Value::as_str),
            Some(
                "response.web_search_call.in_progress"
                    | "response.web_search_call.searching"
                    | "response.web_search_call.completed"
            )
        ) && let Some(current) = event
            .get("item_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if id.as_deref().is_some_and(|existing| existing != current) {
                return Err(invalid(SendEvidence::OutcomeUnknown));
            }
            id = Some(current.to_owned());
        }
        let mut items = Vec::new();
        if event.pointer("/item/type").and_then(Value::as_str) == Some("web_search_call") {
            items.push(&event["item"]);
        }
        if let Some(output) = event.pointer("/response/output").and_then(Value::as_array) {
            items.extend(output.iter().filter(|v| v["type"] == "web_search_call"));
        }
        for item in items {
            let current = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid(SendEvidence::OutcomeUnknown))?;
            if id.as_deref().is_some_and(|existing| existing != current) {
                return Err(invalid(SendEvidence::OutcomeUnknown));
            }
            id = Some(current.to_owned());
            if terminal_status_conflicts(&event, item) {
                return Err(invalid(SendEvidence::OutcomeUnknown));
            }
            if item["status"] == "completed" {
                completed = true;
                if let Some(sources) = item.pointer("/action/sources").and_then(Value::as_array) {
                    results = enrich_sources(
                        sources,
                        &mut titles,
                        crate::provider::MAX_BUFFERED_RESPONSE_BYTES,
                    )?;
                }
            }
        }
        if events::event_is_success_terminal(&event) {
            if event.pointer("/response/status").and_then(Value::as_str) != Some("completed") {
                return Err(invalid(SendEvidence::OutcomeUnknown));
            }
            let output = event
                .pointer("/response/output")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid(SendEvidence::OutcomeUnknown))?;
            let calls: Vec<_> = output
                .iter()
                .filter(|item| item["type"] == "web_search_call")
                .collect();
            if calls.len() != 1
                || calls[0]["status"] != "completed"
                || calls[0].get("id").and_then(Value::as_str) != id.as_deref()
            {
                return Err(invalid(SendEvidence::OutcomeUnknown));
            }
            terminal = true;
            break;
        }
    }
    if !terminal || !completed || id.is_none() {
        return Err(invalid(SendEvidence::OutcomeUnknown));
    }
    Ok(results)
}

fn terminal_status_conflicts(event: &Value, item: &Value) -> bool {
    events::event_is_success_terminal(event)
        && item
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status != "completed")
}

fn json_string_bound(value: &str) -> usize {
    value.bytes().fold(2usize, |size, byte| {
        size.saturating_add(match byte {
            b'"' | b'\\' => 2,
            0..=31 => 6,
            _ => 1,
        })
    })
}

pub(super) fn enrich_sources(
    sources: &[Value],
    titles: &mut std::collections::HashMap<String, String>,
    limit: usize,
) -> Result<Vec<Value>, ApiFailure> {
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    let mut size = 2usize;
    for source in sources {
        let Some(url) = source.get("url").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(url) {
            continue;
        }
        let explicit = source.get("title").and_then(Value::as_str);
        let title = explicit.or_else(|| titles.get(url).map(String::as_str));
        let entry = 96usize
            .saturating_add(json_string_bound(url))
            .saturating_add(title.map_or(0, json_string_bound));
        size = size.saturating_add(entry);
        if size > limit {
            return Err(invalid(SendEvidence::OutcomeUnknown));
        }
        let mut result = json!({"type":"url", "url":url});
        if let Some(title) = title {
            result["title"] = title.into();
        }
        if let Some(title) = explicit {
            titles.insert(url.to_owned(), title.to_owned());
        }
        results.push(result);
    }
    Ok(results)
}
