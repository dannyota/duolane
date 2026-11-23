use super::AppState;
use super::openai::{log_native_request_completed, native_session_id, native_source_error};
use super::request_monitor::{RequestMonitorGuard, monitor_response_body, redacted_query};
use crate::{
    logging::create_logger,
    monitor::EndpointKind,
    provider::RequestContext,
    providers::codex::images::{
        ImageOperation, ImageRequestError, MAX_EDIT_REQUEST_BYTES, MAX_GENERATION_REQUEST_BYTES,
        MultipartEditInput, UploadedImage, image_error_response, prepare_json_request,
        prepare_multipart_edit,
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

pub(super) async fn handler_image_generation(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    dispatch_image_request(state, req, ImageOperation::Generation).await
}

pub(super) async fn handler_image_edit(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    dispatch_image_request(state, req, ImageOperation::Edit).await
}

async fn dispatch_image_request(
    state: Arc<AppState>,
    req: Request<Body>,
    operation: ImageOperation,
) -> Response {
    let started_at = Instant::now();
    let log = create_logger("server");
    let req_id = Uuid::new_v4().to_string();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let path = uri.path().to_string();
    log.info(
        "request",
        Some(Map::from_iter([
            ("reqId".to_string(), json!(&req_id)),
            ("method".to_string(), json!("POST")),
            ("path".to_string(), json!(&path)),
            ("query".to_string(), json!(redacted_query(&uri))),
        ])),
    );
    let session_id = native_session_id(&headers);
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.request_started(&req_id, session_id.clone(), None, EndpointKind::Images);
        monitor.codex_source(&req_id, "chatgpt");
    }
    let request_guard = RequestMonitorGuard::new(state.monitor.clone(), req_id.clone());
    if let Some(response) = native_source_error(&state.registry).await {
        log_native_request_completed(
            &log,
            &req_id,
            operation.label(),
            None,
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    }

    if uri.query().is_some() {
        let response = image_error_response(ImageRequestError {
            status: StatusCode::BAD_REQUEST,
            message: "Image endpoints do not accept query parameters".to_string(),
            param: None,
            code: Some("invalid_request"),
        });
        log_native_request_completed(
            &log,
            &req_id,
            operation.label(),
            None,
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
    let prepared = if content_type.starts_with("application/json") {
        let limit = match operation {
            ImageOperation::Generation => MAX_GENERATION_REQUEST_BYTES,
            ImageOperation::Edit => MAX_EDIT_REQUEST_BYTES,
        };
        let body = match axum::body::to_bytes(req.into_body(), limit).await {
            Ok(body) => body,
            Err(_) => {
                let response = image_error_response(ImageRequestError {
                    status: StatusCode::PAYLOAD_TOO_LARGE,
                    message: "Image request exceeded the size limit".to_string(),
                    param: None,
                    code: Some("request_too_large"),
                });
                log_native_request_completed(
                    &log,
                    &req_id,
                    operation.label(),
                    None,
                    response.status(),
                    started_at,
                );
                return monitor_response_body(response, request_guard);
            }
        };
        prepare_json_request(operation, &body)
    } else if operation == ImageOperation::Edit && content_type.starts_with("multipart/form-data") {
        let multipart = match Multipart::from_request(req, &()).await {
            Ok(multipart) => multipart,
            Err(_) => {
                let response = image_error_response(ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Invalid multipart image edit request".to_string(),
                    param: None,
                    code: Some("invalid_multipart"),
                });
                log_native_request_completed(
                    &log,
                    &req_id,
                    operation.label(),
                    None,
                    response.status(),
                    started_at,
                );
                return monitor_response_body(response, request_guard);
            }
        };
        parse_multipart_image_edit(multipart).await
    } else {
        let response = image_error_response(ImageRequestError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: if operation == ImageOperation::Edit {
                "Image edit request must use application/json or multipart/form-data".to_string()
            } else {
                "Image generation request must use application/json".to_string()
            },
            param: None,
            code: Some("unsupported_media_type"),
        });
        log_native_request_completed(
            &log,
            &req_id,
            operation.label(),
            None,
            response.status(),
            started_at,
        );
        return monitor_response_body(response, request_guard);
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let response = image_error_response(error);
            log_native_request_completed(
                &log,
                &req_id,
                operation.label(),
                None,
                response.status(),
                started_at,
            );
            return monitor_response_body(response, request_guard);
        }
    };
    let model = prepared.model.clone();
    if let Some(monitor) = state.monitor.as_ref() {
        monitor.provider_selected(&req_id, "codex", &model, None);
    }
    let context = RequestContext {
        recovery_budget: Default::default(),
        req_id: req_id.clone(),
        session_id,
        traffic: None,
        monitor: state.monitor.clone(),
        passthrough: None,
    };
    let response = match state.images.as_ref() {
        Some(backend) => backend.handle(operation, prepared, context).await,
        None => image_error_response(ImageRequestError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Codex Images API is unavailable".to_string(),
            param: None,
            code: Some("images_api_unavailable"),
        }),
    };
    log_native_request_completed(
        &log,
        &req_id,
        operation.label(),
        Some(&model),
        response.status(),
        started_at,
    );
    monitor_response_body(response, request_guard)
}

async fn parse_multipart_image_edit(
    mut multipart: Multipart,
) -> Result<crate::providers::codex::images::PreparedImageRequest, ImageRequestError> {
    let mut input = MultipartEditInput::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ImageRequestError {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid multipart image edit request".to_string(),
            param: None,
            code: Some("invalid_multipart"),
        })?
    {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "image" | "image[]" => {
                let bytes = field.bytes().await.map_err(|_| ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Failed to read uploaded image".to_string(),
                    param: Some("image"),
                    code: Some("invalid_image"),
                })?;
                input.images.push(UploadedImage { bytes });
            }
            "prompt" => {
                input.prompt = Some(multipart_text(field, "prompt").await?);
            }
            "model" => {
                input.model = Some(multipart_text(field, "model").await?);
            }
            "background" => {
                input.background = Some(multipart_text(field, "background").await?);
            }
            "quality" => {
                input.quality = Some(multipart_text(field, "quality").await?);
            }
            "size" => {
                input.size = Some(multipart_text(field, "size").await?);
            }
            "n" => {
                let raw = multipart_text(field, "n").await?;
                input.n = Some(raw.parse::<u8>().map_err(|_| ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "'n' must be an integer between 1 and 10".to_string(),
                    param: Some("n"),
                    code: Some("invalid_request"),
                })?);
            }
            "mask" => {
                return Err(ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: "Image masks are not supported by the Codex image backend".to_string(),
                    param: Some("mask"),
                    code: Some("unsupported_parameter"),
                });
            }
            _ => {
                return Err(ImageRequestError {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("Unsupported multipart field '{name}'"),
                    param: None,
                    code: Some("unsupported_parameter"),
                });
            }
        }
    }
    prepare_multipart_edit(input)
}

pub(super) async fn multipart_text(
    field: axum::extract::multipart::Field<'_>,
    param: &'static str,
) -> Result<String, ImageRequestError> {
    field.text().await.map_err(|_| ImageRequestError {
        status: StatusCode::BAD_REQUEST,
        message: format!("Invalid multipart '{param}' field"),
        param: Some(param),
        code: Some("invalid_multipart"),
    })
}
