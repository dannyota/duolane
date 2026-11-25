use super::Accounts;
use crate::claude_accounts::profile_state::Tone;
use crate::tui::actions::{self, Finished};
use crossterm::event::KeyCode;

const NAME_CHARS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tui) enum Kind {
    Add,
    Login,
    Remove,
    Switch,
}

impl Kind {
    fn title(self) -> &'static str {
        match self {
            Self::Add => "Add profile and sign in?",
            Self::Login => "Sign in again?",
            Self::Remove => "Sign out and delete profile?",
            Self::Switch => "Switch and resume here?",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Add => "Add",
            Self::Login => "Sign in to",
            Self::Remove => "Remove",
            Self::Switch => "Switch to",
        }
    }

    fn handoff(self) -> bool {
        self != Self::Remove
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tui) struct Action {
    pub(in crate::tui) kind: Kind,
    pub(in crate::tui) name: String,
    account: Option<String>,
}

impl Action {
    pub(in crate::tui) fn label(&self) -> String {
        format!("{} {}", self.kind.verb(), self.name)
    }

    pub(in crate::tui) fn args(&self) -> Vec<String> {
        let command = match self.kind {
            Kind::Add | Kind::Login => "login",
            Kind::Remove => "remove",
            Kind::Switch => "switch",
        };
        let mut args = vec!["accounts".to_owned(), command.to_owned(), self.name.clone()];
        if self.kind == Kind::Remove {
            args.push("--yes".into());
        }
        args
    }
}

#[derive(Default)]
pub(super) struct Controls {
    input: Option<String>,
    pending: Option<Action>,
    visible: bool,
    runner: actions::Runner,
    handoff: Option<Action>,
    message: String,
    tone: Tone,
    buttons: Vec<(ratatui::layout::Rect, KeyCode)>,
}

impl Controls {
    pub(super) fn active(&self) -> bool {
        self.input.is_some() || self.pending.is_some()
    }

    pub(super) fn visible(&self) -> bool {
        self.pending.is_some() && self.visible
    }

    pub(super) fn cancel(&mut self) {
        self.input = None;
        self.pending = None;
        self.visible = false;
    }

    pub(super) fn clear_message(&mut self) {
        self.message.clear();
    }

    fn say(&mut self, tone: Tone, message: String) {
        self.tone = tone;
        self.message = message;
    }

    pub(super) fn button_at(&self, at: ratatui::layout::Position) -> Option<KeyCode> {
        self.buttons
            .iter()
            .find(|(rect, _)| rect.contains(at))
            .map(|(_, key)| *key)
    }

    #[cfg(test)]
    pub(super) fn message(&self) -> Option<String> {
        match self.runner.running() {
            Some(label) => Some(format!("Running: {label}")),
            None => (!self.message.is_empty()).then(|| self.message.clone()),
        }
    }
}

fn valid_new_name(name: &str) -> bool {
    name != "default" && crate::claude_accounts::valid_profile_tag(name)
}

