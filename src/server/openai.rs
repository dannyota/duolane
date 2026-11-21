use super::request_monitor::{
    RequestMonitorGuard, headers_to_record, monitor_response_body, redacted_query,
};
use super::{AppState, current_millis};
use crate::{
    logging::{Logger, create_logger},
    monitor::EndpointKind,
    openai_compat::{MAX_OPENAI_REQUEST_BYTES, OpenAiError, request::extract_model},
    provider::RequestContext,
    providers::codex::{
        chat_completions::request::translate_request,
        native::{openai_error, prepare_native_request},
    },
    session::{self},
    traffic::{TrafficCaptureOptions, create_traffic_capture},
};
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::Response,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

pub(super) async fn handler_responses(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    let query = redacted_query(&uri);
    log.info(
        "request",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!(method.as_str())),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(&query)),
        ])),
    );

    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(&req_id, session_id.clone(), None, EndpointKind::Responses);
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_OPENAI_REQUEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            let response = openai_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "Request body exceeded the size limit".to_string(),
                None,
                Some("request_too_large"),
            );
            log_native_request_completed(
                &log,
                &req_id,
                "responses",
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let body: Value = match parse_native_json_body(&body_bytes) {
        Ok(body) => body,
        Err(response) => {
            log_native_request_completed(
                &log,
                &req_id,
                "responses",
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let (requested_model, normalized_model) = match extract_model(&body) {
        Ok(model) => model,
        Err(error) => return monitor_response_body(error.response(), request_guard),
    };
    let now = current_millis();
    let session_state = session::existing_session(session_id.as_deref(), now);
    let affinity = session_state
        .as_ref()
        .and_then(|session| session.affinity_provider.as_ref());
    let Some(provider) = state
        .registry
        .provider_for_model(&normalized_model, affinity)
    else {
        let response = OpenAiError::invalid(
            format!(
                "Unknown model \"{normalized_model}\". {}",
                state.registry.unknown_codex_model_message().await
            ),
            Some("model"),
        )
        .response();
        return monitor_response_body(response, request_guard);
    };
    if provider.name() != "codex" {
        let response = OpenAiError::unsupported_provider(provider.name()).response();
        log_routed_openai_request_completed(
            &log,
            &req_id,
            "responses",
            provider.name(),
            Some(&normalized_model),
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    }
    let prepared = match prepare_native_request(&body, crate::config::codex_full_lane()) {
        Ok(prepared) => prepared,
        Err(response) => return monitor_response_body(response, request_guard),
    };
    let current = session::record_session_request(
        session_id.as_deref(),
        session_state.as_ref(),
        provider.name(),
        &normalized_model,
        now,
    );
    let effort = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(monitor) = state.monitor.as_ref() {
        if let Some(current) = current.as_ref() {
            monitor.session_sequence_resolved(&req_id, current.seq);
        }
        monitor.provider_selected(&req_id, provider.name(), &normalized_model, effort);
    }
    let traffic = create_traffic_capture(TrafficCaptureOptions {
        req_id: req_id.clone(),
        session_id: session_id.clone(),
        session_seq: current.as_ref().map(|session| session.seq),
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
                "sessionSeq": current.as_ref().map(|session| session.seq),
                "kind": "responses",
                "provider": provider.name(),
                "model": &normalized_model,
                "requestedModel": &requested_model,
                "method": method.as_str(),
                "path": &path,
                "query": &query,
                "headers": headers_to_record(&headers),
            }),
        );
        capture.write_json("010-openai-responses-request", &body);
    }
    let context = RequestContext {
        codex_profile: None,
        recovery_budget: Default::default(),
        req_id: req_id.clone(),
        session_id,
        traffic,
        monitor: state.monitor.clone(),
        passthrough: None,
    };
    let response = match state.native_responses.as_ref() {
        Some(backend) => backend.handle(body, prepared, context).await,
        None => openai_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Native Responses API is disabled",
            None,
            None,
        ),
    };
    log_routed_openai_request_completed(
        &log,
        &req_id,
        "responses",
        provider.name(),
        Some(&normalized_model),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}

pub(super) async fn handler_chat_completions(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    let query = redacted_query(&uri);
    log.info(
        "request",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!(method.as_str())),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(&query)),
        ])),
    );

    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(
            &req_id,
            session_id.clone(),
            None,
            EndpointKind::ChatCompletions,
        );
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_OPENAI_REQUEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            let response = openai_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "Request body exceeded the size limit".to_string(),
                None,
                Some("request_too_large"),
            );
            log_native_request_completed(
                &log,
                &req_id,
                "chat_completions",
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let body = match parse_native_json_body(&body_bytes) {
        Ok(body) => body,
        Err(response) => {
            log_native_request_completed(
                &log,
                &req_id,
                "chat_completions",
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let (requested_model, normalized_model) = match extract_model(&body) {
        Ok(model) => model,
        Err(error) => return monitor_response_body(error.response(), request_guard),
    };
    let now = current_millis();
    let session_state = session::existing_session(session_id.as_deref(), now);
    let affinity = session_state
        .as_ref()
        .and_then(|session| session.affinity_provider.as_ref());
    let Some(provider) = state
        .registry
        .provider_for_model(&normalized_model, affinity)
    else {
        let response = OpenAiError::invalid(
            format!(
                "Unknown model \"{normalized_model}\". {}",
                state.registry.unknown_codex_model_message().await
            ),
            Some("model"),
        )
        .response();
        return monitor_response_body(response, request_guard);
    };
    if provider.name() != "codex" {
        let response = OpenAiError::unsupported_provider(provider.name()).response();
        log_routed_openai_request_completed(
            &log,
            &req_id,
            "chat_completions",
            provider.name(),
            Some(&normalized_model),
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    }
    let translated = match translate_request(body.clone()) {
        Ok(translated) => translated,
        Err(error) => return monitor_response_body(error.response(), request_guard),
    };
    let current = session::record_session_request(
        session_id.as_deref(),
        session_state.as_ref(),
        provider.name(),
        &normalized_model,
        now,
    );
    let effort = translated.effort.clone();
    if let Some(monitor) = state.monitor.as_ref() {
        if let Some(current) = current.as_ref() {
            monitor.session_sequence_resolved(&req_id, current.seq);
        }
        monitor.provider_selected(&req_id, provider.name(), &normalized_model, effort.clone());
    }
    let traffic = create_traffic_capture(TrafficCaptureOptions {
        req_id: req_id.clone(),
        session_id: session_id.clone(),
        session_seq: current.as_ref().map(|session| session.seq),
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
                "sessionSeq": current.as_ref().map(|session| session.seq),
                "kind": "chat_completions",
                "provider": provider.name(),
                "model": &normalized_model,
                "requestedModel": &requested_model,
                "effort": &effort,
                "method": method.as_str(),
                "path": &path,
                "query": &query,
                "headers": headers_to_record(&headers),
            }),
        );
        capture.write_json("010-openai-chat-completions-request", &body);
        capture.write_json("020-upstream-request", &translated.upstream);
    }
    let context = RequestContext {
        codex_profile: None,
        recovery_budget: Default::default(),
        req_id: req_id.clone(),
        session_id,
        traffic,
        monitor: state.monitor.clone(),
        passthrough: None,
    };
    let response = match state.chat_completions.as_ref() {
        Some(backend) => backend.handle(translated, context).await,
        None => openai_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "Chat Completions API is disabled",
            None,
            None,
        ),
    };
    log_routed_openai_request_completed(
        &log,
        &req_id,
        "chat_completions",
        provider.name(),
        Some(&normalized_model),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}

pub(super) fn native_session_id(headers: &http::HeaderMap) -> Option<String> {
    [
        "x-claude-code-session-id",
        "session_id",
        "x-client-request-id",
    ]
    .into_iter()
    .find_map(|name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

#[allow(clippy::result_large_err)]
fn parse_native_json_body(body: &[u8]) -> Result<Value, Response> {
    if body.is_empty() {
        return Err(openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Invalid JSON: empty body",
            None,
            Some("invalid_json"),
        ));
    }
    serde_json::from_slice(body).map_err(|error| {
        openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("Invalid JSON: {error}"),
            None,
            Some("invalid_json"),
        )
    })
}

fn log_routed_openai_request_completed(
    log: &Logger,
    req_id: &str,
    endpoint: &str,
    provider: &str,
    model: Option<&str>,
    status: StatusCode,
    started_at: Instant,
) {
    log.info(
        "request_completed",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(req_id)),
            ("endpoint".to_string(), json!(endpoint)),
            ("provider".to_string(), json!(provider)),
            ("model".to_string(), json!(model)),
            ("countTokens".to_string(), json!(false)),
            ("status".to_string(), json!(status.as_u16())),
            ("ms".to_string(), json!(started_at.elapsed().as_millis())),
        ])),
    );
}

pub(super) fn log_native_request_completed(
    log: &Logger,
    req_id: &str,
    endpoint: &str,
    model: Option<&str>,
    status: StatusCode,
    started_at: Instant,
) {
    log.info(
        "request_completed",
        Some(serde_json::Map::from_iter([
            ("reqId".to_string(), json!(req_id)),
            ("endpoint".to_string(), json!(endpoint)),
            ("provider".to_string(), json!("codex")),
            ("model".to_string(), json!(model)),
            ("countTokens".to_string(), json!(false)),
            ("status".to_string(), json!(status.as_u16())),
            ("ms".to_string(), json!(started_at.elapsed().as_millis())),
        ])),
    );
}
