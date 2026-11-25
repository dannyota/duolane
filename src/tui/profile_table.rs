use super::{DIM, DIM_WHITE, GREEN, PURPLE, RED, SELECTED_BG, TEAL, WHITE, YELLOW, cells};
use crate::claude_accounts::profile_state::{ProfileState, Tone, WindowView};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const BAR: usize = 6;
const WINDOW: usize = 11;

pub(super) struct Row<'a> {
    pub(super) name: &'a str,
    pub(super) saved: bool,
    pub(super) focused: bool,
    pub(super) state: Option<&'a ProfileState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Columns {
    name: usize,
    account: usize,
    plan: usize,
    bars: bool,
    login: usize,
    status: usize,
    seen: usize,
    note: usize,
}

fn tone(tone: Tone) -> Color {
    match tone {
        Tone::Good => GREEN,
        Tone::Warn => YELLOW,
        Tone::Bad => RED,
        Tone::Dim => DIM,
    }
}

fn account(state: Option<&ProfileState>) -> String {
    match state {
        Some(state) => match (&state.email, &state.login) {
            (Some(email), _) => email.clone(),
            (None, Some((login, _))) => login.clone(),
            (None, None) => "-".into(),
        },
        None => "-".into(),
    }
}

fn login(state: Option<&ProfileState>) -> (String, Tone) {
    match state.and_then(|state| state.login.as_ref()) {
        Some((text, tone)) if state.is_some_and(|state| state.email.is_some()) => {
            (text.trim_end_matches(" left").to_owned(), *tone)
        }
        _ => ("-".into(), Tone::Dim),
    }
}

fn seen(state: Option<&ProfileState>) -> (String, bool) {
    match state.and_then(|state| state.seen.as_ref().map(|seen| (seen, state.stale))) {
        Some((seen, true)) => (format!("stale {}", seen.trim_end_matches(" ago")), true),
        Some((seen, false)) => (seen.clone(), false),
        None => ("-".into(), false),
    }
}

fn note(state: Option<&ProfileState>) -> String {
    state
        .and_then(|state| state.same_as.as_ref())
        .map(|other| format!("same as {other}"))
        .unwrap_or_default()
}

fn longest<'a>(rows: &'a [Row<'a>], title: &str, text: impl Fn(&'a Row<'a>) -> String) -> usize {
    rows.iter()
        .map(|row| cells::display_width(&text(row)))
        .chain([title.len()])
        .max()
        .unwrap_or(0)
}

impl Columns {
    fn width(&self) -> usize {
        let window = if self.bars { WINDOW + BAR + 1 } else { WINDOW };
        3 + self.name
            + [
                self.account,
                self.plan,
                window,
                window,
                self.login,
                self.status,
                self.seen,
                self.note,
            ]
            .into_iter()
            .filter(|width| *width > 0)
            .map(|width| width + 2)
            .sum::<usize>()
    }

