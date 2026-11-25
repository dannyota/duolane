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
        assert_eq!(text.matches("20%").count(), 2, "{text}");
        assert_eq!(text.matches("40%").count(), 2, "{text}");
        assert!(text.contains("42.0%") && text.contains("12.0%"), "{text}");
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
        let row = text.lines().find(|line| line.contains("default")).unwrap();
        assert!(
            row.contains("5h 20% 1h30m") && row.contains("week 40% 1d12h"),
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
