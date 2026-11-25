use super::*;

impl MonitorApp {
    pub(super) fn handle_key_sized(
        &mut self,
        key: crossterm::event::KeyEvent,
        state: &MonitorSnapshot,
        size: (u16, u16),
        frame_size: (u16, u16),
    ) -> Option<MonitorExit> {
        if key.code == KeyCode::Enter
            && self.usage_open
            && self.accounts.confirming()
            && size != frame_size
        {
            return None;
        }
        self.handle_key(key, state)
    }

    pub(super) fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        state: &MonitorSnapshot,
    ) -> Option<MonitorExit> {
        match key.code {
            KeyCode::Char('q') if self.is_attached() => return Some(MonitorExit::Detached),
            KeyCode::Char('c')
                if self.is_attached() && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                return Some(MonitorExit::Detached);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.handle_ctrl_c() {
                    return Some(MonitorExit::ForceQuit);
                }
            }
            _ if self.phase == MonitorPhase::ShuttingDown => {}
            KeyCode::Char('y') if self.phase == MonitorPhase::ConfirmingShutdown => {
                self.begin_shutdown()
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q')
                if self.phase == MonitorPhase::ConfirmingShutdown =>
            {
                self.cancel_shutdown_confirmation()
            }
            _ if self.phase == MonitorPhase::ConfirmingShutdown => {}
            KeyCode::Char('q') => self.request_shutdown_confirmation(),
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('b') => self.show_setup = !self.show_setup,
            KeyCode::Char('1') => {
                self.usage_open = false;
                self.accounts.cancel();
            }
            KeyCode::Char('2') => {
                self.usage_open = true;
                self.accounts.enter();
            }
            KeyCode::Char('r') if self.usage_open && !self.show_help && !self.show_setup => {
                self.accounts.enter();
                self.accounts.refresh_limits();
            }
            KeyCode::Esc
                if self.usage_open
                    && self.accounts.confirming()
                    && !self.show_help
                    && !self.show_setup =>
            {
                self.accounts.cancel()
            }
            KeyCode::Enter if self.usage_open && !self.show_help && !self.show_setup => {
                self.accounts.confirm()
            }
            KeyCode::Down
                if self.usage_open
                    && !self.accounts.confirming()
                    && !self.show_help
                    && !self.show_setup =>
            {
                self.accounts.down()
            }
            KeyCode::Up
                if self.usage_open
                    && !self.accounts.confirming()
                    && !self.show_help
                    && !self.show_setup =>
            {
                self.accounts.up()
            }
            _ if self.usage_open && !matches!(key.code, KeyCode::Esc) => {}
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::Down => self.move_down(state.sessions.len(), state.recent.len(), true),
            KeyCode::Char('j') => self.move_down(state.sessions.len(), state.recent.len(), false),
            KeyCode::Up => self.move_up(state.sessions.len(), state.recent.len(), true),
            KeyCode::Char('k') => self.move_up(state.sessions.len(), state.recent.len(), false),
            KeyCode::Right => self.focus = FocusPane::Recent,
            KeyCode::Left => self.focus = FocusPane::Sessions,
            KeyCode::Enter => {
                self.detail = match self.focus {
                    FocusPane::Sessions if !state.sessions.is_empty() => {
                        Some(DetailView::Session(DetailSelection::Keyboard))
                    }
                    FocusPane::Recent if !state.recent.is_empty() => {
                        Some(DetailView::Request(DetailSelection::Keyboard))
                    }
                    _ => None,
                }
            }
            KeyCode::Esc => {
                if self.show_help {
                    self.show_help = false;
                } else if self.show_setup {
                    self.show_setup = false;
                } else {
                    self.detail = None;
                }
            }
            _ => {}
        };
        None
    }
}
