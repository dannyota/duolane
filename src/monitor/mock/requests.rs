use std::{
    collections::VecDeque,
    time::{Duration, Instant, SystemTime},
};

use super::{
    ActiveRequest, COMPLETED_PHASE, CompletedRequest, EndpointKind, REQUEST_TICKS, RequestStatus,
    TICK_MILLIS,
};

pub(super) fn add_simulated_requests(
    now: SystemTime,
    instant_now: Instant,
    tick: u64,
    active: &mut Vec<ActiveRequest>,
    recent: &mut VecDeque<CompletedRequest>,
) {
    let cycle = tick / REQUEST_TICKS;
    let phase = tick % REQUEST_TICKS;
    let completed_cycles = cycle + u64::from(phase >= COMPLETED_PHASE);
    let first_cycle = completed_cycles.saturating_sub(4);

    for completed_cycle in first_cycle..completed_cycles {
        let completion_tick = completed_cycle
            .saturating_mul(REQUEST_TICKS)
            .saturating_add(COMPLETED_PHASE);
        let finished_ago = Duration::from_millis(
            tick.saturating_sub(completion_tick)
                .saturating_mul(TICK_MILLIS),
        );
        recent.push_front(simulated_completed_request(
            now,
            completed_cycle,
            finished_ago,
        ));
    }

    if phase < COMPLETED_PHASE {
        active.insert(0, simulated_active_request(now, instant_now, cycle, phase));
    }
}

pub(super) fn simulated_active_request(
    now: SystemTime,
    instant_now: Instant,
    cycle: u64,
    phase: u64,
) -> ActiveRequest {
    let profile = simulation_profile(cycle);
    let elapsed = Duration::from_millis(phase.saturating_mul(TICK_MILLIS));
    let status = match phase {
        0..=2 => RequestStatus::Started,
        3..=5 => RequestStatus::ProviderSelected,
        6..=9 => RequestStatus::Upstream,
        _ => RequestStatus::Streaming,
    };
    let request_id = format!("req-simulated-{cycle:03}");
    let session_id = format!("demo-session-{cycle:03}");
    let mut request = active_request(
        now,
        instant_now,
        &request_id,
        Some(&session_id),
        Some(cycle + 1),
        EndpointKind::Messages,
        elapsed,
        status,
    );
    if phase >= 2 {
        request.project = Some(profile.project.to_string());
    }
    if phase >= 3 {
        request.provider = Some(profile.provider.to_string());
        request.model = Some(profile.model.to_string());
        request.effort = profile.effort.map(str::to_string);
    }
    if phase >= 10 {
        let generation_ticks = phase - 9;
        let generation_duration =
            Duration::from_millis(generation_ticks.saturating_mul(TICK_MILLIS));
        let output_tokens = generation_ticks
            .saturating_mul(9 + cycle % 5)
            .saturating_add(generation_ticks.saturating_mul(generation_ticks) / 3);
        request.generation_started_at = Some(now - generation_duration);
        request.generation_started_instant = Some(instant_now - generation_duration);
        request.generation_finished_at = Some(now);
        request.generation_duration = Some(generation_duration);
        request.streamed_bytes = output_tokens.saturating_mul(24);
        request.stream_chunks = generation_ticks.saturating_mul(3);
        request.input_tokens = Some(1_800 + cycle.saturating_mul(137));
        request.output_tokens = Some(output_tokens);
    }
    request
}

