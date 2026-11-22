use super::api::SendEvidence;
use super::client::CodexError;
use super::sources::{CredentialId, FailureKind, SourceFailure};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Copy)]
struct AttemptTrace {
    kind: FailureKind,
    send: SendEvidence,
    credential: Option<CredentialId>,
    final_only: bool,
    websocket_started: bool,
    buffered_websocket: bool,
}

impl std::fmt::Debug for AttemptTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttemptTrace").finish_non_exhaustive()
    }
}

impl Default for AttemptTrace {
    fn default() -> Self {
        Self {
            kind: FailureKind::Final,
            send: SendEvidence::OutcomeUnknown,
            credential: None,
            final_only: false,
            websocket_started: false,
            buffered_websocket: false,
        }
    }
}

pub(super) fn buffered_transient_exhausted(
    ctx: &crate::provider::RequestContext,
    transport: crate::config::CodexTransport,
) -> CodexError {
    if (transport == crate::config::CodexTransport::Http
        || (transport == crate::config::CodexTransport::WebSocket
            && ctx.recovery_budget.is_tracked()))
        && !ctx.recovery_budget.hosted()
    {
        ctx.recovery_budget.transient_exhausted()
    } else {
        CodexError::recovery_exhausted()
    }
}

const MAX_RECOVERY_ATTEMPTS: u32 = 3;
const MAX_EMPTY_RECOVERY_ATTEMPTS: u32 = 2;
const RECOVERY_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
struct State {
    attempts: u32,
    empty_attempts: u32,
    deadline: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct RecoveryBudget {
    state: Arc<Mutex<State>>,
    window: Duration,
    gate: Option<super::sources::ReplayGate>,
    trace: Option<Arc<Mutex<AttemptTrace>>>,
}

impl Default for RecoveryBudget {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            window: RECOVERY_WINDOW,
            gate: None,
            trace: None,
        }
    }
}

impl RecoveryBudget {
    pub(super) fn with_gate(gate: super::sources::ReplayGate) -> Self {
        Self {
            gate: Some(gate),
            trace: Some(Arc::default()),
            ..Self::default()
        }
    }

    pub(super) fn stamp(
        &self,
        kind: FailureKind,
        send: SendEvidence,
        credential: Option<CredentialId>,
    ) {
        if let Some(trace) = &self.trace {
            let mut trace = trace.lock().unwrap_or_else(|err| err.into_inner());
            let send = if trace.websocket_started && trace.send == SendEvidence::OutcomeUnknown {
                SendEvidence::OutcomeUnknown
            } else {
                send
            };
            let websocket_started = trace.websocket_started;
            let buffered_websocket = trace.buffered_websocket;
            *trace = AttemptTrace {
                kind,
                send,
                credential,
                final_only: false,
                websocket_started,
                buffered_websocket,
            };
        }
    }

    pub(super) fn enable_buffered_websocket(&self) {
        if let Some(trace) = &self.trace {
            trace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .buffered_websocket = true;
        }
    }

    pub(super) fn buffered_websocket(&self) -> bool {
        self.trace.as_ref().is_some_and(|trace| {
            trace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .buffered_websocket
        })
    }

    pub(super) fn begin_websocket(&self) {
        if let Some(trace) = &self.trace {
            let mut trace = trace.lock().unwrap_or_else(|err| err.into_inner());
            if !trace.websocket_started {
                trace.websocket_started = true;
                trace.send = SendEvidence::NotSent;
            }
        }
    }

    pub(super) fn is_tracked(&self) -> bool {
        self.trace.is_some()
    }

    pub(super) fn begin_http_attempt(&self) {
        if let Some(trace) = &self.trace {
            let mut trace = trace.lock().unwrap_or_else(|err| err.into_inner());
            trace.kind = FailureKind::Final;
            trace.send = SendEvidence::NotSent;
            trace.final_only = false;
        }
    }

    pub(super) fn record_websocket_handshake_error(&self, error: &CodexError) {
        if self.is_tracked()
            && error.origin == super::client::CodexErrorOrigin::WebSocketHandshake
            && error.usage_limit.is_some()
        {
            self.set_kind(FailureKind::UsageLimit);
        }
    }

    pub(super) fn websocket_permanent_error(
        &self,
        response: &super::client::OwnerAwareCodexResponse,
    ) -> Option<CodexError> {
        if !self.is_tracked() {
            return None;
        }
        let permanent = super::client::explicit_permanent_buffered_error(&response.body);
        let context = super::client::codex_status_error_message(&response.body)
            .filter(|message| super::events::is_context_overflow(None, message));
        if permanent.is_none() && context.is_none() {
            return None;
        }
        self.mark_final_only();
        let (status, message, retry_after) = if let Some(failure) = permanent {
            (
                failure.status,
                failure.anthropic_message(),
                failure.retry_after,
            )
        } else {
            (
                400,
                super::events::context_window_message(true, &context.unwrap_or_default()),
                None,
            )
        };
        Some(CodexError {
            status,
            message: message.clone(),
            detail: Some(message),
            retry_after,
            usage_limit: None,
            origin: super::client::CodexErrorOrigin::BufferedWebSocket,
        })
    }

    pub(super) fn mark_final_only(&self) {
        if let Some(trace) = &self.trace {
            trace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .final_only = true;
        }
    }

    pub(super) fn is_final_only(&self) -> bool {
        self.trace.as_ref().is_some_and(|trace| {
            trace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .final_only
        })
    }

