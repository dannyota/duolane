pub(super) fn needed_height(
    area: ratatui::layout::Rect,
    snapshot: &crate::monitor::snapshot::MonitorSnapshot,
    count: usize,
) -> u16 {
    let claude = 4_u16.saturating_add((count.min(u16::MAX as usize / 3) * 3) as u16);
    let width = if area.width >= 90 {
        area.width / 2
    } else {
        area.width
    }
    .saturating_sub(2);
    let now = std::time::SystemTime::now();
    let quota = snapshot
        .codex_quota
        .as_ref()
        .filter(|q| now.duration_since(q.observed_at).is_ok());
    let lines = vec![
        ratatui::text::Line::from(quota_summary(quota, now)),
        super::usage_limits::codex("Primary", quota.and_then(|q| q.primary.as_ref()), now),
        super::usage_limits::codex("Secondary", quota.and_then(|q| q.secondary.as_ref()), now),
    ];
    let chatgpt = super::usage_limits::wrapped_height(&lines, width).saturating_add(2);
    if area.width >= 90 {
        claude.max(chatgpt)
    } else {
        claude.saturating_add(chatgpt)
    }
}

pub(super) struct Accounts {
    local: bool,
    limits: std::collections::BTreeMap<String, crate::claude_accounts::limits::Limits>,
    refreshed: Option<std::time::Instant>,
    names: Vec<String>,
    selected: String,
    focus: usize,
    overridden: bool,
    pending: Option<(String, String)>,
    confirmation_visible: bool,
    status: String,
    rows: ratatui::layout::Rect,
    subscription: ratatui::layout::Rect,
    first: usize,
    row_height: usize,
}

impl Accounts {
    pub(super) fn new(local: bool) -> Self {
        Self {
            local: local && cfg!(unix),
            limits: Default::default(),
            refreshed: None,
            names: Vec::new(),
            selected: String::new(),
            focus: 0,
            overridden: false,
            pending: None,
            confirmation_visible: false,
            status: String::new(),
            rows: ratatui::layout::Rect::default(),
            subscription: ratatui::layout::Rect::default(),
            first: 0,
            row_height: 3,
        }
    }

    pub(super) fn count(&self) -> usize {
        self.names.len()
    }

    pub(super) fn refresh_limits(&mut self) {
        self.refresh_with(crate::claude_accounts::limits::load);
    }

    fn refresh_with(
        &mut self,
        load: impl FnOnce()
            -> std::collections::BTreeMap<String, crate::claude_accounts::limits::Limits>,
    ) {
        if self.local {
            self.limits = load();
            self.refreshed = Some(std::time::Instant::now());
        }
    }

