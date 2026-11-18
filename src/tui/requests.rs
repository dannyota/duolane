use super::cells::{
    column_constraints, column_header, detail_cell, display_session_id, ellipsize, error_indicator,
    format_duration, format_system_time, http_code_cell, model_cell, muted_cell, number_cell,
    panel, provider_cell, rate_cell, render_empty_table_state, spinner, status_style,
    table_column_width, target_cell, text_cell, token_value,
};
use super::layout::{
    CODE_WIDTH, ColumnSpec, DURATION_WIDTH, EFFORT_WIDTH, ENDPOINT_WIDTH, ERROR_WIDTH, ID_WIDTH,
    LayoutTier, MODEL_MEDIUM_WIDTH, MODEL_NARROW_WIDTH, MODEL_WIDE_WIDTH, PROJECT_MEDIUM_WIDTH,
    PROJECT_WIDE_WIDTH, PROVIDER_WIDTH, RATE_WIDTH, STATUS_WIDTH, TIME_WIDTH, TOKEN_WIDTH,
};
use super::{PANEL_BG, SELECTED_BG};
use crate::monitor::snapshot::{ActiveSnapshot, CompletedSnapshot};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Cell, Row, Table, TableState};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ActiveColumn {
    Started,
    Status,
    Project,
    Session,
    Provider,
    Model,
    Target,
    Effort,
    Endpoint,
    Input,
    Output,
    Rate,
    Elapsed,
}

pub(super) fn active_columns(tier: LayoutTier) -> Vec<ColumnSpec<ActiveColumn>> {
    use ActiveColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::flex(C::Endpoint, "Endpoint", Alignment::Left, 1),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
    }
}