    pub(super) fn send(&self) -> SendEvidence {
        self.trace
            .as_ref()
            .map(|trace| trace.lock().unwrap_or_else(|err| err.into_inner()).send)
            .unwrap_or(SendEvidence::OutcomeUnknown)
    }

    pub(super) fn set_send(&self, send: SendEvidence) {
        if let Some(trace) = &self.trace {
            trace.lock().unwrap_or_else(|err| err.into_inner()).send = send;
        }
    }

    pub(super) fn set_kind(&self, kind: FailureKind) {
        if let Some(trace) = &self.trace {
            trace.lock().unwrap_or_else(|err| err.into_inner()).kind = kind;
        }
    }

    pub(super) fn set_credential(&self, credential: CredentialId) {
        if let Some(trace) = &self.trace {
            trace
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .credential = Some(credential);
        }
    }

    pub(super) fn failure(&self, error: CodexError) -> SourceFailure {
        let trace = self
            .trace
            .as_ref()
            .map(|trace| *trace.lock().unwrap_or_else(|err| err.into_inner()))
            .unwrap_or_default();
        SourceFailure {
            kind: trace.kind,
            send: trace.send,
            credential: trace.credential,
            error,
        }
    }

    pub(super) fn mark_ambiguous_hosted(&self) {
        if let Some(gate) = &self.gate
            && gate.has_hosted_tool()
        {
            gate.mark_ambiguous_hosted();
        }
    }

    pub(super) fn http_exhausted(&self, tracked: bool) -> CodexError {
        if tracked {
            self.transient_exhausted()
        } else {
            CodexError::recovery_exhausted()
        }
    }

    pub(super) fn transient_exhausted(&self) -> CodexError {
        if let Some(trace) = &self.trace {
            let mut trace = trace.lock().unwrap_or_else(|err| err.into_inner());
            if !trace.final_only {
                trace.kind = FailureKind::TransientExhausted;
            }
        }
        CodexError::recovery_exhausted()
    }

    pub(super) fn hosted(&self) -> bool {
        self.gate.as_ref().is_some_and(|gate| gate.hosted())
    }

    pub(super) fn mark_committed(&self) {
        if let Some(gate) = &self.gate {
            gate.mark_committed();
        }
    }

    pub(super) fn failed_http_body(&self, decoder: &super::client_sse::HttpSseDecoder) {
        self.body_uncertain();
        if decoder.pending_hosted() {
            self.mark_hosted();
        }
    }

    pub(super) fn body_uncertain(&self) {
        self.set_send(SendEvidence::OutcomeUnknown);
        self.mark_ambiguous_hosted();
    }

    pub(super) fn mark_hosted(&self) {
        if let Some(gate) = &self.gate {
            gate.mark_hosted();
        }
    }

    pub(super) fn quota_denied_before_execution(status: u16, body: &[u8], hosted: bool) -> bool {
        status == 429
            && !hosted
            && serde_json::from_slice::<serde_json::Value>(body)
                .ok()
                .is_some_and(|payload| {
                    payload.as_object().is_some_and(|object| {
                        object.len() == 1
                            && object
                                .get("error")
                                .and_then(serde_json::Value::as_object)
                                .is_some_and(|error| {
                                    error.keys().all(|key| {
                                        matches!(
                                            key.as_str(),
                                            "type"
                                                | "message"
                                                | "code"
                                                | "plan_type"
                                                | "resets_at"
                                                | "resets_in_seconds"
                                        )
                                    })
                                })
                    })
                })
    }

    #[cfg(test)]
    pub(super) fn with_window(window: Duration) -> Self {
        Self {
            state: Arc::default(),
            window,
            gate: None,
            trace: None,
        }
    }

    pub(super) fn start(&self) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state
            .deadline
            .get_or_insert_with(|| Instant::now() + self.window);
    }

    pub(super) fn cancel_unspent_start(&self) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if state.attempts == 0 {
            state.deadline = None;
        }
    }

    pub(super) async fn wait_retry(&self, delay_ms: u64, empty: bool) -> bool {
        if !self.try_retry(Duration::from_millis(delay_ms), empty) {
            return false;
        }
        crate::retry::sleep(delay_ms).await;
        !self.expired()
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .deadline
    }

    pub(super) fn expired(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub(super) fn try_retry(&self, delay: Duration, empty: bool) -> bool {
        if self.hosted() {
            return false;
        }
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        let deadline = *state
            .deadline
            .get_or_insert_with(|| Instant::now() + self.window);
        if state.attempts >= MAX_RECOVERY_ATTEMPTS
            || (empty && state.empty_attempts >= MAX_EMPTY_RECOVERY_ATTEMPTS)
            || Instant::now() + delay >= deadline
        {
            return false;
        }
        state.attempts += 1;
        state.empty_attempts += u32::from(empty);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn refresh_wait_counts_against_recovery_window() {
        let budget = RecoveryBudget::default();
        budget.start();
        tokio::time::advance(Duration::from_secs(25)).await;
        assert!(budget.try_retry(Duration::ZERO, false));
        tokio::time::advance(Duration::from_secs(20)).await;
        assert!(!budget.try_retry(Duration::from_secs(20), false));
    }

    #[test]
    fn clones_share_total_and_empty_attempt_limits() {
        let outer = RecoveryBudget::default();
        let nested = outer.clone();
        assert!(outer.try_retry(Duration::ZERO, true));
        assert!(nested.try_retry(Duration::ZERO, true));
        assert!(!outer.try_retry(Duration::ZERO, true));
        assert!(nested.try_retry(Duration::ZERO, false));
        assert!(!outer.try_retry(Duration::ZERO, false));
    }
}
