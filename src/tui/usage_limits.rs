use super::*;
use ratatui::text::{Line, Span};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn bar(value: Option<f64>, lane: Color, decimals: bool) -> Vec<Span<'static>> {
    let Some(value) = value.filter(|v| v.is_finite() && (0.0..=100.0).contains(v)) else {
        return vec![Span::raw("Unknown")];
    };
    let color = if value >= 80.0 {
        RED
    } else if value >= 50.0 {
        YELLOW
    } else {
        lane
    };
    let filled = (value / 10.0).round() as usize;
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled("░".repeat(10 - filled), Style::default().fg(DIM)),
        Span::raw(if decimals {
            format!(" {value:.1}%")
        } else {
            format!(" {value:.0}%")
        }),
    ]
}

#[cfg(unix)]
pub(super) fn claude(
    label: &str,
    limits: Option<&crate::claude_accounts::limits::Limits>,
    length: u64,
    now: u64,
) -> Line<'static> {
    let window = limits.and_then(|limits| {
        if length == 18_000 {
            limits.five_hour.as_ref()
        } else {
            limits.seven_day.as_ref()
        }
    });
    let valid = window.filter(|w| w.used.is_finite() && (0.0..=100.0).contains(&w.used));
    let value = valid.zip(limits).map(|(w, l)| {
        crate::claude_accounts::roster::percent(w, l.observed_at, length, now) as f64
    });
    let mut spans = vec![Span::raw(format!("  {label:<5} "))];
    spans.extend(bar(value, PURPLE, false));
    let reset = valid
        .and_then(|w| w.resets_at)
        .filter(|v| *v > now)
        .map(|v| crate::claude_accounts::roster::span(v - now))
        .unwrap_or_else(|| "Unknown".into());
    spans.push(Span::raw(format!("  reset {reset}")));
    Line::from(spans)
}

#[cfg(not(unix))]
pub(super) fn claude(
    label: &str,
    _: Option<&crate::claude_accounts::limits::Limits>,
    _: u64,
    _: u64,
) -> Line<'static> {
    Line::from(format!("  {label:<5} Unknown"))
}

pub(super) fn codex_label(
    label: &str,
    window: Option<&crate::monitor::CodexQuotaWindow>,
) -> String {
    match window.and_then(|w| w.window_minutes) {
        Some(300) => "5h".to_owned(),
        Some(10080) => "week".to_owned(),
        Some(n) if (1..=525_600_000).contains(&n) => format!("{label} ({n}m)"),
        _ => label.to_owned(),
    }
}

#[cfg(test)]
pub(super) fn codex(
    label: &str,
    window: Option<&crate::monitor::CodexQuotaWindow>,
    now: SystemTime,
) -> Line<'static> {
    codex_in_zone(label, window, now, jiff::tz::TimeZone::system())
}

pub(super) fn codex_in_zone(
    label: &str,
    window: Option<&crate::monitor::CodexQuotaWindow>,
    now: SystemTime,
    zone: jiff::tz::TimeZone,
) -> Line<'static> {
    let label = codex_label(label, window);
    let mut spans = vec![Span::raw(format!("  {label:<9} "))];
    spans.extend(bar(window.and_then(|w| w.used_percent), TEAL, true));
    let reset = window
        .and_then(|w| w.reset_at)
        .filter(|v| (1..=253_402_300_799).contains(v))
        .and_then(|v| UNIX_EPOCH.checked_add(Duration::from_secs(v)))
        .map(|time| {
            let date = cells::format_system_time_in_zone(time, zone.clone());
            match time.duration_since(now) {
                Ok(left) => format!("{date} (in {})", cells::format_duration(left)),
                Err(_) => format!("{date} (passed)"),
            }
        })
        .unwrap_or_else(|| "Unknown".into());
    spans.push(Span::raw(format!("  reset {reset}")));
    Line::from(spans)
}

pub(super) fn compact(label: &str, line: Line<'static>) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(format!(" {label} "))];
    if line.spans.len() < 5 {
        spans.push(Span::raw("Unknown"));
        return spans;
    }
    let filled = line.spans[1].content.chars().count();
    let count = (filled * 4 + 5) / 10;
    spans.push(Span::styled("█".repeat(count), line.spans[1].style));
    spans.push(Span::styled("░".repeat(4 - count), line.spans[2].style));
    spans.push(line.spans[3].clone());
    spans
}

