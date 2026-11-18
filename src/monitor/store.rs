use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Instant, SystemTime},
};

use super::{
    ActiveRequest, CompletedRequest, EndpointKind, MAX_SESSION_USAGE_ENTRIES, MonitorEvent,
    MonitorState, RequestStatus, SESSION_PRUNE_INTERVAL, SESSION_USAGE_RETENTION, bounded_text,
    sessions::{first_session_output_bucket, session_summaries, session_token_bucket},
};

#[derive(Debug)]
pub(super) struct MonitorStore {
    pub(super) started_at: SystemTime,
    pub(super) started_instant: Instant,
    pub(super) last_session_prune_instant: Instant,
    pub(super) active: HashMap<String, ActiveRequest>,
    pub(super) recent: VecDeque<CompletedRequest>,
    pub(super) session_usage: HashMap<Option<String>, SessionUsage>,
    pub(super) session_output_buckets: HashMap<Option<String>, Vec<(u64, u64)>>,
    pub(super) recent_limit: usize,
}

#[derive(Debug)]
pub(super) struct SessionUsage {
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) last_seen: SystemTime,
}

impl Default for SessionUsage {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            last_seen: SystemTime::now(),
        }
    }
}

impl MonitorStore {
    pub(super) fn apply(&mut self, event: MonitorEvent) {
        match event {
            MonitorEvent::RequestStarted {
                request_id,
                session_id,
                session_seq,
                endpoint,
            } => {
                self.active.insert(
                    request_id.clone(),
                    ActiveRequest {
                        request_id,
                        session_id,
                        session_seq,
                        project: None,
                        provider: None,
                        model: None,
                        effort: None,
                        endpoint,
                        started_at: SystemTime::now(),
                        started_instant: Instant::now(),
                        generation_started_at: None,
                        generation_started_instant: None,
                        generation_initial_output_tokens: 0,
                        generation_finished_at: None,
                        generation_duration: None,
                        status: RequestStatus::Started,
                        streamed_bytes: 0,
                        stream_chunks: 0,
                        input_tokens: None,
                        output_tokens: None,
                        error: None,
                        traffic_capture_path: None,
                    },
                );
            }
            MonitorEvent::ProjectResolved {
                request_id,
                project,
            } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.project = Some(project);
                }
            }
            MonitorEvent::SessionSequenceResolved {
                request_id,
                session_seq,
            } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.session_seq = Some(session_seq);
                }
            }
            MonitorEvent::ProviderSelected {
                request_id,
                provider,
                model,
                effort,
            } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.provider = Some(provider);
                    active.model = Some(model);
                    active.effort = effort;
                    active.status = RequestStatus::ProviderSelected;
                }
            }
            MonitorEvent::ModelResolved { request_id, model } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.model = Some(bounded_text(match active.model.take() {
                        Some(incoming) if incoming != model => format!("{incoming} → {model}"),
                        Some(incoming) => incoming,
                        None => model,
                    }));
                }
            }
            MonitorEvent::CompactionStarted { request_id } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.status = RequestStatus::Compacting;
                }
            }
            MonitorEvent::UpstreamStarted { request_id } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.status = RequestStatus::Upstream;
                }
            }
            MonitorEvent::GenerationStarted { request_id } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.generation_started_at = Some(SystemTime::now());
                    active.generation_started_instant = Some(Instant::now());
                    active.generation_initial_output_tokens = active.output_tokens.unwrap_or(0);
                    active.generation_finished_at = None;
                    active.generation_duration = None;
                }
            }
            MonitorEvent::TrafficCapturePath { request_id, path } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.traffic_capture_path = Some(path);
                }
            }
            MonitorEvent::StreamProgress {
                request_id,
                bytes,
                chunks,
                input_tokens,
                output_tokens,
            } => {
                let mut usage_update = None;
                let mut history_update = None;
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.status = RequestStatus::Streaming;
                    if active.generation_started_instant.is_none() {
                        active.generation_started_at = Some(SystemTime::now());
                        active.generation_started_instant = Some(Instant::now());
                        active.generation_initial_output_tokens =
                            output_tokens.or(active.output_tokens).unwrap_or(0);
                    } else {
                        active.generation_finished_at = Some(SystemTime::now());
                        active.generation_duration = active
                            .generation_started_instant
                            .map(|started| started.elapsed());
                    }
                    active.streamed_bytes = active.streamed_bytes.saturating_add(bytes);
                    active.stream_chunks = active.stream_chunks.saturating_add(chunks);
                    let input_delta = update_token_count(&mut active.input_tokens, input_tokens);
                    let output_delta = update_token_count(&mut active.output_tokens, output_tokens);
                    usage_update = Some((active.session_id.clone(), input_delta, output_delta));
                } else if let Some(completed) = self
                    .recent
                    .iter_mut()
                    .find(|request| request.request_id == request_id)
                {
                    if let Some(started) = completed.generation_started_instant {
                        completed.generation_finished_at = Some(SystemTime::now());
                        completed.generation_duration = Some(started.elapsed());
                    }
                    completed.streamed_bytes = completed.streamed_bytes.saturating_add(bytes);
                    completed.stream_chunks = completed.stream_chunks.saturating_add(chunks);
                    let input_delta = update_token_count(&mut completed.input_tokens, input_tokens);
                    let output_delta =
                        update_token_count(&mut completed.output_tokens, output_tokens);
                    usage_update = Some((completed.session_id.clone(), input_delta, output_delta));
                    if output_delta > 0 {
                        history_update = Some((
                            completed.session_id.clone(),
                            completed
                                .generation_finished_at
                                .unwrap_or(completed.finished_at),
                            output_delta,
                        ));
                    }
                }
                if let Some((session_id, input_delta, output_delta)) = usage_update {
                    self.record_session_usage(session_id, input_delta, output_delta);
                }
                if let Some((session_id, timestamp, tokens)) = history_update {
                    self.record_session_output(session_id, timestamp, tokens);
                }
            }
            MonitorEvent::UsageUpdated {
                request_id,
                input_tokens,
                output_tokens,
            } => {
                let mut usage_update = None;
                let mut history_update = None;
                if let Some(active) = self.active.get_mut(&request_id) {
                    if output_tokens.is_some()
                        && let Some(started) = active.generation_started_instant
                    {
                        active.generation_finished_at = Some(SystemTime::now());
                        active.generation_duration = Some(started.elapsed());
                    }
                    let input_delta = update_token_count(&mut active.input_tokens, input_tokens);
                    let output_delta = update_token_count(&mut active.output_tokens, output_tokens);
                    usage_update = Some((active.session_id.clone(), input_delta, output_delta));
                } else if let Some(completed) = self
                    .recent
                    .iter_mut()
                    .find(|request| request.request_id == request_id)
                {
                    if output_tokens.is_some()
                        && let Some(started) = completed.generation_started_instant
                    {
                        completed.generation_finished_at = Some(SystemTime::now());
                        completed.generation_duration = Some(started.elapsed());
                    }
                    let input_delta = update_token_count(&mut completed.input_tokens, input_tokens);
                    let output_delta =
                        update_token_count(&mut completed.output_tokens, output_tokens);
                    usage_update = Some((completed.session_id.clone(), input_delta, output_delta));
                    if output_delta > 0 {
                        history_update = Some((
                            completed.session_id.clone(),
                            completed
                                .generation_finished_at
                                .unwrap_or(completed.finished_at),
                            output_delta,
                        ));
                    }
                }
                if let Some((session_id, input_delta, output_delta)) = usage_update {
                    self.record_session_usage(session_id, input_delta, output_delta);
                }
                if let Some((session_id, timestamp, tokens)) = history_update {
                    self.record_session_output(session_id, timestamp, tokens);
                }
            }
            MonitorEvent::RequestCompleted {
                request_id,
                http_status,
                input_tokens,
                output_tokens,
            } => {
                self.finish(
                    &request_id,
                    RequestStatus::Completed,
                    Some(http_status),
                    input_tokens,
                    output_tokens,
                    None,
                );
            }
            MonitorEvent::RequestFailed {
                request_id,
                http_status,
                error,
            } => {
                self.finish(
                    &request_id,
                    RequestStatus::Failed,
                    http_status,
                    None,
                    None,
                    Some(error),
                );
            }
            MonitorEvent::RequestAbandoned { request_id, error } => {
                self.finish_active(
                    &request_id,
                    RequestStatus::Failed,
                    None,
                    None,
                    None,
                    Some(error),
                );
            }
        }
        self.prune_session_history_if_due(SystemTime::now());
    }

    fn finish_active(
        &mut self,
        request_id: &str,
        status: RequestStatus,
        http_status: Option<u16>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        error: Option<String>,
    ) {
        if self.active.contains_key(request_id) {
            self.finish(
                request_id,
                status,
                http_status,
                input_tokens,
                output_tokens,
                error,
            );
        }
    }

    fn finish(
        &mut self,
        request_id: &str,
        status: RequestStatus,
        http_status: Option<u16>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        error: Option<String>,
    ) {
        let mut active = self
            .active
            .remove(request_id)
            .unwrap_or_else(|| ActiveRequest {
                request_id: request_id.to_string(),
                session_id: None,
                session_seq: None,
                project: None,
                provider: None,
                model: None,
                effort: None,
                endpoint: EndpointKind::Messages,
                started_at: SystemTime::now(),
                started_instant: Instant::now(),
                generation_started_at: None,
                generation_started_instant: None,
                generation_initial_output_tokens: 0,
                generation_finished_at: None,
                generation_duration: None,
                status: RequestStatus::Started,
                streamed_bytes: 0,
                stream_chunks: 0,
                input_tokens: None,
                output_tokens: None,
                error: None,
                traffic_capture_path: None,
            });
        if output_tokens.is_some()
            && let Some(started) = active.generation_started_instant
        {
            active.generation_finished_at = Some(SystemTime::now());
            active.generation_duration = Some(started.elapsed());
        }
        let input_delta = update_token_count(&mut active.input_tokens, input_tokens);
        let output_delta = update_token_count(&mut active.output_tokens, output_tokens);
        self.record_session_usage(active.session_id.clone(), input_delta, output_delta);
        let completed = CompletedRequest {
            request_id: active.request_id,
            session_id: active.session_id,
            session_seq: active.session_seq,
            project: active.project,
            provider: active.provider,
            model: active.model,
            effort: active.effort,
            endpoint: active.endpoint,
            started_at: active.started_at,
            finished_at: SystemTime::now(),
            generation_started_at: active.generation_started_at,
            generation_started_instant: active.generation_started_instant,
            generation_initial_output_tokens: active.generation_initial_output_tokens,
            generation_finished_at: active.generation_finished_at,
            generation_duration: active.generation_duration,
            status,
            http_status,
            latency: active.started_instant.elapsed(),
            streamed_bytes: active.streamed_bytes,
            stream_chunks: active.stream_chunks,
            input_tokens: active.input_tokens,
            output_tokens: active.output_tokens,
            error: error.or(active.error),
            traffic_capture_path: active.traffic_capture_path,
        };
        if let Some(tokens) = completed.output_tokens.filter(|tokens| *tokens > 0) {
            self.record_session_output(
                completed.session_id.clone(),
                completed
                    .generation_finished_at
                    .unwrap_or(completed.finished_at),
                tokens,
            );
        }
        self.recent.push_front(completed);
        while self.recent.len() > self.recent_limit {
            self.recent.pop_back();
        }
    }

    fn record_session_usage(
        &mut self,
        session_id: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
    ) {
        self.make_room_for_session(&session_id);
        let usage = self.session_usage.entry(session_id).or_default();
        usage.input_tokens = usage.input_tokens.saturating_add(input_tokens);
        usage.output_tokens = usage.output_tokens.saturating_add(output_tokens);
        usage.last_seen = SystemTime::now();
    }

    fn make_room_for_session(&mut self, session_id: &Option<String>) {
        if self.session_usage.contains_key(session_id)
            || self.session_usage.len() < MAX_SESSION_USAGE_ENTRIES
        {
            return;
        }
        if let Some(oldest) = self
            .session_usage
            .iter()
            .min_by_key(|(_, usage)| usage.last_seen)
            .map(|(id, _)| id.clone())
        {
            self.session_usage.remove(&oldest);
            self.session_output_buckets.remove(&oldest);
        }
    }

    fn record_session_output(
        &mut self,
        session_id: Option<String>,
        timestamp: SystemTime,
        tokens: u64,
    ) {
        let bucket = session_token_bucket(timestamp);
        let buckets = self.session_output_buckets.entry(session_id).or_default();
        match buckets.binary_search_by_key(&bucket, |(bucket, _)| *bucket) {
            Ok(index) => buckets[index].1 = buckets[index].1.saturating_add(tokens),
            Err(index) => buckets.insert(index, (bucket, tokens)),
        }
    }

    fn prune_session_history_if_due(&mut self, now: SystemTime) {
        if self.last_session_prune_instant.elapsed() >= SESSION_PRUNE_INTERVAL {
            self.prune_session_history(now);
        }
    }

    pub(super) fn prune_session_history(&mut self, now: SystemTime) {
        let retained_sessions: HashSet<_> = self
            .active
            .values()
            .map(|request| request.session_id.clone())
            .chain(self.recent.iter().map(|request| request.session_id.clone()))
            .collect();
        self.session_usage.retain(|session_id, usage| {
            let retained_by_age = match now.duration_since(usage.last_seen) {
                Ok(age) => age < SESSION_USAGE_RETENTION,
                Err(_) => true,
            };
            retained_sessions.contains(session_id) || retained_by_age
        });
        let first_bucket = first_session_output_bucket(now);
        for buckets in self.session_output_buckets.values_mut() {
            buckets.retain(|(bucket, _)| *bucket >= first_bucket);
        }
        self.session_output_buckets
            .retain(|_, buckets| !buckets.is_empty());
        self.last_session_prune_instant = Instant::now();
    }

    pub(super) fn snapshot(&mut self) -> MonitorState {
        let now = SystemTime::now();
        self.prune_session_history_if_due(now);
        let mut active: Vec<_> = self.active.values().cloned().collect();
        active.sort_by_key(|request| request.started_at);
        let sessions = session_summaries(
            &active,
            &self.recent,
            &self.session_usage,
            &self.session_output_buckets,
            now,
        );
        MonitorState {
            started_at: self.started_at,
            uptime: self.started_instant.elapsed(),
            sessions,
            active,
            recent: self.recent.iter().cloned().collect(),
        }
    }
}

pub(super) fn update_token_count(current: &mut Option<u64>, incoming: Option<u64>) -> u64 {
    let Some(incoming) = incoming else {
        return 0;
    };
    let previous = current.unwrap_or(0);
    if incoming > previous || current.is_none() {
        *current = Some(incoming);
    }
    incoming.saturating_sub(previous)
}
