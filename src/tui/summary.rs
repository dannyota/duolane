use super::*;
use ratatui::{
    text::Line,
    widgets::{BorderType, Borders},
};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn height(width: u16) -> u16 {
    match layout::LayoutTier::for_outer_width(width) {
        layout::LayoutTier::Emergency => 0,
        layout::LayoutTier::Narrow | layout::LayoutTier::Medium => 3,
        _ => 5,
    }
}

pub(super) fn render(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &MonitorSnapshot,
    accounts: &accounts::Accounts,
    now: SystemTime,
    zone: jiff::tz::TimeZone,
) {
    if area.height == 0 {
        return;
    }
    let compact = matches!(
        layout::LayoutTier::for_outer_width(area.width),
        layout::LayoutTier::Narrow | layout::LayoutTier::Medium
    );
    let cells = if compact {
        Layout::vertical([Constraint::Length(1); 3]).split(area)
    } else {
        Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(area)
    };
    for (index, (name, provider)) in [("Claude", "anthropic"), ("Codex", "codex")]
        .into_iter()
        .enumerate()
    {
        let active = state
            .active
            .iter()
            .filter(|r| r.provider.as_deref() == Some(provider))
            .count();
        let queued = state
            .active
            .iter()
            .filter(|r| r.provider.as_deref() == Some(provider) && r.queue_place.is_some())
            .count();
        let (wait, rate) = usage_stats::lane_stats(&state.recent, provider);
        let wait = wait.map_or_else(|| "-".to_owned(), |v| format!("{v:.1}s"));
        let rate = rate.map_or_else(|| "-".to_owned(), |v| format!("{v:.1}/s"));
        let stats = format!("{active} active {queued} queued {rate} wait {wait}");
        if compact {
            frame.render_widget(
                Paragraph::new(format!("{name} active {active} queued {queued} {rate}")),
                cells[index],
            );
            continue;
        }
        let mut lines = vec![Line::from(stats.clone())];
        if index == 0 {
            if let Some((profile, limits)) = accounts
                .local
                .then(|| {
                    accounts
                        .limits
                        .iter()
                        .max_by_key(|(_, limits)| limits.observed_at)
                })
                .flatten()
            {
                let profile = crate::text::terminal_text(profile);
                let width = usize::from(cells[index].width.saturating_sub(2))
                    .saturating_sub(stats.len() + 2)
                    .max(4);
                lines[0] = Line::from(format!("{}  {stats}", cells::ellipsize(&profile, width)));
                let seconds = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                lines.push(usage_limits::claude("5h", Some(limits), 18_000, seconds));
                lines.push(usage_limits::claude("week", Some(limits), 604_800, seconds));
            }
        } else if let Some(quota) = state
            .codex_quota
            .as_ref()
            .filter(|q| now.duration_since(q.observed_at).is_ok())
        {
            lines.push(usage_limits::codex_in_zone(
                "Primary",
                quota.primary.as_ref(),
                now,
                zone.clone(),
            ));
            lines.push(usage_limits::codex_in_zone(
                "Secondary",
                quota.secondary.as_ref(),
                now,
                zone.clone(),
            ));
        }
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .title(format!(" {name} "))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(SEPARATOR)),
            ),
            cells[index],
        );
    }
    let (queue, flight, starts, reused, http, origin) =
        state.codex_connections.as_ref().map_or_else(
            || {
                (
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                )
            },
            |c| {
                let total = c.websocket_reused.saturating_add(c.websocket_new);
                (
                    c.queued_attempts.to_string(),
                    c.handshakes_in_flight.to_string(),
                    if c.handshake_spacing_ms == 0 {
                        "-".into()
                    } else {
                        format!("{:.1}/s", 1000.0 / c.handshake_spacing_ms as f64)
                    },
                    if total == 0 {
                        "-".into()
                    } else {
                        format!("{:.0}%", 100.0 * c.websocket_reused as f64 / total as f64)
                    },
                    c.http_fallbacks.to_string(),
                    c.origin_403.to_string(),
                )
            },
        );
    let lines = vec![
        Line::from(format!("queue {queue}  handshakes {flight}")),
        Line::from(format!("starts {starts}  reused {reused}")),
        Line::from(format!("HTTP fallbacks {http}  origin 403 {origin}")),
    ];
    if compact {
        let mut compact = format!("queue {queue} reuse {reused}");
        if state
            .codex_connections
            .as_ref()
            .is_some_and(|c| c.http_fallbacks > 0)
        {
            compact.push_str(&format!(" fallback {http}"));
        }
        if state
            .codex_connections
            .as_ref()
            .is_some_and(|c| c.origin_403 > 0)
        {
            compact.push_str(&format!(" 403 {origin}"));
        }
        frame.render_widget(Paragraph::new(compact), cells[2]);
    } else {
        let color = if state
            .codex_connections
            .as_ref()
            .is_some_and(|c| c.queued_attempts > 0)
        {
            YELLOW
        } else {
            SEPARATOR
        };
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .title(" Connections ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(color)),
            ),
            cells[2],
        );
    }
}

pub(super) fn layout(
    body: Rect,
    state: &MonitorSnapshot,
    detail_height: u16,
) -> std::rc::Rc<[Rect]> {
    let detail_open = detail_height > 0;
    let detail_min = detail_height.min(body.height.saturating_sub(14).max(body.height.min(10)));
    let requested_summary = summary::height(body.width);
    let reserve = if detail_open {
        detail_min
            + u16::from(!state.active.is_empty()) * 4
            + u16::from(!state.recent.is_empty()) * 4
            + 1
    } else {
        0
    };
    let summary_height = if requested_summary <= body.height.saturating_sub(reserve) {
        requested_summary
    } else {
        0
    };
    let remaining = body.height.saturating_sub(summary_height);
    let sessions_height = (state
        .sessions
        .len()
        .saturating_add(3)
        .min(u16::MAX as usize) as u16)
        .min(body.height * 30 / 100)
        .max(detail_min)
        .min(remaining);
    let events_count = requests::event_count(&state.recent);
    let events_height = if events_count == 0 {
        1
    } else {
        (events_count.saturating_add(3).min(u16::MAX as usize) as u16).min(body.height * 25 / 100)
    }
    .min(remaining.saturating_sub(sessions_height));
    let available = remaining.saturating_sub(sessions_height + events_height);
    let active_height =
        (state.active.len().saturating_add(3).min(u16::MAX as usize) as u16).min(available / 2);
    Layout::vertical([
        Constraint::Length(summary_height),
        Constraint::Length(sessions_height),
        Constraint::Length(active_height),
        Constraint::Length(available - active_height),
        Constraint::Length(events_height),
    ])
    .split(body)
}
