mod extra;

use super::*;
use crate::monitor::{CodexQuotaSnapshot, CodexQuotaWindow};
use std::time::{Duration, SystemTime};

#[test]
fn attached_profiles_are_unknown_and_never_load_limits() {
    let mut accounts = Accounts::new(false);
    accounts.enter();
    accounts.refresh_with(|| panic!("read-only monitor loaded limits"));
    let state = crate::monitor::mock_state().into();
    let buffer = crate::tui::tests::draw(100, 20, |frame| {
        accounts.render(frame, frame.area(), &state)
    });
    let text = crate::tui::tests::buffer_text(&buffer);
    assert!(text.contains("Claude profiles: Unknown"));
    assert!(!text.contains("Claude example"));
}

#[cfg(unix)]
#[test]
fn compact_profiles_reserve_names_with_missing_resets() {
    let mut accounts = Accounts::new(true);
    accounts.names = vec![
        "abcdefghijkl-one".into(),
        "mnopqrstuvwx-two".into(),
        "yzabcdefghij-three".into(),
    ];
    for name in &accounts.names {
        accounts.limits.insert(
            name.clone(),
            crate::claude_accounts::limits::Limits {
                five_hour: Some(crate::claude_accounts::limits::Window {
                    used: 0.2,
                    resets_at: None,
                }),
                seven_day: Some(crate::claude_accounts::limits::Window {
                    used: 0.4,
                    resets_at: None,
                }),
                observed_at: 1,
            },
        );
    }
    let state = crate::monitor::mock_state().into();
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(90, 13, |f| {
        accounts.render(f, f.area(), &state)
    }));
    for name in ["abcdefghijkl", "mnopqrstuvwx", "yzabcdefghij"] {
        assert!(text.contains(name), "{text}");
    }
}

#[test]
fn compact_two_line_windows_use_relative_resets() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: now - Duration::from_secs(901),
        primary: Some(CodexQuotaWindow {
            used_percent: Some(42.0),
            window_minutes: Some(120),
            reset_at: Some(1_700_003_600),
        }),
        secondary: Some(CodexQuotaWindow {
            used_percent: Some(12.0),
            window_minutes: Some(240),
            reset_at: Some(1_700_007_200),
        }),
    });
    let mut app = crate::tui::mouse_tests::app_for_accounts_test(Accounts::new(false));
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(100, 10, |f| {
        crate::tui::render_at(f, &mut app, &state, now, jiff::tz::TimeZone::UTC);
    }));
    assert!(
        text.contains("120m 42.0% 1h0m") && text.contains("240m 12.0% 2h0m"),
        "{text}"
    );
}

#[test]
fn compact_missing_lengths_preserve_both_passed_resets() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
    let window = CodexQuotaWindow {
        used_percent: Some(100.0),
        window_minutes: None,
        reset_at: Some(1_699_999_999),
    };
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: now,
        primary: Some(window.clone()),
        secondary: Some(window),
    });
    let mut app = crate::tui::mouse_tests::app_for_accounts_test(Accounts::new(false));
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(90, 8, |f| {
        crate::tui::render_at(f, &mut app, &state, now, jiff::tz::TimeZone::UTC);
    }));
    assert!(
        text.contains("P 100.0% passed") && text.contains("S 100.0% passed"),
        "{text}"
    );
}

