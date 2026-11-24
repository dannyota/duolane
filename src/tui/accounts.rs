pub(super) struct Accounts {
    local: bool,
    names: Vec<String>,
    selected: String,
    focus: usize,
    overridden: bool,
    pending: Option<(String, String)>,
    confirmation_visible: bool,
    status: String,
    rows: ratatui::layout::Rect,
    first: usize,
}

impl Accounts {
    pub(super) fn new(local: bool) -> Self {
        Self {
            local: local && cfg!(unix),
            names: Vec::new(),
            selected: String::new(),
            focus: 0,
            overridden: false,
            pending: None,
            confirmation_visible: false,
            status: String::new(),
            rows: ratatui::layout::Rect::default(),
            first: 0,
        }
    }

    pub(super) fn enter(&mut self) {
        self.cancel();
        if !self.local {
            return;
        }
        #[cfg(unix)]
        {
            self.overridden =
                std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|value| !value.is_empty());
        }
        #[cfg(unix)]
        match crate::claude_accounts::monitor_roster() {
            Ok(roster) => {
                self.names = roster.names;
                self.selected = roster.selected;
                self.overridden = roster.overridden;
                self.focus = self.focus.min(self.names.len().saturating_sub(1));
                self.status.clear();
            }
            Err(_) => {
                self.names.clear();
                self.selected.clear();
                self.focus = 0;
                self.status = "Unable to load local profiles".into();
            }
        }
    }

    #[cfg(test)]
    pub(super) fn names(&self) -> &[String] {
        &self.names
    }
    pub(super) fn confirming(&self) -> bool {
        self.pending.is_some()
    }
    pub(super) fn cancel(&mut self) {
        self.pending = None;
        self.confirmation_visible = false;
    }
    pub(super) fn down(&mut self) {
        self.focus = self.focus.saturating_add(1).min(self.names.len());
    }
    pub(super) fn up(&mut self) {
        self.focus = self.focus.saturating_sub(1);
    }
    pub(super) fn confirm(&mut self) {
        self.confirm_with(|name, expected| {
            #[cfg(unix)]
            {
                crate::claude_accounts::save_selection(name, Some(expected)).map_err(|_| ())
            }
            #[cfg(not(unix))]
            {
                let _ = (name, expected);
                Err(())
            }
        });
    }

    fn confirm_with(&mut self, save: impl FnOnce(&str, &str) -> Result<(), ()>) {
        if self.pending.is_some() && !self.confirmation_visible {
            return;
        }
        if let Some((name, expected)) = self.pending.take() {
            self.confirmation_visible = false;
            if save(&name, &expected).is_ok() {
                self.selected = name;
                self.status = "Saved for next launch".into();
            } else {
                self.status = "Selection not saved; reload and retry".into();
            }
        } else if self.local
            && !self.overridden
            && let Some(name) = self
                .names
                .get(self.focus)
                .filter(|name| **name != self.selected)
        {
            self.confirmation_visible = false;
            self.pending = Some((name.clone(), self.selected.clone()));
        }
    }
}

fn quota_text(
    quota: Option<&crate::monitor::CodexQuotaSnapshot>,
    now: std::time::SystemTime,
) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let Some(quota) = quota else {
        return "Last reported: Unknown".into();
    };
    let Ok(age) = now.duration_since(quota.observed_at) else {
        return "Last reported: Unknown".into();
    };
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_secs());
    let stale = age >= Duration::from_secs(900)
        || [quota.primary.as_ref(), quota.secondary.as_ref()]
            .into_iter()
            .flatten()
            .any(|window| {
                window
                    .reset_at
                    .filter(|v| (1..=253_402_300_799).contains(v))
                    .zip(now_secs)
                    .is_some_and(|(reset, now)| reset <= now)
            });
    let mut lines = vec![format!(
        "Last reported: {} ago{}",
        super::cells::format_duration(age),
        if stale { "  Stale" } else { "" }
    )];
    for (title, window) in [
        ("Primary", quota.primary.as_ref()),
        ("Secondary", quota.secondary.as_ref()),
    ] {
        let Some(window) = window else {
            lines.push(format!("{title}: Unknown"));
            continue;
        };
        let length = window
            .window_minutes
            .filter(|v| (1..=525_600_000).contains(v))
            .map(|v| format!("{v} min"))
            .unwrap_or_else(|| "Unknown".into());
        let used = window
            .used_percent
            .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
            .map(|v| {
                let filled = (v / 10.0).round() as usize;
                format!(
                    "{v:.1}% used [{}{}]",
                    "█".repeat(filled),
                    "░".repeat(10 - filled)
                )
            })
            .unwrap_or_else(|| "Unknown used".into());
        let reset = window
            .reset_at
            .filter(|v| (1..=253_402_300_799).contains(v))
            .and_then(|v| UNIX_EPOCH.checked_add(Duration::from_secs(v)))
            .and_then(|time| {
                let text = super::cells::format_system_time(time);
                (text != "-").then(|| match time.duration_since(now) {
                    Ok(left) => format!("{text} (in {})", super::cells::format_duration(left)),
                    Err(_) => format!("{text} (passed)"),
                })
            })
            .unwrap_or_else(|| "Unknown".into());
        lines.push(format!("{title}  {length}  {used}"));
        lines.push(format!("Reset: {reset}"));
    }
    lines.join("\n")
}

