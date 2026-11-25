use super::mouse_tests::app;
use super::tests::{buffer_text, draw, placeholder_position};
use super::*;
use crate::monitor::mock_state;

#[test]
fn overview_shows_lane_and_connection_summaries() {
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    let text = buffer_text(&draw(160, 50, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    }));
    assert!(text.contains("Connections"));
    assert!(text.contains("HTTP fallbacks"));
    assert!(text.contains("origin 403"));
}

#[test]
fn overview_connections_show_values_and_yellow_queue_border() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.codex_connections = Some(crate::monitor::snapshot::CodexConnections {
        queued_attempts: 2,
        handshakes_in_flight: 3,
        handshake_spacing_ms: 250,
        websocket_reused: 3,
        websocket_new: 1,
        http_fallbacks: 7,
        origin_403: 8,
    });
    let mut app = app();
    let screen = draw(160, 50, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    });
    let text = buffer_text(&screen);
    assert!(text.contains("queue 2  handshakes 3"));
    assert!(text.contains("starts 4.0/s  reused 75%"));
    assert!(text.contains("HTTP fallbacks 7  origin 403 8"));
    let (x, y) = placeholder_position(&screen, "Connections").unwrap();
    assert_eq!(screen[(x - 1, y)].fg, YELLOW);
}

#[test]
fn overview_connections_show_unknown_values() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.codex_connections = None;
    let mut app = app();
    let text = buffer_text(&draw(160, 50, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    }));
    assert!(text.contains("queue -  handshakes -"));
    assert!(text.contains("starts -  reused -"));
    assert!(text.contains("HTTP fallbacks -  origin 403 -"));
}

#[test]
fn overview_collapses_events_and_compacts_summary() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.recent.clear();
    let mut app = app();
    let screen = draw(100, 40, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    });
    let text = buffer_text(&screen);
    let (x, y) = placeholder_position(&screen, "Claude").unwrap();
    assert!(x < 34);
    assert_eq!(y, 1);
    assert!(text.contains("Events 0  none"));
    assert!(!text.contains("HTTP fallbacks"));
}

#[test]
fn overview_queue_counts_requests_without_session() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.sessions.truncate(1);
    state.sessions[0].session_id = None;
    state.active.truncate(1);
    state.active[0].session_id = None;
    state.active[0].queue_place = Some(1);
    let mut app = app();
    let screen = draw(160, 50, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    });
    let (x, y) = placeholder_position(&screen, "Queue").unwrap();
    assert_eq!(screen[(x + 4, y + 1)].symbol(), "1");
}

#[test]
fn overview_sizes_sessions_and_expands_events() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.sessions.truncate(1);
    state.recent.truncate(1);
    state.recent[0].status = crate::monitor::RequestStatus::Failed;
    state.recent[0].retry_requested = false;
    let mut app = app();
    let screen = draw(160, 40, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    });
    let (_, sessions_y) = placeholder_position(&screen, "Sessions").unwrap();
    let (_, active_y) = placeholder_position(&screen, "Active requests").unwrap();
    assert_eq!(active_y - sessions_y, 4);
    let (_, events_y) = placeholder_position(&screen, "Events").unwrap();
    assert!(events_y < 38);
    state.sessions = vec![state.sessions[0].clone(); 30];
    let screen = draw(160, 40, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    });
    let (_, sessions_y) = placeholder_position(&screen, "Sessions").unwrap();
    let (_, active_y) = placeholder_position(&screen, "Active requests").unwrap();
    assert_eq!(active_y - sessions_y, 11);
}

#[test]
fn compact_overview_shows_readable_lane_and_connection_values() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.active.clear();
    state.recent.clear();
    state.codex_connections = Some(crate::monitor::snapshot::CodexConnections {
        queued_attempts: 2,
        handshakes_in_flight: 3,
        handshake_spacing_ms: 250,
        websocket_reused: 3,
        websocket_new: 1,
        http_fallbacks: 7,
        origin_403: 8,
    });
    let mut app = app();
    app.accounts.local = true;
    app.accounts.limits.insert(
        "a-very-long-profile-name-that-must-not-appear".into(),
        crate::claude_accounts::limits::Limits::default(),
    );
    let text = buffer_text(&draw(119, 40, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    }));
    assert!(text.contains("Claude active 0 queued 0 -"));
    assert!(text.contains("Codex active 0 queued 0 -"));
    assert!(text.contains("queue 2 reuse 75% fallback 7 403 8"));
    assert!(!text.contains("a-very-long-profile-name"));
}