#[cfg(unix)]
#[test]
fn compact_limits_show_both_windows_for_two_profiles_and_subscription() {
    let mut accounts = Accounts::new(true);
    accounts.names = vec!["default".into(), "backup".into()];
    accounts.selected = "default".into();
    let mut state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
    let limits = crate::claude_accounts::limits::Limits {
        five_hour: Some(crate::claude_accounts::limits::Window {
            used: 0.2,
            resets_at: None,
        }),
        seven_day: Some(crate::claude_accounts::limits::Window {
            used: 0.4,
            resets_at: None,
        }),
        observed_at: 1,
    };
    accounts.limits.insert("default".into(), limits);
    accounts.limits.insert("backup".into(), limits);
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: SystemTime::now(),
        primary: Some(CodexQuotaWindow {
            used_percent: Some(42.0),
            window_minutes: Some(300),
            reset_at: None,
        }),
        secondary: Some(CodexQuotaWindow {
            used_percent: Some(12.0),
            window_minutes: Some(10080),
            reset_at: None,
        }),
    });
    let mut app = crate::tui::mouse_tests::app_for_accounts_test(accounts);
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(80, 24, |f| {
        crate::tui::render(f, &mut app, &state)
    }));
    assert!(
        text.contains("default") && text.contains("backup"),
        "{text}"
    );
    assert_eq!(text.matches("20%").count(), 3, "{text}");
    assert_eq!(text.matches("40%").count(), 3, "{text}");
    assert!(text.contains("42.0%") && text.contains("12.0%"), "{text}");
    assert!(
        text.contains("all accounts: 5H 20% used, 7D 40% used"),
        "{text}"
    );
}

#[test]
fn read_only_monitor_never_checks_account_details() {
    let mut accounts = Accounts::new(false);
    accounts.open();
    accounts.reload();
    assert!(!accounts.details.checking());
    assert_eq!(accounts.details.summary(), "Accounts not checked");
}

#[cfg(unix)]
#[test]
fn full_rows_show_status_age_staleness_account_details_and_total() {
    use crate::claude_accounts::limits::{Limits, Window};
    let now = 1_700_000_000;
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(now);
    let limits = |five, observed_at| Limits {
        five_hour: Some(Window {
            used: five,
            resets_at: Some(now + 1_740),
        }),
        seven_day: Some(Window {
            used: 0.1,
            resets_at: Some(now + 86_400),
        }),
        observed_at,
    };
    let mut accounts = Accounts::new(true);
    accounts.names = vec!["default".into(), "backup".into()];
    accounts.identities = [
        ("default".into(), "default".into()),
        ("backup".into(), "test".into()),
    ]
    .into();
    accounts.selected = "default".into();
    accounts.limits.insert("default".into(), limits(1.0, now));
    accounts
        .limits
        .insert("backup".into(), limits(0.2, now - 7_200));
    accounts.details.request_with(false, |_| {
        Some(crate::claude_accounts::profile_state::Roster::from_status(
            &[
                (
                    "default",
                    r#"{"loggedIn":true,"email":"first@example.com","subscriptionType":"team"}"#,
                ),
                ("backup", r#"{"loggedIn":false}"#),
            ],
        ))
    });
    for _ in 0..500 {
        accounts.details.poll();
        if !accounts.details.checking() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(120, 14, |f| {
        accounts.render_at(f, f.area(), &state, time, jiff::tz::TimeZone::UTC)
    }));
    assert!(text.contains("● Saved: default  ★ Suggested"), "{text}");
    assert!(
        text.contains("●  default  first@example.com  team  ██████ 100% 29m"),
        "{text}"
    );
    assert!(text.contains("limited 29m  now"), "{text}");
    assert!(text.contains("   backup   not signed in"), "{text}");
    assert!(text.contains("available    stale 2h"), "{text}");
    assert_eq!(text.matches('★').count(), 1, "{text}");
    assert!(
        text.contains("[Enter Use]  [L Sign in]  [D Remove]  [S Switch]  [A Add]  [R Refresh]"),
        "{text}"
    );
    assert!(
        text.contains("default · first@example.com (team) · Accounts checked"),
        "{text}"
    );
    assert!(
        text.contains("all accounts: 5H 60% used, 7D 10% used"),
        "{text}"
    );
}

#[cfg(unix)]
#[test]
fn compact_profile_prioritizes_resets_over_bars() {
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let now = 1_700_000_000;
    let mut accounts = Accounts::new(true);
    accounts.names = vec!["default".into(), "backup".into()];
    accounts.limits.insert(
        "default".into(),
        crate::claude_accounts::limits::Limits {
            five_hour: Some(crate::claude_accounts::limits::Window {
                used: 0.2,
                resets_at: Some(now + 5400),
            }),
            seven_day: Some(crate::claude_accounts::limits::Window {
                used: 0.4,
                resets_at: Some(now + 129600),
            }),
            observed_at: now,
        },
    );
    let state = crate::monitor::mock_state().into();
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(100, 8, |f| {
        accounts.render_at(f, f.area(), &state, time, jiff::tz::TimeZone::UTC)
    }));
    let row = text
        .lines()
        .find(|line| line.contains(" default "))
        .unwrap();
    assert!(
        row.contains("20% 1h30m") && row.contains("40% 1d12h"),
        "{text}"
    );
}

