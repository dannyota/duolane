use super::api::SendEvidence;
use super::auth::manager::ObservedCredential;
use super::client::CodexError;
use super::source_selector::{AttemptFailure, Selection, select};
use super::sources::{
    CodexSources, CredentialId, CredentialObservation, FailureKind, SourceFailure,
};
use crate::config::codex_auth::{AuthSource, load_order};
use crate::provider::RequestContext;
use std::ffi::OsStr;
use std::time::Duration;

fn context() -> RequestContext {
    RequestContext {
        recovery_budget: Default::default(),
        req_id: String::new(),
        session_id: None,
        traffic: None,
        monitor: None,
        passthrough: None,
    }
}
fn observed(source: AuthSource) -> ObservedCredential {
    ObservedCredential {
        id: CredentialId::new(source, b"toy"),
        observation: CredentialObservation(1),
    }
}
#[tokio::test]
async fn spent_source_switches_with_new_model_and_budget() {
    let order = load_order(
        std::path::Path::new("unused"),
        Some(OsStr::new("chatgpt,api-key")),
    )
    .unwrap();
    let sources = CodexSources::new(order);
    let result = select(
        &sources,
        &context(),
        false,
        "terra",
        |source| async move { Ok::<_, ()>(Some(observed(source))) },
        |source, _| {
            Some(if source == AuthSource::ChatGpt {
                "gpt-1"
            } else {
                "gpt-2"
            })
        },
        |attempt, credential, model| async move {
            for _ in 0..3 {
                assert!(
                    attempt
                        .context
                        .recovery_budget
                        .try_retry(Duration::ZERO, false)
                );
            }
            if attempt.source == AuthSource::ChatGpt {
                Err(AttemptFailure {
                    failure: SourceFailure {
                        kind: FailureKind::UsageLimit,
                        send: SendEvidence::DeniedBeforeExecution,
                        error: CodexError::new(429, "spent".into()),
                        credential: Some(credential.id),
                    },
                    response: None::<()>,
                })
            } else {
                Ok(model)
            }
        },
    )
    .await;
    match result {
        Selection::Success { source, value, .. } => {
            assert_eq!(source, AuthSource::ApiKey);
            assert_eq!(value, "gpt-2");
        }
        _ => panic!("expected success"),
    }
}

#[tokio::test]
async fn default_order_never_dispatches_api() {
    let sources = CodexSources::new(load_order(std::path::Path::new("unused"), None).unwrap());
    let result = select(
        &sources,
        &context(),
        false,
        "exact",
        |source| async move { Ok::<_, ()>(Some(observed(source))) },
        |_, model| Some(model.to_owned()),
        |attempt, _, model| async move {
            assert_eq!(attempt.source, AuthSource::ChatGpt);
            Ok::<_, AttemptFailure<()>>(model)
        },
    )
    .await;
    assert!(
        matches!(result, Selection::Success { source: AuthSource::ChatGpt, value, .. } if value == "exact")
    );
}

#[tokio::test]
async fn replaced_credential_is_not_marked() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let sources = CodexSources::new(
        load_order(std::path::Path::new("unused"), Some(OsStr::new("chatgpt"))).unwrap(),
    );
    let reads = AtomicUsize::new(0);
    let old = observed(AuthSource::ChatGpt);
    let new = ObservedCredential {
        id: CredentialId::new(AuthSource::ChatGpt, b"new"),
        observation: CredentialObservation(2),
    };
    let result = select(
        &sources,
        &context(),
        false,
        "x",
        |_| async {
            Ok::<_, ()>(Some(if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                old
            } else {
                new
            }))
        },
        |_, _| Some(()),
        |_, current, _| async move {
            Err::<(), _>(AttemptFailure {
                failure: SourceFailure {
                    kind: FailureKind::RejectedCredential,
                    send: SendEvidence::DeniedBeforeExecution,
                    error: CodexError::new(401, "rejected".into()),
                    credential: Some(current.id),
                },
                response: None::<()>,
            })
        },
    )
    .await;
    assert!(matches!(result, Selection::Failed { .. }));
    assert!(
        sources
            .skip(AuthSource::ChatGpt, new.id, tokio::time::Instant::now())
            .is_none()
    );
}

