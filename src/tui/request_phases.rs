use super::layout::*;
use super::requests::{ActiveColumn, RecentColumn};
use ratatui::layout::Alignment;

pub(super) fn active_columns(tier: LayoutTier) -> Vec<ColumnSpec<ActiveColumn>> {
    use ActiveColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Phase", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, 5),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, 8),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::flex(C::Timeline, "Timeline", Alignment::Left, 1),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Phase", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, 5),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, 8),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::flex(C::Timeline, "Timeline", Alignment::Left, 1),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Phase", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Phase", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Phase", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
    }
}

pub(super) fn recent_columns(tier: LayoutTier) -> Vec<ColumnSpec<RecentColumn>> {
    use RecentColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, 5),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Wait, "Wait", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::First, "First", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Latency, "Total", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Timeline, "Timeline", Alignment::Left, 8),
            ColumnSpec::flex(C::Details, "Details", Alignment::Left, 1),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, 5),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Wait, "Wait", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::First, "First", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Latency, "Total", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
            ColumnSpec::flex(C::Spacer, "", Alignment::Left, 1),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Total", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Latency, "Total", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Latency, "Total", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
    }
}

use super::cells::{format_duration, number_cell};
use super::{BLUE, GREEN, YELLOW};
use crate::monitor::{
    RequestStatus,
    snapshot::{ActiveSnapshot, CompletedSnapshot},
};
use ratatui::{
    style::Style,
    text::{Line, Span},
    widgets::Cell,
};
use std::time::{Duration, SystemTime};

pub(super) fn active_phase(request: &ActiveSnapshot) -> Option<String> {
    if let Some(place) = request.queue_place {
        return Some(format!("queued #{place}"));
    }
    if !matches!(
        request.status,
        RequestStatus::Upstream | RequestStatus::Streaming
    ) {
        return None;
    }
    Some(
        if request.first_output_at.is_some() {
            "streaming"
        } else if request.first_upstream_event_at.is_some() {
            "thinking"
        } else {
            "connecting"
        }
        .to_owned(),
    )
}

fn elapsed(start: SystemTime, end: SystemTime) -> Duration {
    end.duration_since(start).unwrap_or_default()
}

pub(super) fn recent_time(start: SystemTime, end: Option<SystemTime>) -> Cell<'static> {
    number_cell(
        end.map(|end| format_duration(elapsed(start, end)))
            .unwrap_or_else(|| "-".into()),
    )
}

fn timeline(
    total: Duration,
    first: Duration,
    second: Duration,
    width: usize,
    recent: bool,
) -> Cell<'static> {
    let total = total.as_secs_f64();
    let count = (total.ceil() as usize).min(width);
    let mut spans = Vec::with_capacity(count);
    for i in 0..count {
        let point = (i as f64 + 0.5) * total / count as f64;
        let (symbol, color) = if point < first.as_secs_f64() {
            ("░", YELLOW)
        } else if !recent && point < second.as_secs_f64() {
            ("▒", BLUE)
        } else {
            ("█", GREEN)
        };
        spans.push(Span::styled(symbol, Style::default().fg(color)));
    }
    Cell::from(Line::from(spans))
}

pub(super) fn active_timeline(
    request: &ActiveSnapshot,
    now: SystemTime,
    width: usize,
) -> Cell<'static> {
    if active_phase(request).is_none() {
        return Cell::from("");
    }
    let total = elapsed(request.started_at, now);
    let first = request
        .first_upstream_event_at
        .map(|time| elapsed(request.started_at, time))
        .unwrap_or(total)
        .min(total);
    let second = request
        .first_output_at
        .map(|time| elapsed(request.started_at, time))
        .unwrap_or(total)
        .min(total)
        .max(first);
    timeline(total, first, second, width, false)
}

pub(super) fn recent_timeline(request: &CompletedSnapshot, width: usize) -> Cell<'static> {
    let first = request
        .first_upstream_event_at
        .map(|time| elapsed(request.started_at, time))
        .unwrap_or_default()
        .min(request.latency);
    timeline(request.latency, first, request.latency, width, true)
}
