pub mod auth;
pub mod chat_completions;
mod cli;
pub mod client;
mod client_buffered;
mod client_capture;
mod client_error;
mod client_headers;
mod client_http_stream;
mod client_live;
mod client_oneshot;
mod client_proxy;
mod client_sse;
#[cfg(test)]
mod client_test_support;
pub mod compaction;
pub mod continuation;
pub mod count_tokens;
mod error_response;
pub(crate) mod events;
pub mod images;
mod live_completion;
mod live_response;
pub mod native;
#[cfg(test)]
mod provider_test_support;
mod request_state;
pub mod request_summary;
pub mod search;
pub mod transcription;
pub mod translate;
pub mod websocket;
mod websocket_connect;
mod websocket_handshake;
mod websocket_pool;
mod websocket_proxy;
mod websocket_request;
mod websocket_stream;
#[cfg(test)]
mod websocket_test_support;

use async_trait::async_trait;
use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use std::sync::Arc;
use std::time::Instant;

use crate::anthropic::error::json_error;
use crate::anthropic::schema::{CountTokensResponse, MessagesRequest};
use crate::anthropic::sse::usage_from_anthropic_sse;
use crate::config;
use crate::logging::create_logger;
use crate::provider::{CliHandlers, Provider, RequestContext};
use crate::request_identity::ConversationIdentity;
use crate::retry::{compute_backoff_delay, sleep};

use self::auth::token_store::file_store;
use self::client::CodexHttpClient;
use self::compaction::{
    CompactionError, abort_compaction_attempt, apply_compaction_replay, begin_compaction,
    request_compaction, store_compaction,
};
use self::continuation::{abort_continuation_for_owner, continuation_candidate_for_owner};
use self::count_tokens::count_translated_tokens;
use self::error_response::*;
use self::live_completion::*;
use self::live_response::*;
use self::request_state::*;
use self::translate::accumulate::accumulate_response_with_traffic;
use self::translate::model_allowlist::{
    assert_allowed_model, resolve_model_request_with_config_override,
};
use self::translate::request::{TranslateOptions, is_compact_messages_request, translate_request};
use self::translate::stream::translate_stream_bytes_with_traffic;

pub(crate) fn clear_session_compaction(session_id: &str) {
    compaction::clear_compaction(session_id);
}

pub struct CodexProvider {
    client: Arc<CodexHttpClient>,
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexProvider {
    pub fn new() -> Self {
        Self {
            client: Arc::new(CodexHttpClient::new()),
        }
    }
}

impl CodexProvider {
    async fn handle_messages_inner(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        let response = self
            .handle_messages_for_account(&body, ctx.clone(), conversation_identity.clone())
            .await;
        if response.extensions().get::<CodexAccountChanged>().is_none() {
            return response;
        }
        self.handle_messages_for_account(&body, ctx, conversation_identity)
            .await
    }

