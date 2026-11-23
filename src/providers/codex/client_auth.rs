pub(super) async fn retry_auth<T, F, Fut>(
    budget: &super::recovery::RecoveryBudget,
    mut operation: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut retries = 0;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if budget.is_tracked()
                    && matches!(
                        super::auth::manager::classify_auth_failure(&error),
                        super::auth::manager::AuthFailure::Transient
                    )
                {
                    let delay = crate::retry::compute_backoff_delay(retries, None);
                    if budget.wait_retry(delay.wait_ms, false).await {
                        retries += 1;
                        continue;
                    }
                    budget.stamp(
                        super::sources::FailureKind::TransientExhausted,
                        budget.send(),
                        None,
                    );
                } else {
                    trace_auth_failure(budget, &error);
                }
                return Err(error);
            }
        }
    }
}

pub(super) fn trace_auth_failure(budget: &super::recovery::RecoveryBudget, error: &anyhow::Error) {
    if let super::auth::manager::AuthFailure::Rejected(id) =
        super::auth::manager::classify_auth_failure(error)
    {
        budget.stamp(
            super::sources::FailureKind::RejectedCredential,
            super::api::SendEvidence::NotSent,
            Some(id),
        );
    }
}

#[cfg(test)]
mod auth_retry_tests {
    use super::super::client::CodexError;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn tracked_transient_auth_retries_and_exhausts() {
        let budget = super::super::recovery::RecoveryBudget::with_gate(
            super::super::sources::ReplayGate::new(false),
        );
        budget.begin_http_attempt();
        let calls = AtomicUsize::new(0);
        let result: anyhow::Result<()> = retry_auth(&budget, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(super::super::auth::manager::test_transient_error())
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let failure = budget.failure(CodexError::recovery_exhausted());
        assert_eq!(
            failure.kind,
            super::super::sources::FailureKind::TransientExhausted
        );
        assert_eq!(failure.send, super::super::api::SendEvidence::NotSent);
        assert!(failure.credential.is_none());

        let budget = super::super::recovery::RecoveryBudget::with_gate(
            super::super::sources::ReplayGate::new(true),
        );
        budget.set_send(super::super::api::SendEvidence::OutcomeUnknown);
        let result: anyhow::Result<()> = retry_auth(&budget, || async {
            Err(super::super::auth::manager::test_transient_error())
        })
        .await;
        assert!(result.is_err());
        let failure = budget.failure(CodexError::recovery_exhausted());
        assert_eq!(
            failure.kind,
            super::super::sources::FailureKind::TransientExhausted
        );
        assert_eq!(
            failure.send,
            super::super::api::SendEvidence::OutcomeUnknown
        );
        assert!(failure.credential.is_none());

        let untracked = super::super::recovery::RecoveryBudget::default();
        let calls = AtomicUsize::new(0);
        let result: anyhow::Result<()> = retry_auth(&untracked, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(super::super::auth::manager::test_transient_error())
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let budget = super::super::recovery::RecoveryBudget::with_gate(
            super::super::sources::ReplayGate::new(false),
        );
        let calls = AtomicUsize::new(0);
        let result = retry_auth(&budget, || async {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(super::super::auth::manager::test_transient_error())
            } else {
                Ok(())
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
