use super::*;
use crate::tui::{self, *};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use std::os::unix::fs::PermissionsExt;

#[test]
#[ignore]
fn confirmation_event_probe() {
    let config = std::path::PathBuf::from(std::env::var_os("DUOLANE_CONFIG_DIR").unwrap());
    let profile = config.join("claude-accounts/backup");
    std::fs::create_dir_all(&profile).unwrap();
    for path in [&config, &config.join("claude-accounts"), &profile] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let marker = profile.join(".duolane-ready");
    std::fs::write(&marker, b"ready\n").unwrap();
    std::fs::set_permissions(marker, std::fs::Permissions::from_mode(0o600)).unwrap();
    let state = crate::monitor::mock_state().into();
    let mut accounts = Accounts::new(true);
    accounts.enter();
    accounts.focus = 1;
    let mut app = MonitorApp {
        accounts,
        usage_open: true,
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
        shutdown_complete: None,
    };
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.handle_key(key(KeyCode::Enter), &state);
    let mut targets = MouseTargets::default();
    let screen = tui::tests::draw(80, 24, |frame| {
        targets = tui::render(frame, &mut app, &state)
    });
    let text = tui::tests::buffer_text(&screen);
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        assert!(!app.accounts.confirming());
        assert!(!text.contains("Select profile?"));
        assert!(text.contains("CLAUDE_CONFIG_DIR overrides selection"));
        println!("PROBE_COMPLETE");
        return;
    }
    assert!(text.contains("Select profile?"));
    assert!(!text.contains("CLAUDE_CONFIG_DIR"));
    for label in ["1 Overview", "?"] {
        let (column, row) = tui::tests::placeholder_position(&screen, label).unwrap();
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
            &state,
            &targets,
        );
    }
    assert!(app.usage_open && !app.show_help && app.accounts.confirming());
    app.handle_key(key(KeyCode::Char('1')), &state);
    assert!(!app.usage_open && !app.accounts.confirming());
    app.handle_key(key(KeyCode::Char('2')), &state);
    app.accounts.focus = 1;
    app.handle_key(key(KeyCode::Enter), &state);
    app.handle_key(key(KeyCode::Esc), &state);
    assert!(!app.accounts.confirming());
    assert_eq!(
        crate::claude_accounts::monitor_roster().unwrap().selected,
        "default"
    );
    app.handle_key(key(KeyCode::Enter), &state);
    tui::tests::draw(80, 24, |frame| tui::render(frame, &mut app, &state));
    app.handle_key_sized(key(KeyCode::Enter), &state, (79, 24), (80, 24));
    assert!(app.accounts.confirming());
    assert_eq!(
        crate::claude_accounts::monitor_roster().unwrap().selected,
        "default"
    );
    app.handle_key_sized(key(KeyCode::Enter), &state, (80, 24), (80, 24));
    assert_eq!(
        crate::claude_accounts::monitor_roster().unwrap().selected,
        "backup"
    );
    println!("PROBE_COMPLETE");
}

#[test]
fn confirmation_keys_and_clicks_work_at_80_by_24() {
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env("HOME", root.path())
        .env("TMPDIR", root.path())
        .args([
            "--exact",
            "--ignored",
            "tui::accounts::event_tests::confirmation_event_probe",
        ])
        .arg("--nocapture")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("PROBE_COMPLETE"));
}
#[test]
fn environment_override_prevents_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env("HOME", root.path())
        .env("TMPDIR", root.path())
        .env("CLAUDE_CONFIG_DIR", root.path().join("override"))
        .args([
            "--exact",
            "--ignored",
            "tui::accounts::event_tests::confirmation_event_probe",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("PROBE_COMPLETE"));
}