#[cfg(unix)]
#[test]
fn compact_profile_with_long_name_keeps_both_windows_visible() {
    let mut accounts = Accounts::new(true);
    accounts.names = vec!["abcdefghijklmnopqrstuvwxyz123456".into(), "backup".into()];
    accounts.selected = accounts.names[0].clone();
    accounts.limits.insert(
        accounts.names[0].clone(),
        crate::claude_accounts::limits::Limits {
            five_hour: Some(crate::claude_accounts::limits::Window {
                used: 0.2,
                resets_at: None,
            }),
            seven_day: Some(crate::claude_accounts::limits::Window {
                used: 0.4,
                resets_at: None,
            }),
            observed_at: 1,
        },
    );
    let state = crate::monitor::mock_state().into();
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(90, 8, |f| {
        accounts.render(f, f.area(), &state)
    }));
    assert!(text.contains("20%") && text.contains("40%"), "{text}");
}

#[cfg(unix)]
#[test]
fn confirmation_enter_requires_current_frame_size() {
    let mut accounts = Accounts::new(true);
    accounts.names = vec!["default".into(), "backup".into()];
    accounts.selected = "default".into();
    accounts.focus = 1;
    let state = crate::monitor::mock_state().into();
    let mut app = crate::tui::mouse_tests::app_for_accounts_test(accounts);
    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    );
    app.handle_key(key, &state);
    crate::tui::tests::draw(80, 24, |f| crate::tui::render(f, &mut app, &state));
    assert!(
        app.handle_key_query(key, &state, (80, 24), || Err(std::io::Error::other("size")))
            .is_ok()
    );
    assert!(app.accounts.confirming());
    assert_eq!(app.accounts.selected, "default");
    app.handle_key_sized(key, &state, (79, 24), (80, 24));
    assert!(app.accounts.confirming());
}

#[cfg(unix)]
#[test]
fn focusing_and_cancelling_does_not_change_saved_profile() {
    let mut accounts = Accounts::new(true);
    accounts.names = vec!["default".into(), "backup".into()];
    accounts.selected = "default".into();
    accounts.down();
    assert_eq!(accounts.selected, "default");
    accounts.confirm();
    assert!(accounts.confirming());
    accounts.cancel();
    assert!(!accounts.confirming());
    assert_eq!(accounts.selected, "default");
}

#[test]
fn clicks_only_focus_profile_rows_or_subscription() {
    let mut accounts = Accounts::new(true);
    accounts.names = vec!["default".into()];
    let state = crate::monitor::mock_state().into();
    crate::tui::tests::draw(100, 20, |frame| {
        accounts.render(frame, frame.area(), &state)
    });
    accounts.click(accounts.rows.x, accounts.rows.y + 3);
    assert_eq!(accounts.focus, 0);
    accounts.click(accounts.subscription.x, accounts.subscription.y);
    assert_eq!(accounts.focus, 1);
    accounts.click(accounts.rows.x, accounts.rows.y);
    assert_eq!(accounts.focus, 0);
}