#[tokio::test]
async fn replay_gate_vetoes_switch_after_evidence() {
    for evidence in 0..3 {
        let sources = CodexSources::new(
            load_order(
                std::path::Path::new("unused"),
                Some(OsStr::new("chatgpt,api-key")),
            )
            .unwrap(),
        );
        let result = select(
            &sources,
            &context(),
            true,
            "x",
            |source| async move { Ok::<_, ()>(Some(observed(source))) },
            |_, _| Some(()),
            |attempt, current, _| async move {
                assert_eq!(attempt.source, AuthSource::ChatGpt);
                match evidence {
                    0 => attempt.gate.mark_hosted(),
                    1 => attempt.gate.mark_committed(),
                    _ => attempt.gate.mark_ambiguous_hosted(),
                }
                Err::<(), _>(AttemptFailure {
                    failure: SourceFailure {
                        kind: FailureKind::UsageLimit,
                        send: SendEvidence::DeniedBeforeExecution,
                        error: CodexError::new(429, "spent".into()),
                        credential: Some(current.id),
                    },
                    response: Some(7),
                })
            },
        )
        .await;
        assert!(matches!(
            result,
            Selection::Failed {
                source: AuthSource::ChatGpt,
                failure: AttemptFailure {
                    response: Some(7),
                    ..
                },
                ..
            }
        ));
    }
}

#[tokio::test]
async fn last_attempt_error_is_returned() {
    let sources = CodexSources::new(
        load_order(
            std::path::Path::new("unused"),
            Some(OsStr::new("chatgpt,api-key")),
        )
        .unwrap(),
    );
    let result = select(
        &sources,
        &context(),
        false,
        "x",
        |source| async move { Ok::<_, ()>(Some(observed(source))) },
        |_, _| Some(()),
        |attempt, _, _| async move {
            Err::<(), _>(AttemptFailure {
                failure: SourceFailure {
                    kind: FailureKind::TransientExhausted,
                    send: SendEvidence::NotSent,
                    error: CodexError::new(
                        if attempt.source == AuthSource::ChatGpt {
                            500
                        } else {
                            503
                        },
                        "failed".into(),
                    ),
                    credential: None,
                },
                response: Some(attempt.source),
            })
        },
    )
    .await;
    assert!(matches!(
        result,
        Selection::Failed {
            source: AuthSource::ApiKey,
            failure: AttemptFailure {
                failure: SourceFailure {
                    error: CodexError { status: 503, .. },
                    ..
                },
                response: Some(AuthSource::ApiKey)
            },
            ..
        }
    ));
}

#[tokio::test]
async fn skipped_marks_keep_first_kind_and_earliest_reset() {
    use super::sources::SkipMark;
    let sources = CodexSources::new(
        load_order(
            std::path::Path::new("unused"),
            Some(OsStr::new("chatgpt,api-key")),
        )
        .unwrap(),
    );
    let now = tokio::time::Instant::now();
    for source in [AuthSource::ChatGpt, AuthSource::ApiKey] {
        let current = observed(source);
        assert!(sources.register(source, current.id, current.observation));
        sources.mark(
            source,
            &SourceFailure {
                kind: if source == AuthSource::ChatGpt {
                    FailureKind::RejectedCredential
                } else {
                    FailureKind::UsageLimit
                },
                send: SendEvidence::DeniedBeforeExecution,
                error: CodexError::new(429, "spent".into()),
                credential: Some(current.id),
            },
            now,
        );
    }
    let result = select(
        &sources,
        &context(),
        false,
        "x",
        |source| async move { Ok::<_, ()>(Some(observed(source))) },
        |_, _| Some(()),
        |_, _, _| async {
            panic!("marked source dispatched");
            #[allow(unreachable_code)]
            Ok::<(), AttemptFailure<()>>(())
        },
    )
    .await;
    assert!(
        matches!(result, Selection::Skipped { source: AuthSource::ChatGpt, reason: super::source_selector::SkipReason::Marked(SkipMark::RejectedCredential), earliest_reset: Some(reset) } if reset == now + Duration::from_secs(3600))
    );
}

#[tokio::test]
async fn unavailable_exact_model_is_resolved_unchanged_for_next_source() {
    let sources = CodexSources::new(
        load_order(
            std::path::Path::new("unused"),
            Some(OsStr::new("chatgpt,api-key")),
        )
        .unwrap(),
    );
    let result = select(
        &sources,
        &context(),
        false,
        "gpt-9-exact",
        |source| async move { Ok::<_, ()>(Some(observed(source))) },
        |source, original| {
            assert_eq!(original, "gpt-9-exact");
            (source == AuthSource::ApiKey).then(|| original.to_owned())
        },
        |attempt, _, model| async move {
            assert_eq!(attempt.source, AuthSource::ApiKey);
            Ok::<_, AttemptFailure<()>>(model)
        },
    )
    .await;
    assert!(matches!(result, Selection::Success { value, .. } if value == "gpt-9-exact"));
}

