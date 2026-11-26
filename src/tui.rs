mod accounts;
mod cells;
mod chrome;
mod detail;
mod key;
mod layout;
mod mouse;
#[cfg(test)]
mod mouse_tests;
mod overlay;
#[cfg(test)]
mod overview_tests;
#[cfg(test)]
mod phase_tests;
mod pointer;
#[cfg(test)]
mod pointer_tests;
mod request_phases;
mod requests;
mod sessions;
mod summary;
mod usage_limits;
mod usage_stats;

use cells::format_duration;
#[cfg(test)]
use chrome::render_header;
use detail::{render_request_detail, render_session_detail};
use mouse::{MouseTargets, VisibleRows};
use overlay::{
    render_help_overlay, render_setup_overlay, render_shutdown_confirmation,
    render_shutdown_overlay,
};
use requests::{render_active_at, render_events, render_recent};

use std::{
    io::{self, Stdout},
    sync::mpsc,
    time::Duration,
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Paragraph},
};
use tokio::sync::oneshot;

use crate::{
    monitor::{
        MockMonitor, MonitorHandle,
        snapshot::{MonitorSnapshot, SnapshotUpdate},
    },
    paths,
    registry::Registry,
};

const TEAL: Color = Color::Rgb(78, 201, 176);
const WHITE: Color = Color::Rgb(240, 244, 248);
const DIM_WHITE: Color = Color::Rgb(180, 190, 200);
const SEPARATOR: Color = Color::Rgb(72, 74, 82);
const BG: Color = Color::Rgb(18, 18, 22);
const PANEL_BG: Color = Color::Rgb(22, 22, 27);
const SELECTED_BG: Color = Color::Rgb(42, 45, 54);
const GREEN: Color = Color::Rgb(120, 200, 120);
const RED: Color = Color::Rgb(220, 120, 120);
const YELLOW: Color = Color::Rgb(220, 200, 100);
const BLUE: Color = Color::Rgb(120, 170, 230);
const PURPLE: Color = Color::Rgb(190, 140, 240);
const DIM: Color = Color::Rgb(139, 144, 156);
const SESSION_SPARKLINE_MIN_WIDTH: u16 = 170;
const SESSION_SPARKLINE_MAX_TOKENS: u64 = 4_000;

pub struct MonitorUiConfig<'a> {
    pub listen_url: String,
    pub port: u16,
    pub registry: &'a Registry,
    pub shutdown: Option<oneshot::Sender<()>>,
    pub shutdown_complete: Option<mpsc::Receiver<()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorExit {
    ShutdownComplete,
    ForceQuit,
    Detached,
}

pub fn run_monitor(
    handle: MonitorHandle,
    config: MonitorUiConfig<'_>,
) -> Result<MonitorExit, anyhow::Error> {
    run_monitor_loop(
        || SnapshotUpdate::Live(handle.snapshot().into()),
        UiSession::from(config),
    )
}

pub fn run_attached_monitor(
    snapshot: impl FnMut() -> SnapshotUpdate,
    listen_url: String,
) -> Result<MonitorExit, anyhow::Error> {
    let setup_text = format!(
        "Attached to {listen_url}\nClosing this dashboard leaves the proxy running.\nConfigure providers and authentication in the proxy service."
    );
    run_monitor_loop(
        snapshot,
        UiSession {
            local_accounts: false,
            listen_url,
            setup_text,
            shutdown: None,
            shutdown_complete: None,
        },
    )
}

pub fn run_mock_monitor(port: u16, registry: &Registry) -> Result<(), anyhow::Error> {
    let mut monitor = MockMonitor::new();
    run_monitor_loop(
        move || SnapshotUpdate::Live(monitor.snapshot().into()),
        UiSession {
            local_accounts: false,
            listen_url: "mock://tui-demo".to_string(),
            setup_text: mock_setup_text(port, registry),
            shutdown: None,
            shutdown_complete: None,
        },
    )
    .map(|_| ())
}

struct UiSession {
    local_accounts: bool,
    listen_url: String,
    setup_text: String,
    shutdown: Option<oneshot::Sender<()>>,
    shutdown_complete: Option<mpsc::Receiver<()>>,
}

impl From<MonitorUiConfig<'_>> for UiSession {
    fn from(config: MonitorUiConfig<'_>) -> Self {
        Self {
            local_accounts: true,
            setup_text: setup_text(config.port, config.registry),
            listen_url: config.listen_url,
            shutdown: config.shutdown,
            shutdown_complete: config.shutdown_complete,
        }
    }
}

