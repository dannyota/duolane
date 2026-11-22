use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

use super::api::SendEvidence;
use super::client::CodexError;
use super::recovery::RecoveryBudget;
use crate::config::codex_auth::{AuthOrder, AuthSource};
use crate::provider::RequestContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    UsageLimit,
    RejectedCredential,
    TransientExhausted,
    Final,
}

pub struct SourceFailure {
    pub kind: FailureKind,
    pub send: SendEvidence,
    pub error: CodexError,
    pub credential: Option<CredentialId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CredentialId([u8; 32]);

impl CredentialId {
    pub fn new(source: AuthSource, credential: &[u8]) -> Self {
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        digest.update(match source {
            AuthSource::ChatGpt => b"duolane:chatgpt-refresh\0",
            AuthSource::ApiKey => b"duolane:openai-api-key\0",
        });
        digest.update(credential);
        let mut value = [0; 32];
        value.copy_from_slice(digest.finish().as_ref());
        Self(value)
    }
}

const HOSTED: u8 = 1;
const AMBIGUOUS: u8 = 2;
const COMMITTED: u8 = 4;

#[derive(Clone, Default)]
pub struct ReplayGate {
    marks: Arc<AtomicU8>,
    has_hosted_tool: bool,
}

impl ReplayGate {
    pub fn new(has_hosted_tool: bool) -> Self {
        Self {
            has_hosted_tool,
            ..Self::default()
        }
    }