#[cfg(unix)]
#[test]
fn selected_profile_remains_visible_when_list_scrolls() {
    let mut accounts = Accounts::new(true);
    accounts.names = (0..12).map(|n| format!("profile-{n}")).collect();
    accounts.selected = "profile-0".into();
    accounts.focus = 11;
    let state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
    let buffer = crate::tui::tests::draw(100, 20, |frame| {
        accounts.render(frame, frame.area(), &state)
    });
    let text = crate::tui::tests::buffer_text(&buffer);
    assert!(text.contains("Saved: profile-0"));
    accounts.focus = 0;
    let buffer = crate::tui::tests::draw(100, 20, |frame| {
        accounts.render(frame, frame.area(), &state)
    });
    let text = crate::tui::tests::buffer_text(&buffer);
    assert!(text.contains("●  profile-0 "), "{text}");
}

#[cfg(unix)]
#[test]
fn override_is_persistent_on_chatgpt_and_not_effective_selection() {
    let mut accounts = Accounts::new(true);
    accounts.names = vec!["default".into(), "backup".into()];
    accounts.selected = "backup".into();
    accounts.overridden = true;
    accounts.focus = 0;
    let state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
    let mut app = crate::tui::mouse_tests::app_for_accounts_test(accounts);
    app.handle_key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
        &state,
    );
    assert!(!app.accounts.confirming());
    app.accounts.focus = app.accounts.names.len();
    let buffer =
        crate::tui::tests::draw(100, 20, |frame| crate::tui::render(frame, &mut app, &state));
    let text = crate::tui::tests::buffer_text(&buffer);
    assert!(text.contains("Saved: backup"));
    assert!(text.contains("CLAUDE_CONFIG_DIR overrides selection"));
    assert!(!text.contains("Next launch"));
}

#[test]
fn future_quota_hides_bars_and_valid_nonstandard_window_shows_length() {
    let mut accounts = Accounts::new(false);
    let mut state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: SystemTime::now() + Duration::from_secs(3600),
        primary: Some(CodexQuotaWindow {
            used_percent: Some(42.0),
            window_minutes: Some(120),
            reset_at: None,
        }),
        secondary: None,
    });
    let buffer = crate::tui::tests::draw(100, 20, |frame| {
        accounts.render(frame, frame.area(), &state)
    });
    let text = crate::tui::tests::buffer_text(&buffer);
    assert!(text.contains("Last reported: Unknown"));
    assert!(!text.contains("42.0%"));
    state.codex_quota.as_mut().unwrap().observed_at = SystemTime::now();
    let buffer = crate::tui::tests::draw(100, 20, |frame| {
        accounts.render(frame, frame.area(), &state)
    });
    let text = crate::tui::tests::buffer_text(&buffer);
    assert!(text.contains("Primary (120m)"));
    assert!(text.contains("42.0%"));
    let quota = state.codex_quota.as_mut().unwrap();
    quota.observed_at = SystemTime::now() - Duration::from_secs(900);
    quota.primary.as_mut().unwrap().window_minutes = Some(300);
    quota.secondary = Some(CodexQuotaWindow {
        used_percent: Some(12.0),
        window_minutes: Some(10080),
        reset_at: None,
    });
    let screen = crate::tui::tests::draw(100, 20, |frame| {
        accounts.render(frame, frame.area(), &state)
    });
    let text = crate::tui::tests::buffer_text(&screen);
    assert!(text.contains("Stale") && text.contains("5h") && text.contains("week"));
    let quota = state.codex_quota.as_mut().unwrap();
    quota.primary.as_mut().unwrap().window_minutes = Some(525_600_001);
    quota.primary.as_mut().unwrap().used_percent = Some(f64::NAN);
    quota.primary.as_mut().unwrap().reset_at = Some(0);
    quota.observed_at = SystemTime::now();
    let screen = crate::tui::tests::draw(100, 20, |frame| {
        accounts.render(frame, frame.area(), &state)
    });
    let text = crate::tui::tests::buffer_text(&screen);
    assert!(!text.contains("525600001") && !text.contains("Stale"));
    assert!(text.contains("Unknown"));
}
