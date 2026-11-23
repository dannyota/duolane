use std::future::Future;
use tokio::time::Instant;

use super::auth::manager::ObservedCredential;
use super::sources::{
    CodexSources, FailureKind, ReplayGate, SkipMark, SourceAttempt, SourceDecision, SourceFailure,
};
use crate::config::codex_auth::AuthSource;
use crate::provider::RequestContext;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    MissingCredential,
    ModelUnavailable,
    Marked(SkipMark),
    StaleObservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    Skipped(SkipReason),
    Failed(FailureKind),
}

pub struct AttemptFailure<R> {
    pub failure: SourceFailure,
    pub response: Option<R>,
}

pub enum Selection<T, R, E> {
    Success {
        source: AuthSource,
        value: T,
        fallback: Option<(AuthSource, FallbackReason)>,
    },
    Failed {
        source: AuthSource,
        failure: AttemptFailure<R>,
        fallback: Option<(AuthSource, FallbackReason)>,
    },
    Skipped {
        source: AuthSource,
        reason: SkipReason,
        earliest_reset: Option<Instant>,
    },
    ObservationError(E),
}

pub async fn select<T, R, E, M, O, OF, Resolve, Attempt, AF>(
    sources: &CodexSources,
    context: &RequestContext,
    hosted: bool,
    original_model: &str,
    mut observe: O,
    mut resolve: Resolve,
    mut attempt: Attempt,
) -> Selection<T, R, E>
where
    O: FnMut(AuthSource) -> OF,
    OF: Future<Output = Result<Option<ObservedCredential>, E>>,
    Resolve: FnMut(AuthSource, &str) -> Option<M>,
    Attempt: FnMut(SourceAttempt, ObservedCredential, M) -> AF,
    AF: Future<Output = Result<T, AttemptFailure<R>>>,
{
    let gate = ReplayGate::new(hosted);
    let mut first_skip = None;
    let mut earliest_reset: Option<Instant> = None;
    let mut previous = None;
    let mut fallback = None;
    let mut previous_fallback = None;
    for &source in sources.order().sources() {
        let mut selected = None;
        let mut skip = None;
        for _ in 0..2 {
            let current = match observe(source).await {
                Err(err) => {
                    return match previous {
                        Some((source, failure)) => Selection::Failed {
                            source,
                            failure,
                            fallback: previous_fallback,
                        },
                        None => Selection::ObservationError(err),
                    };
                }
                Ok(None) => {
                    skip = Some(SkipReason::MissingCredential);
                    break;
                }
                Ok(Some(current)) => current,
            };
            if !sources.register(source, current.id, current.observation) {
                skip = Some(SkipReason::StaleObservation);
                continue;
            }
            let model = resolve(source, original_model);
            match sources.decision(source, current.id, Instant::now()) {
                SourceDecision::Stale => skip = Some(SkipReason::StaleObservation),
                SourceDecision::Ready | SourceDecision::Marked(_) if model.is_none() => {
                    skip = Some(SkipReason::ModelUnavailable);
                    break;
                }
                SourceDecision::Marked(mark) => {
                    if let SkipMark::UsageLimit { until } = mark {
                        earliest_reset = Some(earliest_reset.map_or(until, |old| old.min(until)));
                    }
                    skip = Some(SkipReason::Marked(mark));
                    break;
                }
                SourceDecision::Ready => {
                    if let Some(model) = model {
                        selected = Some((current, model));
                        skip = None;
                    } else {
                        skip = Some(SkipReason::ModelUnavailable);
                    }
                    break;
                }
            }
        }
        if let Some(reason) = skip {
            first_skip.get_or_insert((source, reason));
            fallback.get_or_insert((source, FallbackReason::Skipped(reason)));
            continue;
        }
        let Some((current, model)) = selected else {
            continue;
        };
        let task = SourceAttempt::new(source, context, gate.clone());
        match attempt(task, current, model).await {
            Ok(value) => {
                return Selection::Success {
                    source,
                    value,
                    fallback,
                };
            }
            Err(failure) => {
                if matches!(
                    failure.failure.kind,
                    FailureKind::UsageLimit | FailureKind::RejectedCredential
                ) && let Ok(Some(latest)) = observe(source).await
                    && failure.failure.credential == Some(latest.id)
                {
                    sources.register(source, latest.id, latest.observation);
                    sources.mark(source, &failure.failure, Instant::now());
                }
                if !gate.may_switch(failure.failure.kind, failure.failure.send) {
                    return Selection::Failed {
                        source,
                        failure,
                        fallback,
                    };
                }
                previous_fallback = fallback;
                fallback = Some((source, FallbackReason::Failed(failure.failure.kind)));
                previous = Some((source, failure));
            }
        }
    }
    if let Some((source, failure)) = previous {
        Selection::Failed {
            source,
            failure,
            fallback: previous_fallback,
        }
    } else if let Some((source, reason)) = first_skip {
        Selection::Skipped {
            source,
            reason,
            earliest_reset,
        }
    } else {
        unreachable!("validated source order is nonempty")
    }
}
