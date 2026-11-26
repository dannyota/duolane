use super::pointer::{Pointer, supported};
use std::collections::HashMap;

#[test]
fn detects_supported_terminals_and_rejects_multiplexers() {
    for (key, value) in [
        ("TERM_PROGRAM", "ghostty"),
        ("TERM", "xterm-ghostty"),
        ("TERM", "xterm-kitty"),
        ("TERM", "foot"),
        ("TERM", "foot-extra"),
    ] {
        assert!(supported(&HashMap::from([(
            key.to_string(),
            value.to_string()
        )])));
    }
    assert!(!supported(&HashMap::from([(
        "TERM_PROGRAM".into(),
        "WezTerm".into()
    )])));
    for variable in ["TMUX", "STY"] {
        assert!(!supported(&HashMap::from([
            ("TERM".into(), "xterm-kitty".into()),
            (variable.into(), String::new()),
        ])));
    }
    for (program, term) in [
        ("tmux", "xterm-kitty"),
        ("ghostty", "screen-256color"),
        ("WezTerm", "tmux-256color"),
        ("unknown", "unknown"),
    ] {
        assert!(!supported(&HashMap::from([
            ("TERM_PROGRAM".to_string(), program.to_string()),
            ("TERM".to_string(), term.to_string())
        ])));
    }
}

#[test]
fn pointer_writes_only_transitions_and_resets_on_exit() {
    let mut pointer = Pointer::new(true);
    let mut output = Vec::new();
    for wanted in [true, true, false, true, true, false, true] {
        pointer.update(&mut output, wanted).unwrap();
    }
    pointer.reset(&mut output).unwrap();
    pointer.reset(&mut output).unwrap();
    assert_eq!(output, b"\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\");
}

#[test]
fn mouse_events_follow_border_and_drag() {
    use super::{mouse::MouseTargets, mouse_tests::app, *};
    use crate::monitor::mock_state;
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    let mut pointer = Pointer::new(true);
    let targets = MouseTargets {
        size: (80, 24),
        active: Some((Rect::new(2, 5, 20, 5), 5)),
        ..Default::default()
    };
    let mut output = Vec::new();
    for (kind, column, row) in [
        (MouseEventKind::Moved, 2, 9),
        (MouseEventKind::Moved, 10, 9),
        (MouseEventKind::Moved, 10, 10),
        (MouseEventKind::Down(MouseButton::Left), 10, 9),
        (MouseEventKind::Drag(MouseButton::Left), 10, 12),
        (MouseEventKind::Up(MouseButton::Left), 10, 12),
        (MouseEventKind::Moved, 10, 9),
    ] {
        let event = MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse_query(event, &state, &targets, || Ok((80, 24)));
        app.refresh_pointer(&mut pointer, &mut output, &targets);
    }
    pointer.reset(&mut output).unwrap();
    assert_eq!(output, b"\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\");
}

#[test]
fn key_overlay_and_moved_border_reset_pointer_after_frame() {
    use super::{mouse::MouseTargets, mouse_tests::app, *};
    use crate::monitor::mock_state;
    let state: MonitorSnapshot = mock_state().into();
    for key in ['?', 'b', '2'] {
        let mut app = app();
        let mut pointer = Pointer::new(true);
        let mut output = Vec::new();
        let targets = MouseTargets {
            size: (80, 24),
            active: Some((Rect::new(2, 5, 20, 5), 5)),
            ..Default::default()
        };
        let mouse = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 10,
            row: 9,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse_query(mouse, &state, &targets, || Ok((80, 24)));
        app.refresh_pointer(&mut pointer, &mut output, &targets);
        app.handle_key(
            crossterm::event::KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
            &state,
        );
        app.refresh_pointer(&mut pointer, &mut output, &targets);
        assert_eq!(output, b"\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\");
    }
    let mut app = app();
    let mut pointer = Pointer::new(true);
    let mut output = Vec::new();
    let mut targets = MouseTargets {
        size: (80, 24),
        active: Some((Rect::new(2, 5, 20, 5), 5)),
        ..Default::default()
    };
    let mouse = MouseEvent {
        kind: MouseEventKind::Moved,
        column: 10,
        row: 9,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse_query(mouse, &state, &targets, || Ok((80, 24)));
    app.refresh_pointer(&mut pointer, &mut output, &targets);
    targets.active = Some((Rect::new(2, 5, 20, 6), 5));
    app.refresh_pointer(&mut pointer, &mut output, &targets);
    assert_eq!(output, b"\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\");
}

#[test]
fn guard_restores_pointer_before_terminal() {
    use super::pointer::restore;
    use std::io::Write;
    let mut pointer = Pointer::new(true);
    let mut output = Vec::new();
    pointer.update(&mut output, true).unwrap();
    restore(&mut pointer, &mut output, |writer| {
        writer.write_all(b"restore").unwrap();
    });
    assert_eq!(
        output,
        b"\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\restore"
    );
}

#[test]
fn failed_pointer_write_does_not_stop_events_or_change_force_quit() {
    use super::{mouse::MouseTargets, mouse_tests::app, *};
    use crate::monitor::mock_state;
    use std::io::{self, Write};
    struct Fail;
    impl Write for Fail {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let state: MonitorSnapshot = mock_state().into();
    let mut app = app();
    let (_shutdown_complete, receiver) = std::sync::mpsc::channel();
    app.shutdown_complete = Some(receiver);
    let mut pointer = Pointer::new(true);
    let targets = MouseTargets {
        size: (80, 24),
        active: Some((Rect::new(2, 5, 20, 5), 5)),
        ..Default::default()
    };
    let mouse = MouseEvent {
        kind: MouseEventKind::Moved,
        column: 10,
        row: 9,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse_query(mouse, &state, &targets, || Ok((80, 24)));
    app.refresh_pointer(&mut pointer, &mut Fail, &targets);
    let key = crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(app.handle_key(key, &state), None);
    assert_eq!(app.handle_key(key, &state), Some(MonitorExit::ForceQuit));
    let mut output = Vec::new();
    pointer.reset(&mut output).unwrap();
    assert_eq!(output, b"\x1b]22;default\x1b\\");
}

#[test]
fn failed_reset_is_retried() {
    use std::io::{self, Write};
    struct Fail;
    impl Write for Fail {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("flush"))
        }
    }
    let mut pointer = Pointer::new(true);
    let mut output = Vec::new();
    pointer.update(&mut output, true).unwrap();
    assert!(pointer.reset(&mut Fail).is_err());
    pointer.reset(&mut output).unwrap();
    assert_eq!(output, b"\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\");
}

#[test]
fn unsupported_pointer_never_writes() {
    let mut pointer = Pointer::new(false);
    let mut output = Vec::new();
    pointer.update(&mut output, true).unwrap();
    pointer.reset(&mut output).unwrap();
    assert!(output.is_empty());
}
