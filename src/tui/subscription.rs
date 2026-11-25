pub(super) fn needed_height(
    area: ratatui::layout::Rect,
    snapshot: &crate::monitor::snapshot::MonitorSnapshot,
    count: usize,
    now: std::time::SystemTime,
    zone: jiff::tz::TimeZone,
) -> u16 {
    let claude = 4_u16
        .saturating_add((count.min(u16::MAX as usize / 3) * 3) as u16)
        .saturating_add(u16::from(count > 0));
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

pub(super) fn quota_summary(
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

pub(super) struct Notes<'a> {
    pub(super) focused: bool,
    pub(super) local: bool,
    pub(super) saved: &'a str,
    pub(super) overridden: bool,
    pub(super) focus_is_saved: bool,
    pub(super) status: &'a str,
}

pub(super) fn render(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    snapshot: &crate::monitor::snapshot::MonitorSnapshot,
    now: std::time::SystemTime,
    zone: jiff::tz::TimeZone,
    notes: Notes<'_>,
) {
    use super::usage_limits::{QuotaLayout, quota_layout};
    use ratatui::{
        style::{Modifier, Style},
        text::Line,
        widgets::{Block, Borders, Paragraph},
    };
    let seconds = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
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
        .title_style(if notes.focused {
            Style::default()
                .fg(super::WHITE)
                .bg(super::SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(super::DIM_WHITE)
        })
        .borders(Borders::ALL)
        .style(Style::default().fg(super::DIM_WHITE));
    let inner = detail_block.inner(area);
    frame.render_widget(detail_block, area);
    let saved = format!(
        "Saved: {}",
        if notes.saved.is_empty() {
            "Unknown"
        } else {
            notes.saved
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
            let compact = |fallback: &str, w: Option<&crate::monitor::CodexQuotaWindow>, line| {
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
        if notes.local {
            detail.push(Line::from(saved));
            if notes.overridden {
                detail.push(Line::from("CLAUDE_CONFIG_DIR overrides selection"));
            } else if notes.focus_is_saved {
                detail.push(Line::from("Selected for next launch"));
            }
            if !notes.status.is_empty() {
                detail.push(Line::from(notes.status.to_owned()));
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
