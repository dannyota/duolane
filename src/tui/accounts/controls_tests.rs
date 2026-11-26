#[cfg(unix)]
mod local {
    use super::super::*;
    use std::sync::{Arc, Mutex};

    fn accounts() -> Accounts {
        let mut accounts = Accounts::new(true);
        accounts.names = vec!["default".into(), "backup".into(), "work".into()];
        accounts.selected = "work".into();
        accounts.focus = 1;
        accounts
            .identities
            .insert("backup".into(), "captured".into());
        accounts
    }

    fn show(accounts: &mut Accounts) -> String {
        crate::tui::tests::buffer_text(&crate::tui::tests::draw(80, 24, |frame| {
            accounts.render_action(frame, frame.area())
        }))
    }

    fn roster(names: &[&str], selected: &str) -> Option<(Vec<String>, String)> {
        Some((
            names.iter().map(|name| (*name).to_owned()).collect(),
            selected.to_owned(),
        ))
    }

    fn never(_: Vec<String>, _: Arc<std::sync::atomic::AtomicBool>) -> Finished {
        panic!("a command ran")
    }

    #[test]
    fn read_only_monitor_ignores_every_action_key() {
        let mut accounts = Accounts::new(false);
        accounts.names = vec!["default".into(), "backup".into()];
        accounts.focus = 1;
        for key in ['a', 'l', 'd', 's'] {
            accounts.action_key(key);
            assert!(!accounts.confirming() && !accounts.typing());
        }
        assert!(accounts.take_handoff().is_none());
    }