    async fn handle_messages_for_account(
        &self,
        body: &MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        let message_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
        let want_stream = body.stream;
        let model = body.model.as_deref().unwrap_or("opus");

        let mut resolved =
            resolve_model_request_with_config_override(model, !body.bypass_provider_model_override);
        if let Err(e) = assert_allowed_model(&resolved.model) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Model \"{model}\" resolves to unsupported model \"{}\"",
                    e.model
                ),
            );
        }
        if search::is_standalone_search_request(body) {
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.model_resolved(&ctx.req_id, &resolved.model);
            }
            let (search_request, query) = match search::build_search_request(
                body,
                &resolved.model,
                ctx.session_id.as_deref(),
            ) {
                Ok(request) => request,
                Err(error) => {
                    return json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        error.to_string(),
                    );
                }
            };
            let log = create_logger("codex");
            let started_at = Instant::now();
            log.info(
                "codex_standalone_search_started",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), serde_json::json!(&ctx.req_id)),
                    ("model".to_string(), serde_json::json!(&resolved.model)),
                    ("stream".to_string(), serde_json::json!(want_stream)),
                ])),
            );
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.upstream_started(&ctx.req_id);
            }
            let search_response = match self.client.post_search(&search_request, &ctx).await {
                Ok(response) => response,
                Err(error) => {
                    log.warn(
                        "codex_standalone_search_failed",
                        Some(serde_json::Map::from_iter([
                            ("reqId".to_string(), serde_json::json!(&ctx.req_id)),
                            ("model".to_string(), serde_json::json!(&resolved.model)),
                            ("status".to_string(), serde_json::json!(error.status)),
                            (
                                "ms".to_string(),
                                serde_json::json!(started_at.elapsed().as_millis()),
                            ),
                        ])),
                    );
                    return map_codex_error_to_response(&error);
                }
            };
            log.info(
                "codex_standalone_search_completed",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), serde_json::json!(&ctx.req_id)),
                    ("model".to_string(), serde_json::json!(&resolved.model)),
                    (
                        "resultCount".to_string(),
                        serde_json::json!(search_response.results.as_ref().map(Vec::len)),
                    ),
                    (
                        "ms".to_string(),
                        serde_json::json!(started_at.elapsed().as_millis()),
                    ),
                ])),
            );
            let input_tokens = search::search_request_input_tokens(&search_request);
            let output_tokens = search::search_response_output_tokens(&search_response);
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.usage_updated(&ctx.req_id, Some(input_tokens), Some(output_tokens));
            }
            return search::anthropic_search_response(
                &search_response,
                &query,
                &message_id,
                model,
                want_stream,
                input_tokens,
                ctx.traffic.as_deref(),
            );
        }
        let use_responses_lite =
            apply_model_lane_for_request(&mut resolved.model, body, config::codex_full_lane());
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved.model);
        }

        let mut translated = match translate_request(
            body,
            TranslateOptions {
                session_id: ctx.session_id.clone(),
                service_tier: resolved.service_tier.clone(),
                model: resolved.model.clone(),
                use_responses_lite,
                reasoning_account: self.client.auth_manager().reasoning_account().await,
            },
        ) {
            Ok(t) => t,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                );
            }
        };

        let compact_boundary = is_compact_messages_request(body);
        let server_compaction_enabled = config::codex_server_compaction();
        let mut compaction_attempt = None;
        if !server_compaction_enabled && let Some(session_id) = ctx.session_id.as_deref() {
            compaction::clear_compaction(session_id);
        }
        if server_compaction_enabled
            && compact_boundary
            && let Some(session_id) = ctx.session_id.as_deref()
        {
            let attempt = begin_compaction(session_id, &translated);
            compaction_attempt = Some(attempt);
            log_compaction_event(
                "server_compaction_triggered",
                &ctx,
                translated.input.len(),
                None,
            );
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.compaction_started(&ctx.req_id);
            }
            let mut compaction_ctx = ctx.clone();
            compaction_ctx.monitor = None;
            match request_compaction(self.client.as_ref(), &translated, &compaction_ctx).await {
                Ok(native_history) => {
                    if store_compaction(session_id, attempt, native_history) {
                        log_compaction_event(
                            "server_compaction_completed",
                            &ctx,
                            translated.input.len(),
                            None,
                        );
                    } else {
                        log_compaction_event(
                            "server_compaction_failed",
                            &ctx,
                            translated.input.len(),
                            Some("compaction state was superseded or exceeded the in-memory limit"),
                        );
                    }
                }
                Err(CompactionError::Upstream(error)) if error.usage_limit.is_some() => {
                    abort_compaction_attempt(Some(session_id), Some(attempt));
                    log_compaction_event(
                        "server_compaction_failed",
                        &ctx,
                        translated.input.len(),
                        Some(&error.to_string()),
                    );
                    return map_codex_error_to_response(&error);
                }
                Err(error) => {
                    abort_compaction_attempt(Some(session_id), Some(attempt));
                    log_compaction_event(
                        "server_compaction_failed",
                        &ctx,
                        translated.input.len(),
                        Some(&error.to_string()),
                    );
                }
            }
        } else if server_compaction_enabled
            && !compact_boundary
            && let Some(replay) = apply_compaction_replay(ctx.session_id.as_deref(), &translated)
        {
            translated = replay.request;
            compaction_attempt = Some(replay.attempt);
        }

        let previous_response_id_enabled = config::codex_previous_response_id();
        let continuation = continuation_candidate_for_owner(
            conversation_identity.as_ref(),
            &translated,
            previous_response_id_enabled,
        );
        let turn_id = continuation.turn_id();
        let configured_transport = config::codex_transport();
        let transport = configured_transport.as_str();
        let upstream_started_at = Instant::now();
        let log = create_logger("codex");
        let req_id = ctx.req_id.clone();
        log.info(
            "codex_upstream_request_started",
            Some(serde_json::Map::from_iter([
                ("reqId".to_string(), serde_json::json!(&req_id)),
                ("transport".to_string(), serde_json::json!(transport)),
                ("model".to_string(), serde_json::json!(&resolved.model)),
                ("stream".to_string(), serde_json::json!(want_stream)),
                (
                    "responsesLite".to_string(),
                    serde_json::json!(use_responses_lite),
                ),
                (
                    "previousResponseIdEnabled".to_string(),
                    serde_json::json!(previous_response_id_enabled),
                ),
                (
                    "hasPreviousResponseId".to_string(),
                    serde_json::json!(continuation.candidate().previous_response_id.is_some()),
                ),
                (
                    "inputDeltaCount".to_string(),
                    serde_json::json!(continuation.candidate().input_delta.as_ref().map(Vec::len)),
                ),
                ("turnId".to_string(), serde_json::json!(turn_id)),
            ])),
        );

        let client = self.client.clone();
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        if want_stream {
            let stream_request = translated.clone();
            let response = live_stream_response(
                client,
                message_id,
                model,
                ctx,
                stream_request,
                continuation,
                LiveStreamCompaction {
                    compact_boundary,
                    attempt: compaction_attempt,
                },
                configured_transport,
                tail_is_successful_tool_result(body),
            )
            .await;
            log.info(
                "codex_upstream_response_ready",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), serde_json::json!(&req_id)),
                    ("transport".to_string(), serde_json::json!(transport)),
                    (
                        "status".to_string(),
                        serde_json::json!(response.status().as_u16()),
                    ),
                    (
                        "ms".to_string(),
                        serde_json::json!(upstream_started_at.elapsed().as_millis()),
                    ),
                ])),
            );
            return response;
        }

        let request_continuation = continuation.clone();
        let mut continuation = Some(continuation);
        let mut attempt = 0_u32;
        let successful_tool_tail = tail_is_successful_tool_result(body);
        let upstream = loop {
            let response = match client
                .post_codex_for_owner(&translated, &ctx, continuation.as_ref())
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    log.warn(
                        "codex_upstream_request_failed",
                        Some(serde_json::Map::from_iter([
                            ("reqId".to_string(), serde_json::json!(&req_id)),
                            ("transport".to_string(), serde_json::json!(transport)),
                            ("status".to_string(), serde_json::json!(e.status)),
                            (
                                "origin".to_string(),
                                serde_json::json!(format!("{:?}", e.origin)),
                            ),
                            ("error".to_string(), serde_json::json!(&e.message)),
                            (
                                "ms".to_string(),
                                serde_json::json!(upstream_started_at.elapsed().as_millis()),
                            ),
                        ])),
                    );
                    abort_compaction_attempt(ctx.session_id.as_deref(), compaction_attempt);
                    abort_continuation_for_owner(&request_continuation);
                    return map_codex_error_to_response(&e);
                }
            };
            if !is_empty_codex_success_completion(&response.body, &translated.reasoning_account) {
                break response;
            }
            if accept_empty_completion(successful_tool_tail, attempt)
                && buffered_terminal_is_completed(&response.body)
            {
                break response;
            }
            let error = empty_buffered_completion_error();
            drop_live_continuation_for_retry(&mut continuation);
            if attempt >= MAX_EMPTY_COMPLETION_RETRIES {
                log.warn(
                    "codex_empty_completion_exhausted",
                    Some(empty_completion_exhausted_fields(
                        &req_id,
                        transport,
                        &resolved.model,
                        attempt + 1,
                        "max_attempts",
                        upstream_started_at.elapsed().as_millis(),
                    )),
                );
                abort_compaction_attempt(ctx.session_id.as_deref(), compaction_attempt);
                abort_continuation_for_owner(&request_continuation);
                return map_codex_error_to_response(&error);
            }
            let delay = compute_backoff_delay(attempt, None);
            if delay.exceeds_budget {
                log.warn(
                    "codex_empty_completion_exhausted",
                    Some(empty_completion_exhausted_fields(
                        &req_id,
                        transport,
                        &resolved.model,
                        attempt + 1,
                        "backoff_budget",
                        upstream_started_at.elapsed().as_millis(),
                    )),
                );
                abort_compaction_attempt(ctx.session_id.as_deref(), compaction_attempt);
                abort_continuation_for_owner(&request_continuation);
                return map_codex_error_to_response(&error);
            }
            log.warn(
                "codex_empty_completion_retry",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), serde_json::json!(&req_id)),
                    (
                        "configuredTransport".to_string(),
                        serde_json::json!(transport),
                    ),
                    ("model".to_string(), serde_json::json!(&resolved.model)),
                    ("failedAttempt".to_string(), serde_json::json!(attempt + 1)),
                    (
                        "maxAttempts".to_string(),
                        serde_json::json!(MAX_EMPTY_COMPLETION_RETRIES + 1),
                    ),
                    ("delayMs".to_string(), serde_json::json!(delay.wait_ms)),
                    (
                        "ms".to_string(),
                        serde_json::json!(upstream_started_at.elapsed().as_millis()),
                    ),
                ])),
            );
            attempt += 1;
            sleep(delay.wait_ms).await;
        };
        if let Some(failure) = events::first_event_failure(&upstream.body) {
            abort_compaction_attempt(ctx.session_id.as_deref(), compaction_attempt);
            abort_continuation_for_owner(&request_continuation);
            return map_codex_event_failure_to_response(&failure);
        }
        log.info(
            "codex_upstream_response_received",
            Some(serde_json::Map::from_iter([
                ("reqId".to_string(), serde_json::json!(&req_id)),
                ("transport".to_string(), serde_json::json!(transport)),
                ("status".to_string(), serde_json::json!(upstream.status)),
                (
                    "bodyBytes".to_string(),
                    serde_json::json!(upstream.body.len()),
                ),
                (
                    "ms".to_string(),
                    serde_json::json!(upstream_started_at.elapsed().as_millis()),
                ),
            ])),
        );

        if want_stream {
            let sse_bytes = match translate_stream_bytes_with_traffic(
                &upstream.body,
                &message_id,
                model,
                count_translated_tokens(&translated),
                &translated.reasoning_account,
                ctx.traffic.as_deref(),
            ) {
                Ok(b) => b,
                Err(e) => {
                    abort_compaction_attempt(ctx.session_id.as_deref(), compaction_attempt);
                    abort_continuation_for_owner(&request_continuation);
                    return map_codex_failure_to_response(&format!(
                        "Stream translation error: {e}"
                    ));
                }
            };
            if let Some(monitor) = ctx.monitor.as_ref() {
                let (input_tokens, output_tokens) = usage_from_anthropic_sse(&sse_bytes);
                monitor.stream_progress(
                    &ctx.req_id,
                    sse_bytes.len() as u64,
                    count_sse_events(&sse_bytes),
                    input_tokens,
                    output_tokens,
                );
            }
            update_continuation_from_upstream(
                ctx.session_id.as_deref(),
                &request_continuation,
                compaction_attempt,
                &translated,
                &upstream.body,
                upstream.socket_id,
                compact_boundary,
            );

            let headers = [
                (http::header::CONTENT_TYPE, "text/event-stream"),
                (http::header::CACHE_CONTROL, "no-cache"),
                (http::header::CONNECTION, "keep-alive"),
            ];
            (headers, sse_bytes).into_response()
        } else {
            match accumulate_response_with_traffic(
                &upstream.body,
                &message_id,
                model,
                &translated.reasoning_account,
                ctx.traffic.as_deref(),
            ) {
                Ok(json) => {
                    if let Some(monitor) = ctx.monitor.as_ref() {
                        monitor.usage_updated(
                            &ctx.req_id,
                            json.pointer("/usage/input_tokens").and_then(|v| v.as_u64()),
                            json.pointer("/usage/output_tokens")
                                .and_then(|v| v.as_u64()),
                        );
                    }
                    update_continuation_from_upstream(
                        ctx.session_id.as_deref(),
                        &request_continuation,
                        compaction_attempt,
                        &translated,
                        &upstream.body,
                        upstream.socket_id,
                        compact_boundary,
                    );
                    (StatusCode::OK, Json(json)).into_response()
                }
                Err(e) => {
                    abort_compaction_attempt(ctx.session_id.as_deref(), compaction_attempt);
                    abort_continuation_for_owner(&request_continuation);
                    map_codex_failure_to_response(&format!("Accumulation error: {e}"))
                }
            }
        }
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn missing_login_hint(&self) -> Option<&'static str> {
        file_store()
            .load_auth()
            .ok()
            .flatten()
            .is_none()
            .then_some("run `codex login`")
    }

    fn supported_models(&self) -> Vec<String> {
        translate::model_allowlist::listed_models()
    }

    fn cli(&self) -> &'static dyn CliHandlers {
        &cli::CODEX_CLI
    }

    async fn handle_messages(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        self.handle_messages_inner(body, ctx, None).await
    }

    async fn handle_messages_with_conversation_identity(
        &self,
        body: MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
    ) -> Response {
        self.handle_messages_inner(body, ctx, conversation_identity)
            .await
    }

    async fn handle_count_tokens(&self, body: MessagesRequest, ctx: RequestContext) -> Response {
        let model = body.model.as_deref().unwrap_or("opus");
        let mut resolved =
            resolve_model_request_with_config_override(model, !body.bypass_provider_model_override);
        if let Err(e) = assert_allowed_model(&resolved.model) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Model \"{model}\" resolves to unsupported model \"{}\"",
                    e.model
                ),
            );
        }
        let use_responses_lite =
            apply_model_lane_for_request(&mut resolved.model, &body, config::codex_full_lane());
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &resolved.model);
        }

        let translated = match translate_request(
            &body,
            TranslateOptions {
                session_id: None,
                service_tier: resolved.service_tier.clone(),
                model: resolved.model.clone(),
                use_responses_lite,
                reasoning_account: self.client.auth_manager().reasoning_account().await,
            },
        ) {
            Ok(t) => t,
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                );
            }
        };

        let tokens = count_translated_tokens(&translated);
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, Some(tokens), None);
        }
        (
            StatusCode::OK,
            Json(CountTokensResponse {
                input_tokens: tokens,
            }),
        )
            .into_response()
    }
}

