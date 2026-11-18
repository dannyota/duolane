use super::AppState;
use super::images::multipart_text;
use super::openai::{log_native_request_completed, native_session_id};
use super::request_monitor::{RequestMonitorGuard, monitor_response_body};
use crate::{
    logging::create_logger,
    monitor::EndpointKind,
    provider::RequestContext,
    providers::codex::transcription::{
        TranscriptionRequestError, prepare_transcription, transcription_error_response,
    },
};
use axum::{
    body::Body,
    extract::{FromRequest, Multipart, State},
    http::{Request, StatusCode},
    response::Response,
};
use serde_json::{Map, json};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

pub(super) async fn handler_transcription(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let headers = req.headers().clone();
    log.info(
        "request",
        Some(Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!("POST")),
            ("path".to_string(), json!("/v1/audio/transcriptions")),
            ("query".to_string(), json!({})),
        ])),
    );
    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(
            &req_id,
            session_id.clone(),
            None,
            EndpointKind::Transcriptions,
        );
        monitor.provider_selected(&req_id, "codex", "codex-transcribe", None);
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    if req.uri().query().is_some() {
        let response = transcription_error_response(TranscriptionRequestError::invalid(
            "Transcription endpoint does not accept query parameters",
            None,
        ));
        log_native_request_completed(
            &log,
            &req_id,
            "transcriptions",
            Some("codex-transcribe"),
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    }
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !content_type.starts_with("multipart/form-data") {
        let response = transcription_error_response(TranscriptionRequestError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: "Transcription request must use multipart/form-data".to_string(),
            param: None,
            code: "unsupported_media_type",
        });
        log_native_request_completed(
            &log,
            &req_id,
            "transcriptions",
            Some("codex-transcribe"),
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    }
    let mut multipart = match Multipart::from_request(req, &()).await {
        Ok(multipart) => multipart,
        Err(error) => {
            let status = axum::response::IntoResponse::into_response(error).status();
            let response = transcription_error_response(TranscriptionRequestError {
                status: if status == StatusCode::PAYLOAD_TOO_LARGE {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::BAD_REQUEST
                },
                message: if status == StatusCode::PAYLOAD_TOO_LARGE {
                    "Transcription request exceeded the size limit".to_string()
                } else {
                    "Invalid multipart transcription request".to_string()
                },
                param: None,
                code: if status == StatusCode::PAYLOAD_TOO_LARGE {
                    "request_too_large"
                } else {
                    "invalid_multipart"
                },
            });
            log_native_request_completed(
                &log,
                &req_id,
                "transcriptions",
                Some("codex-transcribe"),
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let mut audio = None;
    let mut filename = None;
    let mut audio_content_type = None;
    let mut language = None;
    while let Some(field) = match multipart.next_field().await {
        Ok(field) => field,
        Err(_) => {
            let response = transcription_error_response(TranscriptionRequestError::invalid(
                "Invalid multipart transcription request",
                None,
            ));
            return monitor_response_body(response, request_guard);
        }
    } {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" => {
                if audio.is_some() {
                    let response =
                        transcription_error_response(TranscriptionRequestError::invalid(
                            "Transcription request must contain one 'file' field",
                            Some("file"),
                        ));
                    return monitor_response_body(response, request_guard);
                }
                filename = field.file_name().map(str::to_string);
                audio_content_type = field.content_type().map(str::to_string);
                audio = match field.bytes().await {
                    Ok(bytes) => Some(bytes),
                    Err(_) => {
                        let response =
                            transcription_error_response(TranscriptionRequestError::invalid(
                                "Failed to read uploaded audio",
                                Some("file"),
                            ));
                        return monitor_response_body(response, request_guard);
                    }
                };
            }
            "language" => {
                language = match multipart_text(field, "language").await {
                    Ok(value) => Some(value),
                    Err(error) => {
                        return monitor_response_body(
                            transcription_error_response(TranscriptionRequestError {
                                status: error.status,
                                message: error.message,
                                param: error.param,
                                code: error.code.unwrap_or("invalid_multipart"),
                            }),
                            request_guard,
                        );
                    }
                };
            }
            "model" => {
                if multipart_text(field, "model").await.is_err() {
                    let response =
                        transcription_error_response(TranscriptionRequestError::invalid(
                            "Invalid multipart 'model' field",
                            Some("model"),
                        ));
                    return monitor_response_body(response, request_guard);
                }
            }
            _ => {
                let response = transcription_error_response(TranscriptionRequestError::invalid(
                    format!("Unsupported multipart field '{name}'"),
                    None,
                ));
                return monitor_response_body(response, request_guard);
            }
        }
    }
    let prepared = match prepare_transcription(audio, filename, audio_content_type, language) {
        Ok(prepared) => prepared,
        Err(error) => {
            let response = transcription_error_response(error);
            log_native_request_completed(
                &log,
                &req_id,
                "transcriptions",
                Some("codex-transcribe"),
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let context = RequestContext {
        req_id: req_id.clone(),
        session_id,
        traffic: None,
        monitor: state.monitor.clone(),
        passthrough: None,
    };
    let response = match state.transcriptions.as_ref() {
        Some(backend) => backend.handle(prepared, context).await,
        None => transcription_error_response(TranscriptionRequestError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Codex transcription API is unavailable".to_string(),
            param: None,
            code: "transcriptions_api_unavailable",
        }),
    };
    log_native_request_completed(
        &log,
        &req_id,
        "transcriptions",
        Some("codex-transcribe"),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}
