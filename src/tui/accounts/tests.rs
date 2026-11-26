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

#[test]
fn narrow_compact_windows_keep_reset_times_on_their_rows() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let reset = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let mut state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: now,
        primary: Some(CodexQuotaWindow {
            used_percent: Some(42.0),
            window_minutes: Some(300),
            reset_at: Some(reset),
        }),
        secondary: Some(CodexQuotaWindow {
            used_percent: Some(12.0),
            window_minutes: Some(120),
            reset_at: Some(reset + 3600),
        }),
    });
    let mut accounts = Accounts::new(false);
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(40, 7, |f| {
        accounts.render_at(f, f.area(), &state, now, jiff::tz::TimeZone::UTC)
    }));
    assert!(
        text.lines().any(|line| line.contains("5h 42.0% 1h0m")),
        "{text}"
    );
    assert!(
        text.lines().any(|line| line.contains("120m 12.0% 2h0m")),
        "{text}"
    );
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
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(90, 8, |f| {
        accounts.render(f, f.area(), &state)
    }));
    for name in ["abcdefghijkl", "mnopqrstuvwx", "yzabcdefghij"] {
        assert!(text.contains(name), "{text}");
    }
}

#[test]
fn compact_nonstandard_windows_keep_both_relative_resets() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let reset = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let mut state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
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
    let mut accounts = Accounts::new(false);
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(100, 3, |f| {
        accounts.render_at(f, f.area(), &state, now, jiff::tz::TimeZone::UTC)
    }));
    assert!(
        text.contains("120m 42.0% 1h0m") && text.contains("240m 12.0% 2h0m"),
        "{text}"
    );
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
fn compact_single_line_budgets_both_long_windows() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: now - Duration::from_secs(901),
        primary: Some(CodexQuotaWindow {
            used_percent: Some(100.0),
            window_minutes: Some(1440),
            reset_at: Some(1_700_129_600),
        }),
        secondary: Some(CodexQuotaWindow {
            used_percent: Some(100.0),
            window_minutes: Some(2880),
            reset_at: Some(1_700_216_000),
        }),
    });
    let mut app = crate::tui::mouse_tests::app_for_accounts_test(Accounts::new(false));
    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(90, 8, |f| {
        crate::tui::render_at(f, &mut app, &state, now, jiff::tz::TimeZone::UTC);
    }));
    assert!(
        text.contains("1440m 100.0% 1d12h") && text.contains("2880m 100.0% 2d12h"),
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
fn usage_size_sweep_preserves_names_windows_resets_and_staleness() {
    use super::super::usage_limits::{QuotaLayout, quota_layout};
    use crate::claude_accounts::limits::{Limits, Window};
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let seconds = 1_700_000_000;
    let mut accounts = Accounts::new(true);
    accounts.names = vec![
        "abcdefghijkl-profile".into(),
        "mnopqrstuvwx-profile".into(),
        "yzabcdefghij-profile".into(),
    ];
    for (i, name) in accounts.names.iter().enumerate() {
        accounts.limits.insert(
            name.clone(),
            Limits {
                five_hour: Some(Window {
                    used: 0.2,
                    resets_at: (i != 1).then_some(seconds + 3600),
                }),
                seven_day: Some(Window {
                    used: 0.4,
                    resets_at: (i != 1).then_some(seconds + 7200),
                }),
                observed_at: seconds,
            },
        );
    }
    let mut state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
    state.codex_quota = Some(CodexQuotaSnapshot {
        observed_at: now - Duration::from_secs(901),
        primary: Some(CodexQuotaWindow {
            used_percent: Some(42.0),
            window_minutes: Some(120),
            reset_at: Some(seconds + 3600),
        }),
        secondary: Some(CodexQuotaWindow {
            used_percent: Some(12.0),
            window_minutes: Some(240),
            reset_at: Some(seconds + 7200),
        }),
    });
    let mut app = crate::tui::mouse_tests::app_for_accounts_test(accounts);
    for (lengths, resets, percents) in [
        ([120, 240], [3600, 7200], [42.0, 12.0]),
        ([1440, 2880], [129600, 216000], [100.0, 100.0]),
    ] {
        let quota = state.codex_quota.as_mut().unwrap();
        for (i, window) in [
            quota.primary.as_mut().unwrap(),
            quota.secondary.as_mut().unwrap(),
        ]
        .into_iter()
        .enumerate()
        {
            window.window_minutes = Some(lengths[i]);
            window.reset_at = Some(seconds + resets[i]);
            window.used_percent = Some(percents[i]);
        }
        for width in (60..=140).step_by(5) {
            for height in 8..=30 {
                let text =
                    crate::tui::tests::buffer_text(&crate::tui::tests::draw(width, height, |f| {
                        crate::tui::render_at(f, &mut app, &state, now, jiff::tz::TimeZone::UTC);
                    }));
                let lines: Vec<_> = text.lines().collect();
                let rows = app.accounts.rows;
                let names = ["abcdefghijkl", "mnopqrstuvwx", "yzabcdefghij"];
                let count = (usize::from(rows.height) / app.accounts.row_height).min(names.len());
                if count == 0 {
                    assert!(
                        names.iter().all(|name| !text.contains(name)),
                        "{width}x{height}: {text}"
                    );
                }
                {
                    for (i, name) in names.iter().take(count).enumerate() {
                        let index = usize::from(rows.y) + i * app.accounts.row_height;
                        let line = lines[index];
                        assert!(line.contains(name), "{width}x{height}: {text}");
                        if app.accounts.row_height == 1 {
                            let reset = if i == 1 { ["-", "-"] } else { ["1h0m", "2h0m"] };
                            assert!(
                                line.contains(&format!("5h 20% {}", reset[0]))
                                    && line.contains(&format!("week 40% {}", reset[1])),
                                "{width}x{height}: {text}"
                            );
                        } else {
                            let five = lines[index + 1];
                            let week = lines[index + 2];
                            let reset = if i == 1 {
                                ["Unknown", "Unknown"]
                            } else {
                                ["1h0m", "2h0m"]
                            };
                            assert!(
                                five.contains("5h")
                                    && five.contains("20%")
                                    && five.contains(reset[0]),
                                "{width}x{height}: {text}"
                            );
                            assert!(
                                week.contains("week")
                                    && week.contains("40%")
                                    && week.contains(reset[1]),
                                "{width}x{height}: {text}"
                            );
                        }
                    }
                }
                let pane = app.accounts.subscription;
                let inner_width = pane.width.saturating_sub(2);
                let inner_height = pane.height.saturating_sub(2);
                let quota = state.codex_quota.as_ref().unwrap();
                let full = [
                    ratatui::text::Line::from(quota_summary(Some(quota), now)),
                    super::super::usage_limits::codex_in_zone(
                        "Primary",
                        quota.primary.as_ref(),
                        now,
                        jiff::tz::TimeZone::UTC,
                    ),
                    super::super::usage_limits::codex_in_zone(
                        "Secondary",
                        quota.secondary.as_ref(),
                        now,
                        jiff::tz::TimeZone::UTC,
                    ),
                ];
                let layout = quota_layout(
                    super::super::usage_limits::wrapped_height(&full, inner_width),
                    inner_height,
                );
                let start = usize::from(pane.y) + 1;
                let relative = if lengths[0] == 120 {
                    ["1h0m", "2h0m"]
                } else {
                    ["1d12h", "2d12h"]
                };
                assert!(
                    lines[usize::from(pane.y)].contains("Stale"),
                    "{width}x{height}: {text}"
                );
                match layout {
                    QuotaLayout::OneLine | QuotaLayout::TwoLines => {
                        let offset =
                            usize::from(layout == QuotaLayout::TwoLines && inner_height > 2);
                        for i in 0..2 {
                            let line = lines[start
                                + offset
                                + if layout == QuotaLayout::TwoLines {
                                    i
                                } else {
                                    0
                                }];
                            assert!(
                                line.contains(&format!(
                                    "{}m {:.1}% {}",
                                    lengths[i], percents[i], relative[i]
                                )),
                                "{width}x{height}: {text}"
                            );
                            assert!(
                                !line.contains("2023-")
                                    && !line.contains("23:13:20")
                                    && !line.contains("00:13:20"),
                                "{width}x{height}: {text}"
                            );
                        }
                    }
                    QuotaLayout::Full => {
                        let content = lines[start..start + usize::from(inner_height)].join(" ");
                        let mut offset = usize::from(super::super::usage_limits::wrapped_height(
                            &full[..1],
                            inner_width,
                        ));
                        for i in 0..2 {
                            let count = usize::from(super::super::usage_limits::wrapped_height(
                                &full[i + 1..i + 2],
                                inner_width,
                            ));
                            let window = lines[start + offset..start + offset + count].join(" ");
                            let reset = full[i + 1].to_string();
                            let date = reset
                                .split("reset ")
                                .nth(1)
                                .unwrap()
                                .split(" (in ")
                                .next()
                                .unwrap();
                            assert!(
                                window.contains(&format!("{}m", lengths[i]))
                                    && window.contains(&format!("{:.1}%", percents[i]))
                                    && window.contains(date),
                                "{width}x{height}: {text}"
                            );
                            offset += count;
                        }
                        assert!(content.contains("Stale"), "{width}x{height}: {text}");
                    }
                }
            }
        }
    }
}
