use super::*;

#[test]
fn selected_rows_scroll_into_table_viewports() {
    let state: MonitorSnapshot = mock_state().into();
    let sessions = (0..12)
        .map(|index| {
            let mut session = state.sessions[0].clone();
            session.session_id = Some(format!("row-{index:04}"));
            session
        })
        .collect::<Vec<_>>();
    let session_buffer = draw(120, 6, |frame| {
        render_sessions(frame, frame.area(), &sessions, 11, true, state.snapshot_at)
    });
    let session_text = buffer_text(&session_buffer);
    assert!(session_text.contains("row-0011"), "{session_text}");
    assert!(!session_text.contains("row-0000"), "{session_text}");

    let recent = (0..12)
        .map(|index| {
            let mut request = state.recent[0].clone();
            request.project = Some(format!("row-{index:04}"));
            request
        })
        .collect::<Vec<_>>();
    let recent_buffer = draw(120, 6, |frame| {
        render_recent(frame, frame.area(), &recent, 11, true)
    });
    let recent_text = buffer_text(&recent_buffer);
    assert!(recent_text.contains("row-0011"), "{recent_text}");
    assert!(!recent_text.contains("row-0000"), "{recent_text}");
}

#[test]
fn dashboard_shows_codex_sources_at_24_rows() {
    let monitor = MonitorHandle::new(10);
    monitor.request_started("done", None, None, EndpointKind::Messages);
    monitor.provider_selected("done", "codex", "model", None);
    monitor.codex_source("done", "api key (from chatgpt: usage limit)");
    monitor.request_completed("done", 200, None, None);
    monitor.request_started("active", None, None, EndpointKind::Messages);
    monitor.provider_selected("active", "codex", "model", None);
    monitor.codex_source("active", "chatgpt");
    let state: MonitorSnapshot = monitor.snapshot().into();
    let mut app = MonitorApp {
        accounts: accounts::Accounts::new(false),
        usage_open: false,
        listen_url: "test".into(),
        setup_text: String::new(),
        show_setup: false,
        show_help: false,
        detail: Some(DetailView::Request(DetailSelection::Keyboard)),
        focus: FocusPane::Recent,
        selected: 0,
        recent_selected: 0,
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: None,
        shutdown_complete: None,
    };
    app.active_resize.height = 4;
    let buffer = draw(140, 24, |frame| render(frame, &mut app, &state));
    let text = buffer_text(&buffer);
    assert!(text.contains("from chatgpt: usage limit"));
    assert!(text.matches("chatgpt").count() >= 2);
    assert!(text.matches("api key").count() >= 2);
}

#[test]
fn mock_state_renders_representative_panes_at_wide_width() {
    let state: MonitorSnapshot = mock_state().into();
    let mut app = MonitorApp {
        accounts: accounts::Accounts::new(false),
        usage_open: false,
        listen_url: "mock://tui-demo".to_string(),
        setup_text: String::new(),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 0,
        recent_selected: 0,
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: None,
        shutdown_complete: None,
    };

    let buffer = draw(180, 48, |frame| render(frame, &mut app, &state));
    let text = buffer_text(&buffer);

    assert!(text.contains("mock://tui-demo"), "{text}");
    assert!(text.contains("duolane"), "{text}");
    assert!(text.contains("streaming"), "{text}");
    assert!(text.contains("gpt-5.6-terra"), "{text}");
    assert!(text.contains("upstream connection closed"), "{text}");
}

#[test]
fn render_removes_control_characters_before_layout() {
    let monitor = MonitorHandle::default();
    let model = format!("gpt-{}5.6-terra", "\u{1b}".repeat(30));
    monitor.request_started("req", Some("s\u{7}1".into()), None, EndpointKind::Messages);
    monitor.provider_selected("req", "codex", model, None);
    monitor.request_failed("req", Some(502), "upstream\u{1b}[2J\u{9b}31m closed");
    let state: MonitorSnapshot = monitor.snapshot().into();
    let mut app = MonitorApp {
        accounts: accounts::Accounts::new(false),
        usage_open: false,
        listen_url: "http://127.0.0.1:3000".to_string(),
        setup_text: String::new(),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Recent,
        selected: 0,
        recent_selected: 0,
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: None,
        shutdown_complete: None,
    };

    let buffer = draw(180, 48, |frame| render(frame, &mut app, &state));
    let text = buffer_text(&buffer);

    assert!(
        buffer
            .content()
            .iter()
            .all(|cell| !cell.symbol().contains(char::is_control)),
        "{text:?}"
    );
    assert!(text.contains("gpt-5.6-terra"), "{text}");
    assert!(text.contains("upstream[2J31m closed"), "{text}");
}

