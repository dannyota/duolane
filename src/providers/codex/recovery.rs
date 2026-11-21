use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

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
}

impl Default for RecoveryBudget {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            window: RECOVERY_WINDOW,
        }
    }
}

impl RecoveryBudget {
    #[cfg(test)]
    pub(super) fn with_window(window: Duration) -> Self {
        Self {
            state: Arc::default(),
            window,
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