#[tokio::test]
async fn stale_registration_reobserves_without_dispatching_old_identity() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let sources = CodexSources::new(
        load_order(std::path::Path::new("unused"), Some(OsStr::new("chatgpt"))).unwrap(),
    );
    let old = observed(AuthSource::ChatGpt);
    let new = ObservedCredential {
        id: CredentialId::new(AuthSource::ChatGpt, b"new"),
        observation: CredentialObservation(2),
    };
    assert!(sources.register(AuthSource::ChatGpt, new.id, new.observation));
    let reads = AtomicUsize::new(0);
    let result = select(
        &sources,
        &context(),
        false,
        "x",
        |_| async {
            Ok::<_, ()>(Some(if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                old
            } else {
                new
            }))
        },
        |_, _| Some(()),
        |_, current, _| async move { Ok::<_, AttemptFailure<()>>(current.id == new.id) },
    )
    .await;
    assert!(matches!(result, Selection::Success { value: true, .. }));
}

#[tokio::test]
async fn replaced_during_resolution_is_reobserved_before_dispatch() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let sources = CodexSources::new(
        load_order(std::path::Path::new("unused"), Some(OsStr::new("chatgpt"))).unwrap(),
    );
    let old = observed(AuthSource::ChatGpt);
    let new = ObservedCredential {
        id: CredentialId::new(AuthSource::ChatGpt, b"new"),
        observation: CredentialObservation(2),
    };
    let reads = AtomicUsize::new(0);
    let result = select(
        &sources,
        &context(),
        false,
        "x",
        |_| async {
            Ok::<_, ()>(Some(if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                old
            } else {
                new
            }))
        },
        |source, _| {
            assert!(sources.register(source, new.id, new.observation));
            Some(())
        },
        |_, current, _| async move { Ok::<_, AttemptFailure<()>>(current.id == new.id) },
    )
    .await;
    assert!(matches!(result, Selection::Success { value: true, .. }));
}

#[tokio::test]
async fn stale_generation_of_same_identity_still_marks_failure() {
    use super::sources::SkipMark;
    let sources = CodexSources::new(
        load_order(std::path::Path::new("unused"), Some(OsStr::new("chatgpt"))).unwrap(),
    );
    let old = observed(AuthSource::ChatGpt);
    let result = select(
        &sources,
        &context(),
        false,
        "x",
        |_| async { Ok::<_, ()>(Some(old)) },
        |_, _| Some(()),
        |attempt, _, _| {
            assert!(sources.register(attempt.source, old.id, CredentialObservation(2)));
            async move {
                Err::<(), _>(AttemptFailure {
                    failure: SourceFailure {
                        kind: FailureKind::RejectedCredential,
                        send: SendEvidence::DeniedBeforeExecution,
                        error: CodexError::new(401, "rejected".into()),
                        credential: Some(old.id),
                    },
                    response: None::<()>,
                })
            }
        },
    )
    .await;
    assert!(matches!(result, Selection::Failed { .. }));
    assert_eq!(
        sources.skip(AuthSource::ChatGpt, old.id, tokio::time::Instant::now()),
        Some(SkipMark::RejectedCredential)
    );
}

#[tokio::test]
async fn unavailable_model_takes_precedence_over_quota_mark() {
    let source = AuthSource::ApiKey;
    let sources = CodexSources::new(
        load_order(std::path::Path::new("unused"), Some(OsStr::new("api-key"))).unwrap(),
    );
    let credential = observed(source);
    assert!(sources.register(source, credential.id, credential.observation));
    sources.mark(
        source,
        &SourceFailure {
            kind: FailureKind::UsageLimit,
            send: SendEvidence::DeniedBeforeExecution,
            error: CodexError::new(429, "spent".into()),
            credential: Some(credential.id),
        },
        tokio::time::Instant::now(),
    );
    let result = select(
        &sources,
        &context(),
        false,
        "x",
        |source| async move { Ok::<_, ()>(Some(observed(source))) },
        |_, _| None::<()>,
        |_, _, _| async { Ok::<(), AttemptFailure<()>>(()) },
    )
    .await;
    assert!(matches!(
        result,
        Selection::Skipped {
            reason: super::source_selector::SkipReason::ModelUnavailable,
            earliest_reset: None,
            ..
        }
    ));
}