    pub fn mark_hosted(&self) {
        self.marks.fetch_or(HOSTED, Ordering::SeqCst);
    }
    pub fn mark_ambiguous_hosted(&self) {
        self.marks.fetch_or(AMBIGUOUS, Ordering::SeqCst);
    }
    pub fn mark_committed(&self) {
        self.marks.fetch_or(COMMITTED, Ordering::SeqCst);
    }
    pub fn hosted(&self) -> bool {
        self.marks.load(Ordering::SeqCst) & HOSTED != 0
    }
    pub fn ambiguous_hosted(&self) -> bool {
        self.marks.load(Ordering::SeqCst) & AMBIGUOUS != 0
    }
    pub fn committed(&self) -> bool {
        self.marks.load(Ordering::SeqCst) & COMMITTED != 0
    }
    pub fn may_switch(&self, kind: FailureKind, send: SendEvidence) -> bool {
        self.marks.load(Ordering::SeqCst) == 0
            && match (self.has_hosted_tool, kind, send) {
                (_, FailureKind::Final, _) => false,
                (false, _, _) => true,
                (true, FailureKind::TransientExhausted, SendEvidence::NotSent) => true,
                (
                    true,
                    FailureKind::UsageLimit | FailureKind::RejectedCredential,
                    SendEvidence::NotSent | SendEvidence::DeniedBeforeExecution,
                ) => true,
                _ => false,
            }
    }
}

pub struct SourceAttempt {
    pub source: AuthSource,
    pub context: RequestContext,
    pub gate: ReplayGate,
}

impl SourceAttempt {
    pub fn new(source: AuthSource, context: &RequestContext, gate: ReplayGate) -> Self {
        let mut context = context.clone();
        context.recovery_budget = RecoveryBudget::default();
        Self {
            source,
            context,
            gate,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipMark {
    UsageLimit { until: Instant },
    RejectedCredential,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CredentialObservation(pub u64);

#[derive(Default)]
struct SourceSlot {
    generation: Option<CredentialObservation>,
    current: Option<CredentialId>,
    mark: Option<SkipMark>,
}

pub struct CodexSources {
    order: AuthOrder,
    slots: Mutex<[SourceSlot; 2]>,
}

impl CodexSources {
    pub fn new(order: AuthOrder) -> Self {
        Self {
            order,
            slots: Mutex::default(),
        }
    }

    pub fn order(&self) -> &AuthOrder {
        &self.order
    }

    pub fn register(
        &self,
        source: AuthSource,
        id: CredentialId,
        observation: CredentialObservation,
    ) -> bool {
        let mut slots = self.slots.lock().unwrap_or_else(|err| err.into_inner());
        let slot = &mut slots[index(source)];
        if let Some(current) = slot.generation
            && (observation < current || (observation == current && slot.current != Some(id)))
        {
            return false;
        }
        slot.generation = Some(observation);
        if slot.current != Some(id) {
            slot.current = Some(id);
            slot.mark = None;
        }
        true
    }

    pub fn skip(&self, source: AuthSource, id: CredentialId, now: Instant) -> Option<SkipMark> {
        let slots = self.slots.lock().unwrap_or_else(|err| err.into_inner());
        let slot = &slots[index(source)];
        if slot.current != Some(id) {
            return None;
        }
        slot.mark
            .filter(|mark| !matches!(mark, SkipMark::UsageLimit { until } if now >= *until))
    }

    pub fn mark(&self, source: AuthSource, failure: &SourceFailure, now: Instant) {
        let mut slots = self.slots.lock().unwrap_or_else(|err| err.into_inner());
        let slot = &mut slots[index(source)];
        if failure.credential.is_none()
            || slot.current != failure.credential
            || slot.mark == Some(SkipMark::RejectedCredential)
        {
            return;
        }
        let mark = match failure.kind {
            FailureKind::RejectedCredential => Some(SkipMark::RejectedCredential),
            FailureKind::UsageLimit => {
                let reset = failure
                    .error
                    .usage_limit
                    .as_ref()
                    .and_then(|limit| limit.resets_at);
                let delay = reset
                    .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)))
                    .map(|reset| {
                        reset
                            .duration_since(SystemTime::now())
                            .unwrap_or(Duration::ZERO)
                    })
                    .unwrap_or(Duration::from_secs(3600));
                now.checked_add(delay)
                    .or_else(|| now.checked_add(Duration::from_secs(3600)))
                    .map(|until| SkipMark::UsageLimit { until })
            }
            FailureKind::TransientExhausted | FailureKind::Final => None,
        };
        if let Some(mark) = mark {
            match (slot.mark, mark) {
                (_, SkipMark::UsageLimit { until }) if until <= now => {}
                (Some(SkipMark::UsageLimit { until: current }), SkipMark::UsageLimit { until })
                    if current > now && current >= until => {}
                _ => slot.mark = Some(mark),
            }
        }
    }
}

fn index(source: AuthSource) -> usize {
    match source {
        AuthSource::ChatGpt => 0,
        AuthSource::ApiKey => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::codex_auth::load_order;
    use crate::providers::codex::events::CodexUsageLimit;
    use std::ffi::OsStr;
    use std::path::Path;

    fn context() -> RequestContext {
        RequestContext {
            recovery_budget: RecoveryBudget::default(),
            req_id: String::new(),
            session_id: None,
            traffic: None,
            monitor: None,
            passthrough: None,
        }
    }

    #[test]
    fn codex_source_gate_preserves_attempt_evidence() {
        let gate = ReplayGate::default();
        let first = SourceAttempt::new(AuthSource::ChatGpt, &context(), gate.clone());
        for _ in 0..3 {
            assert!(
                first
                    .context
                    .recovery_budget
                    .try_retry(Duration::ZERO, false)
            );
        }
        assert!(
            !first
                .context
                .recovery_budget
                .try_retry(Duration::ZERO, false)
        );
        gate.mark_ambiguous_hosted();
        let second = SourceAttempt::new(AuthSource::ApiKey, &first.context, gate.clone());
        for _ in 0..3 {
            assert!(
                second
                    .context
                    .recovery_budget
                    .try_retry(Duration::ZERO, false)
            );
        }
        assert!(
            !second
                .context
                .recovery_budget
                .try_retry(Duration::ZERO, false)
        );
        assert!(!gate.may_switch(
            FailureKind::RejectedCredential,
            SendEvidence::DeniedBeforeExecution
        ));
        assert!(gate.ambiguous_hosted());
        let executed = ReplayGate::default();
        executed.mark_hosted();
        assert!(executed.hosted());
        assert!(!executed.may_switch(FailureKind::UsageLimit, SendEvidence::DeniedBeforeExecution));
        let committed = ReplayGate::default();
        committed.mark_committed();
        assert!(committed.committed());
        assert!(!committed.may_switch(
            FailureKind::TransientExhausted,
            SendEvidence::DeniedBeforeExecution
        ));
        let fresh = ReplayGate::default();
        assert!(fresh.may_switch(
            FailureKind::RejectedCredential,
            SendEvidence::DeniedBeforeExecution
        ));
        assert!(fresh.may_switch(
            FailureKind::TransientExhausted,
            SendEvidence::DeniedBeforeExecution
        ));
        assert!(!fresh.may_switch(FailureKind::Final, SendEvidence::DeniedBeforeExecution));
        let hosted_request = ReplayGate::new(true);
        assert!(!hosted_request.may_switch(
            FailureKind::RejectedCredential,
            SendEvidence::OutcomeUnknown
        ));
        assert!(!hosted_request.may_switch(FailureKind::UsageLimit, SendEvidence::OutcomeUnknown));
        assert!(!hosted_request.may_switch(
            FailureKind::TransientExhausted,
            SendEvidence::DeniedBeforeExecution
        ));
        assert!(fresh.may_switch(
            FailureKind::TransientExhausted,
            SendEvidence::OutcomeUnknown
        ));
        assert!(
            hosted_request.may_switch(FailureKind::UsageLimit, SendEvidence::DeniedBeforeExecution)
        );
        assert!(hosted_request.may_switch(FailureKind::TransientExhausted, SendEvidence::NotSent));
        let _next = SourceAttempt::new(AuthSource::ApiKey, &context(), executed.clone());
        assert!(!executed.may_switch(
            FailureKind::RejectedCredential,
            SendEvidence::DeniedBeforeExecution
        ));
        let _next = SourceAttempt::new(AuthSource::ApiKey, &context(), committed.clone());
        assert!(
            !committed.may_switch(FailureKind::UsageLimit, SendEvidence::DeniedBeforeExecution)
        );
    }

    #[test]
    fn codex_source_marks_follow_attempted_credentials() {
        let order = load_order(Path::new("unused"), Some(OsStr::new("chatgpt,api-key"))).unwrap();
        let sources = CodexSources::new(order);
        let now = Instant::now();
        let old = CredentialId::new(AuthSource::ChatGpt, b"old");
        let new = CredentialId::new(AuthSource::ChatGpt, b"new");
        let key = CredentialId::new(AuthSource::ApiKey, b"new");
        assert!(new != key);
        assert!(sources.register(AuthSource::ChatGpt, old, CredentialObservation(1)));
        assert!(sources.register(AuthSource::ChatGpt, new, CredentialObservation(2)));
        let failure = SourceFailure {
            kind: FailureKind::RejectedCredential,
            send: SendEvidence::DeniedBeforeExecution,
            error: CodexError::new(401, String::new()),
            credential: Some(old),
        };
        sources.mark(AuthSource::ChatGpt, &failure, now);
        assert!(sources.skip(AuthSource::ChatGpt, new, now).is_none());
        let failure = SourceFailure {
            credential: Some(new),
            ..failure
        };
        sources.mark(AuthSource::ChatGpt, &failure, now);
        assert_eq!(
            sources.skip(AuthSource::ChatGpt, new, now),
            Some(SkipMark::RejectedCredential)
        );
        sources.mark(
            AuthSource::ChatGpt,
            &SourceFailure {
                credential: Some(old),
                ..failure
            },
            now,
        );
        assert_eq!(
            sources.skip(AuthSource::ChatGpt, new, now),
            Some(SkipMark::RejectedCredential)
        );
        assert!(!sources.register(AuthSource::ChatGpt, old, CredentialObservation(1)));
        assert!(sources.register(AuthSource::ChatGpt, new, CredentialObservation(3)));
        assert_eq!(
            sources.skip(AuthSource::ChatGpt, new, now),
            Some(SkipMark::RejectedCredential)
        );
        let quota_after_rejection = SourceFailure {
            kind: FailureKind::UsageLimit,
            send: SendEvidence::DeniedBeforeExecution,
            error: CodexError::new(429, String::new()),
            credential: Some(new),
        };
        sources.mark(AuthSource::ChatGpt, &quota_after_rejection, now);
        assert_eq!(
            sources.skip(AuthSource::ChatGpt, new, now),
            Some(SkipMark::RejectedCredential)
        );
        assert!(sources.register(AuthSource::ChatGpt, old, CredentialObservation(4)));
        assert!(sources.skip(AuthSource::ChatGpt, old, now).is_none());
        assert!(sources.register(AuthSource::ApiKey, key, CredentialObservation(1)));
        let mut quota = SourceFailure {
            kind: FailureKind::UsageLimit,
            send: SendEvidence::DeniedBeforeExecution,
            error: CodexError::new(429, String::new()),
            credential: Some(key),
        };
        sources.mark(AuthSource::ApiKey, &quota, now);
        assert_eq!(
            sources.skip(AuthSource::ApiKey, key, now),
            Some(SkipMark::UsageLimit {
                until: now + Duration::from_secs(3600)
            })
        );
        assert!(
            sources
                .skip(AuthSource::ApiKey, key, now + Duration::from_secs(3600))
                .is_none()
        );
        let reset = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 30;
        quota.error.usage_limit = Some(Box::new(CodexUsageLimit {
            message: String::new(),
            resets_at: Some(reset),
            window: None,
        }));
        sources.mark(AuthSource::ApiKey, &quota, now);
        assert_eq!(
            sources.skip(AuthSource::ApiKey, key, now),
            Some(SkipMark::UsageLimit {
                until: now + Duration::from_secs(3600)
            })
        );
        let now = now + Duration::from_secs(3600);
        sources.mark(AuthSource::ApiKey, &quota, now);
        let Some(SkipMark::UsageLimit { until }) = sources.skip(AuthSource::ApiKey, key, now)
        else {
            panic!("missing quota mark")
        };
        assert!(until > now && until <= now + Duration::from_secs(30));
        assert!(sources.skip(AuthSource::ApiKey, key, until).is_none());
        quota.error.usage_limit.as_mut().unwrap().resets_at = Some(0);
        sources.mark(AuthSource::ApiKey, &quota, now);
        assert_eq!(
            sources.skip(AuthSource::ApiKey, key, now),
            Some(SkipMark::UsageLimit { until })
        );
        quota.error.usage_limit.as_mut().unwrap().resets_at = Some(u64::MAX);
        sources.mark(AuthSource::ApiKey, &quota, now);
        assert_eq!(
            sources.skip(AuthSource::ApiKey, key, now),
            Some(SkipMark::UsageLimit {
                until: now + Duration::from_secs(3600)
            })
        );
        assert!(sources.register(AuthSource::ChatGpt, new, CredentialObservation(5)));
        let transient = SourceFailure {
            kind: FailureKind::TransientExhausted,
            send: SendEvidence::NotSent,
            error: CodexError::new(503, String::new()),
            credential: Some(new),
        };
        sources.mark(AuthSource::ChatGpt, &transient, now);
        assert!(sources.skip(AuthSource::ChatGpt, new, now).is_none());
    }
}
