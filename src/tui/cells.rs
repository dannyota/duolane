use super::layout::{ColumnSpec, ColumnWidth, LayoutTier};
use super::{
    BLUE, DIM, DIM_WHITE, GREEN, PANEL_BG, PURPLE, RED, SEPARATOR, SESSION_SPARKLINE_MAX_TOKENS,
    TEAL, YELLOW,
};
use crate::monitor::SESSION_TOKEN_BUCKET_SECS;
use crate::monitor::snapshot::{CompletedSnapshot, MonitorSnapshot};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use ratatui::buffer::CellWidth;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

pub(super) fn panel(title: &str, focused: bool) -> Block<'static> {
    let color = if focused { TEAL } else { SEPARATOR };
    Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(if focused { TEAL } else { DIM_WHITE })
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(PANEL_BG))
}

pub(super) fn table_header_aligned(
    cells: impl IntoIterator<Item = (&'static str, Alignment)>,
) -> Row<'static> {
    Row::new(
        cells
            .into_iter()
            .map(|(cell, alignment)| {
                Cell::from(
                    Line::from(Span::styled(cell, Style::default().fg(TEAL))).alignment(alignment),
                )
            })
            .collect::<Vec<_>>(),
    )
    .style(Style::default().add_modifier(Modifier::BOLD))
}

pub(super) fn render_empty_table_state(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    focused: bool,
    message: &str,
) {
    frame.render_widget(panel(title, focused), area);
    let content = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if content.width == 0 || content.height == 0 {
        return;
    }

    let line = Rect {
        y: content.y + content.height.saturating_sub(1) / 2,
        height: 1,
        ..content
    };
    frame.render_widget(
        Paragraph::new(ellipsize(message, line.width.into()))
            .alignment(Alignment::Center)
            .style(Style::default().fg(DIM).bg(PANEL_BG)),
        line,
    );
}

pub(super) fn muted_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(Span::styled(value.into(), Style::default().fg(DIM)))
}

pub(super) fn text_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(Span::styled(value.into(), Style::default().fg(DIM_WHITE)))
}

pub(super) fn model_cell(value: Option<&str>, width: usize) -> Cell<'static> {
    text_cell(ellipsize(value.unwrap_or("-"), width))
}

pub(super) fn table_column_width(area: Rect, widths: &[Constraint], column: usize) -> usize {
    let table_width = area.width.saturating_sub(2);
    Layout::horizontal(widths.to_vec())
        .spacing(1)
        .split(Rect::new(0, 0, table_width, 1))
        .get(column)
        .map_or(0, |rect| usize::from(rect.width))
}

pub(super) fn project_constraints<'a, K: PartialEq>(
    columns: &[ColumnSpec<K>],
    project_key: K,
    area: Rect,
    minimum_width: u16,
    projects: impl Iterator<Item = Option<&'a str>>,
) -> Vec<Constraint> {
    let mut widths = column_constraints(columns);
    if !matches!(
        LayoutTier::for_outer_width(area.width),
        LayoutTier::Expanded | LayoutTier::Wide
    ) {
        return widths;
    }
    let Some(project) = columns.iter().position(|column| column.key == project_key) else {
        return widths;
    };
    let Some(flex) = columns
        .iter()
        .position(|column| matches!(column.width, ColumnWidth::Flex(_)))
    else {
        return widths;
    };
    let ColumnWidth::Fixed(base) = columns[project].width else {
        return widths;
    };
    let desired = projects
        .map(|value| display_width(value.unwrap_or("-")))
        .max()
        .unwrap_or(0)
        .max(columns[project].header.len())
        .clamp(usize::from(base), 40);
    let minimum_flex = table_column_width(Rect::new(0, 0, minimum_width, 1), &widths, flex);
    let available = table_column_width(area, &widths, flex).saturating_sub(minimum_flex);
    widths[project] =
        Constraint::Length(base + (desired - usize::from(base)).min(available) as u16);
    widths
}

fn display_width(value: &str) -> usize {
    Span::raw(value)
        .styled_graphemes(Style::default())
        .map(|grapheme| usize::from(grapheme.symbol.cell_width()))
        .sum()
}

pub(super) fn ellipsize(value: &str, width: usize) -> String {
    let span = Span::raw(value);
    if display_width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }

    let mut result = String::new();
    let mut used = 0;
    for grapheme in span.styled_graphemes(Style::default()) {
        let grapheme_width = usize::from(grapheme.symbol.cell_width());
        if used + grapheme_width > width - 1 {
            break;
        }
        result.push_str(grapheme.symbol);
        used += grapheme_width;
    }
    result.push('…');
    result
}

pub(super) fn display_session_id(session_id: Option<&str>) -> &str {
    let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
        return "no-session";
    };
    if uuid::Uuid::parse_str(session_id).is_ok() {
        return session_id
            .split_once('-')
            .map_or(session_id, |(first, _)| first);
    }
    session_id
}

