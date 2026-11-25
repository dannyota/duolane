use super::*;

impl CodexHttpClient {
    pub(in super::super) async fn stream_codex_http_events_for_owner_tracked(
        self: &Arc<Self>,
        body: &ResponsesRequest,
        ctx: &RequestContext,
        http_body: &CodexHttpBodyCache,
    ) -> Result<super::super::websocket::CodexWebSocketEventStream, CodexError> {
        let receiver = self
            .stream_codex_http_events_inner(body, ctx, http_body, ctx.recovery_budget.is_tracked())
            .await?;
        Ok(super::super::websocket::CodexWebSocketEventStream::http(
            receiver,
        ))
    }
}
