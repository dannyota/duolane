use super::mouse_tests::app;
use super::tests::{buffer_text, draw};
use super::*;
use crate::monitor::{RequestStatus, mock_state};
use std::time::{Duration, UNIX_EPOCH};

#[test]
fn overview_renders_request_phases_and_recent_times() {
    let mut state: MonitorSnapshot = mock_state().into();
    let now = UNIX_EPOCH + Duration::from_secs(100);
    state.active.truncate(1);
    state.active[0].status = RequestStatus::Upstream;
    state.active[0].started_at = now - Duration::from_secs(9);
    state.active[0].first_upstream_event_at = Some(now - Duration::from_secs(7));
    state.active[0].first_output_at = Some(now - Duration::from_secs(4));
    state.active[0].queue_place = None;
    state.recent.truncate(1);
    state.recent[0].started_at = now - Duration::from_secs(10);
    state.recent[0].first_upstream_event_at = Some(now - Duration::from_secs(8));
    state.recent[0].first_output_at = None;
    state.recent[0].latency = Duration::from_secs(10);
    let mut app = app();
    let screen = draw(180, 50, |frame| {
        render_at(frame, &mut app, &state, now, jiff::tz::TimeZone::UTC);
    });
    let text = buffer_text(&screen);
    assert!(text.contains("streaming"), "{text}");
    assert!(text.contains("Timeline"), "{text}");
    assert!(text.contains("Wait"), "{text}");
    assert!(text.contains("First"), "{text}");
    assert!(text.contains("Total"), "{text}");
    let row = text
        .lines()
        .find(|line| line.contains("200") && line.contains("10s"))
        .unwrap();
    let values: Vec<_> = row.split_whitespace().collect();
    assert!(
        values.windows(3).any(|parts| parts == ["2s", "-", "10s"]),
        "{row}"
    );
    state.recent[0].first_output_at = Some(now - Duration::from_secs(5));
    let text = buffer_text(&draw(180, 5, |frame| {
        super::requests::render_recent(frame, frame.area(), &state.recent, 0, false);
    }));
    let row = text
        .lines()
        .find(|line| line.contains("200") && line.contains("10s"))
        .unwrap();
    let values: Vec<_> = row.split_whitespace().collect();
    assert!(
        values.windows(3).any(|parts| parts == ["2s", "5s", "10s"]),
        "{row}"
    );
    assert!(row.contains("░░"), "{row}");
    assert!(row.contains("█"), "{row}");
}

#[test]
fn active_phase_labels_follow_snapshot_times() {
    let mut state: MonitorSnapshot = mock_state().into();
    let request = &mut state.active[0];
    let now = UNIX_EPOCH + Duration::from_secs(100);
    request.started_at = now - Duration::from_secs(9);
    request.status = RequestStatus::Upstream;
    request.first_upstream_event_at = None;
    request.first_output_at = None;
    for (queue, upstream, output, expected) in [
        (Some(2), None, None, "queued #2"),
        (None, None, None, "connecting"),
        (None, Some(now - Duration::from_secs(7)), None, "thinking"),
        (
            None,
            Some(now - Duration::from_secs(7)),
            Some(now - Duration::from_secs(4)),
            "streaming",
        ),
    ] {
        request.queue_place = queue;
        request.first_upstream_event_at = upstream;
        request.first_output_at = output;
        let text = buffer_text(&draw(160, 5, |frame| {
            super::requests::render_active_at(
                frame,
                frame.area(),
                std::slice::from_ref(request),
                0,
                now,
            );
        }));
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn active_timeline_renders_three_phases_and_scales() {
    let mut state: MonitorSnapshot = mock_state().into();
    let request = &mut state.active[0];
    let now = UNIX_EPOCH + Duration::from_secs(1000);
    request.status = RequestStatus::Upstream;
    request.queue_place = None;
    for (duration, upstream, output) in [(9, 2, 5), (900, 200, 500)] {
        request.started_at = now - Duration::from_secs(duration);
        request.first_upstream_event_at = Some(request.started_at + Duration::from_secs(upstream));
        request.first_output_at = Some(request.started_at + Duration::from_secs(output));
        let text = buffer_text(&draw(160, 5, |frame| {
            super::requests::render_active_at(
                frame,
                frame.area(),
                std::slice::from_ref(request),
                0,
                now,
            );
        }));
        let row = text
            .lines()
            .find(|line| line.contains("streaming") && !line.contains("requests"))
            .unwrap();
        assert!(row.contains("░"));
        assert!(row.contains("▒"));
        assert!(row.contains("█"));
        if duration == 9 {
            assert!(row.contains("░░▒▒▒████"), "{row}");
        }
    }
}

#[test]
fn expanded_tables_keep_tokens_and_active_timeline() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.active.truncate(1);
    state.recent.truncate(1);
    state.active[0].model = Some("a".repeat(60));
    state.recent[0].model = Some("a".repeat(60));
    let active = buffer_text(&draw(120, 5, |frame| {
        super::requests::render_active_at(frame, frame.area(), &state.active, 0, UNIX_EPOCH)
    }));
    let recent = buffer_text(&draw(120, 5, |frame| {
        super::requests::render_recent(frame, frame.area(), &state.recent, 0, false);
    }));
    assert!(active.contains("Timeline"), "{active}");
    let header = recent
        .lines()
        .find(|line| line.contains("Finished"))
        .unwrap();
    let words: Vec<_> = header.split_whitespace().collect();
    assert!(words.contains(&"In"), "{recent}");
    assert!(words.contains(&"Out"), "{recent}");
    assert!(!recent.contains("Timeline"), "{recent}");
    let wide = buffer_text(&draw(180, 5, |frame| {
        super::requests::render_recent(frame, frame.area(), &state.recent, 0, false);
    }));
    assert!(wide.contains("Timeline"), "{wide}");
}

#[test]
fn compact_request_tables_omit_phase_metrics() {
    let state: MonitorSnapshot = mock_state().into();
    let active = buffer_text(&draw(119, 5, |frame| {
        super::requests::render_active_at(frame, frame.area(), &state.active, 0, UNIX_EPOCH)
    }));
    let recent = buffer_text(&draw(119, 5, |frame| {
        super::requests::render_recent(frame, frame.area(), &state.recent, 0, false);
    }));
    assert!(!active.contains("Timeline"));
    assert!(!recent.contains("Timeline"));
    assert!(!recent.contains("Wait"));
    assert!(!recent.contains("First"));
}