pub(super) fn number_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(
        Line::from(Span::styled(value.into(), Style::default().fg(DIM_WHITE)))
            .alignment(Alignment::Right),
    )
}

pub(super) fn status_cell(value: &str) -> Cell<'static> {
    Cell::from(Span::styled(value.to_string(), status_style(value)))
}

pub(super) fn status_style(value: &str) -> Style {
    Style::default().fg(status_color(value))
}

pub(super) fn status_color(value: &str) -> Color {
    match value {
        "completed" => GREEN,
        "streaming" => TEAL,
        "compacting" => PURPLE,
        "failed" => RED,
        "upstream" => BLUE,
        "selected" | "started" => YELLOW,
        _ => DIM_WHITE,
    }
}

pub(super) fn http_status_style(status: Option<u16>) -> Style {
    Style::default().fg(http_status_color(status))
}

pub(super) fn http_status_color(status: Option<u16>) -> Color {
    match status {
        Some(200..=299) => GREEN,
        Some(400..=499) => YELLOW,
        Some(500..=599) => RED,
        Some(_) => DIM_WHITE,
        None => DIM,
    }
}

pub(super) fn rate_cell(value: String) -> Cell<'static> {
    let color = if value.contains("tok/s") {
        TEAL
    } else if value == "-" {
        DIM
    } else {
        DIM_WHITE
    };
    Cell::from(
        Line::from(Span::styled(value, Style::default().fg(color))).alignment(Alignment::Right),
    )
}

pub(super) fn dashboard_snapshot(state: &MonitorSnapshot) -> MonitorSnapshot {
    let mut terminal_state = state.for_terminal();
    for provider in terminal_state
        .sessions
        .iter_mut()
        .map(|row| &mut row.provider)
        .chain(
            terminal_state
                .active
                .iter_mut()
                .map(|row| &mut row.provider),
        )
        .chain(
            terminal_state
                .recent
                .iter_mut()
                .map(|row| &mut row.provider),
        )
    {
        if provider.as_deref() == Some("anthropic") {
            *provider = Some("claude".to_string());
        }
    }
    terminal_state
}

pub(super) fn provider_cell(value: Option<&str>) -> Cell<'static> {
    let value = value.unwrap_or("-");
    let color = match value {
        "codex" => TEAL,
        "claude" => PURPLE,
        "-" => DIM,
        _ => DIM_WHITE,
    };
    Cell::from(Span::styled(value.to_string(), Style::default().fg(color)))
}

pub(super) fn detail_cell(value: &str) -> Cell<'static> {
    if value.is_empty() || value == "-" {
        Cell::from(Span::styled("", Style::default().fg(DIM)))
    } else {
        Cell::from(Span::styled(value.to_string(), Style::default().fg(YELLOW)))
    }
}

pub(super) fn error_indicator(request: &CompletedSnapshot) -> &'static str {
    if request.status == crate::monitor::RequestStatus::Failed
        || request.http_status.is_some_and(|status| status >= 400)
        || request
            .error
            .as_deref()
            .is_some_and(|error| !error.is_empty())
    {
        "!"
    } else {
        ""
    }
}

pub(super) fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

pub(super) fn token_value(value: Option<u64>) -> String {
    value.map(compact_tokens).unwrap_or_else(|| "-".to_string())
}

pub(super) fn spinner(tick: usize) -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[tick % FRAMES.len()]
}

pub(super) fn sparkline_bucket(timestamp: SystemTime) -> u64 {
    timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / SESSION_TOKEN_BUCKET_SECS
}

pub(super) fn token_sparkline(
    samples: &[(SystemTime, u64)],
    width: usize,
    reference_at: SystemTime,
) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    if width == 0 {
        return String::new();
    }

    let mut buckets = HashMap::<u64, u64>::new();
    for (timestamp, tokens) in samples {
        let bucket = sparkline_bucket(*timestamp);
        let total = buckets.entry(bucket).or_default();
        *total = total.saturating_add(*tokens);
    }

    let current_bucket = sparkline_bucket(reference_at);
    let first_bucket = current_bucket.saturating_sub(width.saturating_sub(1) as u64);
    (first_bucket..=current_bucket)
        .map(|bucket| {
            let value = buckets.get(&bucket).copied().unwrap_or(0);
            if value == 0 {
                return ' ';
            }
            let scaled = value.min(SESSION_SPARKLINE_MAX_TOKENS);
            let level = (u128::from(scaled) * LEVELS.len() as u128)
                .div_ceil(u128::from(SESSION_SPARKLINE_MAX_TOKENS))
                .saturating_sub(1) as usize;
            LEVELS[level]
        })
        .collect()
}