pub(super) fn simulated_completed_request(
    now: SystemTime,
    cycle: u64,
    finished_ago: Duration,
) -> CompletedRequest {
    let profile = simulation_profile(cycle);
    let failed = cycle % 4 == 3;
    let request_id = format!("req-simulated-{cycle:03}");
    let session_id = format!("demo-session-{cycle:03}");
    let latency = Duration::from_secs(5);
    let mut request = completed_request(
        now,
        &request_id,
        Some(&session_id),
        Some(cycle + 1),
        EndpointKind::Messages,
        finished_ago,
        latency,
        if failed {
            RequestStatus::Failed
        } else {
            RequestStatus::Completed
        },
        Some(if failed { 503 } else { 200 }),
    );
    request.project = Some(profile.project.to_string());
    request.provider = Some(profile.provider.to_string());
    request.model = Some(profile.model.to_string());
    request.effort = profile.effort.map(str::to_string);
    request.input_tokens = Some(1_800 + cycle.saturating_mul(137));
    if failed {
        request.error = Some("simulated upstream overload after streaming began".to_string());
    } else {
        request.generation_duration = Some(Duration::from_millis(2_750));
        request.stream_chunks = 33 + cycle % 9;
        request.output_tokens = Some(128 + cycle.saturating_mul(11));
        request.streamed_bytes = request.output_tokens.unwrap_or(0).saturating_mul(24);
    }
    request
}

#[derive(Clone, Copy)]
pub(super) struct SimulationProfile {
    project: &'static str,
    provider: &'static str,
    model: &'static str,
    effort: Option<&'static str>,
}

pub(super) fn simulation_profile(cycle: u64) -> SimulationProfile {
    const PROFILES: [SimulationProfile; 4] = [
        SimulationProfile {
            project: "live-dashboard",
            provider: "codex",
            model: "gpt-5.6-sol",
            effort: Some("high"),
        },
        SimulationProfile {
            project: "api-client",
            provider: "anthropic",
            model: "claude-sonnet-5-5",
            effort: Some("medium"),
        },
        SimulationProfile {
            project: "editor-extension",
            provider: "anthropic",
            model: "claude-haiku-5-5",
            effort: None,
        },
        SimulationProfile {
            project: "agent-workbench",
            provider: "codex",
            model: "gpt-6.1-terra",
            effort: Some("low"),
        },
    ];
    let index = usize::try_from(cycle % PROFILES.len() as u64).unwrap_or(0);
    PROFILES[index]
}

#[allow(clippy::too_many_arguments)]
pub(super) fn active_request(
    now: SystemTime,
    instant_now: Instant,
    request_id: &str,
    session_id: Option<&str>,
    session_seq: Option<u64>,
    endpoint: EndpointKind,
    elapsed: Duration,
    status: RequestStatus,
) -> ActiveRequest {
    ActiveRequest {
        request_id: request_id.to_string(),
        session_id: session_id.map(str::to_string),
        session_seq,
        project: None,
        provider: None,
        model: None,
        effort: None,
        endpoint,
        started_at: now - elapsed,
        started_instant: instant_now - elapsed,
        generation_started_at: None,
        generation_started_instant: None,
        generation_initial_output_tokens: 0,
        generation_finished_at: None,
        generation_duration: None,
        status,
        streamed_bytes: 0,
        stream_chunks: 0,
        input_tokens: None,
        output_tokens: None,
        error: None,
        traffic_capture_path: None,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn completed_request(
    now: SystemTime,
    request_id: &str,
    session_id: Option<&str>,
    session_seq: Option<u64>,
    endpoint: EndpointKind,
    finished_ago: Duration,
    latency: Duration,
    status: RequestStatus,
    http_status: Option<u16>,
) -> CompletedRequest {
    let finished_at = now - finished_ago;
    CompletedRequest {
        request_id: request_id.to_string(),
        session_id: session_id.map(str::to_string),
        session_seq,
        project: None,
        provider: None,
        model: None,
        effort: None,
        endpoint,
        started_at: finished_at - latency,
        finished_at,
        generation_started_at: None,
        generation_started_instant: None,
        generation_initial_output_tokens: 0,
        generation_finished_at: None,
        generation_duration: None,
        status,
        http_status,
        latency,
        streamed_bytes: 0,
        stream_chunks: 0,
        input_tokens: None,
        output_tokens: None,
        error: None,
        traffic_capture_path: None,
    }
}
