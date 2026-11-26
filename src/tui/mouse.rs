use super::*;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::{Block, Row, Table, TableState};

#[derive(Default)]
pub(super) struct VisibleRows {
    body: Rect,
    indices: Vec<usize>,
}

impl VisibleRows {
    fn hit(&self, column: u16, row: u16) -> Option<usize> {
        if !self.body.contains(Position::new(column, row)) {
            return None;
        }
        self.indices.get(usize::from(row - self.body.y)).copied()
    }
}

pub(super) enum MouseTarget {
    Session(usize),
    Request(usize),
}

pub(super) struct ActiveResize {
    pub(super) height: u16,
    dragging: bool,
}

impl Default for ActiveResize {
    fn default() -> Self {
        Self {
            height: 10,
            dragging: false,
        }
    }
}

#[derive(Default)]
pub(super) struct MouseTargets {
    pub(super) size: (u16, u16),
    pub(super) active: Option<(Rect, u16)>,
    pub(super) sessions: VisibleRows,
    pub(super) recent: VisibleRows,
    pub(super) events: VisibleRows,
    pub(super) keys: Vec<(Rect, crossterm::event::KeyCode)>,
}

impl MouseTargets {
    pub(super) fn key(&self, column: u16, row: u16) -> Option<crossterm::event::KeyCode> {
        self.keys
            .iter()
            .find(|(rect, _)| rect.contains(Position::new(column, row)))
            .map(|(_, key)| *key)
    }

    pub(super) fn hit(&self, column: u16, row: u16) -> Option<MouseTarget> {
        self.sessions
            .hit(column, row)
            .map(MouseTarget::Session)
            .or_else(|| {
                self.recent
                    .hit(column, row)
                    .or_else(|| self.events.hit(column, row))
                    .map(MouseTarget::Request)
            })
    }
}

pub(super) fn render_clickable_table(
    frame: &mut Frame<'_>,
    area: Rect,
    table: Table<'_>,
    block: Block<'_>,
    header: Row<'_>,
    state: &mut TableState,
    indices: impl Iterator<Item = usize>,
) -> VisibleRows {
    const HEADER_HEIGHT: u16 = 1;
    let inner = block.inner(area);
    let header_height = HEADER_HEIGHT.min(inner.height);
    let body = Rect::new(
        inner.x,
        inner.y.saturating_add(header_height),
        inner.width,
        inner.height - header_height,
    );
    frame.render_stateful_widget(
        table.block(block).header(header.height(HEADER_HEIGHT)),
        area,
        state,
    );
    let indices = indices
        .skip(state.offset())
        .take(usize::from(body.height))
        .collect();
    VisibleRows { body, indices }
}

impl super::MonitorApp {
    pub(super) fn handle_mouse_query(
        &mut self,
        mouse: MouseEvent,
        state: &MonitorSnapshot,
        targets: &MouseTargets,
        size: impl FnOnce() -> std::io::Result<(u16, u16)>,
    ) -> Option<MonitorExit> {
        if matches!(
            mouse.kind,
            MouseEventKind::Up(MouseButton::Left)
                | MouseEventKind::Moved
                | MouseEventKind::Down(MouseButton::Left)
        ) {
            self.active_resize.dragging = false;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left)
            && !(self.active_resize.dragging
                && mouse.kind == MouseEventKind::Drag(MouseButton::Left))
        {
            return None;
        }
        size()
            .ok()
            .and_then(|size| self.handle_mouse_sized(mouse, state, targets, size))
    }

    pub(super) fn handle_mouse_sized(
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

    pub(super) fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        state: &MonitorSnapshot,
        targets: &MouseTargets,
    ) -> Option<MonitorExit> {
        if matches!(
            mouse.kind,
            MouseEventKind::Up(MouseButton::Left)
                | MouseEventKind::Moved
                | MouseEventKind::Down(MouseButton::Left)
        ) {
            self.active_resize.dragging = false;
        }
        if self.show_help
            || self.accounts.confirming()
            || self.show_setup
            || self.phase != MonitorPhase::Running
            || self.usage_open && self.active_resize.dragging
        {
            self.active_resize.dragging = false;
            return None;
        }
        if self.active_resize.dragging {
            if mouse.kind == MouseEventKind::Drag(MouseButton::Left)
                && let Some((active, recent_height)) = targets.active
            {
                let available = active.height.saturating_add(recent_height);
                if available > 6 {
                    self.active_resize.height = mouse
                        .row
                        .saturating_sub(active.y)
                        .saturating_add(1)
                        .min(available.saturating_sub(3))
                        .max(3);
                }
            }
            return None;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        if let Some((active, _)) = targets.active
            && active.height >= 2
            && mouse.row == active.y + active.height - 1
            && mouse.column >= active.x
            && mouse.column < active.right()
        {
            self.active_resize.dragging = true;
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
}