    pub(super) fn fit(rows: &[Row<'_>], width: usize) -> Self {
        let mut columns = Self {
            name: longest(rows, "PROFILE", |row| row.name.to_owned()).min(20),
            account: longest(rows, "ACCOUNT", |row| account(row.state)),
            plan: longest(rows, "PLAN", |row| {
                row.state
                    .and_then(|state| state.plan.clone())
                    .unwrap_or_default()
            }),
            bars: true,
            login: longest(rows, "LOGIN", |row| login(row.state).0),
            status: longest(rows, "STATUS", |row| {
                row.state
                    .map(|state| state.status.clone())
                    .unwrap_or_default()
            }),
            seen: longest(rows, "SEEN", |row| seen(row.state).0),
            note: longest(rows, "", |row| note(row.state)),
        };
        let steps: [fn(&mut Self, usize); 8] = [
            |c, _| c.note = 0,
            |c, over| c.account = c.account.saturating_sub(over).max(16.min(c.account)),
            |c, _| c.seen = 0,
            |c, _| c.login = 0,
            |c, _| c.bars = false,
            |c, _| c.plan = 0,
            |c, _| c.account = 0,
            |c, over| c.name = c.name.saturating_sub(over).max(8.min(c.name)),
        ];
        for step in steps {
            let over = columns.width().saturating_sub(width);
            if over == 0 {
                break;
            }
            step(&mut columns, over);
        }
        columns
    }

    fn cell(spans: &mut Vec<Span<'static>>, text: &str, width: usize, style: Style) {
        if width == 0 {
            return;
        }
        let text = cells::ellipsize(text, width);
        let pad = width.saturating_sub(cells::display_width(&text));
        spans.push(Span::styled(format!("  {text}{}", " ".repeat(pad)), style));
    }

    fn window(&self, spans: &mut Vec<Span<'static>>, view: Option<&WindowView>, base: Style) {
        let Some(view) = view else {
            let width = if self.bars { WINDOW + BAR + 1 } else { WINDOW };
            return Self::cell(spans, "-", width, base.fg(DIM));
        };
        spans.push(Span::styled("  ", base));
        if self.bars {
            let color = match view.percent {
                80.. => RED,
                50.. => YELLOW,
                _ => PURPLE,
            };
            let filled = ((view.percent.min(100) as usize * BAR) + 50) / 100;
            spans.push(Span::styled("█".repeat(filled), base.fg(color)));
            spans.push(Span::styled("░".repeat(BAR - filled), base.fg(DIM)));
            spans.push(Span::styled(" ", base));
        }
        spans.push(Span::styled(
            format!(
                "{:>4} {:<6}",
                format!("{}%", view.percent),
                view.reset.as_deref().unwrap_or("")
            ),
            base,
        ));
    }

    pub(super) fn header(&self) -> Line<'static> {
        let style = Style::default().fg(DIM);
        let mut spans = vec![Span::styled(
            format!(
                "   {}",
                cells::ellipsize(
                    &format!("{:<width$}", "PROFILE", width = self.name),
                    self.name
                )
            ),
            style,
        )];
        let window = if self.bars { WINDOW + BAR + 1 } else { WINDOW };
        for (title, width) in [
            ("ACCOUNT", self.account),
            ("PLAN", self.plan),
            ("5H", window),
            ("WEEK", window),
            ("LOGIN", self.login),
            ("STATUS", self.status),
            ("SEEN", self.seen),
        ] {
            Self::cell(&mut spans, title, width, style);
        }
        Line::from(spans)
    }