#[test]
fn shutdown_confirmation_can_be_cancelled_before_signalling_server() {
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let mut app = MonitorApp {
        accounts: accounts::Accounts::new(false),
        usage_open: false,
        listen_url: "http://127.0.0.1:3000".to_string(),
        setup_text: String::new(),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 0,
        recent_selected: 0,
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: Some(shutdown_tx),
        shutdown_complete: Some(mpsc::channel().1),
    };

    app.request_shutdown_confirmation();
    assert_eq!(app.phase, MonitorPhase::ConfirmingShutdown);
    assert!(matches!(
        shutdown_rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));

    let state: MonitorSnapshot = MonitorHandle::default().snapshot().into();
    let screen = draw(80, 24, |frame| render(frame, &mut app, &state));
    let text = buffer_text(&screen);
    assert!(text.contains("Shut down proxy?"), "{text}");
    assert!(text.contains("y confirm"), "{text}");
    assert!(text.contains("n/Esc/q cancel"), "{text}");

    app.cancel_shutdown_confirmation();
    assert_eq!(app.phase, MonitorPhase::Running);
    assert!(matches!(
        shutdown_rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));

    app.request_shutdown_confirmation();
    app.begin_shutdown();
    assert_eq!(app.phase, MonitorPhase::ShuttingDown);
    assert_eq!(shutdown_rx.try_recv(), Ok(()));
}

#[test]
fn ctrl_c_starts_shutdown_then_requests_force_quit() {
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (_shutdown_complete_tx, shutdown_complete_rx) = mpsc::channel();
    let mut app = MonitorApp {
        accounts: accounts::Accounts::new(false),
        usage_open: false,
        listen_url: "http://127.0.0.1:3000".to_string(),
        setup_text: String::new(),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 0,
        recent_selected: 0,
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: Some(shutdown_tx),
        shutdown_complete: Some(shutdown_complete_rx),
    };

    assert!(!app.handle_ctrl_c());
    assert!(app.handle_ctrl_c());

    assert_eq!(app.phase, MonitorPhase::ShuttingDown);
    assert_eq!(shutdown_rx.try_recv(), Ok(()));
    let state: MonitorSnapshot = MonitorHandle::default().snapshot().into();
    let screen = draw(80, 24, |frame| render(frame, &mut app, &state));
    let text = buffer_text(&screen);
    assert!(text.contains("Shutting down..."));
    assert!(text.contains("Press Ctrl-C to force quit"));
}

#[test]
fn shutdown_completion_accepts_notification_and_sender_drop() {
    let (complete_tx, complete_rx) = mpsc::channel();
    let app = MonitorApp {
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
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: None,
        shutdown_complete: Some(complete_rx),
    };

    assert!(!app.shutdown_is_complete());
    complete_tx.send(()).unwrap();
    assert!(app.shutdown_is_complete());

    let (complete_tx, complete_rx) = mpsc::channel();
    let mut app = app;
    app.shutdown_complete = Some(complete_rx);
    drop(complete_tx);
    assert!(app.shutdown_is_complete());
}

#[test]
fn header_renders_configured_listen_url() {
    let app = MonitorApp {
        accounts: accounts::Accounts::new(false),
        usage_open: false,
        listen_url: "http://[::]:18765".to_string(),
        setup_text: String::new(),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 0,
        recent_selected: 0,
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: None,
        shutdown_complete: Some(mpsc::channel().1),
    };
    let state: MonitorSnapshot = MonitorHandle::default().snapshot().into();

    let header = draw(120, 1, |frame| {
        render_header(frame, frame.area(), &app, &state)
    });

    assert!(buffer_text(&header).contains("http://[::]:18765"));
    assert!(buffer_text(&header).contains("active"));
}

#[test]
fn clamp_selection_caps_to_available_sessions() {
    let mut app = MonitorApp {
        accounts: accounts::Accounts::new(false),
        usage_open: false,
        listen_url: "http://127.0.0.1:3000".to_string(),
        setup_text: String::new(),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 10,
        recent_selected: 10,
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: None,
        shutdown_complete: Some(mpsc::channel().1),
    };

    app.clamp_selection(3, 4);
    assert_eq!(app.selected, 2);
    assert_eq!(app.recent_selected, 3);

    app.clamp_selection(0, 0);
    assert_eq!(app.selected, 0);
    assert_eq!(app.recent_selected, 0);
}

#[test]
fn arrow_navigation_moves_between_focus_panes_at_edges() {
    let mut app = MonitorApp {
        accounts: accounts::Accounts::new(false),
        usage_open: false,
        listen_url: "http://127.0.0.1:3000".to_string(),
        setup_text: String::new(),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 1,
        recent_selected: 0,
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: None,
        shutdown_complete: Some(mpsc::channel().1),
    };

    app.move_down(2, 3, true);
    assert_eq!(app.focus, FocusPane::Recent);
    assert_eq!(app.recent_selected, 0);

    app.move_up(2, 3, true);
    assert_eq!(app.focus, FocusPane::Sessions);
    assert_eq!(app.selected, 1);
}

#[test]
fn vim_navigation_stays_within_focused_pane() {
    let mut app = MonitorApp {
        accounts: accounts::Accounts::new(false),
        usage_open: false,
        listen_url: "http://127.0.0.1:3000".to_string(),
        setup_text: String::new(),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 1,
        recent_selected: 0,
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: None,
        shutdown_complete: Some(mpsc::channel().1),
    };

    app.move_down(2, 3, false);
    assert_eq!(app.focus, FocusPane::Sessions);
    assert_eq!(app.selected, 1);

    app.focus = FocusPane::Recent;
    app.move_up(2, 3, false);
    assert_eq!(app.focus, FocusPane::Recent);
    assert_eq!(app.recent_selected, 0);
}
