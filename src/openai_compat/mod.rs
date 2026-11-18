pub mod request;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

pub const MAX_OPENAI_REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct OpenAiError {
    pub status: StatusCode,
    pub message: Box<str>,
    pub param: Option<Box<str>>,
}

impl OpenAiError {
    pub fn invalid(message: impl Into<String>, param: Option<impl Into<String>>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into().into_boxed_str(),
            param: param.map(|value| value.into().into_boxed_str()),
        }
    }

    pub fn unsupported_provider(provider: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: format!("provider '{provider}' does not support OpenAI-compatible generation")
                .into_boxed_str(),
            param: Some("model".into()),
        }
    }

    pub fn response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "message": self.message,
                    "type": "invalid_request_error",
                    "param": self.param,
                    "code": Value::Null,
                }
            })),
        )
            .into_response()
    }
}