pub(super) fn token_sparkline_line(
    samples: &[(SystemTime, u64)],
    width: usize,
    reference_at: SystemTime,
) -> Line<'static> {
    let mut sparkline = token_sparkline(samples, width, reference_at);
    let current = sparkline
        .pop()
        .map_or_else(String::new, |value| value.to_string());
    Line::from(vec![
        Span::styled(sparkline, Style::default().fg(BLUE)),
        Span::styled(current, Style::default().fg(DIM)),
    ])
}

pub(super) fn column_constraints<K>(columns: &[ColumnSpec<K>]) -> Vec<Constraint> {
    columns.iter().map(ColumnSpec::constraint).collect()
}

pub(super) fn column_header<K>(columns: &[ColumnSpec<K>]) -> Row<'static> {
    table_header_aligned(
        columns
            .iter()
            .map(|column| (column.header, column.alignment)),
    )
}

pub(super) fn target_cell(
    provider: Option<&str>,
    model: Option<&str>,
    width: usize,
) -> Cell<'static> {
    let provider = provider.unwrap_or("-");
    let model = model.unwrap_or("-");
    text_cell(ellipsize(&format!("{provider}/{model}"), width))
}

pub(super) fn http_code_cell(status: Option<u16>) -> Cell<'static> {
    Cell::from(
        Line::from(Span::styled(
            status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "-".to_string()),
            http_status_style(status),
        ))
        .alignment(Alignment::Right),
    )
}

pub(super) fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

pub(super) fn format_system_time(time: SystemTime) -> String {
    format_system_time_in_zone(time, TimeZone::system())
}

pub(super) fn format_system_time_in_zone(time: SystemTime, time_zone: TimeZone) -> String {
    let Ok(timestamp) = Timestamp::try_from(time) else {
        return "-".to_string();
    };
    Zoned::new(timestamp, time_zone)
        .strftime("%H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{BLUE, DIM};
    use jiff::tz::TimeZone;
    use std::time::{Duration, SystemTime};

    #[test]
    fn format_system_time_applies_non_utc_time_zone() {
        let timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(20 * 60 * 60);
        let time_zone = TimeZone::fixed(jiff::tz::offset(5));

        assert_eq!(format_system_time_in_zone(timestamp, time_zone), "01:00:00");
    }

    #[test]
    fn display_session_id_shortens_uuids() {
        assert_eq!(
            display_session_id(Some("57c7c914-ada4-4f40-9672-985f950fbb66")),
            "57c7c914"
        );
    }

    #[test]
    fn display_session_id_handles_atypical_ids() {
        assert_eq!(display_session_id(Some("custom-session")), "custom-session");
        assert_eq!(display_session_id(Some("")), "no-session");
        assert_eq!(display_session_id(None), "no-session");
    }

    #[test]
    fn ellipsize_marks_truncated_values() {
        assert_eq!(ellipsize("claude-sonnet-4-6", 16), "claude-sonnet-4…");
        assert_eq!(ellipsize("gpt-5.6-sol", 16), "gpt-5.6-sol");
        assert_eq!(ellipsize("anything", 0), "");
    }

    #[test]
    fn token_sparkline_uses_fixed_wall_clock_buckets() {
        let samples = [
            (SystemTime::UNIX_EPOCH + Duration::from_secs(78), 2_000),
            (SystemTime::UNIX_EPOCH + Duration::from_secs(85), 3_000),
            (SystemTime::UNIX_EPOCH + Duration::from_secs(100), 4_000),
        ];
        let bucket_start = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let bucket_end = SystemTime::UNIX_EPOCH + Duration::from_secs(109);
        let next_bucket = SystemTime::UNIX_EPOCH + Duration::from_secs(110);

        assert_eq!(token_sparkline(&[], 4, bucket_start), "    ");
        assert_eq!(token_sparkline(&samples, 4, bucket_start), "▄▆ █");
        assert_eq!(token_sparkline(&samples, 4, bucket_end), "▄▆ █");
        assert_eq!(token_sparkline(&samples, 4, next_bucket), "▆ █ ");
        assert_eq!(token_sparkline(&samples, 0, bucket_start), "");
    }

    #[test]
    fn token_sparkline_uses_fixed_shared_scale() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let current = (now, 2_000);
        let offscreen_peak = (SystemTime::UNIX_EPOCH, 4_000);

        assert_eq!(token_sparkline(&[current], 2, now), " ▄");
        assert_eq!(token_sparkline(&[offscreen_peak, current], 2, now), " ▄");
        assert_eq!(token_sparkline(&[(now, 10_000)], 1, now), "█");
    }

    #[test]
    fn token_sparkline_dims_the_current_bucket() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let samples = [
            (SystemTime::UNIX_EPOCH + Duration::from_secs(95), 2_000),
            (now, 4_000),
        ];

        let line = token_sparkline_line(&samples, 2, now);

        assert_eq!(line.spans[0].content, "▄");
        assert_eq!(line.spans[0].style.fg, Some(BLUE));
        assert_eq!(line.spans[1].content, "█");
        assert_eq!(line.spans[1].style.fg, Some(DIM));
    }
}
