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
mod profile_rows;
mod requests;
mod sessions;
mod subscription;
mod usage_limits;
mod usage_stats;

use cells::format_duration;
#[cfg(test)]
use chrome::render_header;
use detail::{render_request_detail, render_session_detail};
use mouse::{MouseTarget, MouseTargets, VisibleRows};
use overlay::{
    render_help_overlay, render_setup_overlay, render_shutdown_confirmation,
    render_shutdown_overlay,
};
use requests::{render_active, render_events, render_recent};
use sessions::render_sessions;

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
    let (_guard, mut terminal) = setup_terminal()?;
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
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: config.shutdown,
        shutdown_complete: config.shutdown_complete,
    };

    app.accounts.enter();
    app.accounts.refresh_limits();
    let run_result = run_monitor_events(&mut terminal, &mut snapshot, &mut app);
    if run_result.is_err() {
        app.begin_shutdown();
        let state = snapshot();
        let _ = terminal.draw(|frame| {
            render(frame, &mut app, state.snapshot());
        });
        app.wait_for_shutdown_completion();
    }
    let cursor_result = terminal.show_cursor();
    let exit = run_result?;
    cursor_result?;
    Ok(exit)
}

fn run_monitor_events(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut snapshot: impl FnMut() -> SnapshotUpdate,
    app: &mut MonitorApp,
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
                    if let Some(exit) =
                        app.handle_mouse_query(mouse, state, &targets, crossterm::terminal::size)
                    {
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
    tick: usize,
    phase: MonitorPhase,
    shutdown: Option<oneshot::Sender<()>>,
    shutdown_complete: Option<mpsc::Receiver<()>>,
}

impl MonitorApp {
    fn handle_mouse_query(
        &mut self,
        mouse: MouseEvent,
        state: &MonitorSnapshot,
        targets: &MouseTargets,
        size: impl FnOnce() -> std::io::Result<(u16, u16)>,
    ) -> Option<MonitorExit> {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        size()
            .ok()
            .and_then(|size| self.handle_mouse_sized(mouse, state, targets, size))
    }

    fn handle_mouse_sized(
        &mut self,
        mouse: MouseEvent,
        state: &MonitorSnapshot,
        targets: &MouseTargets,
        size: (u16, u16),
    ) -> Option<MonitorExit> {
        if targets.size != size {
            return None;
        }
        self.handle_mouse(mouse, state, targets)
    }

    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        state: &MonitorSnapshot,
        targets: &MouseTargets,
    ) -> Option<MonitorExit> {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left)
            || self.show_help
            || self.accounts.confirming()
            || self.show_setup
            || self.phase != MonitorPhase::Running
        {
            return None;
        }
        if let Some(code) = targets.key(mouse.column, mouse.row) {
            return self.handle_key(
                crossterm::event::KeyEvent::new(code, KeyModifiers::NONE),
                state,
            );
        }
        if self.usage_open {
            self.accounts.click(mouse.column, mouse.row);
            return None;
        }
        match targets.hit(mouse.column, mouse.row) {
            Some(MouseTarget::Session(index)) => {
                if let Some(session) = state.sessions.get(index) {
                    self.focus = FocusPane::Sessions;
                    self.selected = index;
                    self.detail = Some(DetailView::Session(DetailSelection::Pinned(
                        session.session_id.clone(),
                    )));
                }
            }
            Some(MouseTarget::Request(index)) => {
                if let Some(request) = state.recent.get(index) {
                    self.focus = FocusPane::Recent;
                    self.recent_selected = index;
                    self.detail = Some(DetailView::Request(DetailSelection::Pinned(
                        request.request_id.clone(),
                    )));
                }
            }
            None => {}
        }
        None
    }

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
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.mouse_capture_attempted {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn setup_terminal() -> Result<(TerminalGuard, Terminal<CrosstermBackend<Stdout>>), anyhow::Error> {
    enable_raw_mode()?;
    let mut guard = TerminalGuard {
        mouse_capture_attempted: false,
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
        None => render_sessions(
            frame,
            root[1],
            &terminal_state.sessions,
            app.selected,
            app.focus == FocusPane::Sessions,
            terminal_state.snapshot_at,
        ),
    };
    render_active(frame, root[2], &terminal_state.active, app.tick);
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
mod tests {
    use super::*;

    #[test]
    fn accounts_read_only_modes_never_expose_local_names() {
        let mut accounts = accounts::Accounts::new(false);
        accounts.enter();
        assert!(accounts.names().is_empty());
        accounts.confirm();
        assert!(!accounts.confirming());
    }
    use crate::monitor::snapshot::MonitorSnapshot;
    use crate::monitor::{EndpointKind, MonitorHandle, mock_state};
    use crate::tui::layout::{ColumnSpec, LayoutTier, MODEL_WIDE_WIDTH, PROVIDER_WIDTH};
    use crate::tui::requests::{
        ActiveColumn, EventColumn, RecentColumn, active_columns, event_columns, recent_columns,
        render_active, render_events, render_recent,
    };
    use crate::tui::sessions::{SessionColumn, render_sessions, session_columns};
    use ratatui::Terminal;
    use ratatui::layout::Alignment;
    use ratatui::{backend::TestBackend, buffer::Buffer};
    use std::time::SystemTime;

    pub(super) fn draw<R>(
        width: u16,
        height: u16,
        render: impl FnOnce(&mut ratatui::Frame<'_>) -> R,
    ) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    pub(super) fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn placeholder_position(buffer: &Buffer, placeholder: &str) -> Option<(u16, u16)> {
        let symbols = placeholder
            .chars()
            .map(|character| character.to_string())
            .collect::<Vec<_>>();
        (0..buffer.area.height).find_map(|y| {
            (0..buffer.area.width).find_map(|x| {
                symbols
                    .iter()
                    .enumerate()
                    .all(|(offset, symbol)| {
                        x + (offset as u16) < buffer.area.width
                            && buffer[(x + offset as u16, y)].symbol() == symbol
                    })
                    .then_some((x, y))
            })
        })
    }

    pub(super) fn assert_centered(buffer: &Buffer, placeholder: &str, expected_y: u16) {
        let (x, y) = placeholder_position(buffer, placeholder).unwrap();
        let left_space = x.saturating_sub(1);
        let right_space = buffer
            .area
            .width
            .saturating_sub(x + placeholder.chars().count() as u16 + 1);
        assert_eq!(y, expected_y);
        assert!(left_space.abs_diff(right_space) <= 1);
    }

    pub(super) fn headers<K>(columns: &[ColumnSpec<K>]) -> Vec<&'static str> {
        columns.iter().map(|column| column.header).collect()
    }

    pub(super) fn fixed_budget<K>(columns: &[ColumnSpec<K>]) -> u16 {
        let widths = columns
            .iter()
            .map(|column| match column.width {
                layout::ColumnWidth::Fixed(width) => width,
                layout::ColumnWidth::Flex(_) => 0,
            })
            .sum::<u16>();
        widths.saturating_add(columns.len().saturating_sub(1) as u16)
    }

    pub(super) fn fixed_width<K: Copy + PartialEq>(
        columns: &[ColumnSpec<K>],
        key: K,
    ) -> Option<u16> {
        columns
            .iter()
            .find(|column| column.key == key)
            .and_then(|column| match column.width {
                layout::ColumnWidth::Fixed(width) => Some(width),
                layout::ColumnWidth::Flex(_) => None,
            })
    }

    pub(super) fn flex_count<K>(columns: &[ColumnSpec<K>]) -> usize {
        columns
            .iter()
            .filter(|column| matches!(column.width, layout::ColumnWidth::Flex(_)))
            .count()
    }

    pub(super) fn alignment<K: Copy + PartialEq>(
        columns: &[ColumnSpec<K>],
        key: K,
    ) -> Option<Alignment> {
        columns
            .iter()
            .find(|column| column.key == key)
            .map(|column| column.alignment)
    }

    #[test]
    fn request_tables_share_time_status_provider_model_rhythm() {
        assert_eq!(
            headers(&active_columns(LayoutTier::Medium))[..4],
            ["Started", "Status", "Provider", "Model"]
        );
        assert_eq!(
            headers(&recent_columns(LayoutTier::Medium))[..4],
            ["Finished", "Code", "Provider", "Model"]
        );
        assert_eq!(
            headers(&event_columns(LayoutTier::Medium))[..4],
            ["Time", "Code", "Provider", "Model"]
        );
    }

    #[test]
    fn wide_tables_use_model_caps_and_shared_provider_widths() {
        let sessions = session_columns(LayoutTier::Wide, true);
        let active = active_columns(LayoutTier::Wide);
        let recent = recent_columns(LayoutTier::Wide);
        let events = event_columns(LayoutTier::Wide);

        assert_eq!(
            fixed_width(&sessions, SessionColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(fixed_width(&active, ActiveColumn::Model), Some(5));
        assert_eq!(fixed_width(&recent, RecentColumn::Model), Some(5));
        assert_eq!(
            fixed_width(&events, EventColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&active, ActiveColumn::Provider),
            Some(PROVIDER_WIDTH)
        );
        assert_eq!(
            fixed_width(&recent, RecentColumn::Provider),
            Some(PROVIDER_WIDTH)
        );
    }

    #[test]
    fn responsive_schemas_fit_their_minimum_terminal_widths() {
        assert!(fixed_budget(&session_columns(LayoutTier::Emergency, false)) <= 75);
        assert!(fixed_budget(&session_columns(LayoutTier::Narrow, false)) <= 76);
        assert!(fixed_budget(&session_columns(LayoutTier::Medium, false)) <= 88);
        assert!(fixed_budget(&session_columns(LayoutTier::Expanded, false)) <= 118);
        assert!(fixed_budget(&session_columns(LayoutTier::Wide, true)) <= 168);

        assert!(fixed_budget(&active_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&active_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&active_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&active_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&active_columns(LayoutTier::Wide)) <= 152);

        assert!(fixed_budget(&recent_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&recent_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&recent_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&recent_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&recent_columns(LayoutTier::Wide)) <= 152);

        assert!(fixed_budget(&event_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&event_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&event_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&event_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&event_columns(LayoutTier::Wide)) <= 152);
    }

    #[test]
    fn each_schema_has_one_meaningful_flexible_column() {
        for tier in [
            LayoutTier::Emergency,
            LayoutTier::Narrow,
            LayoutTier::Medium,
            LayoutTier::Expanded,
            LayoutTier::Wide,
        ] {
            let sessions = session_columns(tier, tier == LayoutTier::Wide);
            let active = active_columns(tier);
            let recent = recent_columns(tier);
            let events = event_columns(tier);

            assert_eq!(flex_count(&sessions), 1);
            assert_eq!(flex_count(&active), 1);
            assert_eq!(flex_count(&recent), 1);
            assert_eq!(flex_count(&events), 1);
            assert_eq!(
                sessions
                    .iter()
                    .filter(|column| column.header.is_empty())
                    .count(),
                1
            );
            assert_eq!(
                active
                    .iter()
                    .filter(|column| column.header.is_empty())
                    .count(),
                usize::from(matches!(tier, LayoutTier::Expanded | LayoutTier::Wide))
            );
            assert_eq!(
                recent
                    .iter()
                    .filter(|column| column.header.is_empty())
                    .count(),
                usize::from(tier == LayoutTier::Expanded)
            );
            assert!(events.iter().all(|column| !column.header.is_empty()));
        }
    }

    #[test]
    fn metric_columns_are_right_aligned() {
        let sessions = session_columns(LayoutTier::Wide, true);
        for key in [
            SessionColumn::Active,
            SessionColumn::Requests,
            SessionColumn::Failures,
            SessionColumn::Input,
            SessionColumn::Output,
            SessionColumn::Rate,
        ] {
            assert_eq!(alignment(&sessions, key), Some(Alignment::Right));
        }

        let active = active_columns(LayoutTier::Wide);
        for key in [
            ActiveColumn::Input,
            ActiveColumn::Output,
            ActiveColumn::Rate,
            ActiveColumn::Elapsed,
        ] {
            assert_eq!(alignment(&active, key), Some(Alignment::Right));
        }

        let recent = recent_columns(LayoutTier::Wide);
        for key in [
            RecentColumn::Code,
            RecentColumn::Latency,
            RecentColumn::Rate,
            RecentColumn::Input,
            RecentColumn::Output,
        ] {
            assert_eq!(alignment(&recent, key), Some(Alignment::Right));
        }
    }

    #[test]
    fn narrow_schemas_use_available_space_for_context() {
        let sessions = session_columns(LayoutTier::Narrow, false);
        assert!(
            sessions
                .iter()
                .any(|column| column.key == SessionColumn::Project)
        );
        assert!(
            sessions
                .iter()
                .any(|column| column.key == SessionColumn::Target)
        );

        let active = active_columns(LayoutTier::Narrow);
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Provider)
        );
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Model)
        );
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Effort)
        );

        let recent = recent_columns(LayoutTier::Narrow);
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Provider)
        );
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Input)
        );
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Output)
        );

        let events = event_columns(LayoutTier::Emergency);
        assert_eq!(headers(&events), ["Time", "Code", "Message"]);
    }

    #[test]
    fn empty_tables_hide_columns_and_center_placeholders() {
        let sessions = draw(40, 9, |frame| {
            render_sessions(frame, frame.area(), &[], 0, true, SystemTime::now())
        });
        let sessions_text = buffer_text(&sessions);
        assert_centered(&sessions, "No sessions", 4);
        assert!(!sessions_text.contains("provider"));
        assert!(sessions_text.contains("No sessions"));

        let active = draw(27, 6, |frame| render_active(frame, frame.area(), &[], 0));
        let active_text = buffer_text(&active);
        assert_centered(&active, "No active requests", 2);
        assert!(!active_text.contains("started"));
        assert!(active_text.contains("No active requests"));

        let recent = draw(40, 9, |frame| {
            render_recent(frame, frame.area(), &[], 0, false)
        });
        let recent_text = buffer_text(&recent);
        assert_centered(&recent, "No recent requests", 4);
        assert!(!recent_text.contains("finished"));
        assert!(recent_text.contains("No recent requests"));

        let events = draw(40, 9, |frame| render_events(frame, frame.area(), &[]));
        let events_text = buffer_text(&events);
        assert_centered(&events, "No events", 4);
        assert!(!events_text.contains("time"));
        assert!(events_text.contains("No events"));
    }

    #[test]
    fn populated_tables_render_rows_without_placeholders() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("sess-1".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.project_resolved("request-1", "example-project");
        monitor.provider_selected(
            "request-1",
            "codex",
            "gpt-5.6-sol",
            Some("high".to_string()),
        );
        let active_state: MonitorSnapshot = monitor.snapshot().into();

        let sessions = draw(170, 8, |frame| {
            render_sessions(
                frame,
                frame.area(),
                &active_state.sessions,
                0,
                true,
                active_state.snapshot_at,
            )
        });
        let sessions_text = buffer_text(&sessions);
        assert!(sessions_text.contains("Provider"));
        assert!(sessions_text.contains("Project"));
        assert!(sessions_text.contains("example-project"));
        assert!(sessions_text.contains("sess-1"));
        assert!(!sessions_text.contains("No sessions"));

        let active = draw(120, 8, |frame| {
            render_active(frame, frame.area(), &active_state.active, 0)
        });
        let active_text = buffer_text(&active);
        assert!(active_text.contains("Started"));
        assert!(active_text.contains("gpt-5.6-sol"));
        assert!(!active_text.contains("No active requests"));

        monitor.request_completed("request-1", 200, Some(100), Some(25));
        let completed_state: MonitorSnapshot = monitor.snapshot().into();
        let recent = draw(140, 8, |frame| {
            render_recent(frame, frame.area(), &completed_state.recent, 0, false)
        });
        let recent_text = buffer_text(&recent);
        assert!(recent_text.contains("Finished"));
        assert!(recent_text.contains("200"));
        assert!(!recent_text.contains("No recent requests"));

        let events = draw(100, 8, |frame| {
            render_events(frame, frame.area(), &completed_state.recent)
        });
        assert!(buffer_text(&events).contains("No events"));
    }

    #[test]
    fn selected_rows_scroll_into_table_viewports() {
        let state: MonitorSnapshot = mock_state().into();
        let sessions = (0..12)
            .map(|index| {
                let mut session = state.sessions[0].clone();
                session.session_id = Some(format!("row-{index:04}"));
                session
            })
            .collect::<Vec<_>>();
        let session_buffer = draw(120, 6, |frame| {
            render_sessions(frame, frame.area(), &sessions, 11, true, state.snapshot_at)
        });
        let session_text = buffer_text(&session_buffer);
        assert!(session_text.contains("row-0011"), "{session_text}");
        assert!(!session_text.contains("row-0000"), "{session_text}");

        let recent = (0..12)
            .map(|index| {
                let mut request = state.recent[0].clone();
                request.project = Some(format!("row-{index:04}"));
                request
            })
            .collect::<Vec<_>>();
        let recent_buffer = draw(120, 6, |frame| {
            render_recent(frame, frame.area(), &recent, 11, true)
        });
        let recent_text = buffer_text(&recent_buffer);
        assert!(recent_text.contains("row-0011"), "{recent_text}");
        assert!(!recent_text.contains("row-0000"), "{recent_text}");
    }

    #[test]
    fn dashboard_shows_codex_sources_at_24_rows() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("done", None, None, EndpointKind::Messages);
        monitor.provider_selected("done", "codex", "model", None);
        monitor.codex_source("done", "api key (from chatgpt: usage limit)");
        monitor.request_completed("done", 200, None, None);
        monitor.request_started("active", None, None, EndpointKind::Messages);
        monitor.provider_selected("active", "codex", "model", None);
        monitor.codex_source("active", "chatgpt");
        let state: MonitorSnapshot = monitor.snapshot().into();
        let mut app = MonitorApp {
            accounts: accounts::Accounts::new(false),
            usage_open: false,
            listen_url: "test".into(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: Some(DetailView::Request(DetailSelection::Keyboard)),
            focus: FocusPane::Recent,
            selected: 0,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: None,
        };
        let buffer = draw(140, 24, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&buffer);
        assert!(text.contains("from chatgpt: usage limit"));
        assert!(text.matches("chatgpt").count() >= 2);
        assert!(text.matches("api key").count() >= 2);
    }

    #[test]
    fn mock_state_renders_representative_panes_at_wide_width() {
        let state: MonitorSnapshot = mock_state().into();
        let mut app = MonitorApp {
            accounts: accounts::Accounts::new(false),
            usage_open: false,
            listen_url: "mock://tui-demo".to_string(),
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

        let buffer = draw(180, 48, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&buffer);

        assert!(text.contains("mock://tui-demo"), "{text}");
        assert!(text.contains("duolane"), "{text}");
        assert!(text.contains("streaming"), "{text}");
        assert!(text.contains("gpt-5.6-terra"), "{text}");
        assert!(text.contains("upstream connection closed"), "{text}");
    }

    #[test]
    fn render_removes_control_characters_before_layout() {
        let monitor = MonitorHandle::default();
        let model = format!("gpt-{}5.6-terra", "\u{1b}".repeat(30));
        monitor.request_started("req", Some("s\u{7}1".into()), None, EndpointKind::Messages);
        monitor.provider_selected("req", "codex", model, None);
        monitor.request_failed("req", Some(502), "upstream\u{1b}[2J\u{9b}31m closed");
        let state: MonitorSnapshot = monitor.snapshot().into();
        let mut app = MonitorApp {
            accounts: accounts::Accounts::new(false),
            usage_open: false,
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Recent,
            selected: 0,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: None,
        };

        let buffer = draw(180, 48, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&buffer);

        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| !cell.symbol().contains(char::is_control)),
            "{text:?}"
        );
        assert!(text.contains("gpt-5.6-terra"), "{text}");
        assert!(text.contains("upstream[2J31m closed"), "{text}");
    }

    #[test]
    fn shutdown_confirmation_can_be_cancelled_before_signalling_server() {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let mut app = MonitorApp {
            accounts: accounts::Accounts::new(false),
            usage_open: false,
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: Some(shutdown_tx),
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.request_shutdown_confirmation();
        assert_eq!(app.phase, MonitorPhase::ConfirmingShutdown);
        assert!(matches!(
            shutdown_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        let state: MonitorSnapshot = MonitorHandle::default().snapshot().into();
        let screen = draw(80, 24, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&screen);
        assert!(text.contains("Shut down proxy?"), "{text}");
        assert!(text.contains("y confirm"), "{text}");
        assert!(text.contains("n/Esc/q cancel"), "{text}");

        app.cancel_shutdown_confirmation();
        assert_eq!(app.phase, MonitorPhase::Running);
        assert!(matches!(
            shutdown_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        app.request_shutdown_confirmation();
        app.begin_shutdown();
        assert_eq!(app.phase, MonitorPhase::ShuttingDown);
        assert_eq!(shutdown_rx.try_recv(), Ok(()));
    }

    #[test]
    fn ctrl_c_starts_shutdown_then_requests_force_quit() {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let (_shutdown_complete_tx, shutdown_complete_rx) = mpsc::channel();
        let mut app = MonitorApp {
            accounts: accounts::Accounts::new(false),
            usage_open: false,
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: Some(shutdown_tx),
            shutdown_complete: Some(shutdown_complete_rx),
        };

        assert!(!app.handle_ctrl_c());
        assert!(app.handle_ctrl_c());

        assert_eq!(app.phase, MonitorPhase::ShuttingDown);
        assert_eq!(shutdown_rx.try_recv(), Ok(()));
        let state: MonitorSnapshot = MonitorHandle::default().snapshot().into();
        let screen = draw(80, 24, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&screen);
        assert!(text.contains("Shutting down..."));
        assert!(text.contains("Press Ctrl-C to force quit"));
    }

    #[test]
    fn shutdown_completion_accepts_notification_and_sender_drop() {
        let (complete_tx, complete_rx) = mpsc::channel();
        let app = MonitorApp {
            accounts: accounts::Accounts::new(false),
            usage_open: false,
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
            shutdown_complete: Some(complete_rx),
        };

        assert!(!app.shutdown_is_complete());
        complete_tx.send(()).unwrap();
        assert!(app.shutdown_is_complete());

        let (complete_tx, complete_rx) = mpsc::channel();
        let mut app = app;
        app.shutdown_complete = Some(complete_rx);
        drop(complete_tx);
        assert!(app.shutdown_is_complete());
    }

    #[test]
    fn header_renders_configured_listen_url() {
        let app = MonitorApp {
            accounts: accounts::Accounts::new(false),
            usage_open: false,
            listen_url: "http://[::]:18765".to_string(),
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
            shutdown_complete: Some(mpsc::channel().1),
        };
        let state: MonitorSnapshot = MonitorHandle::default().snapshot().into();

        let header = draw(120, 1, |frame| {
            render_header(frame, frame.area(), &app, &state)
        });

        assert!(buffer_text(&header).contains("http://[::]:18765"));
        assert!(buffer_text(&header).contains("active"));
    }

    #[test]
    fn clamp_selection_caps_to_available_sessions() {
        let mut app = MonitorApp {
            accounts: accounts::Accounts::new(false),
            usage_open: false,
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 10,
            recent_selected: 10,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.clamp_selection(3, 4);
        assert_eq!(app.selected, 2);
        assert_eq!(app.recent_selected, 3);

        app.clamp_selection(0, 0);
        assert_eq!(app.selected, 0);
        assert_eq!(app.recent_selected, 0);
    }

    #[test]
    fn arrow_navigation_moves_between_focus_panes_at_edges() {
        let mut app = MonitorApp {
            accounts: accounts::Accounts::new(false),
            usage_open: false,
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 1,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.move_down(2, 3, true);
        assert_eq!(app.focus, FocusPane::Recent);
        assert_eq!(app.recent_selected, 0);

        app.move_up(2, 3, true);
        assert_eq!(app.focus, FocusPane::Sessions);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn vim_navigation_stays_within_focused_pane() {
        let mut app = MonitorApp {
            accounts: accounts::Accounts::new(false),
            usage_open: false,
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 1,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.move_down(2, 3, false);
        assert_eq!(app.focus, FocusPane::Sessions);
        assert_eq!(app.selected, 1);

        app.focus = FocusPane::Recent;
        app.move_up(2, 3, false);
        assert_eq!(app.focus, FocusPane::Recent);
        assert_eq!(app.recent_selected, 0);
    }
}
