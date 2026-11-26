pub(super) fn needed_height(
    area: ratatui::layout::Rect,
    snapshot: &crate::monitor::snapshot::MonitorSnapshot,
    count: usize,
    now: std::time::SystemTime,
    zone: jiff::tz::TimeZone,
) -> u16 {
    let claude = 4_u16.saturating_add((count.min(u16::MAX as usize / 3) * 3) as u16);
    let width = if area.width >= 90 {
        area.width / 2
    } else {
        area.width
    }
    .saturating_sub(2);
    let quota = snapshot
        .codex_quota
        .as_ref()
        .filter(|q| now.duration_since(q.observed_at).is_ok());
    let lines = vec![
        ratatui::text::Line::from(quota_summary(quota, now)),
        super::usage_limits::codex_in_zone(
            "Primary",
            quota.and_then(|q| q.primary.as_ref()),
            now,
            zone.clone(),
        ),
        super::usage_limits::codex_in_zone(
            "Secondary",
            quota.and_then(|q| q.secondary.as_ref()),
            now,
            zone,
        ),
    ];
    let chatgpt = super::usage_limits::wrapped_height(&lines, width).saturating_add(2);
    if area.width >= 90 {
        claude.max(chatgpt)
    } else {
        claude.saturating_add(chatgpt)
    }
}

pub(super) struct Accounts {
    pub(super) local: bool,
    pub(super) limits: std::collections::BTreeMap<String, crate::claude_accounts::limits::Limits>,
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
        use super::usage_limits::{QuotaLayout, quota_layout};
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
                let reset = |five: bool| {
                    limits
                        .and_then(|l| {
                            if five {
                                l.five_hour.as_ref()
                            } else {
                                l.seven_day.as_ref()
                            }
                        })
                        .and_then(|w| w.resets_at)
                };
                lines.push(super::usage_limits::compact_line(
                    name,
                    style,
                    [
                        super::usage_limits::CompactWindow {
                            label: "5h".into(),
                            line: five,
                            reset: reset(true),
                        },
                        super::usage_limits::CompactWindow {
                            label: "week".into(),
                            line: week,
                            reset: reset(false),
                        },
                    ],
                    usize::from(self.rows.width),
                    seconds,
                ));
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
            let primary = super::usage_limits::codex_in_zone(
                "Primary",
                quota.and_then(|q| q.primary.as_ref()),
                now,
                zone.clone(),
            );
            let secondary = super::usage_limits::codex_in_zone(
                "Secondary",
                quota.and_then(|q| q.secondary.as_ref()),
                now,
                zone.clone(),
            );
            let layout = quota_layout(
                super::usage_limits::wrapped_height(
                    &[detail[0].clone(), primary.clone(), secondary.clone()],
                    inner.width,
                ),
                inner.height,
            );
            if layout == QuotaLayout::Full {
                detail.push(primary);
                detail.push(secondary);
            } else {
                if inner.height <= 2 {
                    detail.clear();
                }
                let window = |primary: bool| {
                    quota.and_then(|q| {
                        if primary {
                            q.primary.as_ref()
                        } else {
                            q.secondary.as_ref()
                        }
                    })
                };
                let compact =
                    |fallback: &str, w: Option<&crate::monitor::CodexQuotaWindow>, line| {
                        let label = match w.and_then(|w| w.window_minutes) {
                            Some(300) => "5h".into(),
                            Some(10080) => "week".into(),
                            Some(n) if (1..=525_600_000).contains(&n) => format!("{n}m"),
                            _ => fallback.into(),
                        };
                        super::usage_limits::CompactWindow {
                            label,
                            line,
                            reset: w.and_then(|w| w.reset_at),
                        }
                    };
                let windows = [
                    compact("Primary", window(true), primary),
                    compact("Secondary", window(false), secondary),
                ];
                if layout == QuotaLayout::TwoLines {
                    for window in windows {
                        detail.push(super::usage_limits::compact_line(
                            "",
                            Style::default(),
                            [window],
                            usize::from(inner.width),
                            seconds,
                        ));
                    }
                } else {
                    detail.push(super::usage_limits::compact_line(
                        "",
                        Style::default(),
                        windows,
                        usize::from(inner.width),
                        seconds,
                    ));
                }
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
mod tests;

#[cfg(all(test, unix))]
mod event_tests;
