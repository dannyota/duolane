use super::{
    ActiveRequest, CompletedRequest, EndpointKind, MonitorState, RequestStatus, SessionSummary,
    Throughput,
};
use crate::text::terminal_text;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

pub const PROTOCOL_VERSION: u32 = 1;

fn default_snapshot_at() -> SystemTime {
    SystemTime::now()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorResponse {
    pub version: u32,
    pub snapshot: MonitorSnapshot,
}

impl From<MonitorState> for MonitorResponse {
    fn from(state: MonitorState) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            snapshot: state.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum SnapshotUpdate {
    Live(MonitorSnapshot),
    Disconnected {
        snapshot: MonitorSnapshot,
        error: String,
    },
}

impl SnapshotUpdate {
    pub fn snapshot(&self) -> &MonitorSnapshot {
        match self {
            Self::Live(snapshot) | Self::Disconnected { snapshot, .. } => snapshot,
        }
    }

    pub fn connection_error(&self) -> Option<&str> {
        match self {
            Self::Live(_) => None,
            Self::Disconnected { error, .. } => Some(error),
        }
    }

    pub(super) fn updated(self, result: Result<MonitorSnapshot, String>) -> Self {
        match result {
            Ok(snapshot) => Self::Live(snapshot),
            Err(error) => {
                let snapshot = match self {
                    Self::Live(snapshot) | Self::Disconnected { snapshot, .. } => snapshot,
                };
                Self::Disconnected {
                    snapshot,
                    error: terminal_text(&error),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexConnections {
    pub queued_attempts: u64,
    pub handshakes_in_flight: u64,
    pub handshake_spacing_ms: u64,
    pub websocket_reused: u64,
    pub websocket_new: u64,
    pub http_fallbacks: u64,
    pub origin_403: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_connections: Option<CodexConnections>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_quota: Option<super::CodexQuotaSnapshot>,
    pub started_at: SystemTime,
    #[serde(default = "default_snapshot_at")]
    pub snapshot_at: SystemTime,
    pub uptime: Duration,
    pub sessions: Vec<SessionSnapshot>,
    pub active: Vec<ActiveSnapshot>,
    pub recent: Vec<CompletedSnapshot>,
}

impl MonitorSnapshot {
    pub fn for_terminal(&self) -> Self {
        let mut snapshot = self.clone();
        for session in &mut snapshot.sessions {
            terminal_fields([
                &mut session.session_id,
                &mut session.project,
                &mut session.provider,
                &mut session.model,
                &mut session.effort,
            ]);
        }
        for request in &mut snapshot.active {
            request.request_id = terminal_text(&request.request_id);
            terminal_fields([
                &mut request.session_id,
                &mut request.project,
                &mut request.provider,
                &mut request.codex_source,
                &mut request.model,
                &mut request.effort,
                &mut request.error,
            ]);
        }
        for request in &mut snapshot.recent {
            request.request_id = terminal_text(&request.request_id);
            terminal_fields([
                &mut request.session_id,
                &mut request.project,
                &mut request.provider,
                &mut request.codex_source,
                &mut request.model,
                &mut request.effort,
                &mut request.error,
            ]);
        }
        snapshot
    }
}

fn terminal_fields<const N: usize>(fields: [&mut Option<String>; N]) {
    for field in fields.into_iter().flatten() {
        *field = terminal_text(field);
    }
}

impl From<MonitorState> for MonitorSnapshot {
    fn from(state: MonitorState) -> Self {
        Self {
            codex_connections: state.codex_connections,
            codex_quota: state.codex_quota,
            started_at: state.started_at,
            snapshot_at: SystemTime::now(),
            uptime: state.uptime,
            sessions: state.sessions.into_iter().map(Into::into).collect(),
            active: state.active.into_iter().map(Into::into).collect(),
            recent: state.recent.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_place: Option<u32>,
    pub request_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub project: Option<String>,
    pub provider: Option<String>,
    #[serde(default)]
    pub codex_source: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub started_at: SystemTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_upstream_event_at: Option<SystemTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_output_at: Option<SystemTime>,
    pub generation_started_at: Option<SystemTime>,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
    pub status: RequestStatus,
    pub streamed_bytes: u64,
    pub stream_chunks: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub error: Option<String>,
    pub traffic_capture_path: Option<PathBuf>,
    elapsed: Duration,
    throughput: Throughput,
}

impl ActiveSnapshot {
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
    pub fn rate(&self) -> Throughput {
        self.throughput.clone()
    }
}

impl From<ActiveRequest> for ActiveSnapshot {
    fn from(value: ActiveRequest) -> Self {
        let throughput = value.rate();
        let elapsed = value.elapsed();
        Self {
            queue_place: value.queue_place,
            request_id: value.request_id,
            session_id: value.session_id,
            session_seq: value.session_seq,
            project: value.project,
            provider: value.provider,
            codex_source: value.codex_source,
            model: value.model,
            effort: value.effort,
            endpoint: value.endpoint,
            started_at: value.started_at,
            first_upstream_event_at: value.first_upstream_event_at,
            first_output_at: value.first_output_at,
            generation_started_at: value.generation_started_at,
            generation_finished_at: value.generation_finished_at,
            generation_duration: value.generation_duration,
            status: value.status,
            streamed_bytes: value.streamed_bytes,
            stream_chunks: value.stream_chunks,
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            error: value.error,
            traffic_capture_path: value.traffic_capture_path,
            elapsed,
            throughput,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedSnapshot {
    pub request_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub project: Option<String>,
    pub provider: Option<String>,
    #[serde(default)]
    pub codex_source: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub started_at: SystemTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_upstream_event_at: Option<SystemTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_output_at: Option<SystemTime>,
    pub generation_started_at: Option<SystemTime>,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
    pub status: RequestStatus,
    pub streamed_bytes: u64,
    pub stream_chunks: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub error: Option<String>,
    pub traffic_capture_path: Option<PathBuf>,
    pub finished_at: SystemTime,
    pub http_status: Option<u16>,
    #[serde(default)]
    pub retry_requested: bool,
    pub latency: Duration,
    throughput: Throughput,
}

impl CompletedSnapshot {
    pub fn rate(&self) -> Throughput {
        self.throughput.clone()
    }
}

impl From<CompletedRequest> for CompletedSnapshot {
    fn from(value: CompletedRequest) -> Self {
        let throughput = value.rate();
        Self {
            request_id: value.request_id,
            session_id: value.session_id,
            session_seq: value.session_seq,
            project: value.project,
            provider: value.provider,
            codex_source: value.codex_source,
            model: value.model,
            effort: value.effort,
            endpoint: value.endpoint,
            started_at: value.started_at,
            first_upstream_event_at: value.first_upstream_event_at,
            first_output_at: value.first_output_at,
            generation_started_at: value.generation_started_at,
            generation_finished_at: value.generation_finished_at,
            generation_duration: value.generation_duration,
            status: value.status,
            streamed_bytes: value.streamed_bytes,
            stream_chunks: value.stream_chunks,
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            error: value.error,
            traffic_capture_path: value.traffic_capture_path,
            finished_at: value.finished_at,
            http_status: value.http_status,
            retry_requested: value.retry_requested,
            latency: value.latency,
            throughput,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub active_count: usize,
    pub request_count: usize,
    pub failure_count: usize,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub last_seen: SystemTime,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub output_token_samples: Vec<(SystemTime, u64)>,
    pub generation_duration: Duration,
    pub last_status: String,
    throughput: Throughput,
}

impl SessionSnapshot {
    pub fn rate(&self) -> Throughput {
        self.throughput.clone()
    }
}

impl From<SessionSummary> for SessionSnapshot {
    fn from(value: SessionSummary) -> Self {
        let throughput = value.rate();
        Self {
            session_id: value.session_id,
            project: value.project,
            active_count: value.active_count,
            request_count: value.request_count,
            failure_count: value.failure_count,
            provider: value.provider,
            model: value.model,
            effort: value.effort,
            last_seen: value.last_seen,
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            output_token_samples: value.output_token_samples,
            generation_duration: value.generation_duration,
            last_status: value.last_status,
            throughput,
        }
    }
}

impl SessionSnapshot {
    pub fn label(&self) -> String {
        self.session_id
            .clone()
            .unwrap_or_else(|| "no-session".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::MonitorHandle;

    #[test]
    fn codex_connections_are_optional_and_legacy_compatible() {
        let snapshot: MonitorSnapshot = MonitorHandle::default().snapshot().into();
        let mut value = serde_json::to_value(&snapshot).unwrap();
        value.as_object_mut().unwrap().remove("codex_connections");
        let decoded: MonitorSnapshot = serde_json::from_value(value.clone()).unwrap();
        assert!(
            serde_json::to_value(&decoded)
                .unwrap()
                .get("codex_connections")
                .is_none()
        );
        value["codex_connections"] = serde_json::json!({
            "queued_attempts": 2, "handshakes_in_flight": 1, "handshake_spacing_ms": 1000,
            "websocket_reused": 4, "websocket_new": 3, "http_fallbacks": 2, "origin_403": 1
        });
        let decoded: MonitorSnapshot = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap()["codex_connections"],
            value["codex_connections"]
        );
    }

    #[test]
    fn queue_place_is_optional_and_legacy_compatible() {
        let monitor = MonitorHandle::default();
        monitor.request_started("active", None, None, EndpointKind::Messages);
        let mut value = serde_json::to_value(MonitorSnapshot::from(monitor.snapshot())).unwrap();
        assert!(value["active"][0].get("queue_place").is_none());
        value["active"][0]["queue_place"] = serde_json::json!(2);
        let decoded: MonitorSnapshot = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded.active[0].queue_place, Some(2));
        value["active"][0]
            .as_object_mut()
            .unwrap()
            .remove("queue_place");
        let decoded: MonitorSnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.active[0].queue_place, None);
    }

    #[test]
    fn absent_phase_times_remain_compatible_with_legacy_snapshots() {
        let monitor = MonitorHandle::default();
        monitor.request_started("active", None, None, EndpointKind::Messages);
        monitor.request_started("done", None, None, EndpointKind::Messages);
        monitor.request_completed("done", 200, None, None);
        let value = serde_json::to_value(MonitorSnapshot::from(monitor.snapshot())).unwrap();
        for request in [&value["active"][0], &value["recent"][0]] {
            assert!(request.get("first_upstream_event_at").is_none());
            assert!(request.get("first_output_at").is_none());
        }
        let decoded: MonitorSnapshot = serde_json::from_value(value).unwrap();
        assert!(decoded.active[0].first_output_at.is_none());
        assert!(decoded.recent[0].first_upstream_event_at.is_none());
    }

    #[test]
    fn first_upstream_event_survives_generation_restart() {
        let monitor = MonitorHandle::default();
        monitor.request_started("retry", None, None, EndpointKind::Messages);
        monitor.generation_started("retry");
        let first = serde_json::to_value(MonitorSnapshot::from(monitor.snapshot())).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        monitor.generation_started("retry");
        let second = serde_json::to_value(MonitorSnapshot::from(monitor.snapshot())).unwrap();
        assert!(!first["active"][0]["first_upstream_event_at"].is_null());
        assert_eq!(
            first["active"][0]["first_upstream_event_at"],
            second["active"][0]["first_upstream_event_at"]
        );
        assert_ne!(
            first["active"][0]["generation_started_at"],
            second["active"][0]["generation_started_at"]
        );
    }

    #[test]
    fn completed_source_round_trips_and_legacy_snapshot_defaults() {
        let monitor = MonitorHandle::default();
        monitor.request_started("done", None, None, EndpointKind::Messages);
        monitor.codex_source("done", "api key (from chatgpt: usage limit)");
        monitor.request_completed("done", 200, None, None);
        let snapshot: MonitorSnapshot = monitor.snapshot().into();
        let mut value = serde_json::to_value(&snapshot).unwrap();
        let decoded: MonitorSnapshot = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            decoded.recent[0].codex_source.as_deref(),
            Some("api key (from chatgpt: usage limit)")
        );
        value["recent"][0]
            .as_object_mut()
            .unwrap()
            .remove("codex_source");
        let legacy: MonitorSnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.recent[0].codex_source, None);
    }

    #[test]
    fn snapshot_round_trip_preserves_accounting_without_process_clocks() {
        let monitor = MonitorHandle::default();
        monitor.request_started(
            "active",
            Some("session".into()),
            Some(2),
            EndpointKind::Messages,
        );
        monitor.provider_selected("active", "codex", "gpt-6-astra", Some("high".into()));
        monitor.stream_progress("active", 40, 1, Some(8), Some(3));
        monitor.request_started(
            "done",
            Some("session".into()),
            Some(1),
            EndpointKind::CountTokens,
        );
        monitor.request_completed("done", 200, Some(11), None);
        let snapshot = MonitorSnapshot::from(monitor.snapshot());
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("instant"));
        let decoded: MonitorSnapshot = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.snapshot_at, snapshot.snapshot_at);
        assert_eq!(decoded.uptime, snapshot.uptime);
        assert_eq!(decoded.active[0].elapsed(), snapshot.active[0].elapsed());
        assert_eq!(decoded.active[0].rate(), snapshot.active[0].rate());
        assert_eq!(decoded.sessions[0].rate(), snapshot.sessions[0].rate());
        assert_eq!(decoded.recent[0].http_status, Some(200));
    }

    #[test]
    fn legacy_completed_snapshot_defaults_retry_marker_to_false() {
        let monitor = MonitorHandle::default();
        monitor.request_started("done", None, None, EndpointKind::Messages);
        monitor.request_failed(
            "done",
            Some(400),
            "capability_rejected: beta_header:message-threads-2026-08-12",
        );
        let snapshot: MonitorSnapshot = monitor.snapshot().into();
        let mut legacy = serde_json::to_value(snapshot).unwrap();
        legacy["recent"][0]
            .as_object_mut()
            .unwrap()
            .remove("retry_requested");
        let decoded: MonitorSnapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap()["recent"][0]["retry_requested"],
            false
        );
    }

    #[test]
    fn snapshot_deserialization_accepts_legacy_and_newer_fields() {
        let snapshot: MonitorSnapshot = MonitorHandle::default().snapshot().into();
        let mut legacy = serde_json::to_value(&snapshot).unwrap();
        legacy.as_object_mut().unwrap().remove("snapshot_at");
        let decoded: MonitorSnapshot = serde_json::from_value(legacy).unwrap();
        assert!(decoded.snapshot_at >= snapshot.started_at);

        let mut newer = serde_json::to_value(&snapshot).unwrap();
        newer
            .as_object_mut()
            .unwrap()
            .insert("future_field".into(), serde_json::json!(true));
        let decoded: MonitorSnapshot = serde_json::from_value(newer).unwrap();
        assert_eq!(decoded.snapshot_at, snapshot.snapshot_at);
    }
}