#[test]
fn expanded_claude_profile_ellipsizes_before_lane_stats() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.active.clear();
    state.recent.clear();
    let mut app = app();
    app.accounts.local = true;
    app.accounts.limits.insert(
        "x".repeat(40),
        crate::claude_accounts::limits::Limits::default(),
    );
    let text = buffer_text(&draw(120, 40, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    }));
    assert!(text.contains("…  0 active 0 queued - wait -"));
    assert!(!text.contains(&"x".repeat(40)));
}

#[test]
fn failed_request_detail_fits_single_session() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.sessions.truncate(1);
    state.recent.truncate(1);
    state.recent[0].status = crate::monitor::RequestStatus::Failed;
    state.recent[0].http_status = Some(502);
    state.recent[0].error = Some("unique failure detail".into());
    let mut app = app();
    app.detail = Some(DetailView::Request(DetailSelection::Keyboard));
    let text = buffer_text(&draw(120, 40, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    }));
    assert!(
        text.lines()
            .any(|line| line.contains("http status") && line.contains("502"))
    );
    assert!(
        text.lines()
            .any(|line| line.contains("detail") && line.contains("unique failure detail"))
    );
}

#[test]
fn retry_only_events_keep_count_when_collapsed() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.recent.truncate(1);
    state.recent[0].retry_requested = true;
    let mut app = app();
    let text = buffer_text(&draw(120, 40, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    }));
    assert!(
        text.lines()
            .any(|line| line.contains("Events 0") && line.contains("Retry requested: 1"))
    );
}

#[test]
fn compact_summary_stacks_full_values() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.active.clear();
    state.recent.clear();
    state.codex_connections = Some(crate::monitor::snapshot::CodexConnections {
        queued_attempts: 2,
        handshakes_in_flight: 3,
        handshake_spacing_ms: 250,
        websocket_reused: 3,
        websocket_new: 1,
        http_fallbacks: 7,
        origin_403: 8,
    });
    for width in [78, 100] {
        let mut app = app();
        let text = buffer_text(&draw(width, 40, |frame| {
            render_at(
                frame,
                &mut app,
                &state,
                std::time::UNIX_EPOCH,
                jiff::tz::TimeZone::UTC,
            );
        }));
        let lines: Vec<_> = text.lines().collect();
        let claude = lines
            .iter()
            .position(|line| line.contains("Claude active 0 queued 0 -"))
            .unwrap();
        assert!(lines[claude + 1].contains("Codex active 0 queued 0 -"));
        assert!(lines[claude + 2].contains("queue 2 reuse 75% fallback 7 403 8"));
    }
}

#[test]
fn retried_request_wait_uses_first_upstream_event() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.recent.truncate(1);
    let row = &mut state.recent[0];
    row.provider = Some("codex".into());
    row.started_at = std::time::UNIX_EPOCH;
    row.first_upstream_event_at = Some(row.started_at + std::time::Duration::from_secs(1));
    row.generation_started_at = Some(row.started_at + std::time::Duration::from_secs(8));
    let mut app = app();
    let text = buffer_text(&draw(160, 40, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    }));
    assert!(text.lines().any(|line| line.contains("wait 1.0s")));
}

#[test]
fn old_snapshot_wait_is_unmeasured() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.active.clear();
    state.recent.truncate(1);
    let row = &mut state.recent[0];
    row.provider = Some("codex".into());
    row.first_upstream_event_at = None;
    row.generation_started_at = Some(row.started_at + std::time::Duration::from_secs(8));
    row.latency = std::time::Duration::from_secs(10);
    let mut app = app();
    let text = buffer_text(&draw(200, 40, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    }));
    assert_eq!(usage_stats::lane_stats(&state.recent, "codex").0, None);
    assert!(text.lines().any(|line| line.contains("Wait")));
    let recent = buffer_text(&draw(200, 5, |frame| {
        super::requests::render_recent(frame, frame.area(), &state.recent, 0, false);
    }));
    let row = recent.lines().find(|line| line.contains("10s")).unwrap();
    assert!(!row.contains('█'));
    assert!(row.split_whitespace().any(|value| value == "-"));
}

#[test]
fn long_request_detail_fits_tall_terminal() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.sessions.truncate(1);
    state.recent.truncate(1);
    state.recent[0].traffic_capture_path = Some("/capture/unique-path.json".into());
    let mut app = app();
    app.detail = Some(DetailView::Request(DetailSelection::Keyboard));
    let text = buffer_text(&draw(160, 80, |frame| {
        render_at(
            frame,
            &mut app,
            &state,
            std::time::UNIX_EPOCH,
            jiff::tz::TimeZone::UTC,
        );
    }));
    assert!(text.contains("input tokens"));
    assert!(text.contains("/capture/unique-path.json"));
}
