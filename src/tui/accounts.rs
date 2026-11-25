mod controls;
mod quick;

pub(super) struct Accounts {
    controls: controls::Controls,
    pub(super) local: bool,
    pub(super) limits: std::collections::BTreeMap<String, crate::claude_accounts::limits::Limits>,
    details: super::account_details::Details,
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
            controls: Default::default(),
            limits: Default::default(),
            details: Default::default(),
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

    pub(super) fn open(&mut self) {
        self.enter();
        if self.local {
            self.details.request(false);
        }
    }

    pub(super) fn reload(&mut self) {
        self.controls.clear_message();
        self.enter();
        self.refresh_limits();
        if self.local {
            self.details.request(true);
        }
    }

    pub(super) fn refresh_if_due(&mut self) {
        self.details.poll();
        self.poll_action();
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
        (self.pending.is_some() && self.confirmation_visible) || self.controls.visible()
    }
    pub(super) fn confirming(&self) -> bool {
        self.pending.is_some() || self.controls.active()
    }
    pub(super) fn cancel(&mut self) {
        self.controls.cancel();
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
        if self.controls.active() {
            return self.confirm_action();
        }
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

impl Accounts {
    pub(super) fn render_confirmation(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
    ) {
        use ratatui::{
            layout::{Alignment, Rect},
            style::{Modifier, Style},
            text::{Line, Span},
            widgets::{Block, BorderType, Borders, Clear, Paragraph},
        };
        self.confirmation_visible = false;
        let Some((name, _)) = &self.pending else {
            return;
        };
        let detail = self.pending_detail().filter(|_| area.height >= 6);
        let width = 60.min(area.width);
        let height = (5 + u16::from(detail.is_some())).min(area.height);
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let inner = usize::from(width.saturating_sub(2));
        self.confirmation_visible = name.is_ascii() && inner >= name.len().max(22) && height >= 5;
        let dim = Style::default().fg(super::DIM_WHITE);
        let key = Style::default().fg(super::TEAL);
        let mut lines = vec![
            Line::from(Span::styled(
                "Select profile?",
                Style::default()
                    .fg(super::WHITE)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                name.clone(),
                Style::default()
                    .fg(super::TEAL)
                    .add_modifier(Modifier::BOLD),
            )),
        ];
        if let Some(detail) = detail {
            lines.push(Line::from(Span::styled(
                super::cells::ellipsize(&detail, inner),
                dim,
            )));
        }
        lines.push(Line::from(vec![
            Span::styled("Enter", key),
            Span::styled(" save  ", dim),
            Span::styled("Esc", key),
            Span::styled(" cancel  ", dim),
            Span::styled("n", key),
            Span::styled(" next", dim),
        ]));
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center).block(
                Block::default()
                    .title(Span::styled(" Next launch ", key))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(key)
                    .style(Style::default().bg(super::BG)),
            ),
            popup,
        );
    }
    pub(super) fn click(&mut self, x: u16, y: u16) -> Option<crossterm::event::KeyCode> {
        let at = ratatui::layout::Position::new(x, y);
        if self.confirming() {
            return None;
        }
        if let Some(key) = self.controls.button_at(at) {
            return Some(key);
        }
        if self.subscription.contains(at) {
            self.focus = self.names.len();
        } else if self.rows.contains(at) {
            let index = self.first + usize::from(y - self.rows.y) / self.row_height;
            if index < self.names.len() {
                self.focus = index;
            }
        }
        None
    }

    #[cfg(test)]
    pub(super) fn render(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        snapshot: &crate::monitor::snapshot::MonitorSnapshot,
    ) {
        self.render_at(
            frame,
            area,
            snapshot,
            std::time::SystemTime::now(),
            jiff::tz::TimeZone::system(),
        );
    }

