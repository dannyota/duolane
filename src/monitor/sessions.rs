use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, SystemTime},
};

use super::{
    ActiveRequest, CompletedRequest, RequestStatus, SESSION_OUTPUT_HISTORY_BUCKETS,
    SESSION_TOKEN_BUCKET_SECS, SessionSummary, store::SessionUsage,
};

pub(super) fn session_summaries(
    active: &[ActiveRequest],
    recent: &VecDeque<CompletedRequest>,
    session_usage: &HashMap<Option<String>, SessionUsage>,
    session_output_buckets: &HashMap<Option<String>, Vec<(u64, u64)>>,
    now: SystemTime,
) -> Vec<SessionSummary> {
    let mut sessions: HashMap<Option<String>, SessionSummary> = HashMap::new();
    let mut identity_ranks = HashMap::new();
    for request in recent.iter().rev() {
        let entry = sessions
            .entry(request.session_id.clone())
            .or_insert_with(|| SessionSummary {
                session_id: request.session_id.clone(),
                project: request.project.clone(),
                active_count: 0,
                request_count: 0,
                failure_count: 0,
                provider: None,
                model: None,
                effort: None,
                last_seen: request.finished_at,
                input_tokens: 0,
                output_tokens: 0,
                output_token_samples: Vec::new(),
                rate_output_tokens: 0,
                generation_duration: Duration::ZERO,
                last_status: "-".to_string(),
            });
        entry.request_count += 1;
        if request.status == RequestStatus::Failed {
            entry.failure_count += 1;
        }
        entry.project = request.project.clone().or(entry.project.clone());
        let rank = (
            request.input_tokens,
            request.input_tokens.is_none() && request.model.is_some(),
            request.started_at,
        );
        let best = identity_ranks
            .entry(request.session_id.clone())
            .or_insert(rank);
        if rank >= *best {
            *best = rank;
            entry.provider = request.provider.clone();
            entry.model = request.model.clone();
            entry.effort = request.effort.clone();
        }
        entry.last_seen = max_system_time(entry.last_seen, request.finished_at);
        if let (Some(tokens), Some(duration)) = (
            request
                .output_tokens
                .and_then(|tokens| tokens.checked_sub(request.generation_initial_output_tokens))
                .filter(|tokens| *tokens > 0),
            request
                .generation_duration
                .filter(|duration| !duration.is_zero()),
        ) {
            entry.rate_output_tokens = entry.rate_output_tokens.saturating_add(tokens);
            entry.generation_duration = entry.generation_duration.saturating_add(duration);
        }
        entry.last_status = request.status.label().to_string();
    }

    for request in active {
        let entry = sessions
            .entry(request.session_id.clone())
            .or_insert_with(|| SessionSummary {
                session_id: request.session_id.clone(),
                project: request.project.clone(),
                active_count: 0,
                request_count: 0,
                failure_count: 0,
                provider: None,
                model: None,
                effort: None,
                last_seen: request.started_at,
                input_tokens: 0,
                output_tokens: 0,
                output_token_samples: Vec::new(),
                rate_output_tokens: 0,
                generation_duration: Duration::ZERO,
                last_status: "-".to_string(),
            });
        entry.active_count += 1;
        entry.request_count += 1;
        entry.project = request.project.clone().or(entry.project.clone());
        let rank = (
            request.input_tokens,
            request.input_tokens.is_none() && request.model.is_some(),
            request.started_at,
        );
        let best = identity_ranks
            .entry(request.session_id.clone())
            .or_insert(rank);
        if rank >= *best {
            *best = rank;
            entry.provider = request.provider.clone();
            entry.model = request.model.clone();
            entry.effort = request.effort.clone();
        }
        entry.last_seen = max_system_time(entry.last_seen, request.started_at);
        if let (Some(tokens), Some(duration)) = (
            request
                .output_tokens
                .and_then(|tokens| tokens.checked_sub(request.generation_initial_output_tokens))
                .filter(|tokens| *tokens > 0),
            request
                .generation_duration
                .filter(|duration| !duration.is_zero()),
        ) {
            entry.rate_output_tokens = entry.rate_output_tokens.saturating_add(tokens);
            entry.generation_duration = entry.generation_duration.saturating_add(duration);
        }
        entry.last_status = request.status.label().to_string();
    }

    let first_bucket = first_session_output_bucket(now);
    for (session_id, session) in &mut sessions {
        if let Some(usage) = session_usage.get(session_id) {
            session.input_tokens = usage.input_tokens;
            session.output_tokens = usage.output_tokens;
        }
        if let Some(buckets) = session_output_buckets.get(session_id) {
            session.output_token_samples = buckets
                .iter()
                .filter(|(bucket, _)| *bucket >= first_bucket)
                .map(|(bucket, tokens)| (session_token_bucket_start(*bucket), *tokens))
                .collect();
        }
    }

    let mut out: Vec<_> = sessions.into_values().collect();
    out.sort_by_key(SessionSummary::label);
    out
}

pub(super) fn session_token_bucket(timestamp: SystemTime) -> u64 {
    timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / SESSION_TOKEN_BUCKET_SECS
}

pub(super) fn session_token_bucket_start(bucket: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(bucket.saturating_mul(SESSION_TOKEN_BUCKET_SECS))
}

pub(super) fn first_session_output_bucket(now: SystemTime) -> u64 {
    session_token_bucket(now).saturating_sub(SESSION_OUTPUT_HISTORY_BUCKETS.saturating_sub(1))
}

pub(super) fn max_system_time(left: SystemTime, right: SystemTime) -> SystemTime {
    if right.duration_since(left).is_ok() {
        right
    } else {
        left
    }
}
