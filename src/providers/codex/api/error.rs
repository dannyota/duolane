use serde_json::Value;

use super::super::client::CodexError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFailureKind {
    Quota,
    Rejected,
    Transient,
    HeaderTimeout,
    ContextOverflow,
    Invalid,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendEvidence {
    NotSent,
    DeniedBeforeExecution,
    OutcomeUnknown,
}

#[derive(Debug)]
pub struct ApiFailure {
    pub kind: ApiFailureKind,
    pub send: SendEvidence,
    pub hosted_executed: bool,
    pub error: CodexError,
}

impl ApiFailure {
    pub(super) fn local(kind: ApiFailureKind, send: SendEvidence, hosted: bool) -> Self {
        let (status, message) = match kind {
            ApiFailureKind::Quota => (429, "OpenAI API quota exhausted"),
            ApiFailureKind::Rejected => (401, "OpenAI API credential rejected"),
            ApiFailureKind::Transient => (503, "OpenAI API temporarily unavailable"),
            ApiFailureKind::HeaderTimeout => (504, "OpenAI API response headers timed out"),
            ApiFailureKind::ContextOverflow => (400, "OpenAI API context length exceeded"),
            ApiFailureKind::Invalid => (400, "OpenAI API rejected the request"),
            ApiFailureKind::Other => (502, "OpenAI API response could not be processed"),
        };
        Self {
            kind,
            send,
            hosted_executed: hosted,
            error: CodexError::new(status, message.into()),
        }
    }

    pub(super) fn upstream(status: u16, value: Option<&Value>, hosted: bool, stream: bool) -> Self {
        let error = value.and_then(|v| {
            v.get("error")
                .or_else(|| v.pointer("/response/error"))
                .or(Some(v))
        });
        let code = error.and_then(|e| e.get("code")).and_then(Value::as_str);
        let typ = error.and_then(|e| e.get("type")).and_then(Value::as_str);
        let kind = if status >= 500 {
            ApiFailureKind::Transient
        } else if status == 401 {
            ApiFailureKind::Rejected
        } else if (status == 429 || (stream && status == 0))
            && (typ == Some("insufficient_quota")
                || code == Some("insufficient_quota")
                || (status == 429 && code == Some("credit_balance_exhausted")))
        {
            ApiFailureKind::Quota
        } else if code == Some("context_length_exceeded") {
            ApiFailureKind::ContextOverflow
        } else if status == 429 || status == 408 {
            ApiFailureKind::Transient
        } else if (400..500).contains(&status) {
            ApiFailureKind::Invalid
        } else {
            ApiFailureKind::Other
        };
        let send = if !stream
            && !hosted
            && (status == 401 || (status == 429 && kind == ApiFailureKind::Quota))
        {
            SendEvidence::DeniedBeforeExecution
        } else {
            SendEvidence::OutcomeUnknown
        };
        let mut failure = Self::local(kind, send, hosted);
        if (400..=599).contains(&status) && kind != ApiFailureKind::Quota {
            failure.error.status = status;
        }
        failure
    }
}