impl Accounts {
    pub(super) fn click(&mut self, x: u16, y: u16) {
        if self.pending.is_some()
            || x < self.rows.x
            || x >= self.rows.right()
            || y < self.rows.y
            || y >= self.rows.bottom()
        {
            return;
        }
        let index = self.first + usize::from(y - self.rows.y);
        if index <= self.names.len() {
            self.focus = index;
        }
    }

    pub(super) fn render(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        snapshot: &crate::monitor::snapshot::MonitorSnapshot,
    ) {
        use ratatui::{
            layout::{Constraint, Direction, Layout},
            style::{Modifier, Style},
            text::Line,
            widgets::{Block, Borders, Paragraph},
        };
        let direction = if area.width >= 90 {
            Direction::Horizontal
        } else {
            Direction::Vertical
        };
        let constraints = if direction == Direction::Horizontal {
            [Constraint::Percentage(35), Constraint::Percentage(65)]
        } else {
            [Constraint::Percentage(40), Constraint::Percentage(60)]
        };
        let panes = Layout::default()
            .direction(direction)
            .constraints(constraints)
            .split(area);
        let block = Block::default()
            .title(" Accounts ")
            .borders(Borders::ALL)
            .style(Style::default().fg(super::SEPARATOR));
        self.rows = block.inner(panes[0]);
        frame.render_widget(block, panes[0]);
        let count = usize::from(self.rows.height);
        self.first = self.focus.saturating_add(1).saturating_sub(count);
        let mut lines: Vec<Line<'_>> = self
            .names
            .iter()
            .map(|name| {
                Line::from(format!(
                    "Claude {name}{}",
                    if *name == self.selected {
                        "  Saved"
                    } else {
                        ""
                    }
                ))
            })
            .collect();
        lines.push(Line::from("ChatGPT subscription"));
        let visible: Vec<Line<'_>> = lines
            .into_iter()
            .enumerate()
            .skip(self.first)
            .take(count)
            .map(|(index, line)| {
                if index == self.focus {
                    line.style(
                        Style::default()
                            .fg(super::WHITE)
                            .bg(super::SELECTED_BG)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    line.style(Style::default().fg(super::DIM_WHITE))
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(visible), self.rows);
        let detail_block = Block::default()
            .title(" Details ")
            .borders(Borders::ALL)
            .style(Style::default().fg(super::DIM_WHITE));
        let inner = detail_block.inner(panes[1]);
        self.confirmation_visible = false;
        let saved = format!("Saved: {}", self.selected);
        let detail = if let Some((name, _)) = &self.pending {
            let prompt = "Select profile?";
            let actions = "Enter save  Esc cancel";
            self.confirmation_visible = name.is_ascii()
                && usize::from(inner.width)
                    >= saved
                        .len()
                        .max(prompt.len())
                        .max(name.len())
                        .max(actions.len())
                && inner.height >= 4;
            if self.confirmation_visible {
                format!("{saved}\n{prompt}\n{name}\n{actions}")
            } else {
                format!("{saved}\nResize to confirm")
            }
        } else if let Some(name) = self.names.get(self.focus) {
            format!(
                "Claude {name}\nLimits: Unknown\nLogin status: Unknown\n{}\n{}",
                if self.overridden {
                    "Selection disabled"
                } else if *name == self.selected {
                    "Selected for next launch"
                } else {
                    "Enter selects for the next launch"
                },
                self.status
            )
        } else {
            format!(
                "ChatGPT subscription\n{}\n{}",
                quota_text(snapshot.codex_quota.as_ref(), std::time::SystemTime::now()),
                if self.local {
                    self.status.as_str()
                } else {
                    "Read-only monitor"
                }
            )
        };
        let detail = if self.local && self.pending.is_none() {
            let saved = if self.selected.is_empty() {
                "Saved: Unknown".to_owned()
            } else {
                saved
            };
            if self.overridden {
                format!("{saved}\nCLAUDE_CONFIG_DIR overrides selection\n{detail}")
            } else {
                format!("{saved}\n{detail}")
            }
        } else {
            detail
        };
        let detail = Paragraph::new(detail)
            .block(detail_block)
            .wrap(ratatui::widgets::Wrap { trim: false });
        frame.render_widget(detail, panes[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{CodexQuotaSnapshot, CodexQuotaWindow};
    use std::time::{Duration, SystemTime};

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
    fn short_account_screen_renders_quota_and_unknown_safely() {
        let mut accounts = Accounts::new(false);
        let mut state: crate::monitor::snapshot::MonitorSnapshot =
            crate::monitor::mock_state().into();
        state.codex_quota = Some(CodexQuotaSnapshot {
            observed_at: SystemTime::now() - Duration::from_secs(900),
            primary: Some(CodexQuotaWindow {
                used_percent: Some(42.0),
                window_minutes: Some(300),
                reset_at: None,
            }),
            secondary: None,
        });
        let buffer =
            crate::tui::tests::draw(80, 20, |frame| accounts.render(frame, frame.area(), &state));
        let text = crate::tui::tests::buffer_text(&buffer);
        assert!(text.contains("Stale"));
        assert!(text.contains("42.0%"));
        assert!(text.contains("Unknown"));
    }

    #[test]
    fn invalid_snapshot_bounds_are_unknown_and_do_not_make_report_stale() {
        let now = SystemTime::now();
        let quota = CodexQuotaSnapshot {
            observed_at: now,
            primary: Some(CodexQuotaWindow {
                used_percent: None,
                window_minutes: Some(525_600_001),
                reset_at: Some(0),
            }),
            secondary: None,
        };
        let text = quota_text(Some(&quota), now);
        assert!(!text.contains("Stale"));
        assert!(!text.contains("525600001"));
        assert!(text.contains("Reset: Unknown"));
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
        assert!(text.contains("Selected for next launch"));
    }

    #[cfg(unix)]
    #[test]
    fn confirmation_requires_full_prompt_after_resize() {
        let mut accounts = Accounts::new(true);
        accounts.names = vec!["default".into(), "backup".into()];
        accounts.selected = "default".into();
        accounts.focus = 1;
        let state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
        accounts.confirm();
        let mut called = false;
        accounts.confirm_with(|_, _| {
            called = true;
            Ok(())
        });
        assert!(!called);
        let full = crate::tui::tests::draw(100, 20, |frame| {
            accounts.render(frame, frame.area(), &state)
        });
        let full_text = crate::tui::tests::buffer_text(&full);
        assert!(full_text.contains("backup"));
        assert!(full_text.contains("Enter save  Esc cancel"));
        let screen =
            crate::tui::tests::draw(20, 4, |frame| accounts.render(frame, frame.area(), &state));
        assert!(!crate::tui::tests::buffer_text(&screen).contains("Enter save"));
        accounts.confirm_with(|_, _| {
            called = true;
            Ok(())
        });
        assert!(!called);
        assert!(accounts.confirming());
        assert_eq!(accounts.selected, "default");
        crate::tui::tests::draw(100, 20, |frame| {
            accounts.render(frame, frame.area(), &state)
        });
        accounts.confirm_with(|_, _| {
            called = true;
            Ok(())
        });
        assert!(called);
        assert_eq!(accounts.selected, "backup");
    }

    #[cfg(unix)]
    #[test]
    fn override_is_persistent_on_chatgpt_and_not_effective_selection() {
        let mut accounts = Accounts::new(true);
        accounts.names = vec!["default".into(), "backup".into()];
        accounts.selected = "backup".into();
        accounts.overridden = true;
        accounts.focus = accounts.names.len();
        let state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
        let buffer = crate::tui::tests::draw(100, 20, |frame| {
            accounts.render(frame, frame.area(), &state)
        });
        let text = crate::tui::tests::buffer_text(&buffer);
        assert!(text.contains("Saved: backup"));
        assert!(text.contains("CLAUDE_CONFIG_DIR overrides selection"));
        assert!(!text.contains("Next launch"));
    }

    #[test]
    fn reported_limits_mark_stale_and_unknown() {
        let now = SystemTime::now();
        let quota = CodexQuotaSnapshot {
            observed_at: now - Duration::from_secs(900),
            primary: Some(CodexQuotaWindow {
                used_percent: Some(42.0),
                window_minutes: Some(300),
                reset_at: None,
            }),
            secondary: Some(CodexQuotaWindow {
                used_percent: Some(f64::NAN),
                window_minutes: None,
                reset_at: None,
            }),
        };
        let text = quota_text(Some(&quota), now);
        assert!(text.contains("Stale"));
        assert!(text.contains("42"));
        assert!(text.contains("Unknown"));
    }
}
