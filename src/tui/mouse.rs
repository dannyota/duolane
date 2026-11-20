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

#[derive(Default)]
pub(super) struct MouseTargets {
    pub(super) sessions: VisibleRows,
    pub(super) recent: VisibleRows,
    pub(super) events: VisibleRows,
}

impl MouseTargets {
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
