use super::cells::{
    column_constraints, column_header, compact_tokens, display_session_id, ellipsize, model_cell,
    number_cell, panel, provider_cell, rate_cell, render_empty_table_state, status_cell,
    table_column_width, target_cell, text_cell, token_sparkline_line,
};
use super::layout::{
    COUNT_WIDTH, ColumnSpec, EFFORT_WIDTH, ID_WIDTH, LayoutTier, MODEL_NARROW_WIDTH,
    MODEL_WIDE_WIDTH, PROJECT_MEDIUM_WIDTH, PROJECT_WIDE_WIDTH, PROVIDER_WIDTH, RATE_WIDTH,
    STATUS_WIDTH, TOKEN_WIDTH,
};
use super::{PANEL_BG, SELECTED_BG, SESSION_SPARKLINE_MIN_WIDTH, TEAL};
use crate::monitor::snapshot::SessionSnapshot;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Cell, Row, Table, TableState};
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionColumn {
    Marker,
    Id,
    Project,
    Active,
    Requests,
    Failures,
    Counts,
    Provider,
    Model,
    Target,
    Effort,
    Input,
    Output,
    Rate,
    Activity,
    Status,
}

pub(super) fn session_columns(
    tier: LayoutTier,
    show_full_sparkline: bool,
) -> Vec<ColumnSpec<SessionColumn>> {
    use SessionColumn as C;
    match (tier, show_full_sparkline) {
        (LayoutTier::Wide, true) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Active, "A", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Requests, "R", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Failures, "F", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::flex(C::Activity, "Tokens/10s · 4k", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Expanded | LayoutTier::Wide, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_NARROW_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::flex(C::Activity, "Tokens/10s", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Medium, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Activity, "Tok/10s", Alignment::Left, 8),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Narrow, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, 10),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Activity, "Trend", Alignment::Left, 6),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Emergency, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
    }
}

