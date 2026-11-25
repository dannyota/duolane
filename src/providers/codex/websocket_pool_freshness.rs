use super::PoolEntry;
use std::time::Duration;

impl PoolEntry {
    pub(in super::super) fn needs_validation(&self) -> bool {
        self.returned_at
            .lock()
            .unwrap()
            .is_none_or(|returned| returned.elapsed() >= Duration::from_secs(10))
    }
}
