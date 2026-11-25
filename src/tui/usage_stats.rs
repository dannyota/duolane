use super::*;
use crate::monitor::{Throughput, snapshot::CompletedSnapshot};
use ratatui::{
    text::{Line, Span},
    widgets::{Cell, Row, Table},
};
use std::collections::BTreeMap;

#[derive(Default)]
struct Group {
    count: usize,
    waits: Vec<f64>,
    generations: Vec<f64>,
    outputs: Vec<f64>,
    rate_tokens: f64,
    rate_seconds: f64,
}

fn median(values: &mut [f64]) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n == 0 {
        None
    } else if n % 2 == 1 {
        Some(values[n / 2])
    } else {
        Some((values[n / 2 - 1] + values[n / 2]) / 2.0)
    }
}

fn groups(recent: &[CompletedSnapshot]) -> Vec<((String, String, String), Group)> {
    let mut groups = BTreeMap::<(String, String, String), Group>::new();
    for row in recent {
        let key = (
            row.provider.clone().unwrap_or_else(|| "-".into()),
            row.model.clone().unwrap_or_else(|| "-".into()),
            row.effort.clone().unwrap_or_else(|| "-".into()),
        );
        let group = groups.entry(key).or_default();
        group.count += 1;
        if let Some(wait) = row
            .generation_started_at
            .and_then(|started| started.duration_since(row.started_at).ok())
        {
            group.waits.push(wait.as_secs_f64());
        }
        if let Some(generation) = row.generation_duration {
            group.generations.push(generation.as_secs_f64());
        }
        if let Some(output) = row.output_tokens {
            group.outputs.push(output as f64);
        }
        if let (Throughput::TokensPerSecond(rate), Some(duration)) =
            (row.rate(), row.generation_duration)
        {
            let seconds = duration.as_secs_f64();
            if rate.is_finite() && rate >= 0.0 && seconds > 0.0 {
                group.rate_tokens += rate * seconds;
                group.rate_seconds += seconds;
            }
        }
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by(|a, b| {
        a.0.0
            .cmp(&b.0.0)
            .then_with(|| b.1.count.cmp(&a.1.count))
            .then_with(|| a.0.cmp(&b.0))
    });
    groups
}

fn number(value: Option<f64>, suffix: &str) -> String {
    value
        .map(|n| format!("{n:.1}{suffix}"))
        .unwrap_or_else(|| "-".into())
}

pub(super) fn render(
    frame: &mut ratatui::Frame<'_>,
    speed: Rect,
    tokens: Rect,
    state: &MonitorSnapshot,
) {
    let rows = groups(&state.recent)
        .into_iter()
        .map(|((lane, model, effort), mut group)| {
            let wait = median(&mut group.waits);
            let generation = median(&mut group.generations);
            let output = median(&mut group.outputs);
            let rate = (group.rate_seconds > 0.0).then_some(group.rate_tokens / group.rate_seconds);
            let width = usize::from(speed.width.saturating_sub(95));
            let waits = wait.map_or(0, |n| (n * 2.0).round().min(width as f64) as usize);
            let generation_cells = generation.map_or(0, |n| {
                (n * 2.0).round().min(width.saturating_sub(waits) as f64) as usize
            });
            Row::new(vec![
                Cell::from(lane),
                Cell::from(model),
                Cell::from(effort),
                Cell::from(group.count.to_string()),
                Cell::from(number(wait, "s")),
                Cell::from(number(generation, "s")),
                Cell::from(number(output, "")),
                Cell::from(number(rate, " tok/s")),
                Cell::from(Line::from(vec![
                    Span::styled("░".repeat(waits), Style::default().fg(YELLOW)),
                    Span::styled("█".repeat(generation_cells), Style::default().fg(GREEN)),
                ])),
            ])
        });
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(24),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Length(11),
            Constraint::Fill(1),
        ],
    )
    .header(
        Row::new([
            "Lane",
            "Model",
            "Effort",
            "Requests",
            "Wait",
            "Generation",
            "Out",
            "Rate",
            "Time",
        ])
        .style(Style::default().fg(TEAL)),
    )
    .block(cells::panel("Speed by model", false));
    frame.render_widget(table, speed);
    let mut sessions: Vec<_> = state.sessions.iter().collect();
    sessions.sort_by_key(|row| std::cmp::Reverse(row.output_tokens));
    let total: u128 = sessions
        .iter()
        .map(|row| u128::from(row.output_tokens))
        .sum();
    let rows = sessions.into_iter().map(|row| {
        let share = if total == 0 {
            0.0
        } else {
            row.output_tokens as f64 * 100.0 / total as f64
        };
        let width = usize::from(tokens.width.saturating_sub(89));
        let bars = ((share / 100.0) * width as f64).round() as usize;
        Row::new(vec![
            row.project.clone().unwrap_or_else(|| "-".into()),
            row.label(),
            row.provider.clone().unwrap_or_else(|| "-".into()),
            row.model.clone().unwrap_or_else(|| "-".into()),
            cells::compact_tokens(row.input_tokens),
            cells::compact_tokens(row.output_tokens),
            format!("{share:.1}%"),
            "█".repeat(bars),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(24),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(7),
            Constraint::Fill(1),
        ],
    )
    .header(
        Row::new([
            "Project",
            "ID",
            "Lane",
            "Model",
            "In",
            "Out",
            "Share",
            "Share of output",
        ])
        .style(Style::default().fg(TEAL)),
    )
    .block(cells::panel("Tokens by session", false));
    frame.render_widget(table, tokens);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn medians_and_pooled_rate_ignore_requests_without_token_rates() {
        let state = crate::monitor::mock_state();
        let original = state.recent[0].clone();
        let mut rows = Vec::new();
        for (wait, duration, output) in [(2, 2, Some(20)), (4, 4, Some(80)), (8, 6, None)] {
            let mut row = original.clone();
            row.provider = Some("lane".into());
            row.model = Some("model".into());
            row.effort = None;
            row.generation_started_at = Some(row.started_at + Duration::from_secs(wait));
            row.generation_duration = Some(Duration::from_secs(duration));
            row.output_tokens = output;
            let snapshot: CompletedSnapshot = row.into();
            let mut json = serde_json::to_value(snapshot).unwrap();
            json["throughput"] = match output {
                Some(tokens) => {
                    serde_json::json!({"unit": "tokens_per_second", "value": tokens as f64 / duration as f64})
                }
                None => serde_json::json!({"unit": "bytes_per_second", "value": 10.0}),
            };
            rows.push(serde_json::from_value(json).unwrap());
        }
        let mut groups = groups(&rows);
        assert_eq!(groups.len(), 1);
        let group = &mut groups[0].1;
        assert_eq!(group.count, 3);
        assert_eq!(median(&mut group.waits), Some(4.0));
        assert_eq!(median(&mut group.generations), Some(4.0));
        assert_eq!(median(&mut group.outputs), Some(50.0));
        assert!((group.rate_tokens / group.rate_seconds - 100.0 / 6.0).abs() < 0.001);
        assert!(super::groups(&[]).is_empty());
    }
}
