mod extra;

use super::*;

#[test]
fn accounts_read_only_modes_never_expose_local_names() {
    let mut accounts = accounts::Accounts::new(false);
    accounts.enter();
    assert!(accounts.names().is_empty());
    accounts.confirm();
    assert!(!accounts.confirming());
}
use crate::monitor::snapshot::MonitorSnapshot;
use crate::monitor::{EndpointKind, MonitorHandle, mock_state};
use crate::tui::layout::{ColumnSpec, LayoutTier, MODEL_WIDE_WIDTH, PROVIDER_WIDTH};
use crate::tui::requests::{
    ActiveColumn, EventColumn, RecentColumn, active_columns, event_columns, recent_columns,
    render_active, render_events, render_recent,
};
use crate::tui::sessions::{SessionColumn, render_sessions, session_columns};
use ratatui::Terminal;
use ratatui::layout::Alignment;
use ratatui::{backend::TestBackend, buffer::Buffer};
use std::time::SystemTime;

pub(super) fn draw<R>(
    width: u16,
    height: u16,
    render: impl FnOnce(&mut ratatui::Frame<'_>) -> R,
) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            render(frame);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

pub(super) fn buffer_text(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn placeholder_position(buffer: &Buffer, placeholder: &str) -> Option<(u16, u16)> {
    let symbols = placeholder
        .chars()
        .map(|character| character.to_string())
        .collect::<Vec<_>>();
    (0..buffer.area.height).find_map(|y| {
        (0..buffer.area.width).find_map(|x| {
            symbols
                .iter()
                .enumerate()
                .all(|(offset, symbol)| {
                    x + (offset as u16) < buffer.area.width
                        && buffer[(x + offset as u16, y)].symbol() == symbol
                })
                .then_some((x, y))
        })
    })
}

pub(super) fn assert_centered(buffer: &Buffer, placeholder: &str, expected_y: u16) {
    let (x, y) = placeholder_position(buffer, placeholder).unwrap();
    let left_space = x.saturating_sub(1);
    let right_space = buffer
        .area
        .width
        .saturating_sub(x + placeholder.chars().count() as u16 + 1);
    assert_eq!(y, expected_y);
    assert!(left_space.abs_diff(right_space) <= 1);
}

pub(super) fn headers<K>(columns: &[ColumnSpec<K>]) -> Vec<&'static str> {
    columns.iter().map(|column| column.header).collect()
}

pub(super) fn fixed_budget<K>(columns: &[ColumnSpec<K>]) -> u16 {
    let widths = columns
        .iter()
        .map(|column| match column.width {
            layout::ColumnWidth::Fixed(width) => width,
            layout::ColumnWidth::Flex(_) => 0,
        })
        .sum::<u16>();
    widths.saturating_add(columns.len().saturating_sub(1) as u16)
}

pub(super) fn fixed_width<K: Copy + PartialEq>(columns: &[ColumnSpec<K>], key: K) -> Option<u16> {
    columns
        .iter()
        .find(|column| column.key == key)
        .and_then(|column| match column.width {
            layout::ColumnWidth::Fixed(width) => Some(width),
            layout::ColumnWidth::Flex(_) => None,
        })
}

pub(super) fn flex_count<K>(columns: &[ColumnSpec<K>]) -> usize {
    columns
        .iter()
        .filter(|column| matches!(column.width, layout::ColumnWidth::Flex(_)))
        .count()
}

pub(super) fn alignment<K: Copy + PartialEq>(
    columns: &[ColumnSpec<K>],
    key: K,
) -> Option<Alignment> {
    columns
        .iter()
        .find(|column| column.key == key)
        .map(|column| column.alignment)
}

#[test]
fn request_tables_share_time_status_provider_model_rhythm() {
    assert_eq!(
        headers(&active_columns(LayoutTier::Medium))[..4],
        ["Started", "Phase", "Provider", "Model"]
    );
    assert_eq!(
        headers(&recent_columns(LayoutTier::Medium))[..4],
        ["Finished", "Code", "Provider", "Model"]
    );
    assert_eq!(
        headers(&event_columns(LayoutTier::Medium))[..4],
        ["Time", "Code", "Provider", "Model"]
    );
}

#[test]
fn wide_tables_use_model_caps_and_shared_provider_widths() {
    let sessions = session_columns(LayoutTier::Wide, true);
    let active = active_columns(LayoutTier::Wide);
    let recent = recent_columns(LayoutTier::Wide);
    let events = event_columns(LayoutTier::Wide);

    assert_eq!(
        fixed_width(&sessions, SessionColumn::Model),
        Some(MODEL_WIDE_WIDTH)
    );
    assert_eq!(fixed_width(&active, ActiveColumn::Model), Some(5));
    assert_eq!(fixed_width(&recent, RecentColumn::Model), Some(5));
    assert_eq!(
        fixed_width(&events, EventColumn::Model),
        Some(MODEL_WIDE_WIDTH)
    );
    assert_eq!(
        fixed_width(&active, ActiveColumn::Provider),
        Some(PROVIDER_WIDTH)
    );
    assert_eq!(
        fixed_width(&recent, RecentColumn::Provider),
        Some(PROVIDER_WIDTH)
    );
}

#[test]
fn responsive_schemas_fit_their_minimum_terminal_widths() {
    assert!(fixed_budget(&session_columns(LayoutTier::Emergency, false)) <= 75);
    assert!(fixed_budget(&session_columns(LayoutTier::Narrow, false)) <= 76);
    assert!(fixed_budget(&session_columns(LayoutTier::Medium, false)) <= 88);
    assert!(fixed_budget(&session_columns(LayoutTier::Expanded, false)) <= 118);
    assert!(fixed_budget(&session_columns(LayoutTier::Wide, true)) <= 168);

    assert!(fixed_budget(&active_columns(LayoutTier::Emergency)) <= 75);
    assert!(fixed_budget(&active_columns(LayoutTier::Narrow)) <= 76);
    assert!(fixed_budget(&active_columns(LayoutTier::Medium)) <= 88);
    assert!(fixed_budget(&active_columns(LayoutTier::Expanded)) <= 118);
    assert!(fixed_budget(&active_columns(LayoutTier::Wide)) <= 152);

    assert!(fixed_budget(&recent_columns(LayoutTier::Emergency)) <= 75);
    assert!(fixed_budget(&recent_columns(LayoutTier::Narrow)) <= 76);
    assert!(fixed_budget(&recent_columns(LayoutTier::Medium)) <= 88);
    assert!(fixed_budget(&recent_columns(LayoutTier::Expanded)) <= 118);
    assert!(fixed_budget(&recent_columns(LayoutTier::Wide)) <= 152);

    assert!(fixed_budget(&event_columns(LayoutTier::Emergency)) <= 75);
    assert!(fixed_budget(&event_columns(LayoutTier::Narrow)) <= 76);
    assert!(fixed_budget(&event_columns(LayoutTier::Medium)) <= 88);
    assert!(fixed_budget(&event_columns(LayoutTier::Expanded)) <= 118);
    assert!(fixed_budget(&event_columns(LayoutTier::Wide)) <= 152);
}

#[test]
fn each_schema_has_one_meaningful_flexible_column() {
    for tier in [
        LayoutTier::Emergency,
        LayoutTier::Narrow,
        LayoutTier::Medium,
        LayoutTier::Expanded,
        LayoutTier::Wide,
    ] {
        let sessions = session_columns(tier, tier == LayoutTier::Wide);
        let active = active_columns(tier);
        let recent = recent_columns(tier);
        let events = event_columns(tier);

        assert_eq!(flex_count(&sessions), 1);
        assert_eq!(flex_count(&active), 1);
        assert_eq!(flex_count(&recent), 1);
        assert_eq!(flex_count(&events), 1);
        assert_eq!(
            sessions
                .iter()
                .filter(|column| column.header.is_empty())
                .count(),
            1
        );
        assert_eq!(
            active
                .iter()
                .filter(|column| column.header.is_empty())
                .count(),
            0
        );
        assert_eq!(
            recent
                .iter()
                .filter(|column| column.header.is_empty())
                .count(),
            usize::from(tier == LayoutTier::Expanded)
        );
        assert!(events.iter().all(|column| !column.header.is_empty()));
    }
}

#[test]
fn metric_columns_are_right_aligned() {
    let sessions = session_columns(LayoutTier::Wide, true);
    for key in [
        SessionColumn::Active,
        SessionColumn::Requests,
        SessionColumn::Failures,
        SessionColumn::Input,
        SessionColumn::Output,
        SessionColumn::Rate,
    ] {
        assert_eq!(alignment(&sessions, key), Some(Alignment::Right));
    }

    let active = active_columns(LayoutTier::Wide);
    for key in [
        ActiveColumn::Input,
        ActiveColumn::Output,
        ActiveColumn::Rate,
        ActiveColumn::Elapsed,
    ] {
        assert_eq!(alignment(&active, key), Some(Alignment::Right));
    }

    let recent = recent_columns(LayoutTier::Wide);
    for key in [
        RecentColumn::Code,
        RecentColumn::Latency,
        RecentColumn::Rate,
        RecentColumn::Input,
        RecentColumn::Output,
    ] {
        assert_eq!(alignment(&recent, key), Some(Alignment::Right));
    }
}

#[test]
fn narrow_schemas_use_available_space_for_context() {
    let sessions = session_columns(LayoutTier::Narrow, false);
    assert!(
        sessions
            .iter()
            .any(|column| column.key == SessionColumn::Project)
    );
    assert!(
        sessions
            .iter()
            .any(|column| column.key == SessionColumn::Target)
    );

    let active = active_columns(LayoutTier::Narrow);
    assert!(
        active
            .iter()
            .any(|column| column.key == ActiveColumn::Provider)
    );
    assert!(
        active
            .iter()
            .any(|column| column.key == ActiveColumn::Model)
    );
    assert!(
        active
            .iter()
            .any(|column| column.key == ActiveColumn::Effort)
    );

    let recent = recent_columns(LayoutTier::Narrow);
    assert!(
        recent
            .iter()
            .any(|column| column.key == RecentColumn::Provider)
    );
    assert!(
        recent
            .iter()
            .any(|column| column.key == RecentColumn::Input)
    );
    assert!(
        recent
            .iter()
            .any(|column| column.key == RecentColumn::Output)
    );

    let events = event_columns(LayoutTier::Emergency);
    assert_eq!(headers(&events), ["Time", "Code", "Message"]);
}

#[test]
fn empty_tables_hide_columns_and_center_placeholders() {
    let sessions = draw(40, 9, |frame| {
        render_sessions(frame, frame.area(), &[], 0, true, SystemTime::now())
    });
    let sessions_text = buffer_text(&sessions);
    assert_centered(&sessions, "No sessions", 4);
    assert!(!sessions_text.contains("provider"));
    assert!(sessions_text.contains("No sessions"));

    let active = draw(27, 6, |frame| render_active(frame, frame.area(), &[], 0));
    let active_text = buffer_text(&active);
    assert_centered(&active, "No active requests", 2);
    assert!(!active_text.contains("started"));
    assert!(active_text.contains("No active requests"));

    let recent = draw(40, 9, |frame| {
        render_recent(frame, frame.area(), &[], 0, false)
    });
    let recent_text = buffer_text(&recent);
    assert_centered(&recent, "No recent requests", 4);
    assert!(!recent_text.contains("finished"));
    assert!(recent_text.contains("No recent requests"));

    let events = draw(40, 9, |frame| render_events(frame, frame.area(), &[]));
    let events_text = buffer_text(&events);
    assert_centered(&events, "No events", 4);
    assert!(!events_text.contains("time"));
    assert!(events_text.contains("No events"));
}

#[test]
fn populated_tables_render_rows_without_placeholders() {
    let monitor = MonitorHandle::new(10);
    monitor.request_started(
        "request-1",
        Some("sess-1".to_string()),
        Some(1),
        EndpointKind::Messages,
    );
    monitor.project_resolved("request-1", "example-project");
    monitor.provider_selected(
        "request-1",
        "codex",
        "gpt-5.6-sol",
        Some("high".to_string()),
    );
    let active_state: MonitorSnapshot = monitor.snapshot().into();

    let sessions = draw(170, 8, |frame| {
        render_sessions(
            frame,
            frame.area(),
            &active_state.sessions,
            0,
            true,
            active_state.snapshot_at,
        )
    });
    let sessions_text = buffer_text(&sessions);
    assert!(sessions_text.contains("Provider"));
    assert!(sessions_text.contains("Project"));
    assert!(sessions_text.contains("example-project"));
    assert!(sessions_text.contains("sess-1"));
    assert!(!sessions_text.contains("No sessions"));

    let active = draw(120, 8, |frame| {
        render_active(frame, frame.area(), &active_state.active, 0)
    });
    let active_text = buffer_text(&active);
    assert!(active_text.contains("Started"));
    assert!(active_text.contains("gpt-5.6-sol"));
    assert!(!active_text.contains("No active requests"));

    monitor.request_completed("request-1", 200, Some(100), Some(25));
    let completed_state: MonitorSnapshot = monitor.snapshot().into();
    let recent = draw(140, 8, |frame| {
        render_recent(frame, frame.area(), &completed_state.recent, 0, false)
    });
    let recent_text = buffer_text(&recent);
    assert!(recent_text.contains("Finished"));
    assert!(recent_text.contains("200"));
    assert!(!recent_text.contains("No recent requests"));

    let events = draw(100, 8, |frame| {
        render_events(frame, frame.area(), &completed_state.recent)
    });
    assert!(buffer_text(&events).contains("No events"));
}