fn local_roster() -> Option<(Vec<String>, String)> {
    #[cfg(unix)]
    {
        crate::claude_accounts::monitor_roster()
            .ok()
            .map(|roster| (roster.names, roster.selected))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

impl Accounts {
    pub(in crate::tui) fn typing(&self) -> bool {
        self.controls.input.is_some()
    }

    pub(in crate::tui) fn type_key(&mut self, code: KeyCode) {
        let Some(input) = &mut self.controls.input else {
            return;
        };
        match code {
            KeyCode::Char(c)
                if (c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
                    && input.len() < NAME_CHARS =>
            {
                input.push(c);
                self.controls.message.clear();
            }
            KeyCode::Backspace => {
                input.pop();
                self.controls.message.clear();
            }
            KeyCode::Esc => self.controls.cancel(),
            KeyCode::Enter => {
                let name = input.clone();
                if !valid_new_name(&name) {
                    self.controls.message =
                        "Use 1 to 32 of a-z, 0-9, - or _, starting with a letter or digit".into();
                } else if self.names.contains(&name) {
                    self.controls.message = format!("{name} already exists; press l on it");
                } else {
                    self.controls.message.clear();
                    self.controls.input = None;
                    self.controls.pending = Some(Action {
                        kind: Kind::Add,
                        name,
                        account: None,
                    });
                }
            }
            _ => {}
        }
    }

    pub(in crate::tui) fn action_key(&mut self, key: char) {
        if !self.local || self.confirming() {
            return;
        }
        if let Some(label) = self.controls.runner.running() {
            let busy = format!("Busy: {label}");
            self.controls.say(Tone::Warn, busy);
            return;
        }
        self.controls.message.clear();
        if key == 'a' {
            self.controls.input = Some(String::new());
            return;
        }
        let Some(name) = self.names.get(self.focus).cloned() else {
            self.controls
                .say(Tone::Warn, "Focus a Claude profile first".into());
            return;
        };
        let kind = match key {
            'l' => Kind::Login,
            'd' => Kind::Remove,
            's' => Kind::Switch,
            _ => return,
        };
        let refusal = match kind {
            Kind::Login | Kind::Remove if name == "default" => {
                Some("default is managed by Claude Code".to_owned())
            }
            Kind::Switch if self.overridden => {
                Some("CLAUDE_CONFIG_DIR overrides selection".to_owned())
            }
            Kind::Switch if name == self.selected => Some(format!("{name} is already selected")),
            _ => None,
        };
        if let Some(refusal) = refusal {
            self.controls.say(Tone::Warn, refusal);
            return;
        }
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let account = self
            .details
            .roster(&self.names)
            .states(&self.limits, seconds)
            .into_iter()
            .nth(self.focus)
            .and_then(|state| state.account);
        self.controls.pending = Some(Action {
            kind,
            name,
            account,
        });
    }

    pub(super) fn confirm_action(&mut self) {
        self.confirm_action_with(local_roster(), |args| match actions::program() {
            Ok(program) => actions::capture(&program, &args, actions::CAPTURE_TIMEOUT),
            Err(error) => Finished {
                ok: false,
                summary: format!("could not start: {}", error.kind()),
            },
        });
    }

    pub(super) fn confirm_action_with(
        &mut self,
        roster: Option<(Vec<String>, String)>,
        capture: impl FnOnce(Vec<String>) -> Finished + Send + 'static,
    ) {
        if !self.controls.visible() {
            return;
        }
        let Some(action) = self.controls.pending.take() else {
            return;
        };
        self.controls.visible = false;
        let name = &action.name;
        let refusal = match (&roster, action.kind) {
            (None, _) => Some("Could not read the profiles".to_owned()),
            (Some((names, _)), Kind::Add) if names.contains(name) => {
                Some(format!("{name} already exists"))
            }
            (Some(_), Kind::Add) => None,
            (Some((names, _)), _) if !names.contains(name) => {
                Some(format!("{name} is no longer registered"))
            }
            (Some((_, selected)), Kind::Switch) if selected == name => {
                Some(format!("{name} is already selected"))
            }
            _ => None,
        };
        if let Some(refusal) = refusal {
            self.controls
                .say(Tone::Warn, format!("{refusal}; nothing ran"));
            self.enter();
            return;
        }
        if action.kind.handoff() {
            self.controls.handoff = Some(action);
            return;
        }
        let args = action.args();
        if !self
            .controls
            .runner
            .start(action.label(), move || capture(args))
        {
            self.controls
                .say(Tone::Bad, format!("{}: could not start", action.label()));
        }
    }

    pub(in crate::tui) fn take_handoff(&mut self) -> Option<Action> {
        self.controls.handoff.take()
    }

    pub(in crate::tui) fn run_handoff(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    ) -> std::io::Result<()> {
        if let Some(action) = self.take_handoff() {
            let label = action.label();
            let finished = actions::run_handoff(terminal, &label, &action.args())?;
            self.finish(&label, &finished);
        }
        Ok(())
    }

    pub(in crate::tui) fn finish(&mut self, label: &str, finished: &Finished) {
        self.reload();
        self.controls.say(
            if finished.ok { Tone::Good } else { Tone::Bad },
            format!(
                "{label}: {}{}",
                if finished.ok { "" } else { "failed, " },
                finished.summary
            ),
        );
    }

    pub(super) fn note_line(
        &self,
        focused: Option<&crate::tui::profile_table::Row<'_>>,
    ) -> ratatui::text::Line<'static> {
        use ratatui::style::Style;
        use ratatui::text::Line;
        let color = |tone| match tone {
            Tone::Good => crate::tui::GREEN,
            Tone::Bad => crate::tui::RED,
            Tone::Warn | Tone::Dim => crate::tui::YELLOW,
        };
        if let Some(label) = self.controls.runner.running() {
            return Line::from(format!(" … Running: {label}"))
                .style(Style::default().fg(crate::tui::TEAL));
        }
        if !self.controls.message.is_empty() {
            let mark = match self.controls.tone {
                Tone::Good => "✓",
                Tone::Bad => "✗",
                Tone::Warn | Tone::Dim => "!",
            };
            return Line::from(format!(" {mark} {}", self.controls.message))
                .style(Style::default().fg(color(self.controls.tone)));
        }
        if !self.status.is_empty() {
            return Line::from(format!(" {}", self.status))
                .style(Style::default().fg(crate::tui::DIM_WHITE));
        }
        if self.overridden {
            return Line::from(" ! CLAUDE_CONFIG_DIR overrides selection")
                .style(Style::default().fg(crate::tui::YELLOW));
        }
        match focused {
            Some(row) => crate::tui::profile_table::summary(row, &self.details.summary()),
            None => Line::from(format!(" {}", self.details.summary()))
                .style(Style::default().fg(crate::tui::DIM)),
        }
    }

    pub(super) fn render_buttons(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
    ) {
        use ratatui::style::{Modifier, Style};
        use ratatui::text::{Line, Span};
        let name = self.names.get(self.focus);
        let named = name.is_some_and(|name| name != "default");
        let other = name.is_some_and(|name| *name != self.selected) && !self.overridden;
        let buttons = [
            ("Enter", " Use", KeyCode::Enter, other),
            ("L", " Sign in", KeyCode::Char('l'), named),
            ("D", " Remove", KeyCode::Char('d'), named),
            ("S", " Switch", KeyCode::Char('s'), other),
            ("A", " Add", KeyCode::Char('a'), true),
            ("R", " Refresh", KeyCode::Char('r'), true),
        ];
        self.controls.buttons.clear();
        let mut spans = Vec::new();
        let mut x = area.x.saturating_add(1);
        spans.push(Span::raw(" "));
        for (key, label, code, enabled) in buttons {
            let width = (key.len() + label.len() + 2) as u16;
            if x.saturating_add(width) > area.right() {
                break;
            }
            let (edge, hot) = if enabled {
                (
                    Style::default().fg(crate::tui::WHITE),
                    Style::default()
                        .fg(crate::tui::TEAL)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                let dim = Style::default().fg(crate::tui::SEPARATOR);
                (dim, dim)
            };
            spans.extend([
                Span::styled("[", edge),
                Span::styled(key, hot),
                Span::styled(format!("{label}]"), edge),
                Span::raw("  "),
            ]);
            if enabled {
                self.controls
                    .buttons
                    .push((ratatui::layout::Rect::new(x, area.y, width, 1), code));
            }
            x = x.saturating_add(width + 2);
        }
        frame.render_widget(ratatui::widgets::Paragraph::new(Line::from(spans)), area);
    }

    pub(super) fn poll_action(&mut self) {
        if let Some((label, finished)) = self.controls.runner.poll() {
            self.finish(&label, &finished);
        }
    }

    pub(in crate::tui) fn render_action(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
    ) {
        use ratatui::{
            layout::{Alignment, Rect},
            style::Style,
            widgets::{Block, Borders, Clear, Paragraph},
        };
        self.controls.visible = false;
        let lines: Vec<String> = if let Some(input) = &self.controls.input {
            vec![
                "New profile name".into(),
                format!("> {input}_"),
                self.controls.message.clone(),
                "Enter next  Esc cancel".into(),
            ]
        } else if let Some(action) = &self.controls.pending {
            vec![
                action.kind.title().into(),
                match &action.account {
                    Some(account) => format!("{}  {account}", action.name),
                    None => action.name.clone(),
                },
                match action.kind {
                    Kind::Switch => std::env::current_dir()
                        .map(|dir| actions::clean(format!("in {}", dir.display()).as_bytes()))
                        .unwrap_or_default(),
                    Kind::Remove => "Shared project folders stay in place".into(),
                    Kind::Add | Kind::Login => "Opens the Claude sign-in in this terminal".into(),
                },
                "Enter confirm  Esc cancel".into(),
            ]
        } else {
            return;
        };
        let content = lines
            .iter()
            .map(|line| crate::tui::cells::display_width(line))
            .max()
            .unwrap_or(0);
        let width = u16::try_from(content.saturating_add(4))
            .unwrap_or(u16::MAX)
            .clamp(30, 76)
            .min(area.width);
        let height = 6.min(area.height);
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let inner = usize::from(width.saturating_sub(2));
        self.controls.visible = height >= 6
            && lines[..2]
                .iter()
                .all(|line| line.is_ascii() && line.len() <= inner);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(
                lines
                    .iter()
                    .map(|line| crate::tui::cells::ellipsize(line, inner))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .style(Style::default().bg(crate::tui::BG)),
            ),
            popup,
        );
    }
}

#[cfg(test)]
mod tests {
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

        fn never(_: Vec<String>) -> Finished {
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
            let state: crate::monitor::snapshot::MonitorSnapshot =
                crate::monitor::mock_state().into();
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
            let state: crate::monitor::snapshot::MonitorSnapshot =
                crate::monitor::mock_state().into();
            let mut app = crate::tui::mouse_tests::app_for_accounts_test(accounts());
            let click = |app: &mut crate::tui::MonitorApp, label: &str| {
                let mut targets = Default::default();
                let screen = crate::tui::tests::draw(120, 30, |frame| {
                    targets = crate::tui::render(frame, app, &state);
                });
                let (column, row) =
                    crate::tui::tests::placeholder_position(&screen, label).unwrap();
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
        fn terminal_actions_queue_fixed_arguments_and_removal_runs_captured() {
            for (key, args) in [
                ('l', ["accounts", "login", "backup"]),
                ('s', ["accounts", "switch", "backup"]),
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
                move |args| {
                    *record.lock().unwrap() = args;
                    Finished {
                        ok: false,
                        summary:
                            "failed with exit status 1: Error: Claude logout failed; profile kept"
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
                ["accounts", "remove", "backup", "--yes"]
            );
            assert_eq!(
                accounts.controls.message().as_deref(),
                Some(
                    "Remove backup: failed, failed with exit status 1: Error: Claude logout failed; profile kept"
                )
            );
        }
    }
}