pub(super) fn render_active(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    active: &[ActiveSnapshot],
    tick: usize,
) {
    if active.is_empty() {
        render_empty_table_state(frame, area, "Active requests", false, "No active requests");
        return;
    }

    let columns = active_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = active.iter().map(|request| {
        let status = if request.status == crate::monitor::RequestStatus::Compacting {
            format!("{}{}", spinner(tick), request.status.label())
        } else {
            format!("{} {}", spinner(tick), request.status.label())
        };
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    ActiveColumn::Started => muted_cell(format_system_time(request.started_at)),
                    ActiveColumn::Status => Cell::from(Span::styled(
                        status.clone(),
                        status_style(request.status.label()),
                    )),
                    ActiveColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    ActiveColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    ActiveColumn::Provider => provider_cell(request.provider.as_deref()),
                    ActiveColumn::Model => model_cell(request.model.as_deref(), width),
                    ActiveColumn::Target => {
                        target_cell(request.provider.as_deref(), request.model.as_deref(), width)
                    }
                    ActiveColumn::Effort => text_cell(request.effort.as_deref().unwrap_or("-")),
                    ActiveColumn::Endpoint => muted_cell(request.endpoint.label()),
                    ActiveColumn::Input => number_cell(token_value(request.input_tokens)),
                    ActiveColumn::Output => number_cell(token_value(request.output_tokens)),
                    ActiveColumn::Rate => rate_cell(request.rate().label()),
                    ActiveColumn::Elapsed => number_cell(format_duration(request.elapsed())),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(Style::default().bg(PANEL_BG))
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Active requests", false));
    frame.render_widget(table, area);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RecentColumn {
    Finished,
    Code,
    Project,
    Session,
    Provider,
    Model,
    Target,
    Effort,
    Endpoint,
    Latency,
    Rate,
    Input,
    Output,
    Details,
    Error,
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
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::flex(C::Details, "Details", Alignment::Left, 1),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
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
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
    }
}

pub(super) fn render_recent(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    recent: &[CompletedSnapshot],
    selected: usize,
    focused: bool,
) {
    if recent.is_empty() {
        render_empty_table_state(
            frame,
            area,
            "Recent requests",
            focused,
            "No recent requests",
        );
        return;
    }

    let columns = recent_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = recent.iter().enumerate().map(|(index, request)| {
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    RecentColumn::Finished => muted_cell(format_system_time(request.finished_at)),
                    RecentColumn::Code => http_code_cell(request.http_status),
                    RecentColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    RecentColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    RecentColumn::Provider => provider_cell(request.provider.as_deref()),
                    RecentColumn::Model => model_cell(request.model.as_deref(), width),
                    RecentColumn::Target => {
                        target_cell(request.provider.as_deref(), request.model.as_deref(), width)
                    }
                    RecentColumn::Effort => text_cell(request.effort.as_deref().unwrap_or("-")),
                    RecentColumn::Endpoint => muted_cell(request.endpoint.label()),
                    RecentColumn::Latency => number_cell(format_duration(request.latency)),
                    RecentColumn::Rate => rate_cell(request.rate().label()),
                    RecentColumn::Input => number_cell(token_value(request.input_tokens)),
                    RecentColumn::Output => number_cell(token_value(request.output_tokens)),
                    RecentColumn::Details => detail_cell(request.error.as_deref().unwrap_or("")),
                    RecentColumn::Error => detail_cell(error_indicator(request)),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(if focused && index == selected {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default().bg(PANEL_BG)
        })
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Recent requests", focused));
    let mut table_state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, area, &mut table_state);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EventColumn {
    Time,
    Code,
    Project,
    Session,
    Provider,
    Model,
    Message,
}

pub(super) fn event_columns(tier: LayoutTier) -> Vec<ColumnSpec<EventColumn>> {
    use EventColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_MEDIUM_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_MEDIUM_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_NARROW_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
    }
}

pub(super) fn render_events(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    recent: &[CompletedSnapshot],
) {
    let events = recent
        .iter()
        .filter(|request| {
            request.status == crate::monitor::RequestStatus::Failed
                || request.http_status.is_some_and(|status| status >= 400)
                || request.error.is_some()
        })
        .take(12)
        .collect::<Vec<_>>();
    if events.is_empty() {
        render_empty_table_state(frame, area, "Events", false, "No events");
        return;
    }

    let columns = event_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = events.iter().map(|request| {
        let message = request
            .error
            .as_deref()
            .filter(|error| !error.is_empty())
            .unwrap_or("-");
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    EventColumn::Time => muted_cell(format_system_time(request.finished_at)),
                    EventColumn::Code => http_code_cell(request.http_status),
                    EventColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    EventColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    EventColumn::Provider => provider_cell(request.provider.as_deref()),
                    EventColumn::Model => model_cell(request.model.as_deref(), width),
                    EventColumn::Message => detail_cell(message),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(Style::default().bg(PANEL_BG))
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Events", false));
    frame.render_widget(table, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::snapshot::MonitorSnapshot;
    use crate::monitor::{EndpointKind, MonitorHandle, mock_state};
    use crate::tui::PURPLE;
    use crate::tui::cells::status_color;
    use crate::tui::tests::{buffer_text, draw};

    #[test]
    fn active_table_renders_expected_headers_at_tier_boundaries() {
        let state: MonitorSnapshot = mock_state().into();
        let render_at = |width| {
            let buffer = draw(width, 8, |frame| {
                render_active(frame, frame.area(), &state.active, 0)
            });
            buffer_text(&buffer)
        };

        let emergency = render_at(77);
        assert!(emergency.contains("Started"), "{emergency}");
        assert!(emergency.contains("Target"), "{emergency}");
        assert!(!emergency.contains("Rate"), "{emergency}");

        let narrow = render_at(78);
        assert!(narrow.contains("Provider"), "{narrow}");
        assert!(narrow.contains("Model"), "{narrow}");
        assert!(narrow.contains("Effort"), "{narrow}");
        assert!(!narrow.contains("Project"), "{narrow}");

        let medium = render_at(90);
        assert!(medium.contains("Provider"), "{medium}");
        assert!(medium.contains("Model"), "{medium}");
        assert!(medium.contains("Endpoint"), "{medium}");
        assert!(!medium.contains("Project"), "{medium}");

        let expanded = render_at(120);
        assert!(expanded.contains("Project"), "{expanded}");
        assert!(expanded.contains("Session"), "{expanded}");
        assert!(expanded.contains("Endpoint"), "{expanded}");
        assert!(!expanded.contains("In"), "{expanded}");

        let wide = render_at(154);
        assert!(wide.contains("Project"), "{wide}");
        assert!(wide.contains("Session"), "{wide}");
        assert!(wide.contains("In"), "{wide}");
        assert!(wide.contains("Out"), "{wide}");
    }

    #[test]
    fn active_status_keeps_full_label_at_narrow_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.upstream_started("request-1");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let active = draw(88, 6, |frame| {
            render_active(frame, frame.area(), &state.active, 0)
        });

        let active_text = buffer_text(&active);
        assert!(active_text.contains("⠋ upstream"), "{active_text}");
    }

    #[test]
    fn active_compacting_status_is_distinct_at_narrow_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.compaction_started("request-1");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let active = draw(88, 6, |frame| {
            render_active(frame, frame.area(), &state.active, 0)
        });

        let active_text = buffer_text(&active);
        assert!(active_text.contains("⠋compacting"), "{active_text}");
        assert_eq!(status_color("compacting"), PURPLE);
    }

    #[test]
    fn recent_table_uses_error_indicator_at_medium_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-1", "codex", "gpt-5.6-sol", None);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let recent = draw(110, 8, |frame| {
            render_recent(frame, frame.area(), &state.recent, 0, true)
        });
        let recent_text = buffer_text(&recent);

        assert!(recent_text.contains("!"), "{recent_text}");
        assert!(!recent_text.contains("Details"), "{recent_text}");
        assert!(
            !recent_text.contains("upstream unavailable"),
            "{recent_text}"
        );
    }

    #[test]
    fn recent_table_keeps_detail_text_at_wide_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-1", "codex", "gpt-5.6-sol", None);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let recent = draw(180, 8, |frame| {
            render_recent(frame, frame.area(), &state.recent, 0, false)
        });
        let recent_text = buffer_text(&recent);

        assert!(recent_text.contains("Details"), "{recent_text}");
        assert!(
            recent_text.contains("upstream unavailable"),
            "{recent_text}"
        );
    }

    #[test]
    fn events_render_matching_request_rows_without_a_placeholder() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let events = draw(100, 8, |frame| {
            render_events(frame, frame.area(), &state.recent)
        });
        let events_text = buffer_text(&events);
        assert!(events_text.contains("Time"));
        assert!(events_text.contains("502"));
        assert!(events_text.contains("upstream unavailable"));
        assert!(!events_text.contains("No events"));
    }
}
