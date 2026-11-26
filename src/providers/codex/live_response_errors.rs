use super::*;
use crate::providers::codex::recovery::live_retry_exhausted;

pub(super) fn exhausted_response(
    ctx: &RequestContext,
    transport: config::CodexTransport,
    cause: &client::CodexError,
) -> Response {
    let exhausted = live_retry_exhausted(ctx, transport, cause);
    let mut response = map_codex_error_to_response(&exhausted);
    if ctx.recovery_budget.is_tracked() {
        response
            .extensions_mut()
            .insert(Arc::new(ctx.recovery_budget.failure(cause.clone())));
    }
    response
}
pub(in super::super) fn tracked_error_response(
    ctx: &RequestContext,
    error: &client::CodexError,
) -> Response {
    let mut response = map_codex_error_to_response(error);
    if ctx.recovery_budget.is_tracked() {
        response.extensions_mut().insert(Arc::new(
            ctx.recovery_budget.failure_with_cause(error.clone()),
        ));
    }
    response
}
