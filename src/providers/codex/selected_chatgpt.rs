use std::sync::Arc;

use axum::response::Response;

use super::{
    CodexAccountChanged, CodexProvider,
    api::SendEvidence,
    client::CodexError,
    request_state::apply_model_lane_for_request,
    search,
    source_selector::AttemptFailure,
    sources::{FailureKind, SourceAttempt, SourceFailure},
    translate::model_allowlist::ResolvedModel,
};
use crate::{
    anthropic::schema::MessagesRequest, config, provider::RequestContext,
    request_identity::ConversationIdentity,
};

impl CodexProvider {
    pub(super) async fn handle_messages_resolved(
        &self,
        body: &MessagesRequest,
        ctx: RequestContext,
        conversation_identity: Option<ConversationIdentity>,
        mut resolved: ResolvedModel,
    ) -> Response {
        let standalone_search = search::is_standalone_search_request(body);
        let use_responses_lite = !standalone_search
            && apply_model_lane_for_request(&mut resolved, body, config::codex_full_lane());
        let base = config::codex_base_instructions()
            .then(|| super::translate::model_catalog::base_instructions(&resolved.model))
            .flatten();
        let configured_transport = config::codex_transport();
        let websocket_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let compaction_websocket_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle_for_account = |ctx, conversation_identity| {
            self.handle_messages_for_account(
                body,
                ctx,
                conversation_identity,
                &resolved,
                base.as_deref(),
                standalone_search,
                use_responses_lite,
                configured_transport,
                websocket_sent.clone(),
                compaction_websocket_sent.clone(),
            )
        };
        let response = handle_for_account(ctx.clone(), conversation_identity.clone()).await;
        if !should_rebind(&ctx, &response) {
            return response;
        }
        handle_for_account(ctx, conversation_identity).await
    }

    #[allow(clippy::result_large_err)]
    pub async fn tracked_messages_attempt(
        &self,
        body: &MessagesRequest,
        identity: Option<ConversationIdentity>,
        resolved: ResolvedModel,
        attempt: &SourceAttempt,
    ) -> Result<Response, AttemptFailure<Response>> {
        let response = self
            .handle_messages_resolved(body, attempt.context.clone(), identity, resolved)
            .await;
        finish_tracked_response(response, body.stream, attempt)
    }
}

fn should_rebind(ctx: &RequestContext, response: &Response) -> bool {
    !ctx.recovery_budget.hosted() && response.extensions().get::<CodexAccountChanged>().is_some()
}