    pub(super) fn line(&self, row: &Row<'_>) -> Line<'static> {
        let base = if row.focused {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default()
        };
        let text = if row.focused {
            base.fg(WHITE).add_modifier(Modifier::BOLD)
        } else {
            base.fg(DIM_WHITE)
        };
        let state = row.state;
        let name = cells::ellipsize(row.name, self.name);
        let pad = self.name.saturating_sub(cells::display_width(&name));
        let mut spans = vec![
            Span::styled(if row.saved { "●" } else { " " }, base.fg(GREEN)),
            Span::styled(
                if state.is_some_and(|state| state.suggested) {
                    "★"
                } else {
                    " "
                },
                base.fg(TEAL),
            ),
            Span::styled(format!(" {name}{}", " ".repeat(pad)), text),
        ];
        Self::cell(&mut spans, &account(state), self.account, text);
        Self::cell(
            &mut spans,
            state.and_then(|state| state.plan.as_deref()).unwrap_or("-"),
            self.plan,
            text,
        );
        self.window(&mut spans, state.and_then(|s| s.five_hour.as_ref()), text);
        self.window(&mut spans, state.and_then(|s| s.seven_day.as_ref()), text);
        let (login, login_tone) = login(state);
        Self::cell(
            &mut spans,
            &login,
            self.login,
            if login_tone == Tone::Dim {
                text
            } else {
                base.fg(tone(login_tone))
            },
        );
        Self::cell(
            &mut spans,
            state.map_or("unknown", |state| state.status.as_str()),
            self.status,
            base.fg(tone(state.map_or(Tone::Dim, |state| state.status_tone))),
        );
        let (seen, stale) = seen(state);
        Self::cell(
            &mut spans,
            &seen,
            self.seen,
            base.fg(if stale { YELLOW } else { DIM }),
        );
        Self::cell(&mut spans, &note(state), self.note, base.fg(YELLOW));
        Line::from(spans)
    }
}

pub(super) fn summary(row: &Row<'_>, checked: &str) -> Line<'static> {
    let dim = Style::default().fg(DIM_WHITE);
    let mut spans = vec![Span::styled(format!(" {}", row.name), dim)];
    if let Some(state) = row.state {
        if let Some(account) = &state.account {
            spans.push(Span::styled(format!(" · {account}"), dim));
        }
        if let Some((login, login_tone)) = &state.login {
            spans.push(Span::styled(
                format!(" · {login}"),
                if *login_tone == Tone::Dim {
                    dim
                } else {
                    Style::default().fg(tone(*login_tone))
                },
            ));
        }
        if let Some(other) = &state.same_as {
            spans.push(Span::styled(
                format!(" · same account as {other}"),
                Style::default().fg(YELLOW),
            ));
        }
        if state.suggested {
            spans.push(Span::styled(
                " · ★ suggested: most room",
                Style::default().fg(TEAL),
            ));
        }
    }
    spans.push(Span::styled(
        format!(" · {checked}"),
        Style::default().fg(DIM),
    ));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ProfileState {
        ProfileState {
            five_hour: Some(WindowView {
                percent: 23,
                reset: Some("1h42m".into()),
            }),
            seven_day: Some(WindowView {
                percent: 100,
                reset: None,
            }),
            email: Some("first@example.com".into()),
            plan: Some("team".into()),
            account: Some("first@example.com (team)".into()),
            login: Some(("2d left".into(), Tone::Warn)),
            status: "limited 29m".into(),
            status_tone: Tone::Bad,
            seen: Some("2h ago".into()),
            stale: true,
            same_as: Some("default".into()),
            suggested: true,
            ..Default::default()
        }
    }

    fn rows(state: &ProfileState) -> Vec<Row<'_>> {
        vec![
            Row {
                name: "backup",
                saved: true,
                focused: false,
                state: Some(state),
            },
            Row {
                name: "new",
                saved: false,
                focused: true,
                state: None,
            },
        ]
    }

    #[test]
    fn wide_table_shows_every_column_with_tones() {
        let state = state();
        let rows = rows(&state);
        let columns = Columns::fit(&rows, 140);
        assert_eq!(
            columns.header().to_string().trim_end(),
            "   PROFILE  ACCOUNT            PLAN  5H                  WEEK                LOGIN  STATUS       SEEN"
        );
        let line = columns.line(&rows[0]);
        assert_eq!(
            line.to_string().trim_end(),
            "●★ backup   first@example.com  team  █░░░░░  23% 1h42m   ██████ 100%         2d     limited 29m  stale 2h  same as default"
        );
        let color = |text: &str| {
            line.spans
                .iter()
                .find(|span| span.content.trim() == text)
                .and_then(|span| span.style.fg)
        };
        assert_eq!(color("2d"), Some(YELLOW));
        assert_eq!(color("limited 29m"), Some(RED));
        assert_eq!(color("stale 2h"), Some(YELLOW));
        let unknown = columns.line(&rows[1]);
        assert!(unknown.to_string().starts_with("   new      -"));
        assert_eq!(unknown.spans[2].style.bg, Some(SELECTED_BG));
    }

    #[test]
    fn narrow_table_drops_columns_but_keeps_name_windows_and_status() {
        let state = state();
        let rows = rows(&state);
        for width in [100, 78, 60, 52] {
            let columns = Columns::fit(&rows, width);
            let line = columns.line(&rows[0]).to_string();
            assert!(cells::display_width(&line) <= width, "{width}: {line}");
            assert!(
                line.contains("backup")
                    && line.contains("23% 1h42m")
                    && line.contains("100%")
                    && line.contains("limited 29m"),
                "{width}: {line}"
            );
        }
        assert!(Columns::fit(&rows, 78).account == 0 || !Columns::fit(&rows, 78).bars);
    }

    #[test]
    fn summary_gives_the_focused_account_in_full() {
        let state = state();
        let text = summary(&rows(&state)[0], "checked 2m ago").to_string();
        assert_eq!(
            text,
            " backup · first@example.com (team) · 2d left · same account as default · ★ suggested: most room · checked 2m ago"
        );
    }
}
