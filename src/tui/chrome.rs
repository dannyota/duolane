use super::*;

#[cfg(test)]
pub(super) fn render_header(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &MonitorApp,
    state: &MonitorSnapshot,
) {
    render_chrome_header(frame, area, app, state, &mut MouseTargets::default());
}

pub(super) fn render_chrome_header(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &MonitorApp,
    state: &MonitorSnapshot,
    targets: &mut MouseTargets,
) {
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(PANEL_BG)),
        area,
    );
    let mut x = area.x;
    let right = area.right();
    let title = " duolane ";
    let w = (title.len() as u16).min(right.saturating_sub(x));
    frame.render_widget(
        Paragraph::new(title).style(
            Style::default()
                .fg(TEAL)
                .bg(PANEL_BG)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(x, area.y, w, area.height),
    );
    x += w;
    for (label, key, active) in [
        (" 1 Overview ", KeyCode::Char('1'), !app.usage_open),
        (" 2 Usage ", KeyCode::Char('2'), app.usage_open),
    ] {
        let width = (label.len() as u16).min(right.saturating_sub(x));
        let rect = Rect::new(x, area.y, width, area.height);
        let style = if active {
            Style::default()
                .fg(BG)
                .bg(TEAL)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(WHITE).bg(SELECTED_BG)
        };
        frame.render_widget(Paragraph::new(label).style(style), rect);
        targets.keys.push((rect, key));
        x += width;
    }
    let info = [
        format!("  {}", app.listen_url),
        format!("  uptime {}", format_duration(state.uptime)),
        format!("  sessions {}", state.sessions.len()),
        format!("  active {}", state.active.len()),
    ];
    let mut end = info.len();
    while end > 0
        && info[..end].iter().map(String::len).sum::<usize>() > usize::from(right.saturating_sub(x))
    {
        end -= 1;
    }
    let text = info[..end].concat();
    let width = text.len() as u16;
    if width > 0 {
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(DIM_WHITE).bg(PANEL_BG)),
            Rect::new(right - width, area.y, width, area.height),
        );
    }
}

pub(super) fn render_chrome_footer(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &MonitorApp,
    targets: &mut MouseTargets,
) {
    frame.render_widget(Paragraph::new("").style(Style::default().bg(BG)), area);
    let hints = [
        (
            "q",
            if app.is_attached() {
                " detach  "
            } else {
                " quit  "
            },
            KeyCode::Char('q'),
        ),
        ("?", " help  ", KeyCode::Char('?')),
        ("b", " setup  ", KeyCode::Char('b')),
        ("Tab", " pane  ", KeyCode::Tab),
        ("Enter", " open  ", KeyCode::Enter),
        ("1", " view  ", KeyCode::Char('1')),
        ("2", " view  ", KeyCode::Char('2')),
    ];
    let mut x = area.x.saturating_add(1);
    for (label, description, key) in hints.into_iter().chain(app.usage_open.then_some((
        "r",
        " refresh  ",
        KeyCode::Char('r'),
    ))) {
        let width = (label.len() as u16).min(area.right().saturating_sub(x));
        let rect = Rect::new(x, area.y, width, area.height);
        frame.render_widget(
            Paragraph::new(label).style(if key == KeyCode::Enter {
                Style::default().fg(DIM)
            } else {
                Style::default().fg(TEAL).bg(SELECTED_BG)
            }),
            rect,
        );
        if key != KeyCode::Enter {
            targets.keys.push((rect, key));
        }
        x = x.saturating_add(width);
        let width = (description.len() as u16).min(area.right().saturating_sub(x));
        frame.render_widget(
            Paragraph::new(description).style(Style::default().fg(DIM)),
            Rect::new(x, area.y, width, area.height),
        );
        x = x.saturating_add(width);
        if key == KeyCode::Char('r') && app.accounts.local() {
            let text = "a add  l sign in  d remove  s switch";
            let width = (text.len() as u16).min(area.right().saturating_sub(x));
            frame.render_widget(
                Paragraph::new(text).style(Style::default().fg(DIM)),
                Rect::new(x, area.y, width, area.height),
            );
            x = x.saturating_add(width);
        }
        if key == KeyCode::Char('b') {
            let text = "arrows/j/k navigate  ";
            let width = (text.len() as u16).min(area.right().saturating_sub(x));
            frame.render_widget(
                Paragraph::new(text).style(Style::default().fg(DIM)),
                Rect::new(x, area.y, width, area.height),
            );
            x = x.saturating_add(width);
        }
    }
}
