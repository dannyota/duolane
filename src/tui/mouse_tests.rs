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
fn enter_hint_is_never_clickable_but_help_remains_clickable() {
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    click(&mut app, &state, |screen| left_click(screen, "Enter"));
    assert!(app.detail.is_none());
    click(&mut app, &state, |screen| left_click(screen, "?"));
    assert!(app.show_help);
    app.show_help = false;
    app.usage_open = true;
    click(&mut app, &state, |screen| left_click(screen, "Enter"));
    assert!(!app.accounts.confirming());
}

#[test]
fn usage_groups_raw_models_before_sanitizing_labels() {
    let mut state: MonitorSnapshot = mock_state().into();
    let mut first = state.recent[0].clone();
    first.model = Some("model".into());
    let mut second = first.clone();
    second.model = Some("mo\u{7}del".into());
    state.recent = vec![first, second];
    let mut app = app();
    app.usage_open = true;
    let text = detail_text(&mut app, &state);
    let speed = text
        .split("Speed by model")
        .nth(1)
        .unwrap()
        .split("Tokens by session")
        .next()
        .unwrap();
    assert_eq!(speed.matches("model").count(), 2);
    assert!(!text.contains('\u{7}'));
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

#[test]
fn banner_covers_all_footer_targets() {
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    let mut hits = MouseTargets::default();
    let screen = draw(120, 48, |frame| {
        hits = render_with_banner(frame, &mut app, &state, Some("offline"));
    });
    assert!(
        buffer_text(&screen)
            .lines()
            .last()
            .unwrap()
            .trim_end()
            .ends_with("offline")
    );
    assert!(
        app.handle_mouse(
            MouseEvent {
                column: 1,
                row: 47,
                ..left_click(&screen, "Reconnecting")
            },
            &state,
            &hits
        )
        .is_none()
    );
    let screen = draw(120, 48, |frame| {
        hits = render_with_banner(frame, &mut app, &state, None);
    });
    assert_eq!(
        app.handle_mouse(
            MouseEvent {
                column: 1,
                row: 47,
                ..left_click(&screen, "q")
            },
            &state,
            &hits
        ),
        Some(MonitorExit::Detached)
    );
}

#[test]
fn click_requires_frame_size_to_match_current_size() {
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    let mut hits = MouseTargets::default();
    let screen = draw(120, 48, |frame| hits = render(frame, &mut app, &state));
    let event = left_click(&screen, "2 Usage");
    app.handle_mouse_sized(event, &state, &hits, (80, 24));
    assert!(!app.usage_open);
    app.handle_mouse_sized(event, &state, &hits, (120, 48));
    assert!(app.usage_open);
}

#[test]
fn usage_tables_use_remaining_width_and_show_omitted_rows() {
    let mut state: MonitorSnapshot = mock_state().into();
    state.sessions = (0..12)
        .map(|n| {
            let mut s = state.sessions[0].clone();
            s.project = Some(format!("p{n}"));
            s
        })
        .collect();
    state.recent = (0..12)
        .map(|n| {
            let mut r = state.recent[0].clone();
            r.model = Some(format!("m{n}"));
            r
        })
        .collect();
    let mut app = app();
    app.usage_open = true;
    let text = buffer_text(&draw(80, 24, |frame| render(frame, &mut app, &state)));
    let speed = text
        .split("Speed by model")
        .nth(1)
        .unwrap()
        .split("Tokens by session")
        .next()
        .unwrap();
    let tokens = text.split("Tokens by session").nth(1).unwrap();
    assert!(speed.contains('█') || speed.contains('░'), "{text}");
    assert!(tokens.contains('█'), "{text}");
    assert!(speed.contains("more") && tokens.contains("more"), "{text}");
}

#[test]
fn usage_keeps_missing_and_literal_dash_models_separate() {
    let mut state: MonitorSnapshot = mock_state().into();
    let mut first = state.recent[0].clone();
    first.model = None;
    let mut second = first.clone();
    second.model = Some("-".into());
    state.recent = vec![first, second];
    let mut app = app();
    app.usage_open = true;
    let text = detail_text(&mut app, &state);
    let speed = text
        .split("Speed by model")
        .nth(1)
        .unwrap()
        .split("Tokens by session")
        .next()
        .unwrap();
    assert_eq!(
        speed
            .lines()
            .filter(|line| line.contains("claude") || line.contains("codex"))
            .count(),
        2
    );
}

#[cfg(unix)]
pub(super) fn app_for_accounts_test(accounts: accounts::Accounts) -> MonitorApp {
    let mut app = app();
    app.accounts = accounts;
    app.usage_open = true;
    app
}

#[test]
fn attached_usage_shows_both_reset_times_at_90_by_24() {
    use crate::monitor::{CodexQuotaSnapshot, CodexQuotaWindow};
    use std::time::{Duration, UNIX_EPOCH};
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let reset = now.duration_since(UNIX_EPOCH).unwrap().as_secs() + 3600;
    let mut state: MonitorSnapshot = mock_state().into();
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: now,
        primary: Some(CodexQuotaWindow {
            used_percent: Some(42.0),
            window_minutes: Some(300),
            reset_at: Some(reset),
        }),
        secondary: Some(CodexQuotaWindow {
            used_percent: Some(12.0),
            window_minutes: Some(10080),
            reset_at: Some(reset + Duration::from_secs(3600).as_secs()),
        }),
    });
    let mut app = app();
    app.usage_open = true;
    let text = buffer_text(&draw(90, 24, |frame| {
        render_at(frame, &mut app, &state, now, jiff::tz::TimeZone::UTC)
    }));
    let first = cells::format_system_time_in_zone(
        UNIX_EPOCH + Duration::from_secs(reset),
        jiff::tz::TimeZone::UTC,
    );
    let second = cells::format_system_time_in_zone(
        UNIX_EPOCH + Duration::from_secs(reset + 3600),
        jiff::tz::TimeZone::UTC,
    );
    let lines: Vec<_> = text.lines().collect();
    for (label, reset) in [("5h", first), ("week", second)] {
        assert!(
            lines.windows(2).any(|rows| rows[0].contains(label)
                && (rows[0].contains(&reset) || rows[1].contains(&reset))),
            "{text}"
        );
    }
}

