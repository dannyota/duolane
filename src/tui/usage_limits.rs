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

pub(super) fn codex(
    label: &str,
    window: Option<&crate::monitor::CodexQuotaWindow>,
    now: SystemTime,
) -> Line<'static> {
    let label = match window.and_then(|w| w.window_minutes) {
        Some(300) => "5h".to_owned(),
        Some(10080) => "week".to_owned(),
        Some(n) if (1..=525_600_000).contains(&n) => format!("{label} ({n}m)"),
        _ => label.to_owned(),
    };
    let mut spans = vec![Span::raw(format!("  {label:<9} "))];
    spans.extend(bar(window.and_then(|w| w.used_percent), TEAL, true));
    let reset = window
        .and_then(|w| w.reset_at)
        .filter(|v| (1..=253_402_300_799).contains(v))
        .and_then(|v| UNIX_EPOCH.checked_add(Duration::from_secs(v)))
        .map(|time| {
            let date = cells::format_system_time(time);
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bar_thresholds_and_invalid_values() {
        for (value, expected) in [(49.0, TEAL), (50.0, YELLOW), (79.0, YELLOW), (80.0, RED)] {
            assert_eq!(bar(Some(value), TEAL, true)[0].style.fg, Some(expected));
        }
        assert_eq!(bar(Some(101.0), TEAL, true)[0].content, "Unknown");
    }
    #[cfg(unix)]
    #[test]
    fn expired_claude_window_is_zero() {
        use crate::claude_accounts::limits::{Limits, Window};
        let limits = Limits {
            five_hour: Some(Window {
                used: 0.8,
                resets_at: Some(99),
            }),
            seven_day: None,
            observed_at: 1,
        };
        let buffer = crate::tui::tests::draw(80, 2, |frame| {
            frame.render_widget(
                ratatui::widgets::Paragraph::new(vec![
                    claude("5h", Some(&limits), 18_000, 100),
                    codex(
                        "Primary",
                        Some(&crate::monitor::CodexQuotaWindow {
                            used_percent: Some(42.0),
                            window_minutes: Some(300),
                            reset_at: None,
                        }),
                        SystemTime::now(),
                    ),
                ]),
                frame.area(),
            );
        });
        let text = crate::tui::tests::buffer_text(&buffer);
        assert!(text.contains(" 0%"));
        assert!(text.contains("42.0%"));
    }
}
