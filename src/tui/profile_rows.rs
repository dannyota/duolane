use super::{DIM, DIM_WHITE, GREEN, RED, TEAL, YELLOW, cells};
use crate::claude_accounts::profile_state::{ProfileState, Tone};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

fn color(tone: Tone) -> Color {
    match tone {
        Tone::Good => GREEN,
        Tone::Warn => YELLOW,
        Tone::Bad => RED,
        Tone::Dim => DIM,
    }
}

fn status_span(state: &ProfileState) -> Span<'static> {
    Span::styled(
        format!("  {}", state.status),
        Style::default().fg(color(state.status_tone)),
    )
}

fn stale_span() -> Span<'static> {
    Span::styled(" Stale", Style::default().fg(YELLOW))
}

fn suggested_span() -> Span<'static> {
    Span::styled(
        "  Suggested",
        Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
    )
}

pub(super) fn state_spans(state: &ProfileState) -> Vec<Span<'static>> {
    let mut spans = vec![status_span(state)];
    if let Some(seen) = &state.seen {
        spans.push(Span::styled(
            format!("  seen {seen}"),
            Style::default().fg(DIM_WHITE),
        ));
    }
    if state.stale {
        spans.push(stale_span());
    }
    if state.suggested {
        spans.push(suggested_span());
    }
    spans
}

fn width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| cells::display_width(&span.content))
        .sum()
}

pub(super) fn compact_with_state(
    state: Option<&ProfileState>,
    available: usize,
    line: impl Fn(usize) -> Line<'static>,
) -> Line<'static> {
    let plain = line(available);
    let Some(state) = state else {
        return plain;
    };
    let mut full = vec![status_span(state)];
    if state.stale {
        full.push(stale_span());
    }
    if state.suggested {
        full.push(suggested_span());
    }
    for tail in [full, vec![status_span(state)]] {
        let Some(rest) = available.checked_sub(width(&tail)) else {
            continue;
        };
        let mut shorter = line(rest);
        if shorter.spans.first().map(|span| &span.content)
            == plain.spans.first().map(|span| &span.content)
            && width(&shorter.spans) <= rest
        {
            shorter.spans.extend(tail);
            return shorter;
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ProfileState {
        ProfileState {
            status: "limited 29m".into(),
            status_tone: Tone::Bad,
            seen: Some("2h ago".into()),
            stale: true,
            suggested: true,
            ..Default::default()
        }
    }

    #[test]
    fn full_row_shows_status_age_stale_and_suggestion_with_tones() {
        let spans = state_spans(&state());
        let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "  limited 29m  seen 2h ago Stale  Suggested");
        assert_eq!(spans[0].style.fg, Some(RED));
        assert_eq!(spans[2].style.fg, Some(YELLOW));
    }

    #[test]
    fn compact_row_adds_state_only_while_the_name_and_windows_keep_their_room() {
        let line = |width: usize| {
            let name = "backup";
            let windows = " 5h 42% 1h";
            let shown = name.len().min(width.saturating_sub(windows.len()));
            Line::from(vec![
                Span::raw(name[..shown].to_owned()),
                Span::raw(windows),
            ])
        };
        let roomy = compact_with_state(Some(&state()), 60, line).to_string();
        assert!(roomy.ends_with("limited 29m Stale  Suggested"), "{roomy}");
        let tight = compact_with_state(Some(&state()), 30, line).to_string();
        assert_eq!(tight, "backup 5h 42% 1h  limited 29m");
        let none = compact_with_state(Some(&state()), 18, line).to_string();
        assert_eq!(none, "backup 5h 42% 1h");
        assert_eq!(compact_with_state(None, 60, line).to_string(), none);
    }
}
