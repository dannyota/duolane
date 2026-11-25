use super::tests::{buffer_text, draw, placeholder_position};
use super::*;
use crate::monitor::{RequestStatus, mock_state};
use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;

fn app() -> MonitorApp {
    MonitorApp {
        accounts: accounts::Accounts::new(false),
        usage_open: false,
        listen_url: String::new(),
        setup_text: String::new(),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 0,
        recent_selected: 0,
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: None,
        shutdown_complete: None,
    }
}

fn left_click(buffer: &Buffer, text: &str) -> MouseEvent {
    let (column, row) = placeholder_position(buffer, text).unwrap();
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn click(
    app: &mut MonitorApp,
    state: &MonitorSnapshot,
    locate: impl FnOnce(&Buffer) -> MouseEvent,
) {
    let mut hits = Default::default();
    let screen = draw(120, 48, |frame| {
        hits = render(frame, app, state);
    });
    app.handle_mouse(locate(&screen), state, &hits);
}

fn detail_text(app: &mut MonitorApp, state: &MonitorSnapshot) -> String {
    buffer_text(&draw(120, 48, |frame| {
        render(frame, app, state);
    }))
}

#[test]
fn tabs_switch_views_and_ignore_overlay_clicks() {
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    click(&mut app, &state, |screen| left_click(screen, "2 Usage"));
    assert!(app.usage_open);
    click(&mut app, &state, |screen| left_click(screen, "1 Overview"));
    assert!(!app.usage_open);
    app.show_help = true;
    click(&mut app, &state, |screen| left_click(screen, "2 Usage"));
    assert!(!app.usage_open);
}

#[test]
fn help_hint_is_clickable_but_navigation_hint_is_not() {
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    click(&mut app, &state, |screen| left_click(screen, "arrows/j/k"));
    assert!(!app.show_help);
    click(&mut app, &state, |screen| left_click(screen, "?"));
    assert!(app.show_help);
}

#[test]
fn usage_displays_speed_and_session_tokens() {
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    app.usage_open = true;
    let text = detail_text(&mut app, &state);
    assert!(text.contains("Speed by model"));
    assert!(text.contains("Tokens by session"));
    assert!(text.contains("claude-haiku-5-5"));
}

#[test]
fn clicking_a_scrolled_session_row_opens_the_displayed_session() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.sessions = (0..20)
        .map(|index| {
            let mut row = state.sessions[0].clone();
            row.session_id = Some(format!("s-row-{index:02}"));
            row
        })
        .collect();
    let mut app = app();
    app.selected = 19;

    click(&mut app, &state, |screen| {
        assert!(placeholder_position(screen, "s-row-00").is_none());
        left_click(screen, "s-row-18")
    });
    let text = detail_text(&mut app, &state);
    assert!(text.contains("Session detail"));
    assert!(
        text.lines()
            .any(|line| line.contains("  session         s-row-18"))
    );
}

#[test]
fn clicking_a_scrolled_recent_row_opens_the_displayed_request() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.recent = (0..20)
        .map(|index| {
            let mut row = state.recent[0].clone();
            row.request_id = format!("request-{index:02}");
            row.project = Some(format!("project-{index:02}"));
            row.status = RequestStatus::Completed;
            row.http_status = Some(200);
            row.error = None;
            row
        })
        .collect();
    let mut app = app();
    app.focus = FocusPane::Recent;
    app.recent_selected = 19;

    click(&mut app, &state, |screen| {
        assert!(placeholder_position(screen, "project-00").is_none());
        left_click(screen, "project-18")
    });
    let text = detail_text(&mut app, &state);
    assert!(text.contains("Request detail"));
    assert!(text.contains("request-18"));
}

#[test]
fn clicking_a_filtered_event_keeps_its_raw_request_identity_until_eviction() {
    let mut state: MonitorSnapshot = mock_state().into();
    let mut success = state.recent[0].clone();
    success.request_id = "same-id".into();
    success.status = RequestStatus::Completed;
    success.http_status = Some(200);
    success.error = None;
    let mut failure = success.clone();
    failure.request_id = "same-\u{7}id".into();
    failure.http_status = Some(502);
    failure.error = Some("event-only-click".into());
    state.recent = vec![success, failure];
    let mut app = app();

    click(&mut app, &state, |screen| {
        left_click(screen, "event-only-click")
    });
    let detail_has_failure = |text: &str| {
        text.lines()
            .any(|line| line.contains("  http status     502"))
    };
    let text = detail_text(&mut app, &state);
    assert!(text.contains("Request detail"));
    assert!(detail_has_failure(&text));
    assert!(text.contains("same-id"));
    assert!(!text.chars().any(|ch| ch.is_control() && ch != '\n'));

    state.recent.swap(0, 1);
    assert!(detail_has_failure(&detail_text(&mut app, &state)));

    state.recent.remove(0);
    assert!(detail_text(&mut app, &state).contains("No request selected"));
}

#[test]
fn retry_events_do_not_hide_a_lookalike_failure_or_change_click_identity() {
    let mut state: MonitorSnapshot = mock_state().into();
    let mut failure = state.recent[0].clone();
    failure.request_id = "actual-failure".into();
    failure.status = RequestStatus::Failed;
    failure.http_status = Some(400);
    failure.error = Some("capability_rejected: beta_header:message-threads-2026-08-12".into());
    let mut retry = serde_json::to_value(&failure).unwrap();
    retry["retry_requested"] = true.into();
    state.recent = (0..12)
        .map(|index| {
            retry["request_id"] = format!("retry-{index}").into();
            serde_json::from_value(retry.clone()).unwrap()
        })
        .collect();
    state.recent.push(failure);
    let mut app = app();
    click(&mut app, &state, |screen| {
        let text = buffer_text(screen);
        assert!(text.contains("Retry requested: 12"));
        left_click(screen, "capability_rejected")
    });
    assert_eq!(app.recent_selected, 12);
    assert!(detail_text(&mut app, &state).contains("actual-failure"));
}

#[test]
fn only_left_button_down_on_an_uncovered_data_row_opens_detail() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.sessions.truncate(1);
    state.sessions[0].session_id = Some("clck-row".into());
    let mut app = app();
    let row_click = |screen: &Buffer| left_click(screen, "clck-row");

    for invalid in [
        |event: &mut MouseEvent| event.column = 0,
        |event: &mut MouseEvent| event.row -= 1,
        |event: &mut MouseEvent| event.row += 1,
        |event: &mut MouseEvent| event.kind = MouseEventKind::Down(MouseButton::Right),
        |event: &mut MouseEvent| event.kind = MouseEventKind::Up(MouseButton::Left),
    ] {
        click(&mut app, &state, |screen| {
            let mut event = row_click(screen);
            invalid(&mut event);
            event
        });
        assert!(app.detail.is_none());
    }

    app.show_help = true;
    click(&mut app, &state, row_click);
    assert!(app.detail.is_none());
    app.show_help = false;
    app.show_setup = true;
    click(&mut app, &state, row_click);
    assert!(app.detail.is_none());
    app.show_setup = false;
    app.phase = MonitorPhase::ConfirmingShutdown;
    click(&mut app, &state, row_click);
    assert!(app.detail.is_none());
    app.phase = MonitorPhase::ShuttingDown;
    click(&mut app, &state, row_click);
    assert!(app.detail.is_none());

    app.phase = MonitorPhase::Running;
    click(&mut app, &state, row_click);
    assert!(app.detail.is_some());
}