#[cfg(unix)]
fn relative_span(seconds: u64) -> String {
    crate::claude_accounts::roster::span(seconds)
}

#[cfg(not(unix))]
fn relative_span(seconds: u64) -> String {
    match seconds {
        0..60 => "<1m".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h{}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d{}h", seconds / 86400, seconds % 86400 / 3600),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum QuotaLayout {
    Full,
    TwoLines,
    OneLine,
}

pub(super) fn quota_layout(full_height: u16, available_height: u16) -> QuotaLayout {
    if full_height <= available_height {
        QuotaLayout::Full
    } else if available_height >= 2 {
        QuotaLayout::TwoLines
    } else {
        QuotaLayout::OneLine
    }
}

pub(super) struct CompactWindow {
    pub label: String,
    pub line: Line<'static>,
    pub reset: Option<u64>,
}

pub(super) fn compact_line(
    name: &str,
    style: Style,
    windows: impl IntoIterator<Item = CompactWindow>,
    width: usize,
    now: u64,
) -> Line<'static> {
    let mut parts = Vec::new();
    let mut bars = Vec::new();
    for window in windows {
        let spans = compact(&window.label, window.line);
        let value = if spans.len() >= 4 {
            spans[3].content.trim().to_owned()
        } else {
            "Unknown".into()
        };
        let reset = window
            .reset
            .filter(|v| (1..=253_402_300_799).contains(v))
            .map(|v| {
                if v <= now {
                    "passed".into()
                } else {
                    relative_span(v - now)
                }
            })
            .unwrap_or_else(|| "-".into());
        parts.push(format!(" {} {} {}", window.label, value, reset));
        bars.push(if spans.len() >= 4 {
            spans[1..3].to_vec()
        } else {
            Vec::new()
        });
    }
    let mut required: usize = parts.iter().map(|part| cells::display_width(part)).sum();
    if name.is_empty() && parts.len() == 2 && required > width {
        for (part, (long, short)) in parts
            .iter_mut()
            .zip([(" Primary ", " P "), (" Secondary ", " S ")])
        {
            if let Some(rest) = part.strip_prefix(long) {
                *part = format!("{short}{rest}");
            }
        }
        required = parts.iter().map(|part| cells::display_width(part)).sum();
    }
    let name_width = cells::display_width(name);
    let minimum = if name_width > 12 { 13 } else { name_width };
    let available = width
        .saturating_sub(required)
        .max(minimum.min(width))
        .min(name_width);
    let mut result = vec![Span::styled(cells::ellipsize(name, available), style)];
    let add_bars = available == name_width
        && width.saturating_sub(required + available) >= 10
        && bars.iter().all(|bar| !bar.is_empty());
    for (part, bar) in parts.into_iter().zip(bars) {
        result.push(Span::raw(part));
        if add_bars {
            result.push(Span::raw(" "));
            result.extend(bar);
        }
    }
    Line::from(result)
}

pub(super) fn wrapped_height(lines: &[Line<'_>], width: u16) -> u16 {
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        widgets::{Paragraph, Widget, Wrap},
    };
    if width == 0 {
        return u16::MAX;
    }
    let area = Rect::new(0, 0, width, 64);
    let mut buffer = Buffer::empty(area);
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .render(area, &mut buffer);
    (0..area.height)
        .rev()
        .find(|y| (0..width).any(|x| buffer[(x, *y)].symbol() != " "))
        .map_or(0, |y| y + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_reset_distinguishes_past_and_future() {
        let window = |reset| CompactWindow {
            label: "5h".into(),
            line: codex(
                "Primary",
                Some(&crate::monitor::CodexQuotaWindow {
                    used_percent: Some(42.0),
                    window_minutes: Some(300),
                    reset_at: Some(reset),
                }),
                UNIX_EPOCH + Duration::from_secs(100),
            ),
            reset: Some(reset),
        };
        let line =
            compact_line("", Style::default(), [window(99), window(101)], 80, 100).to_string();
        assert!(
            line.contains("42.0% passed") && line.contains("42.0% <1m"),
            "{line}"
        );
    }
    #[test]
    fn bar_thresholds_and_invalid_values() {
        for (value, expected) in [(49.0, TEAL), (50.0, YELLOW), (79.0, YELLOW), (80.0, RED)] {
            assert_eq!(bar(Some(value), TEAL, true)[0].style.fg, Some(expected));
        }
        assert_eq!(bar(Some(101.0), TEAL, true)[0].content, "Unknown");
    }
}