fn run_monitor_loop(
    mut snapshot: impl FnMut() -> SnapshotUpdate,
    config: UiSession,
) -> Result<MonitorExit, anyhow::Error> {
    let pointer_supported = pointer::supported(&paths::environment_snapshot());
    let (mut guard, mut terminal) = setup_terminal(pointer_supported)?;
    let mut app = MonitorApp {
        accounts: accounts::Accounts::new(config.local_accounts),
        usage_open: false,
        listen_url: config.listen_url,
        setup_text: config.setup_text,
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 0,
        recent_selected: 0,
        active_resize: Default::default(),
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: config.shutdown,
        shutdown_complete: config.shutdown_complete,
    };

    app.accounts.enter();
    app.accounts.refresh_limits();
    let run_result = run_monitor_events(&mut terminal, &mut snapshot, &mut app, &mut guard.pointer);
    if run_result.is_err() {
        app.begin_shutdown();
        let state = snapshot();
        let _ = terminal.draw(|frame| {
            render(frame, &mut app, state.snapshot());
        });
        let _ = guard.pointer.reset(terminal.backend_mut());
        app.wait_for_shutdown_completion();
    }
    let _ = guard.pointer.reset(terminal.backend_mut());
    let cursor_result = terminal.show_cursor();
    let exit = run_result?;
    cursor_result?;
    Ok(exit)
}