    pub(super) fn refresh_if_due(&mut self) {
        if self.local
            && self
                .refreshed
                .is_none_or(|time| time.elapsed().as_secs() >= 30)
        {
            self.refresh_limits();
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
    pub(super) fn confirmation_visible(&self) -> bool {
        self.pending.is_some() && self.confirmation_visible
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

fn quota_summary(
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
    let now_secs = now.duration_since(UNIX_EPOCH).ok().map(|v| v.as_secs());
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
    format!(
        "Last reported: {} ago{}",
        super::cells::format_duration(age),
        if stale { "  Stale" } else { "" }
    )
}

impl Accounts {
    pub(super) fn render_confirmation(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
    ) {
        use ratatui::{
            layout::{Alignment, Rect},
            widgets::{Block, Borders, Clear, Paragraph},
        };
        self.confirmation_visible = false;
        let Some((name, _)) = &self.pending else {
            return;
        };
        let width = 44.min(area.width);
        let height = 5.min(area.height);
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        self.confirmation_visible = name.is_ascii()
            && usize::from(width.saturating_sub(2)) >= name.len().max(22)
            && height >= 5;
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(format!("Select profile?\n{name}\nEnter save  Esc cancel"))
                .alignment(Alignment::Center)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .style(ratatui::style::Style::default().bg(super::BG)),
                ),
            popup,
        );
    }
    pub(super) fn click(&mut self, x: u16, y: u16) {
        if self.pending.is_none()
            && self
                .subscription
                .contains(ratatui::layout::Position::new(x, y))
        {
            self.focus = self.names.len();
            return;
        }
        if self.pending.is_some()
            || x < self.rows.x
            || x >= self.rows.right()
            || y < self.rows.y
            || y >= self.rows.bottom()
        {
            return;
        }
        let index = self.first + usize::from(y - self.rows.y) / self.row_height;
        if index < self.names.len() {
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
        let claude_height = (3_u16
            .saturating_add((self.names.len().min(u16::MAX as usize / 3) * 3) as u16)
            .saturating_add(u16::from(self.overridden)))
        .min(area.height.saturating_sub(5));
        let constraints = if direction == Direction::Vertical {
            [Constraint::Length(claude_height), Constraint::Min(0)]
        } else {
            [Constraint::Percentage(50), Constraint::Percentage(50)]
        };
        let panes = Layout::default()
            .direction(direction)
            .constraints(constraints)
            .split(area);
        let block = Block::default()
            .title(" Claude profiles ")
            .borders(Borders::ALL)
            .style(Style::default().fg(super::SEPARATOR));
        self.rows = block.inner(panes[0]);
        frame.render_widget(block, panes[0]);
        if self.local {
            let info = ratatui::layout::Rect::new(
                self.rows.x,
                self.rows.y,
                self.rows.width,
                self.rows.height.min(1 + u16::from(self.overridden)),
            );
            let saved = if self.selected.is_empty() {
                "Unknown"
            } else {
                &self.selected
            };
            let mut info_lines = vec![Line::from(format!("Saved: {saved}"))];
            if self.overridden {
                info_lines.push(Line::from("CLAUDE_CONFIG_DIR overrides selection"));
            }
            frame.render_widget(Paragraph::new(info_lines), info);
            self.rows.y += info.height;
            self.rows.height -= info.height;
        }
        self.row_height = if usize::from(self.rows.height) >= self.names.len() * 3 {
            3
        } else {
            1
        };
        let count = usize::from(self.rows.height) / self.row_height;
        self.first = self
            .focus
            .min(self.names.len().saturating_sub(1))
            .saturating_add(1)
            .saturating_sub(count.max(1));
        let now = std::time::SystemTime::now();
        let seconds = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut lines: Vec<Line<'_>> = Vec::new();
        if !self.local && self.names.is_empty() {
            lines.push(Line::from("Claude profiles: Unknown"));
        }
        for (index, name) in self.names.iter().enumerate().skip(self.first).take(count) {
            let style = if index == self.focus {
                Style::default()
                    .fg(super::WHITE)
                    .bg(super::SELECTED_BG)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(super::DIM_WHITE)
            };
            let title = format!(
                "Claude {name}{}",
                if *name == self.selected { " Saved" } else { "" }
            );
            let limits = self.limits.get(name);
            let five = super::usage_limits::claude("5h", limits, 18_000, seconds);
            let week = super::usage_limits::claude("week", limits, 604_800, seconds);
            if self.row_height == 3 {
                lines.push(Line::from(title).style(style));
                lines.push(five);
                lines.push(week);
            } else {
                let suffix = if *name == self.selected { " Saved" } else { "" };
                let width =
                    usize::from(self.rows.width).saturating_sub(7 + suffix.len() + name.len());
                let windows = super::usage_limits::compact_pair(five, week, width);
                let available = usize::from(self.rows.width)
                    .saturating_sub(Line::from(windows.clone()).width() + 7 + suffix.len());
                let title = format!(
                    "Claude {}{suffix}",
                    super::cells::ellipsize(name, available)
                );
                let mut spans = vec![ratatui::text::Span::styled(title, style)];
                spans.extend(windows);
                lines.push(Line::from(spans));
            }
        }
        frame.render_widget(Paragraph::new(lines), self.rows);
        let quota = snapshot
            .codex_quota
            .as_ref()
            .filter(|q| now.duration_since(q.observed_at).is_ok());
        let title = if quota_summary(quota, now).contains("Stale") {
            " Stale ChatGPT subscription "
        } else {
            " ChatGPT subscription "
        };
        let detail_block = Block::default()
            .title(title)
            .title_style(if self.focus == self.names.len() {
                Style::default()
                    .fg(super::WHITE)
                    .bg(super::SELECTED_BG)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(super::DIM_WHITE)
            })
            .borders(Borders::ALL)
            .style(Style::default().fg(super::DIM_WHITE));
        self.subscription = panes[1];
        let inner = detail_block.inner(panes[1]);
        frame.render_widget(detail_block, panes[1]);
        self.confirmation_visible = false;
        let saved = format!(
            "Saved: {}",
            if self.selected.is_empty() {
                "Unknown"
            } else {
                &self.selected
            }
        );
        let mut detail: Vec<Line<'_>> = Vec::new();
        {
            let quota = snapshot
                .codex_quota
                .as_ref()
                .filter(|q| now.duration_since(q.observed_at).is_ok());
            detail.push(Line::from(quota_summary(quota, now)));
            let primary =
                super::usage_limits::codex("Primary", quota.and_then(|q| q.primary.as_ref()), now);
            let secondary = super::usage_limits::codex(
                "Secondary",
                quota.and_then(|q| q.secondary.as_ref()),
                now,
            );
            if super::usage_limits::wrapped_height(
                &[detail[0].clone(), primary.clone(), secondary.clone()],
                inner.width,
            ) <= inner.height
            {
                detail.push(primary);
                detail.push(secondary);
            } else if inner.height >= 2 {
                if inner.height == 2 {
                    detail.clear();
                }
                detail.push(Line::from(super::usage_limits::compact_with_reset(
                    primary,
                    inner.width,
                )));
                detail.push(Line::from(super::usage_limits::compact_with_reset(
                    secondary,
                    inner.width,
                )));
            } else {
                if inner.height < 2 {
                    detail.clear();
                }
                detail.push(Line::from(super::usage_limits::compact_pair(
                    primary, secondary, 0,
                )));
            }
            if self.local {
                detail.push(Line::from(saved));
                if self.overridden {
                    detail.push(Line::from("CLAUDE_CONFIG_DIR overrides selection"));
                } else if self.names.get(self.focus) == Some(&self.selected) {
                    detail.push(Line::from("Selected for next launch"));
                }
                if !self.status.is_empty() {
                    detail.push(Line::from(self.status.clone()));
                }
            } else {
                detail.push(Line::from("Read-only monitor"));
            }
        }
        frame.render_widget(
            Paragraph::new(detail).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
    }
}

#[cfg(test)]
mod tests {
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
        let now = SystemTime::now();
        let reset = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let mut state: crate::monitor::snapshot::MonitorSnapshot =
            crate::monitor::mock_state().into();
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
            accounts.render(f, f.area(), &state)
        }));
        let second = super::super::cells::format_system_time(
            SystemTime::UNIX_EPOCH + Duration::from_secs(reset + 3600),
        );
        assert!(text.contains(&second), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn compact_limits_show_both_windows_for_two_profiles_and_subscription() {
        let mut accounts = Accounts::new(true);
        accounts.names = vec!["default".into(), "backup".into()];
        accounts.selected = "default".into();
        let mut state: crate::monitor::snapshot::MonitorSnapshot =
            crate::monitor::mock_state().into();
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
        assert_eq!(text.matches("20%").count(), 2, "{text}");
        assert_eq!(text.matches("40%").count(), 2, "{text}");
        assert!(text.contains("42.0%") && text.contains("12.0%"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn compact_profile_prioritizes_resets_over_bars() {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
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
            accounts.render(f, f.area(), &state)
        }));
        let row = text.lines().find(|line| line.contains("default")).unwrap();
        assert!(
            row.contains("20%") && row.contains("40%") && row.contains("1h") && row.contains("1d"),
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
    fn confirmation_overlay_fits_short_terminal() {
        let mut accounts = Accounts::new(true);
        accounts.names = vec!["default".into(), "backup".into()];
        accounts.selected = "default".into();
        accounts.focus = 1;
        accounts.confirm();
        let state = crate::monitor::mock_state().into();
        let mut app = crate::tui::MonitorApp {
            accounts,
            usage_open: true,
            listen_url: String::new(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: crate::tui::FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            tick: 0,
            phase: crate::tui::MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: None,
        };
        let screen =
            crate::tui::tests::draw(80, 24, |frame| crate::tui::render(frame, &mut app, &state));
        let text = crate::tui::tests::buffer_text(&screen);
        assert!(text.contains("Enter save  Esc cancel"));
        assert!(text.contains("Saved: default"));
        app.accounts.confirm_with(|name, expected| {
            assert_eq!((name, expected), ("backup", "default"));
            Ok(())
        });
        assert_eq!(app.accounts.selected, "backup");
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
            accounts.render(frame, frame.area(), &state);
            accounts.render_confirmation(frame, frame.area());
        });
        let full_text = crate::tui::tests::buffer_text(&full);
        assert!(full_text.contains("backup"));
        assert!(full_text.contains("Enter save  Esc cancel"));
        let screen = crate::tui::tests::draw(20, 4, |frame| {
            accounts.render(frame, frame.area(), &state);
            accounts.render_confirmation(frame, frame.area());
        });
        assert!(!crate::tui::tests::buffer_text(&screen).contains("Enter save"));
        accounts.confirm_with(|_, _| {
            called = true;
            Ok(())
        });
        assert!(!called);
        assert!(accounts.confirming());
        assert_eq!(accounts.selected, "default");
        crate::tui::tests::draw(100, 20, |frame| {
            accounts.render(frame, frame.area(), &state);
            accounts.render_confirmation(frame, frame.area());
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
        let mut state: crate::monitor::snapshot::MonitorSnapshot =
            crate::monitor::mock_state().into();
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
}

#[cfg(all(test, unix))]
mod event_tests {
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
}
