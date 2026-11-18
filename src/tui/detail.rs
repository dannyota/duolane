use super::cells::{
    compact_tokens, display_session_id, format_duration, format_system_time, http_status_color,
    panel, status_color, token_value,
};
use super::{DIM, DIM_WHITE, PANEL_BG, RED, TEAL, WHITE, YELLOW};
use crate::monitor::snapshot::MonitorSnapshot;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub(super) fn render_session_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &MonitorSnapshot,
    selected: usize,
) {
    let lines = if let Some(session) = state.sessions.get(selected) {
        vec![
            detail_line("session", session.label(), WHITE),
            detail_line("project", session.project.as_deref().unwrap_or("-"), TEAL),
            detail_line("active requests", session.active_count.to_string(), YELLOW),
            detail_line(
                "total requests",
                session.request_count.to_string(),
                DIM_WHITE,
            ),
            detail_line("failures", session.failure_count.to_string(), RED),
            detail_line("provider", session.provider.as_deref().unwrap_or("-"), TEAL),
            detail_line("model", session.model.as_deref().unwrap_or("-"), DIM_WHITE),
            detail_line("effort", session.effort.as_deref().unwrap_or("-"), YELLOW),
            detail_line(
                "input tokens",
                compact_tokens(session.input_tokens),
                DIM_WHITE,
            ),
            detail_line(
                "output tokens",
                compact_tokens(session.output_tokens),
                DIM_WHITE,
            ),
            detail_line(
                "total tokens",
                format!(
                    "{}/{}",
                    compact_tokens(session.input_tokens),
                    compact_tokens(session.output_tokens)
                ),
                DIM_WHITE,
            ),
            detail_line("rate", session.rate().label(), TEAL),
            detail_line(
                "last status",
                session.last_status.as_str(),
                status_color(&session.last_status),
            ),
        ]
    } else {
        vec![Line::from(Span::styled(
            "No session selected",
            Style::default().fg(DIM),
        ))]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL_BG))
            .block(panel("Session detail", true)),
        area,
    );
}

pub(super) fn render_request_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &MonitorSnapshot,
    selected: usize,
) {
    let lines = if let Some(request) = state.recent.get(selected) {
        let mut lines = vec![
            detail_line("request", request.request_id.clone(), WHITE),
            detail_line(
                "session",
                display_session_id(request.session_id.as_deref()),
                TEAL,
            ),
            detail_line(
                "session seq",
                request
                    .session_seq
                    .map(|seq| seq.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                DIM_WHITE,
            ),
            detail_line("endpoint", request.endpoint.label(), DIM_WHITE),
            detail_line("started", format_system_time(request.started_at), DIM_WHITE),
            detail_line(
                "finished",
                format_system_time(request.finished_at),
                DIM_WHITE,
            ),
            detail_line(
                "status",
                request.status.label(),
                status_color(request.status.label()),
            ),
            detail_line(
                "http status",
                request
                    .http_status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                http_status_color(request.http_status),
            ),
            detail_line("provider", request.provider.as_deref().unwrap_or("-"), TEAL),
            detail_line("model", request.model.as_deref().unwrap_or("-"), DIM_WHITE),
            detail_line("effort", request.effort.as_deref().unwrap_or("-"), YELLOW),
            detail_line("latency", format_duration(request.latency), DIM_WHITE),
            detail_line("rate", request.rate().label(), TEAL),
            detail_line("input tokens", token_value(request.input_tokens), DIM_WHITE),
            detail_line(
                "output tokens",
                token_value(request.output_tokens),
                DIM_WHITE,
            ),
            detail_line(
                "stream bytes",
                request.streamed_bytes.to_string(),
                DIM_WHITE,
            ),
            detail_line(
                "stream chunks",
                request.stream_chunks.to_string(),
                DIM_WHITE,
            ),
        ];
        if let Some(error) = request.error.as_deref().filter(|error| !error.is_empty()) {
            lines.push(detail_line("detail", error, YELLOW));
        }
        if let Some(path) = &request.traffic_capture_path {
            lines.push(detail_line(
                "capture",
                path.to_string_lossy().into_owned(),
                DIM_WHITE,
            ));
        }
        lines
    } else {
        vec![Line::from(Span::styled(
            "No request selected",
            Style::default().fg(DIM),
        ))]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL_BG))
            .block(panel("Request detail", true)),
        area,
    );
}

pub(super) fn detail_line<'a>(
    label: &'static str,
    value: impl Into<String>,
    value_color: Color,
) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {label:<16}"), Style::default().fg(DIM)),
        Span::styled(value.into(), Style::default().fg(value_color)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::snapshot::MonitorSnapshot;
    use crate::monitor::{EndpointKind, MonitorHandle, mock_state};
    use crate::tui::tests::{buffer_text, draw};

    #[test]
    fn mock_request_detail_exposes_error_and_capture_fields() {
        let state: MonitorSnapshot = mock_state().into();
        let failed = state
            .recent
            .iter()
            .position(|request| request.request_id == "req-failed-claude")
            .unwrap();

        let detail = draw(140, 22, |frame| {
            render_request_detail(frame, frame.area(), &state, failed)
        });
        let text = buffer_text(&detail);

        assert!(text.contains("req-failed-claude"), "{text}");
        assert!(text.contains("upstream connection closed"), "{text}");
        assert!(text.contains("req-failed-claude.json"), "{text}");
    }

    #[test]
    fn request_detail_renders_full_error_text() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("sess-1".to_string()),
            Some(7),
            EndpointKind::Messages,
        );
        monitor.provider_selected(
            "request-1",
            "codex",
            "gpt-5.6-sol",
            Some("high".to_string()),
        );
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state: MonitorSnapshot = monitor.snapshot().into();

        let detail = draw(120, 20, |frame| {
            render_request_detail(frame, frame.area(), &state, 0)
        });
        let detail_text = buffer_text(&detail);

        assert!(detail_text.contains("Request detail"), "{detail_text}");
        assert!(detail_text.contains("request-1"), "{detail_text}");
        assert!(detail_text.contains("sess-1"), "{detail_text}");
        assert!(
            detail_text.contains("upstream unavailable"),
            "{detail_text}"
        );
    }
}