    #[test]
    fn name_entry_takes_keys_that_otherwise_drive_the_monitor() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
        let mut app = crate::tui::mouse_tests::app_for_accounts_test(accounts());
        let (shutdown, _receiver) = tokio::sync::oneshot::channel();
        app.shutdown = Some(shutdown);
        let press = |app: &mut crate::tui::MonitorApp, code| {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), &state)
        };
        press(&mut app, KeyCode::Char('a'));
        for c in ['q', '1', 'b', 'r', 'd', '2'] {
            assert!(press(&mut app, KeyCode::Char(c)).is_none());
        }
        assert!(app.usage_open && !app.show_setup);
        assert_eq!(app.accounts.controls.input.as_deref(), Some("q1brd2"));
        press(&mut app, KeyCode::Esc);
        assert!(!app.accounts.typing());
        press(&mut app, KeyCode::Char('d'));
        assert!(app.accounts.confirming());
        press(&mut app, KeyCode::Char('1'));
        assert!(!app.usage_open && !app.accounts.confirming());
        press(&mut app, KeyCode::Char('d'));
        assert!(!app.accounts.confirming());
    }

    #[test]
    fn clicking_a_button_acts_as_its_key_and_a_disabled_button_does_nothing() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
        let mut app = crate::tui::mouse_tests::app_for_accounts_test(accounts());
        let click = |app: &mut crate::tui::MonitorApp, label: &str| {
            let mut targets = Default::default();
            let screen = crate::tui::tests::draw(120, 30, |frame| {
                targets = crate::tui::render(frame, app, &state);
            });
            let (column, row) = crate::tui::tests::placeholder_position(&screen, label).unwrap();
            app.handle_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: column + 1,
                    row,
                    modifiers: KeyModifiers::NONE,
                },
                &state,
                &targets,
            );
        };
        click(&mut app, "[D Remove]");
        assert!(app.accounts.confirming());
        assert_eq!(
            app.accounts
                .controls
                .pending
                .as_ref()
                .map(|action| action.kind),
            Some(Kind::Remove)
        );
        click(&mut app, "[L Sign in]");
        assert_eq!(
            app.accounts
                .controls
                .pending
                .as_ref()
                .map(|action| action.kind),
            Some(Kind::Remove)
        );
        app.accounts.cancel();
        app.accounts.focus = 0;
        click(&mut app, "[D Remove]");
        assert!(!app.accounts.confirming());
        click(&mut app, "[A Add]");
        assert!(app.accounts.typing());
    }

    #[test]
    fn actions_refuse_default_the_saved_profile_and_an_overridden_selection() {
        let mut accounts = accounts();
        accounts.focus = 0;
        for key in ['l', 'd'] {
            accounts.action_key(key);
            assert!(!accounts.confirming());
            assert_eq!(
                accounts.controls.message().as_deref(),
                Some("default is managed by Claude Code")
            );
        }
        accounts.focus = 2;
        accounts.action_key('s');
        assert!(!accounts.confirming());
        assert_eq!(
            accounts.controls.message().as_deref(),
            Some("work is already selected")
        );
        accounts.focus = 1;
        accounts.overridden = true;
        accounts.action_key('s');
        assert!(!accounts.confirming());
        accounts.focus = 3;
        accounts.action_key('d');
        assert_eq!(
            accounts.controls.message().as_deref(),
            Some("Focus a Claude profile first")
        );
    }

    #[test]
    fn new_profile_name_accepts_only_a_valid_unused_name() {
        let mut accounts = accounts();
        accounts.action_key('a');
        assert!(accounts.typing());
        for c in "-Bad name/../x".chars() {
            accounts.type_key(KeyCode::Char(c));
        }
        assert_eq!(accounts.controls.input.as_deref(), Some("-adnamex"));
        accounts.type_key(KeyCode::Enter);
        assert!(accounts.typing() && !accounts.controls.visible());
        for _ in 0..8 {
            accounts.type_key(KeyCode::Backspace);
        }
        for c in "backup".chars() {
            accounts.type_key(KeyCode::Char(c));
        }
        accounts.type_key(KeyCode::Enter);
        assert!(accounts.typing());
        assert!(show(&mut accounts).contains("backup already exists"));
        for c in "-2".chars().chain("x".repeat(40).chars()) {
            accounts.type_key(KeyCode::Char(c));
        }
        assert_eq!(accounts.controls.input.as_ref().map(String::len), Some(32));
        accounts.type_key(KeyCode::Enter);
        assert!(!accounts.typing() && accounts.confirming());
        let text = show(&mut accounts);
        assert!(text.contains("Add profile and sign in?"), "{text}");
        accounts.cancel();
        assert!(!accounts.confirming());
        accounts.action_key('a');
        accounts.type_key(KeyCode::Esc);
        assert!(!accounts.typing() && !accounts.confirming());
    }

    #[test]
    fn confirmed_action_runs_only_for_the_profile_that_is_still_registered() {
        let mut accounts = accounts();
        accounts.action_key('d');
        accounts.confirm_action_with(roster(&["default", "backup", "work"], "work"), never);
        assert!(accounts.confirming());
        let text = show(&mut accounts);
        assert!(
            text.contains("Sign out and delete profile?") && text.contains("backup"),
            "{text}"
        );
        accounts.confirm_action_with(roster(&["default", "work"], "work"), never);
        assert!(!accounts.confirming());
        assert_eq!(
            accounts.controls.message().as_deref(),
            Some("backup is no longer registered; nothing ran")
        );

        let mut accounts = self::accounts();
        accounts.action_key('s');
        show(&mut accounts);
        accounts.confirm_action_with(roster(&["default", "backup", "work"], "backup"), never);
        assert_eq!(
            accounts.controls.message().as_deref(),
            Some("backup is already selected; nothing ran")
        );
        assert!(accounts.take_handoff().is_none());

        let mut accounts = self::accounts();
        accounts.action_key('l');
        show(&mut accounts);
        accounts.confirm_action_with(None, never);
        assert_eq!(
            accounts.controls.message().as_deref(),
            Some("Could not read the profiles; nothing ran")
        );
    }

    #[test]
    fn shutdown_cancels_scheduled_handoff() {
        let mut app = crate::tui::mouse_tests::app_for_accounts_test(accounts());
        app.accounts.action_key('l');
        show(&mut app.accounts);
        app.accounts
            .confirm_action_with(roster(&["default", "backup", "work"], "work"), never);
        app.begin_shutdown();
        assert!(app.accounts.take_handoff().is_none());
    }

    #[test]
    fn long_unicode_account_does_not_block_confirmation() {
        let mut accounts = accounts();
        accounts.action_key('l');
        accounts.controls.pending.as_mut().unwrap().account = Some("用".repeat(100));
        let text = show(&mut accounts);
        assert!(text.contains("backup"));
        accounts.confirm_action_with(roster(&["default", "backup", "work"], "work"), never);
        assert!(accounts.take_handoff().is_some());
    }

    #[test]
    fn terminal_actions_queue_fixed_arguments_and_removal_runs_captured() {
        for (key, args) in [
            ('l', ["accounts", "login", "backup", "--expect", "captured"]),
            (
                's',
                ["accounts", "switch", "backup", "--expect", "captured"],
            ),
        ] {
            let mut accounts = accounts();
            accounts.action_key(key);
            show(&mut accounts);
            accounts.confirm_action_with(roster(&["default", "backup", "work"], "work"), never);
            let action = accounts.take_handoff().unwrap();
            assert_eq!(action.args(), args);
            assert!(accounts.take_handoff().is_none() && !accounts.confirming());
        }
        let mut accounts = accounts();
        accounts.action_key('d');
        show(&mut accounts);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        accounts.confirm_action_with(
            roster(&["default", "backup", "work"], "work"),
            move |args, _| {
                *record.lock().unwrap() = args;
                Finished {
                    ok: false,
                    summary: "failed with exit status 1: Error: Claude logout failed; profile kept"
                        .into(),
                }
            },
        );
        assert!(accounts.take_handoff().is_none());
        accounts.action_key('d');
        assert!(!accounts.confirming());
        for _ in 0..500 {
            accounts.poll_action();
            if accounts.controls.runner.running().is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "accounts", "remove", "backup", "--yes", "--expect", "captured"
            ]
        );
        assert_eq!(
            accounts.controls.message().as_deref(),
            Some(
                "Remove backup: failed, failed with exit status 1: Error: Claude logout failed; profile kept"
            )
        );
    }
}