    pub(super) fn render_at(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
        snapshot: &crate::monitor::snapshot::MonitorSnapshot,
        now: std::time::SystemTime,
        zone: jiff::tz::TimeZone,
    ) {
        use ratatui::{
            layout::{Constraint, Direction, Layout},
            style::Style,
            text::Line,
            widgets::{Block, Borders, Paragraph},
        };
        let horizontal = area.width >= if self.local { 180 } else { 90 };
        let claude_height = if self.local {
            let wanted = u16::try_from(self.names.len().saturating_add(5)).unwrap_or(u16::MAX);
            wanted.min(
                area.height
                    .saturating_sub(if area.height >= wanted.saturating_add(5) {
                        5
                    } else {
                        3
                    }),
            )
        } else {
            3.min(area.height.saturating_sub(5))
        };
        let panes = Layout::default()
            .direction(if horizontal {
                Direction::Horizontal
            } else {
                Direction::Vertical
            })
            .constraints(match (horizontal, self.local) {
                (false, _) => [Constraint::Length(claude_height), Constraint::Min(0)],
                (true, true) => [Constraint::Min(0), Constraint::Length(52)],
                (true, false) => [Constraint::Percentage(50), Constraint::Percentage(50)],
            })
            .split(area);
        let seconds = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let roster = self.details.roster(&self.names);
        let states = roster.states(&self.limits, seconds);
        let mut block = Block::default()
            .title(" Claude profiles ")
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .style(Style::default().fg(super::SEPARATOR));
        if self.local {
            block = block.title_top(
                Line::from(format!(
                    " ● Saved: {}  ★ Suggested ",
                    if self.selected.is_empty() {
                        "Unknown"
                    } else {
                        &self.selected
                    }
                ))
                .style(Style::default().fg(super::DIM_WHITE))
                .right_aligned(),
            );
        }
        if !self.names.is_empty() {
            block = block.title_bottom(
                Line::from(format!(" {} ", roster.total(&self.limits, seconds)))
                    .style(Style::default().fg(super::DIM_WHITE)),
            );
        }
        let inner = block.inner(panes[0]);
        frame.render_widget(block, panes[0]);
        self.rows = inner;
        self.row_height = 1;
        if !self.local {
            frame.render_widget(Paragraph::new("Claude profiles: Unknown"), inner);
        } else {
            let rows: Vec<super::profile_table::Row<'_>> = self
                .names
                .iter()
                .enumerate()
                .map(|(index, name)| super::profile_table::Row {
                    name,
                    saved: *name == self.selected,
                    focused: index == self.focus,
                    state: states.get(index),
                })
                .collect();
            let columns = super::profile_table::Columns::fit(&rows, usize::from(inner.width));
            let height = usize::from(inner.height);
            let line = |y: u16| ratatui::layout::Rect::new(inner.x, y, inner.width, 1);
            let (buttons, message, header) = (
                height >= 2,
                height >= rows.len() + 2,
                height >= rows.len() + 3,
            );
            if header {
                frame.render_widget(Paragraph::new(columns.header()), line(inner.y));
                self.rows.y += 1;
            }
            self.rows.height = inner
                .height
                .saturating_sub(u16::from(header) + u16::from(buttons) + u16::from(message));
            let count = usize::from(self.rows.height);
            self.first = self
                .focus
                .min(rows.len().saturating_sub(1))
                .saturating_add(1)
                .saturating_sub(count.max(1));
            let lines: Vec<Line<'_>> = rows
                .iter()
                .skip(self.first)
                .take(count)
                .map(|row| columns.line(row))
                .collect();
            frame.render_widget(Paragraph::new(lines), self.rows);
            let note = self.note_line(rows.get(self.focus));
            if message {
                frame.render_widget(Paragraph::new(note), line(inner.bottom() - 1));
            }
            if buttons {
                self.render_buttons(frame, line(inner.bottom() - 1 - u16::from(message)));
            }
        }
        self.subscription = panes[1];
        self.confirmation_visible = false;
        super::subscription::render(
            frame,
            panes[1],
            snapshot,
            now,
            zone,
            super::subscription::Notes {
                focused: self.focus == self.names.len(),
                local: self.local,
                saved: &self.selected,
                overridden: self.overridden,
                focus_is_saved: self.names.get(self.focus) == Some(&self.selected),
                status: &self.status,
            },
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
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
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
        let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(90, 13, |f| {
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
        let mut state: crate::monitor::snapshot::MonitorSnapshot =
            crate::monitor::mock_state().into();
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
        let mut state: crate::monitor::snapshot::MonitorSnapshot =
            crate::monitor::mock_state().into();
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
        let mut state: crate::monitor::snapshot::MonitorSnapshot =
            crate::monitor::mock_state().into();
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
        let mut state: crate::monitor::snapshot::MonitorSnapshot =
            crate::monitor::mock_state().into();
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
        let mut state: crate::monitor::snapshot::MonitorSnapshot =
            crate::monitor::mock_state().into();
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
                    let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(
                        width,
                        height,
                        |f| {
                            crate::tui::render_at(
                                f,
                                &mut app,
                                &state,
                                now,
                                jiff::tz::TimeZone::UTC,
                            );
                        },
                    ));
                    let lines: Vec<_> = text.lines().collect();
                    let rows = app.accounts.rows;
                    let names = ["abcdefghijkl", "mnopqrstuvwx", "yzabcdefghij"];
                    let count =
                        (usize::from(rows.height) / app.accounts.row_height).min(names.len());
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
                            let (five, week) = if i == 1 {
                                ("20%", "40%")
                            } else {
                                ("20% 1h0m", "40% 2h0m")
                            };
                            assert!(
                                line.contains(five) && line.contains(week),
                                "{width}x{height}: {text}"
                            );
                        }
                    }
                    let pane = app.accounts.subscription;
                    let inner_width = pane.width.saturating_sub(2);
                    let inner_height = pane.height.saturating_sub(2);
                    let quota = state.codex_quota.as_ref().unwrap();
                    let full = [
                        ratatui::text::Line::from(crate::tui::subscription::quota_summary(
                            Some(quota),
                            now,
                        )),
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
                            let mut offset = usize::from(
                                super::super::usage_limits::wrapped_height(&full[..1], inner_width),
                            );
                            for i in 0..2 {
                                let count =
                                    usize::from(super::super::usage_limits::wrapped_height(
                                        &full[i + 1..i + 2],
                                        inner_width,
                                    ));
                                let window =
                                    lines[start + offset..start + offset + count].join(" ");
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
        accounts.selected = "default".into();
        accounts.limits.insert("default".into(), limits(1.0, now));
        accounts
            .limits
            .insert("backup".into(), limits(0.2, now - 7_200));
        accounts.details.request_with(false, |_| {
            Some(
                crate::claude_accounts::profile_state::Roster::from_status(&[
                    (
                        "default",
                        r#"{"loggedIn":true,"email":"first@example.com","subscriptionType":"team"}"#,
                    ),
                    ("backup", r#"{"loggedIn":false}"#),
                ]),
            )
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
        assert!(text.contains("●  profile-0 "), "{text}");
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
