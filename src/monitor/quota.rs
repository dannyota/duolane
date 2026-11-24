use std::time::{Instant, SystemTime};

use crate::providers::codex::translate::{
    reasoning_signature::ReasoningAccount,
    reducer::{CodexRateLimitWindow, parse_codex_rate_limits},
};

use super::{EndpointKind, MonitorHandle};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CodexQuotaSnapshot {
    pub observed_at: SystemTime,
    pub primary: Option<CodexQuotaWindow>,
    pub secondary: Option<CodexQuotaWindow>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CodexQuotaWindow {
    pub used_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    pub reset_at: Option<u64>,
}

#[derive(Debug, Default)]
pub(super) struct QuotaState {
    pub(super) snapshot: Option<CodexQuotaSnapshot>,
    latest: Option<(Instant, String)>,
    account: ReasoningAccount,
    generation: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct CodexQuotaObserver {
    monitor: MonitorHandle,
    generation: u64,
}

impl MonitorHandle {
    pub(crate) fn bind_codex_quota(
        &self,
        request_id: &str,
        account: &ReasoningAccount,
    ) -> Option<CodexQuotaObserver> {
        let mut store = self.store.lock().ok()?;
        let request = store.active.get(request_id)?;
        if request.endpoint != EndpointKind::Messages {
            return None;
        }
        let order = (request.started_instant, request_id.to_owned());
        let quota = &mut store.quota;
        if quota.latest.as_ref().is_some_and(|latest| order < *latest) {
            return None;
        }
        quota.latest = Some(order);
        if quota.account != *account {
            quota.account = account.clone();
            quota.generation = quota.generation.wrapping_add(1);
            quota.snapshot = None;
        }
        account.tag()?;
        Some(CodexQuotaObserver {
            monitor: self.clone(),
            generation: quota.generation,
        })
    }
}

impl CodexQuotaObserver {
    pub(crate) fn observe(&self, payload: &serde_json::Value) {
        if payload.get("type").and_then(|v| v.as_str()) != Some("codex.rate_limits") {
            return;
        }
        let Some(limits) = parse_codex_rate_limits(payload) else {
            return;
        };
        let primary = limits.primary.and_then(window);
        let secondary = limits.secondary.and_then(window);
        if primary.is_none() && secondary.is_none() {
            return;
        }
        if let Ok(mut store) = self.monitor.store.lock()
            && store.quota.generation == self.generation
        {
            store.quota.snapshot = Some(CodexQuotaSnapshot {
                observed_at: SystemTime::now(),
                primary,
                secondary,
            });
        }
    }
}

fn window(value: CodexRateLimitWindow) -> Option<CodexQuotaWindow> {
    let result = CodexQuotaWindow {
        used_percent: value
            .used_percent
            .filter(|v| v.is_finite() && (0.0..=100.0).contains(v)),
        window_minutes: value
            .window_minutes
            .filter(|v| (1..=525_600_000).contains(v)),
        reset_at: value.reset_at.filter(|v| (1..=253_402_300_799).contains(v)),
    };
    (result.used_percent.is_some() || result.window_minutes.is_some() || result.reset_at.is_some())
        .then_some(result)
}