fn codex_generation_event(payload: &serde_json::Value) -> bool {
    !matches!(
        payload.get("type").and_then(|value| value.as_str()),
        Some("codex.rate_limits" | "keepalive") | None
    )
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use tokio::net::TcpListener;

    use super::provider_test_support::*;
    use super::*;

    #[test]
    fn generation_timing_ignores_control_events() {
        assert!(!codex_generation_event(&serde_json::json!({
            "type": "codex.rate_limits"
        })));
        assert!(!codex_generation_event(&serde_json::json!({
            "type": "keepalive"
        })));
        assert!(codex_generation_event(&serde_json::json!({
            "type": "response.created"
        })));
    }

    #[test]
    fn supported_models_includes_fast_variants() {
        let provider = CodexProvider::new();
        let models = provider.supported_models();
        assert!(models.contains(&"gpt-5.6-sol".to_string()));
        assert!(models.contains(&"gpt-5.6-sol-fast".to_string()));
        assert!(models.contains(&"gpt-5.6-terra".to_string()));
        assert!(models.contains(&"gpt-5.6-luna".to_string()));
        assert!(models.contains(&"gpt-6-sol".to_string()));
        assert!(models.contains(&"gpt-6-sol-fast".to_string()));
        assert!(models.contains(&"gpt-6-luna".to_string()));
        assert!(models.contains(&"gpt-6.1-sol".to_string()));
        assert!(models.contains(&"gpt-6.1-sol-fast".to_string()));
        assert!(models.contains(&"gpt-6.1-sol-ultrafast".to_string()));
        assert!(models.contains(&"gpt-6.1-terra".to_string()));
        assert!(models.contains(&"gpt-6.1-terra-fast".to_string()));
        assert!(!models.contains(&"gpt-6.1-terra-ultrafast".to_string()));
        assert!(models.contains(&"gpt-5.4".to_string()));
        assert!(models.contains(&"gpt-5.4-mini".to_string()));
        assert!(models.contains(&"gpt-6-astra-fast".to_string()));
        assert!(models.contains(&"gpt-6-astra-ultrafast".to_string()));
        assert!(!models.contains(&"gpt-6-sol-ultrafast".to_string()));
    }

    #[test]
    fn live_start_payload_retry_detection_uses_event_failure_classification() {
        assert!(
            events::classify_event_failure(&serde_json::json!({
                "type": "codex.rate_limits",
                "rate_limits": {"limit_reached": true}
            }))
            .is_none()
        );
        assert!(
            events::classify_event_failure(&serde_json::json!({
                "type": "response.failed",
                "response": {"error": {"type": "overloaded_error", "message": "overloaded"}}
            }))
            .is_some_and(|failure| failure.retryable())
        );
        assert!(
            !events::classify_event_failure(&serde_json::json!({
                "type": "response.failed",
                "response": {"error": {"status": 400, "code": "invalid_prompt", "message": "bad request"}}
            }))
            .is_some_and(|failure| failure.retryable())
        );
    }

    async fn run_live_failure_case(
        session_id: &str,
        event: serde_json::Value,
        expected_attempts: usize,
    ) -> Response {
        let owner = ConversationIdentity::Main(session_id.to_string());
        continuation::clear_continuation_for_owner(Some(&owner));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        let request = live_test_request("one");
        let continuation = continuation_candidate_for_owner(Some(&owner), &request, true);
        let compaction_attempt = begin_compaction(session_id, &request);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..expected_attempts {
                let (socket, _) = listener.accept().await.unwrap();
                let mut websocket = tokio_tungstenite::accept_async(socket).await.unwrap();
                let _ = next_live_websocket_request(&mut websocket).await;
                emit_live_event(&mut websocket, &event).await;
                drop(websocket);
            }
        });
        let client = authenticated_live_test_client(format!("http://{addr}/responses"));
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            live_stream_response(
                client,
                "message".to_string(),
                &request.model,
                live_test_context(session_id),
                request.clone(),
                continuation.clone(),
                LiveStreamCompaction {
                    compact_boundary: false,
                    attempt: Some(compaction_attempt),
                },
                config::CodexTransport::WebSocket,
                false,
            ),
        )
        .await
        .expect("live failure case timed out");
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("live failure server timed out")
            .expect("live failure server failed");

        assert!(!continuation::is_current_turn_for_owner(&continuation));
        assert!(!store_compaction(
            session_id,
            compaction_attempt,
            Vec::new()
        ));
        websocket::invalidate_codex_websocket_pool_owner(&owner);
        response
    }

    #[tokio::test]
    async fn usage_limit_fast_fails_websocket_and_aborts_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let response = run_live_failure_case(
            "live-usage-limit-cleanup",
            serde_json::json!({
                "type": "error",
                "status_code": 429,
                "error": {
                    "type": "usage_limit_reached",
                    "message": "The usage limit has been reached",
                    "resets_at": 1788879437u64,
                    "resets_in_seconds": 9568
                },
                "headers": {
                    "X-Codex-Primary-Window-Minutes": "300",
                    "X-Codex-Primary-Reset-After-Seconds": "9569",
                    "X-Codex-Secondary-Window-Minutes": "10080",
                    "X-Codex-Secondary-Reset-After-Seconds": "596369"
                }
            }),
            1,
        )
        .await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-status"],
            "rejected"
        );
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-reset"],
            "1788879437"
        );
        assert_eq!(
            response.headers()["anthropic-ratelimit-unified-representative-claim"],
            "five_hour"
        );
        assert!(!response.headers().contains_key(http::header::RETRY_AFTER));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["message"], "The usage limit has been reached");
    }

    #[tokio::test]
    async fn retry_exhaustion_aborts_live_request_state_after_eleven_attempts() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        crate::retry::set_zero_retry_delay_for_tests(true);
        let status = run_live_failure_case(
            "live-retry-exhaustion-cleanup",
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "status": 429,
                        "code": "rate_limit_exceeded",
                        "message": "rate limit reached",
                        "retry_after_seconds": 0
                    }
                }
            }),
            11,
        )
        .await
        .status();
        crate::retry::set_zero_retry_delay_for_tests(false);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn excessive_retry_after_aborts_live_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let status = run_live_failure_case(
            "live-excessive-retry-after-cleanup",
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "status": 429,
                        "code": "rate_limit_exceeded",
                        "message": "rate limit reached",
                        "retry_after_seconds": 31
                    }
                }
            }),
            1,
        )
        .await
        .status();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn nonretryable_live_error_aborts_request_state() {
        let _registry_guard = continuation::lock_continuation_registry_for_async_tests().await;
        let _pool_guard = websocket::lock_codex_websocket_pool_for_tests().await;
        let status = run_live_failure_case(
            "live-nonretryable-cleanup",
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "status": "failed",
                    "error": {
                        "status": 400,
                        "code": "invalid_prompt",
                        "message": "invalid request"
                    }
                }
            }),
            1,
        )
        .await
        .status();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