#[test]
fn compact_subscription_uses_validated_window_labels() {
    use crate::monitor::{CodexQuotaSnapshot, CodexQuotaWindow};
    use std::time::SystemTime;
    let mut state: MonitorSnapshot = mock_state().into();
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: SystemTime::now(),
        primary: Some(CodexQuotaWindow {
            used_percent: Some(42.0),
            window_minutes: Some(300),
            reset_at: None,
        }),
        secondary: Some(CodexQuotaWindow {
            used_percent: Some(12.0),
            window_minutes: Some(120),
            reset_at: None,
        }),
    });
    let mut app = app();
    app.usage_open = true;
    let text = buffer_text(&draw(100, 10, |frame| render(frame, &mut app, &state)));
    assert!(text.contains("5h"), "{text}");
    assert!(text.contains("Secondary (120m)"), "{text}");
}

#[test]
fn key_events_only_query_size_when_saving_confirmation() {
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    let key = crossterm::event::KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
    assert!(
        app.handle_key_query(key, &state, (80, 24), || Err(std::io::Error::other("size")))
            .is_ok()
    );
    assert!(app.show_help);
}

#[test]
fn usage_keeps_missing_and_literal_dash_provider_and_effort_separate() {
    let mut state: MonitorSnapshot = mock_state().into();
    let mut row = state.recent[0].clone();
    row.model = Some("distinct".into());
    row.provider = None;
    row.effort = None;
    let mut provider = row.clone();
    provider.provider = Some("-".into());
    let mut effort = row.clone();
    effort.effort = Some("-".into());
    state.recent = vec![row, provider, effort];
    let mut app = app();
    app.usage_open = true;
    let text = detail_text(&mut app, &state);
    assert_eq!(text.matches("distinct").count(), 3, "{text}");
}

#[test]
fn stale_subscription_remains_marked_when_summary_does_not_fit() {
    use crate::monitor::{CodexQuotaSnapshot, CodexQuotaWindow};
    use std::time::{Duration, SystemTime};
    let mut state: MonitorSnapshot = mock_state().into();
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: SystemTime::now() - Duration::from_secs(901),
        primary: Some(CodexQuotaWindow {
            used_percent: Some(42.0),
            window_minutes: Some(300),
            reset_at: None,
        }),
        secondary: None,
    });
    let mut app = app();
    app.usage_open = true;
    let text = buffer_text(&draw(100, 10, |frame| render(frame, &mut app, &state)));
    assert!(text.contains("42.0%") && text.contains("Stale"), "{text}");
    state.codex_quota.as_mut().unwrap().observed_at = SystemTime::now() + Duration::from_secs(60);
    for height in [8, 10, 24] {
        let text = buffer_text(&draw(100, height, |frame| render(frame, &mut app, &state)));
        assert!(!text.contains("42.0%"), "{text}");
    }
}

#[test]
fn single_line_subscription_keeps_both_reset_times() {
    use crate::monitor::{CodexQuotaSnapshot, CodexQuotaWindow};
    use std::time::{Duration, UNIX_EPOCH};
    let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let reset = 1_700_003_600;
    let mut state: MonitorSnapshot = mock_state().into();
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: now,
        primary: Some(CodexQuotaWindow {
            used_percent: Some(42.0),
            window_minutes: Some(120),
            reset_at: Some(reset),
        }),
        secondary: Some(CodexQuotaWindow {
            used_percent: Some(12.0),
            window_minutes: Some(240),
            reset_at: Some(reset + 3600),
        }),
    });
    let mut app = app();
    app.usage_open = true;
    let text = buffer_text(&draw(100, 8, |frame| {
        render_at(frame, &mut app, &state, now, jiff::tz::TimeZone::UTC)
    }));
    assert!(text.contains("120m 42.0% 1h0m"), "{text}");
    assert!(text.contains("240m 12.0% 2h0m"), "{text}");
}

#[test]
fn mouse_size_query_failure_does_not_interrupt_monitor() {
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    let mut hits = MouseTargets::default();
    let screen = draw(120, 48, |frame| hits = render(frame, &mut app, &state));
    let click = left_click(&screen, "2 Usage");
    let movement = MouseEvent {
        kind: MouseEventKind::Moved,
        ..click
    };
    assert!(
        app.handle_mouse_query(click, &state, &hits, || Err(std::io::Error::other("size")))
            .is_none()
    );
    assert!(
        app.handle_mouse_query(movement, &state, &hits, || panic!(
            "mouse move queried size"
        ))
        .is_none()
    );
    assert!(!app.usage_open);
    assert!(
        app.handle_mouse_query(click, &state, &hits, || Ok((120, 48)))
            .is_none()
    );
    assert!(app.usage_open);
}
