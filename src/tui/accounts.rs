mod controls;
mod quick;

pub(super) struct Accounts {
    controls: controls::Controls,
    pub(super) local: bool,
    pub(super) limits: std::collections::BTreeMap<String, crate::claude_accounts::limits::Limits>,
    details: super::account_details::Details,
    refreshed: Option<std::time::Instant>,
    names: Vec<String>,
    identities: std::collections::BTreeMap<String, String>,
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
            identities: Default::default(),
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
                self.identities = roster.identities;
                self.selected = roster.selected;
                self.overridden = roster.overridden;
                self.focus = self.focus.min(self.names.len().saturating_sub(1));
                self.status.clear();
            }
            Err(_) => {
                self.names.clear();
                self.identities.clear();
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
        let roster = self.details.roster(&self.names, &self.identities);
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
mod tests;

#[cfg(all(test, unix))]
mod event_tests;