pub(super) fn render_sessions(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    sessions: &[SessionSnapshot],
    selected: usize,
    focused: bool,
    snapshot_at: SystemTime,
) {
    if sessions.is_empty() {
        render_empty_table_state(frame, area, "Sessions", focused, "No sessions");
        return;
    }

    let tier = LayoutTier::for_outer_width(area.width);
    let show_full_sparkline = tier == LayoutTier::Wide && area.width >= SESSION_SPARKLINE_MIN_WIDTH;
    let columns = session_columns(tier, show_full_sparkline);
    let widths = column_constraints(&columns);
    let rows = sessions.iter().enumerate().map(|(index, session)| {
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    SessionColumn::Marker => {
                        let marker = if focused && index == selected {
                            ">"
                        } else {
                            " "
                        };
                        Cell::from(Span::styled(marker, Style::default().fg(TEAL)))
                    }
                    SessionColumn::Id => {
                        text_cell(display_session_id(session.session_id.as_deref()))
                    }
                    SessionColumn::Project => {
                        text_cell(ellipsize(session.project.as_deref().unwrap_or("-"), width))
                    }
                    SessionColumn::Active => number_cell(session.active_count.to_string()),
                    SessionColumn::Requests => number_cell(session.request_count.to_string()),
                    SessionColumn::Failures => number_cell(session.failure_count.to_string()),
                    SessionColumn::Counts => number_cell(format!(
                        "{}/{}/{}",
                        session.active_count, session.request_count, session.failure_count
                    )),
                    SessionColumn::Provider => provider_cell(session.provider.as_deref()),
                    SessionColumn::Model => model_cell(session.model.as_deref(), width),
                    SessionColumn::Target => {
                        target_cell(session.provider.as_deref(), session.model.as_deref(), width)
                    }
                    SessionColumn::Effort => text_cell(session.effort.as_deref().unwrap_or("-")),
                    SessionColumn::Input => number_cell(compact_tokens(session.input_tokens)),
                    SessionColumn::Output => number_cell(compact_tokens(session.output_tokens)),
                    SessionColumn::Rate => rate_cell(session.rate().label()),
                    SessionColumn::Activity => Cell::from(token_sparkline_line(
                        &session.output_token_samples,
                        width,
                        snapshot_at,
                    )),
                    SessionColumn::Status => status_cell(&session.last_status),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(if index == selected {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default().bg(PANEL_BG)
        })
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Sessions", focused));
    let mut table_state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, area, &mut table_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::snapshot::MonitorSnapshot;
    use crate::monitor::{EndpointKind, MonitorHandle};
    use crate::tui::cells::token_sparkline;
    use crate::tui::tests::{buffer_text, draw};
    use std::time::{Duration, SystemTime};

    #[test]
    fn activity_graph_ignores_positive_viewer_clock_skew() {
        let snapshot_at = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let viewer_now = snapshot_at + Duration::from_secs(3_600);
        let samples = [(snapshot_at, 4_000)];
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("session-1".into()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.request_completed("request-1", 200, Some(10), Some(4_000));
        let mut snapshot: MonitorSnapshot = monitor.snapshot().into();
        snapshot.snapshot_at = snapshot_at;
        snapshot.sessions[0].output_token_samples = samples.into();

        assert_eq!(token_sparkline(&samples, 4, viewer_now), "    ");
        let rendered = draw(90, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &snapshot.sessions,
                0,
                true,
                snapshot.snapshot_at,
            )
        });
        assert!(buffer_text(&rendered).contains('█'));
    }

    #[test]
    fn activity_graph_ignores_negative_viewer_clock_skew() {
        let snapshot_at = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let viewer_now = snapshot_at - Duration::from_secs(3_600);
        let samples = [(snapshot_at, 4_000)];
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("session-1".into()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.request_completed("request-1", 200, Some(10), Some(4_000));
        let mut snapshot: MonitorSnapshot = monitor.snapshot().into();
        snapshot.snapshot_at = snapshot_at;
        snapshot.sessions[0].output_token_samples = samples.into();

        assert_eq!(token_sparkline(&samples, 4, viewer_now), "    ");
        let rendered = draw(90, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &snapshot.sessions,
                0,
                true,
                snapshot.snapshot_at,
            )
        });
        assert!(buffer_text(&rendered).contains('█'));
    }

    #[test]
    fn session_sparkline_appears_at_medium_width_and_expands() {
        let monitor = MonitorHandle::new(10);
        for (index, tokens) in [1_000, 2_000, 4_000].into_iter().enumerate() {
            let request_id = format!("request-{index}");
            monitor.request_started(
                &request_id,
                Some("sess-1".to_string()),
                Some(index as u64 + 1),
                EndpointKind::Messages,
            );
            monitor.provider_selected(&request_id, "codex", "gpt-5.6-sol", None);
            monitor.request_completed(&request_id, 200, Some(100), Some(tokens));
        }
        let state: MonitorSnapshot = monitor.snapshot().into();
        let render_at = |width| {
            let buffer = draw(width, 8, |frame| {
                render_sessions(
                    frame,
                    frame.area(),
                    &state.sessions,
                    0,
                    true,
                    state.snapshot_at,
                )
            });
            buffer_text(&buffer)
        };
        let spark_chars = |text: &str| {
            text.chars()
                .filter(|ch| matches!(ch, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'))
                .count()
        };

        let emergency = render_at(77);
        assert!(!emergency.contains("Trend"), "{emergency}");
        assert_eq!(spark_chars(&emergency), 0, "{emergency}");

        let narrow = render_at(78);
        assert!(narrow.contains("Trend"), "{narrow}");
        assert!(spark_chars(&narrow) > 0, "{narrow}");

        let medium = render_at(90);
        assert!(medium.contains("Tok/10s"), "{medium}");
        assert!(spark_chars(&medium) > 0, "{medium}");

        let expanded = render_at(120);
        assert!(expanded.contains("Tokens/10s"), "{expanded}");
        assert!(spark_chars(&expanded) > 0, "{expanded}");

        let wide = render_at(SESSION_SPARKLINE_MIN_WIDTH);
        assert!(wide.contains("Tokens/10s · 4k"), "{wide}");
        assert!(spark_chars(&wide) > 0, "{wide}");
    }
}