#[allow(clippy::result_large_err)]
fn finish_tracked_response(
    response: Response,
    stream: bool,
    attempt: &SourceAttempt,
) -> Result<Response, AttemptFailure<Response>> {
    if let Some(failure) = response.extensions().get::<Arc<SourceFailure>>() {
        let failure = SourceFailure {
            kind: failure.kind,
            send: failure.send,
            error: failure.error.clone(),
            credential: failure.credential,
        };
        return Err(AttemptFailure {
            failure,
            response: Some(response),
        });
    }
    if response.status().is_success() {
        if !stream {
            attempt.gate.mark_committed();
        }
        return Ok(response);
    }
    Err(AttemptFailure {
        failure: SourceFailure {
            kind: FailureKind::Final,
            send: SendEvidence::OutcomeUnknown,
            credential: None,
            error: CodexError::new(response.status().as_u16(), "Codex request failed".into()),
        },
        response: Some(response),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::codex_auth::AuthSource;
    use crate::providers::codex::{
        error_response::map_codex_error_to_response,
        live_completion::empty_buffered_completion_error, sources::ReplayGate,
    };

    #[test]
    fn hosted_execution_vetoes_account_rebind() {
        let context = super::super::client_test_support::http_test_context();
        let mut response = crate::anthropic::error::json_error(
            http::StatusCode::CONFLICT,
            "api_error",
            "account changed",
        );
        response.extensions_mut().insert(CodexAccountChanged);
        assert!(should_rebind(&context, &response));
        let gate = ReplayGate::new(true);
        let attempt = SourceAttempt::new(AuthSource::ChatGpt, &context, gate.clone());
        assert!(should_rebind(&attempt.context, &response));
        gate.mark_hosted();
        assert!(!should_rebind(&attempt.context, &response));
    }

    #[test]
    fn compaction_error_retains_typed_quota_and_response_headers() {
        let context = super::super::client_test_support::http_test_context();
        let attempt = SourceAttempt::new(AuthSource::ChatGpt, &context, ReplayGate::new(false));
        let id = super::super::sources::CredentialId::for_chatgpt_auth(
            &super::super::client_test_support::http_test_auth(),
        );
        attempt.context.recovery_budget.stamp(
            FailureKind::UsageLimit,
            SendEvidence::DeniedBeforeExecution,
            Some(id),
        );
        let mut error = CodexError::new(429, "quota".into());
        error.usage_limit = Some(Box::new(super::super::events::CodexUsageLimit {
            message: "quota".into(),
            resets_at: Some(2_000_000_000),
            window: None,
        }));
        let response =
            super::super::live_response::tracked_error_response(&attempt.context, &error);
        assert_eq!(response.status(), http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get("anthropic-ratelimit-unified-reset")
                .and_then(|v| v.to_str().ok()),
            Some("2000000000")
        );
        let failure = response.extensions().get::<Arc<SourceFailure>>().unwrap();
        assert!(failure.kind == FailureKind::UsageLimit);
        assert!(failure.send == SendEvidence::DeniedBeforeExecution);
        assert!(failure.credential == Some(id));
        let untracked = super::super::live_response::tracked_error_response(&context, &error);
        assert_eq!(untracked.status(), response.status());
        assert_eq!(untracked.headers(), response.headers());
        assert!(untracked.extensions().get::<Arc<SourceFailure>>().is_none());
    }

    #[test]
    fn compaction_recovery_exhaustion_retains_recorded_cause() {
        let context = super::super::client_test_support::http_test_context();
        let attempt = SourceAttempt::new(AuthSource::ChatGpt, &context, ReplayGate::new(false));
        let id = super::super::sources::CredentialId::for_chatgpt_auth(
            &super::super::client_test_support::http_test_auth(),
        );
        attempt.context.recovery_budget.stamp(
            FailureKind::TransientExhausted,
            SendEvidence::OutcomeUnknown,
            Some(id),
        );
        let cause = CodexError::new(503, "temporary failure".into());
        attempt.context.recovery_budget.record_cause(&cause);
        let response = super::super::live_response::tracked_error_response(
            &attempt.context,
            &CodexError::recovery_exhausted(),
        );
        let failure = response.extensions().get::<Arc<SourceFailure>>().unwrap();
        assert!(failure.kind == FailureKind::TransientExhausted);
        assert_eq!(failure.error.status, 503);
        assert_eq!(failure.error.message, "temporary failure");
        assert!(failure.credential == Some(id));
    }

    #[test]
    fn unproven_errors_and_empty_completion_are_never_replayable() {
        for response in [
            crate::anthropic::error::json_error(
                http::StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "unproven",
            ),
            crate::anthropic::error::json_error(
                http::StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "unproven",
            ),
            map_codex_error_to_response(&empty_buffered_completion_error()),
        ] {
            let status = response.status();
            let gate = ReplayGate::new(false);
            let context = super::super::client_test_support::http_test_context();
            let attempt = SourceAttempt::new(AuthSource::ChatGpt, &context, gate.clone());
            let failure = match finish_tracked_response(response, false, &attempt) {
                Err(failure) => failure,
                Ok(_) => panic!("expected final failure"),
            };
            assert!(failure.failure.kind == FailureKind::Final);
            assert!(failure.failure.send == SendEvidence::OutcomeUnknown);
            assert!(failure.failure.credential.is_none());
            assert!(!gate.may_switch(failure.failure.kind, failure.failure.send));
            assert_eq!(failure.response.unwrap().status(), status);
        }
    }
}
