use super::auto_review::{apply_auto_review_model, is_claude_auto_review_request};
use super::request_monitor::{
    FailedResponseLogContext, RequestLogContext, RequestMonitorGuard, headers_to_record,
    log_request_completed, monitor_failed, monitor_response_body, record_failed_response,
    redacted_query, with_request_id,
};
use super::{AppState, current_millis, parse_json_body};
use crate::{
    anthropic::{MAX_ANTHROPIC_REQUEST_BYTES, json_error},
    logging::create_logger,
    monitor::EndpointKind,
    project,
    provider::RequestContext,
    registry::normalize_incoming_model,
    request_identity::ConversationIdentity,
    session::{self},
    traffic::{TrafficCaptureOptions, create_traffic_capture},
};
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::Response,
};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

pub(super) async fn handler_messages(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    dispatch_request(state, req, false).await
}

pub(super) async fn handler_count_tokens(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    dispatch_request(state, req, true).await
}

async fn dispatch_request(
    state: Arc<AppState>,
    req: Request<Body>,
    count_tokens: bool,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let claude_profile = req
        .extensions()
        .get::<crate::claude_accounts::limits::LaunchProfile>()
        .map(|profile| profile.0.clone());
    let conversation_identity = (!count_tokens)
        .then(|| ConversationIdentity::from_headers(&headers))
        .flatten();
    let path = uri.path().to_string();
    let query = redacted_query(&uri);
    let endpoint = if count_tokens {
        EndpointKind::CountTokens
    } else {
        EndpointKind::Messages
    };
    log.info(
        "request",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!(method.as_str())),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(&query)),
        ])),
    );
    let session_id = req
        .headers()
        .get("x-claude-code-session-id")
        .and_then(|value| value.to_str().ok())
        .map(std::string::ToString::to_string);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(&req_id, session_id.clone(), None, endpoint);
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    let now = current_millis();
    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_ANTHROPIC_REQUEST_BYTES).await
    {
        Ok(bytes) => bytes,
        Err(_) => {
            let response = json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "Request body exceeded the size limit",
            );
            log_request_completed(
                &log,
                RequestLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    status: response.status(),
                    started_at,
                },
            );
            let (response, details) = record_failed_response(
                &log,
                FailedResponseLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    started_at,
                },
                response,
            )
            .await;
            monitor_failed(
                state.monitor.as_ref(),
                &req_id,
                Some(response.status()),
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Invalid JSON"),
            );
            return with_request_id(response, &req_id);
        }
    };

    let mut body: crate::anthropic::schema::MessagesRequest = match parse_json_body(&body_bytes) {
        Ok(body) => body,
        Err(response) => {
            let status = response.status();
            log_request_completed(
                &log,
                RequestLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    status: response.status(),
                    started_at,
                },
            );
            let (response, details) = record_failed_response(
                &log,
                FailedResponseLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    started_at,
                },
                *response,
            )
            .await;
            monitor_failed(
                state.monitor.as_ref(),
                &req_id,
                Some(status),
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Invalid JSON"),
            );
            return with_request_id(response, &req_id);
        }
    };

    if let Some(monitor) = state.monitor.as_ref() {
        project::resolve_in_background(&body, monitor.clone(), req_id.clone());
    }

    let model = match body.model.as_deref() {
        Some(model) => model,
        None => {
            let response = json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Missing \"model\" in request body. {}",
                    state.registry.unknown_model_message().await
                ),
            );
            log_request_completed(
                &log,
                RequestLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    status: response.status(),
                    started_at,
                },
            );
            let (response, details) = record_failed_response(
                &log,
                FailedResponseLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: None,
                    count_tokens,
                    started_at,
                },
                response,
            )
            .await;
            monitor_failed(
                state.monitor.as_ref(),
                &req_id,
                Some(response.status()),
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Missing model"),
            );
            return with_request_id(response, &req_id);
        }
    };

    let mut normalized_model = normalize_incoming_model(model);
    body.model = Some(normalized_model.clone());
    let session_state = if let Some(session_id) = session_id.as_deref() {
        session::existing_session(Some(session_id), now)
    } else {
        None
    };
    let session_affinity = session_state
        .as_ref()
        .and_then(|state| state.affinity_provider.as_ref());
    let selected_sources = !count_tokens
        && state.registry.source_startup().is_some_and(|startup| {
            startup
                .sources()
                .order()
                .sources()
                .contains(&crate::config::codex_auth::AuthSource::ApiKey)
        });
    let lookup = |model: &str, affinity| {
        if selected_sources {
            state
                .registry
                .translated_provider_for_model(model, affinity)
        } else {
            state.registry.provider_for_model(model, affinity)
        }
    };
    let original_provider = lookup(&normalized_model, session_affinity);
    let configured_auto_review_model = crate::config::auto_review_model();
    let auto_review_route = original_provider.as_ref().and_then(|provider| {
        apply_auto_review_model(
            &mut body,
            count_tokens,
            configured_auto_review_model
                .as_deref()
                .or_else(|| (selected_sources && provider.name() == "codex").then_some("luna")),
            provider.name(),
        )
    });
    if auto_review_route.is_some() {
        normalized_model = normalize_incoming_model(body.model.as_deref().expect("override model"));
        body.model = Some(normalized_model.clone());
    }

    let provider = if auto_review_route.is_some() {
        lookup(&normalized_model, None)
    } else {
        original_provider
    };

    let provider = match provider {
        Some(provider) => provider,
        None => {
            log.warn(
                "unknown model",
                Some(serde_json::Map::from_iter([
                    ("reqId".to_string(), json!(&req_id)),
                    ("model".to_string(), json!(&normalized_model)),
                ])),
            );
            let response = json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "Unknown model \"{normalized_model}\". {}",
                    state.registry.unknown_model_message().await
                ),
            );
            log_request_completed(
                &log,
                RequestLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: Some(&normalized_model),
                    count_tokens,
                    status: response.status(),
                    started_at,
                },
            );
            let (response, details) = record_failed_response(
                &log,
                FailedResponseLogContext {
                    req_id: &req_id,
                    provider: None,
                    model: Some(&normalized_model),
                    count_tokens,
                    started_at,
                },
                response,
            )
            .await;
            monitor_failed(
                state.monitor.as_ref(),
                &req_id,
                Some(response.status()),
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or("Unknown model"),
            );
            return with_request_id(response, &req_id);
        }
    };

    if provider.name() != "anthropic"
        && let Some(thread) = body.extra.remove("thread")
        && thread.get("type").and_then(Value::as_str) != Some("create")
    {
        let message = "capability_rejected: beta_header:message-threads-2026-08-12";
        let response = json_error(StatusCode::BAD_REQUEST, "invalid_request_error", message);
        log_request_completed(
            &log,
            RequestLogContext {
                req_id: &req_id,
                provider: Some(provider.name()),
                model: Some(&normalized_model),
                count_tokens,
                status: response.status(),
                started_at,
            },
        );
        let (response, details) = record_failed_response(
            &log,
            FailedResponseLogContext {
                req_id: &req_id,
                provider: Some(provider.name()),
                model: Some(&normalized_model),
                count_tokens,
                started_at,
            },
            response,
        )
        .await;
        if let Some(monitor) = state.monitor.as_ref() {
            monitor.request_retry_requested(
                &req_id,
                details
                    .as_ref()
                    .map(|details| details.message.as_str())
                    .unwrap_or(message),
            );
        }
        return with_request_id(response, &req_id);
    }

    body.bypass_provider_model_override = auto_review_route.is_some() && provider.name() == "codex";
    if !count_tokens && provider.name() == "codex" && is_claude_auto_review_request(&body) {
        body.auto_review_effort = crate::config::auto_review_effort();
    }

    if let Some(route) = auto_review_route.as_ref() {
        log.info(
            "auto-review route selected",
            Some(Map::from_iter([
                ("reqId".to_string(), json!(&req_id)),
                ("requestedModel".to_string(), json!(&route.requested_model)),
                ("overrideModel".to_string(), json!(&route.override_model)),
                ("provider".to_string(), json!(provider.name())),
            ])),
        );
    }

    if !count_tokens
        && auto_review_route.is_none()
        && provider.name() != "codex"
        && let Some(session_id) = session_id.as_deref()
    {
        crate::providers::codex::clear_session_compaction(session_id);
    }

    let effort = crate::providers::translate_shared::read_effort(&body)
        .ok()
        .flatten()
        .map(str::to_string);
    let current = session::record_session_request_with_affinity_update(
        session_id.as_deref(),
        session_state.as_ref(),
        provider.name(),
        &normalized_model,
        auto_review_route.is_none(),
        now,
    );
    if let Some(monitor) = state.monitor.as_ref() {
        if let Some(current) = current.as_ref() {
            monitor.session_sequence_resolved(&req_id, current.seq);
        }
        monitor.provider_selected(&req_id, provider.name(), &normalized_model, effort);
        if provider.name() == "codex" && (count_tokens || !selected_sources) {
            monitor.codex_source(&req_id, "chatgpt");
        }
    }

    let traffic = create_traffic_capture(TrafficCaptureOptions {
        req_id: req_id.clone(),
        session_id: session_id.clone(),
        session_seq: current.as_ref().map(|s| s.seq),
        provider: Some(provider.name().to_string()),
        state_dir_override: None,
    })
    .map(Arc::new);

    if let Some(capture) = traffic.as_ref() {
        if let Some(monitor) = state.monitor.as_ref() {
            monitor.traffic_capture_path(&req_id, capture.root().to_path_buf());
        }
        capture.write_json(
            "000-metadata",
            &json!({
                "reqId": &req_id,
                "sessionId": &session_id,
                "sessionSeq": current.as_ref().map(|s| s.seq),
                "kind": if count_tokens { "count_tokens" } else { "messages" },
                "provider": provider.name(),
                "model": &normalized_model,
                "method": method.as_str(),
                "path": &path,
                "query": &query,
                "headers": headers_to_record(&headers),
            }),
        );
        capture.write_json(
            "010-anthropic-request",
            &serde_json::to_value(&body).unwrap_or_else(|_| json!({})),
        );
    }

    let context = RequestContext {
        recovery_budget: Default::default(),
        req_id: req_id.clone(),
        session_id,
        traffic,
        monitor: state.monitor.clone(),
        passthrough: Some(crate::provider::Passthrough {
            raw_body: body_bytes,
            headers,
            path_and_query: uri
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| path.clone()),
            claude_profile,
        }),
    };

    let response = if count_tokens {
        provider.handle_count_tokens(body, context).await
    } else if selected_sources && provider.name() == "codex" {
        crate::providers::codex::selected_messages::handle(
            &state.registry,
            &body,
            &context,
            if auto_review_route.is_some() {
                None
            } else {
                conversation_identity.as_ref()
            },
        )
        .await
    } else {
        provider
            .handle_messages_with_conversation_identity(
                body,
                context,
                if auto_review_route.is_some() {
                    None
                } else {
                    conversation_identity
                },
            )
            .await
    };
    log_request_completed(
        &log,
        RequestLogContext {
            req_id: &req_id,
            provider: Some(provider.name()),
            model: Some(&normalized_model),
            count_tokens,
            status: response.status(),
            started_at,
        },
    );
    let status = response.status();
    if status.is_success() {
        return monitor_response_body(response, request_guard);
    }

    let (response, details) = record_failed_response(
        &log,
        FailedResponseLogContext {
            req_id: &req_id,
            provider: Some(provider.name()),
            model: Some(&normalized_model),
            count_tokens,
            started_at,
        },
        response,
    )
    .await;
    if let Some(details) = details.as_ref() {
        monitor_failed(
            state.monitor.as_ref(),
            &req_id,
            Some(status),
            details.message.as_str(),
        );
    } else {
        monitor_failed(
            state.monitor.as_ref(),
            &req_id,
            Some(status),
            format!("HTTP {}", status.as_u16()),
        );
    }
    with_request_id(response, &req_id)
}
