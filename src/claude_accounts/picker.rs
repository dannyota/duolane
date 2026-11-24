use anyhow::Result;
use crossterm::cursor::{Hide, MoveToColumn, MoveUp, Show};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Stylize;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};
use std::io::Write;

const HELP: &str = "up/down move, enter select, esc cancel";

struct RawMode;

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), Show);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn fit(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

fn rewind(out: &mut impl Write, rows: usize) -> Result<()> {
    let lines = u16::try_from(rows.saturating_add(2)).unwrap_or(u16::MAX);
    queue!(
        out,
        MoveUp(lines),
        MoveToColumn(0),
        Clear(ClearType::FromCursorDown)
    )?;
    Ok(())
}

pub(super) fn pick(
    header: &str,
    rows: &[String],
    start: usize,
    width: usize,
    color: bool,
    mut next: impl FnMut() -> Result<Event>,
    out: &mut impl Write,
) -> Result<Option<usize>> {
    if rows.is_empty() {
        return Ok(None);
    }
    let width = width.saturating_sub(3).max(1);
    let mut index = start.min(rows.len() - 1);
    let mut drawn = false;
    loop {
        if drawn {
            rewind(out, rows.len())?;
        }
        write!(out, "  {}\r\n", fit(header, width))?;
        for (position, row) in rows.iter().enumerate() {
            let text = fit(row, width);
            match (position == index, color) {
                (true, true) => write!(out, "{}\r\n", format!("> {text}").cyan().bold())?,
                (true, false) => write!(out, "> {text}\r\n")?,
                (false, _) => write!(out, "  {text}\r\n")?,
            }
        }
        write!(out, "\r\n  {}", fit(HELP, width))?;
        out.flush()?;
        drawn = true;
        let choice = loop {
            let Event::Key(key) = next()? else {
                break None;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    index = (index + rows.len() - 1) % rows.len();
                    break None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    index = (index + 1) % rows.len();
                    break None;
                }
                KeyCode::Enter => break Some(Some(index)),
                KeyCode::Esc | KeyCode::Char('q') => break Some(None),
                KeyCode::Char('c' | 'd') if control => break Some(None),
                _ => {}
            }
        };
        if let Some(choice) = choice {
            rewind(out, rows.len())?;
            out.flush()?;
            return Ok(choice);
        }
    }
}

pub(super) fn fits(rows: usize) -> bool {
    crossterm::terminal::size().is_ok_and(|(_, height)| rows.saturating_add(4) <= height as usize)
}

pub(super) fn interactive(
    header: &str,
    rows: &[String],
    start: usize,
    color: bool,
) -> Result<Option<usize>> {
    let (width, _) = crossterm::terminal::size()?;
    crossterm::terminal::enable_raw_mode()?;
    let _raw = RawMode;
    let mut out = std::io::stdout();
    execute!(out, Hide)?;
    pick(
        header,
        rows,
        start,
        width as usize,
        color,
        || Ok(crossterm::event::read()?),
        &mut out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn choose(start: usize, keys: &[KeyCode]) -> (Option<usize>, String) {
        let rows = [
            "default".to_string(),
            "backup".into(),
            "third-profile".into(),
        ];
        let mut keys = keys.iter();
        let mut out = Vec::new();
        let choice = pick(
            "PROFILE",
            &rows,
            start,
            10,
            false,
            || {
                let code = keys.next().copied().unwrap_or(KeyCode::Esc);
                Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            },
            &mut out,
        )
        .unwrap();
        (choice, String::from_utf8(out).unwrap())
    }

    #[test]
    fn claude_accounts_picker_moves_wraps_and_cancels() {
        let (choice, screen) = choose(1, &[KeyCode::Down, KeyCode::Enter]);
        assert_eq!(choice, Some(2));
        assert!(screen.contains("> backup\r\n") && screen.contains("> third-p\r\n"));
        assert!(screen.contains("  default\r\n") && !screen.contains("third-profile"));
        assert_eq!(choose(0, &[KeyCode::Up, KeyCode::Enter]).0, Some(2));
        assert_eq!(choose(9, &[KeyCode::Down, KeyCode::Enter]).0, Some(0));
        assert_eq!(choose(0, &[KeyCode::Down, KeyCode::Char('x')]).0, None);
    }
}