fn run_monitor_events(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut snapshot: impl FnMut() -> SnapshotUpdate,
    app: &mut MonitorApp,
    pointer: &mut pointer::Pointer,
) -> Result<MonitorExit, anyhow::Error> {
    loop {
        app.accounts.refresh_if_due();
        let update = snapshot();
        let state = update.snapshot();
        app.clamp_selection(state.sessions.len(), state.recent.len());
        app.tick = app.tick.wrapping_add(1);
        let mut targets = MouseTargets::default();
        terminal.draw(|frame| {
            targets = render_with_banner(
                frame,
                app,
                state,
                update.connection_error().map(|e| e.to_string()).as_deref(),
            );
        })?;
        app.refresh_pointer(pointer, terminal.backend_mut(), &targets);
        if app.shutdown_is_complete() {
            return Ok(MonitorExit::ShutdownComplete);
        }
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(exit) =
                        app.handle_key_query(key, state, targets.size, crossterm::terminal::size)?
                    {
                        return Ok(exit);
                    }
                }
                Event::Mouse(mouse) => {
                    let exit =
                        app.handle_mouse_query(mouse, state, &targets, crossterm::terminal::size);
                    app.refresh_pointer(pointer, terminal.backend_mut(), &targets);
                    if let Some(exit) = exit {
                        return Ok(exit);
                    }
                }
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusPane {
    Sessions,
    Recent,
}

impl FocusPane {
    fn next(self) -> Self {
        match self {
            Self::Sessions => Self::Recent,
            Self::Recent => Self::Sessions,
        }
    }
}

enum DetailView {
    Session(DetailSelection<Option<String>>),
    Request(DetailSelection<String>),
}

enum DetailSelection<T> {
    Keyboard,
    Pinned(T),
}

impl<T: PartialEq> DetailSelection<T> {
    fn index<'a>(
        &'a self,
        keyboard_index: usize,
        mut identities: impl ExactSizeIterator<Item = &'a T>,
    ) -> usize {
        match self {
            Self::Keyboard => keyboard_index,
            Self::Pinned(identity) => {
                let unavailable = identities.len();
                identities
                    .position(|candidate| candidate == identity)
                    .unwrap_or(unavailable)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorPhase {
    Running,
    ConfirmingShutdown,
    ShuttingDown,
}

struct MonitorApp {
    accounts: accounts::Accounts,
    usage_open: bool,
    listen_url: String,
    setup_text: String,
    show_setup: bool,
    show_help: bool,
    detail: Option<DetailView>,
    focus: FocusPane,
    selected: usize,
    recent_selected: usize,
    active_resize: mouse::ActiveResize,
    tick: usize,
    phase: MonitorPhase,
    shutdown: Option<oneshot::Sender<()>>,
    shutdown_complete: Option<mpsc::Receiver<()>>,
}

impl MonitorApp {
    fn is_attached(&self) -> bool {
        self.shutdown.is_none() && self.shutdown_complete.is_none()
    }
    fn handle_ctrl_c(&mut self) -> bool {
        if self.phase == MonitorPhase::ShuttingDown {
            true
        } else {
            self.begin_shutdown();
            false
        }
    }

    fn request_shutdown_confirmation(&mut self) {
        if self.phase == MonitorPhase::Running {
            self.phase = MonitorPhase::ConfirmingShutdown;
        }
    }

    fn cancel_shutdown_confirmation(&mut self) {
        if self.phase == MonitorPhase::ConfirmingShutdown {
            self.phase = MonitorPhase::Running;
        }
    }

    fn begin_shutdown(&mut self) {
        if self.phase == MonitorPhase::ShuttingDown {
            return;
        }
        self.phase = MonitorPhase::ShuttingDown;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    fn shutdown_is_complete(&self) -> bool {
        let Some(shutdown_complete) = &self.shutdown_complete else {
            return self.phase == MonitorPhase::ShuttingDown;
        };
        match shutdown_complete.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => true,
            Err(mpsc::TryRecvError::Empty) => false,
        }
    }

    fn wait_for_shutdown_completion(&self) {
        if !self.shutdown_is_complete()
            && let Some(shutdown_complete) = &self.shutdown_complete
        {
            let _ = shutdown_complete.recv();
        }
    }

    fn clamp_selection(&mut self, sessions: usize, recent: usize) {
        self.selected = self.selected.min(sessions.saturating_sub(1));
        self.recent_selected = self.recent_selected.min(recent.saturating_sub(1));
    }

    fn move_down(&mut self, sessions: usize, recent: usize, switch_panes: bool) {
        match self.focus {
            FocusPane::Sessions => {
                if switch_panes && self.selected >= sessions.saturating_sub(1) && recent > 0 {
                    self.focus = FocusPane::Recent;
                    self.recent_selected = 0;
                } else {
                    self.selected = self
                        .selected
                        .saturating_add(1)
                        .min(sessions.saturating_sub(1));
                }
            }
            FocusPane::Recent => {
                self.recent_selected = self
                    .recent_selected
                    .saturating_add(1)
                    .min(recent.saturating_sub(1));
            }
        }
    }

    fn move_up(&mut self, sessions: usize, recent: usize, switch_panes: bool) {
        match self.focus {
            FocusPane::Sessions => self.selected = self.selected.saturating_sub(1),
            FocusPane::Recent => {
                if switch_panes && self.recent_selected == 0 && sessions > 0 {
                    self.focus = FocusPane::Sessions;
                    self.selected = sessions.saturating_sub(1);
                } else {
                    self.recent_selected = self
                        .recent_selected
                        .saturating_sub(1)
                        .min(recent.saturating_sub(1));
                }
            }
        }
    }
}

impl Drop for MonitorApp {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

struct TerminalGuard {
    mouse_capture_attempted: bool,
    pointer: pointer::Pointer,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        pointer::restore(&mut self.pointer, &mut io::stdout(), |writer| {
            if self.mouse_capture_attempted {
                let _ = execute!(writer, DisableMouseCapture);
            }
            let _ = execute!(writer, LeaveAlternateScreen);
            let _ = disable_raw_mode();
        });
    }
}

fn setup_terminal(
    pointer_supported: bool,
) -> Result<(TerminalGuard, Terminal<CrosstermBackend<Stdout>>), anyhow::Error> {
    enable_raw_mode()?;
    let mut guard = TerminalGuard {
        mouse_capture_attempted: false,
        pointer: pointer::Pointer::new(pointer_supported),
    };
    execute!(io::stdout(), EnterAlternateScreen)?;
    guard.mouse_capture_attempted = true;
    execute!(io::stdout(), EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    Ok((guard, terminal))
}

fn render_with_banner(
    frame: &mut ratatui::Frame<'_>,
    app: &mut MonitorApp,
    state: &MonitorSnapshot,
    error: Option<&str>,
) -> MouseTargets {
    let mut targets = render(frame, app, state);
    if let Some(error) = error {
        let area = frame.area();
        let banner = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(1),
            area.width,
            1,
        );
        targets.keys.retain(|(rect, _)| rect.y != banner.y);
        frame.render_widget(ratatui::widgets::Clear, banner);
        frame.render_widget(
            Paragraph::new(format!("Reconnecting; showing last snapshot: {error}"))
                .style(Style::default().fg(YELLOW).bg(BG)),
            banner,
        );
    }
    targets
}

fn render(
    frame: &mut ratatui::Frame<'_>,
    app: &mut MonitorApp,
    state: &MonitorSnapshot,
) -> MouseTargets {
    render_at(
        frame,
        app,
        state,
        std::time::SystemTime::now(),
        jiff::tz::TimeZone::system(),
    )
}

fn render_at(
    frame: &mut ratatui::Frame<'_>,
    app: &mut MonitorApp,
    state: &MonitorSnapshot,
    now: std::time::SystemTime,
    zone: jiff::tz::TimeZone,
) -> MouseTargets {
    let area = frame.area();
    let terminal_state = &cells::dashboard_snapshot(state);
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Percentage(30),
            Constraint::Percentage(20),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Length(1),
        ])
        .split(area);

    let mut targets = MouseTargets {
        size: (area.width, area.height),
        ..Default::default()
    };
    chrome::render_chrome_header(frame, root[0], app, terminal_state, &mut targets);
    if app.usage_open {
        let body = Rect::new(
            area.x,
            root[1].y,
            area.width,
            root[5].y.saturating_sub(root[1].y),
        );
        let panes = usage_stats::layout(body, state, app.accounts.count(), now, zone.clone());
        app.accounts
            .render_at(frame, panes[0], terminal_state, now, zone);
        usage_stats::render(frame, panes[1], panes[2], terminal_state, &state.recent);
        chrome::render_chrome_footer(frame, root[5], app, &mut targets);
        app.accounts.render_confirmation(frame, area);
        if app.show_setup {
            render_setup_overlay(frame, area, &app.setup_text);
        }
        if app.show_help {
            render_help_overlay(frame, area, app.is_attached());
        }
        match app.phase {
            MonitorPhase::Running => {}
            MonitorPhase::ConfirmingShutdown => render_shutdown_confirmation(frame, area),
            MonitorPhase::ShuttingDown => render_shutdown_overlay(frame, area, app.tick),
        }
        return targets;
    }
    let body = Rect::new(
        area.x,
        root[1].y,
        area.width,
        root[5].y.saturating_sub(root[1].y),
    );
    let parts = summary::layout(
        body,
        state,
        detail::height(app, state),
        app.active_resize.height,
    );
    summary::render(frame, parts[0], terminal_state, &app.accounts, now, zone);
    let root = [root[0], parts[1], parts[2], parts[3], parts[4], root[5]];
    let sessions = match &app.detail {
        Some(DetailView::Session(detail)) => {
            let index = detail.index(
                app.selected,
                state.sessions.iter().map(|session| &session.session_id),
            );
            render_session_detail(frame, root[1], terminal_state, index);
            VisibleRows::default()
        }
        Some(DetailView::Request(detail)) => {
            let index = detail.index(
                app.recent_selected,
                state.recent.iter().map(|request| &request.request_id),
            );
            render_request_detail(frame, root[1], terminal_state, index);
            VisibleRows::default()
        }
        None => sessions::render_sessions_with_queue(
            frame,
            root[1],
            &terminal_state.sessions,
            &terminal_state.active,
            app.selected,
            app.focus == FocusPane::Sessions,
            terminal_state.snapshot_at,
        ),
    };
    targets.active = Some((root[2], root[3].height));
    render_active_at(frame, root[2], &terminal_state.active, app.tick, now);
    let recent = render_recent(
        frame,
        root[3],
        &terminal_state.recent,
        app.recent_selected,
        app.focus == FocusPane::Recent,
    );
    let events = render_events(frame, root[4], &terminal_state.recent);
    chrome::render_chrome_footer(frame, root[5], app, &mut targets);

    if app.show_setup {
        render_setup_overlay(frame, area, &app.setup_text);
    }
    if app.show_help {
        render_help_overlay(frame, area, app.is_attached());
    }
    match app.phase {
        MonitorPhase::Running => {}
        MonitorPhase::ConfirmingShutdown => render_shutdown_confirmation(frame, area),
        MonitorPhase::ShuttingDown => render_shutdown_overlay(frame, area, app.tick),
    }
    targets.sessions = sessions;
    targets.recent = recent;
    targets.events = events;
    targets
}

fn mock_setup_text(port: u16, registry: &Registry) -> String {
    format!(
        "Mock mode uses deterministic simulated monitor traffic.\nNo proxy server is listening.\nRun `duolane serve` to start the proxy.\n\n{}",
        setup_text(port, registry)
    )
}

pub fn setup_text(port: u16, registry: &Registry) -> String {
    let model_summary = registry
        .grouped_models()
        .get("codex")
        .map(|models| format!("codex: {} models", models.len()))
        .unwrap_or_default();
    let mut lines = vec![
        format!("Logs: {}", paths::log_file().display()),
        format!("Config: {}", paths::config_dir().display()),
        format!("Providers: {model_summary}"),
    ];
    lines.push(format!(
        "export ANTHROPIC_BASE_URL=\"http://localhost:{port}\""
    ));
    lines.push("export ANTHROPIC_MODEL=\"gpt-6-sol\"".to_string());
    lines.push("export ANTHROPIC_SMALL_FAST_MODEL=\"gpt-6-luna\"".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests;
