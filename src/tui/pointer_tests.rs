use super::pointer::{Pointer, supported};
use std::collections::HashMap;

#[test]
fn detects_supported_terminals_and_rejects_multiplexers() {
    for (key, value) in [
        ("TERM_PROGRAM", "ghostty"),
        ("TERM_PROGRAM", "WezTerm"),
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
    app.active_resize.pointer = Pointer::new(true);
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
        app.update_pointer(&mut output, event, &targets).unwrap();
    }
    app.active_resize.pointer.reset(&mut output).unwrap();
    assert_eq!(output, b"\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\\x1b]22;ns-resize\x1b\\\x1b]22;default\x1b\\");
}

#[test]
fn unsupported_pointer_never_writes() {
    let mut pointer = Pointer::new(false);
    let mut output = Vec::new();
    pointer.update(&mut output, true).unwrap();
    pointer.reset(&mut output).unwrap();
    assert!(output.is_empty());
}
