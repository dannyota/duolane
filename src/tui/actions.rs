use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

pub(super) const OUTPUT_LIMIT: usize = 64 * 1024;
pub(super) const CAPTURE_TIMEOUT: Duration = Duration::from_secs(60);
const SUMMARY_CHARS: usize = 120;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Finished {
    pub(super) ok: bool,
    pub(super) summary: String,
}

impl Finished {
    fn failed(summary: impl Into<String>) -> Self {
        Self {
            ok: false,
            summary: summary.into(),
        }
    }
}

pub(super) fn program() -> std::io::Result<PathBuf> {
    std::env::current_exe()
}

pub(super) fn clean(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\n'
            || !(c.is_control()
                || matches!(
                    c,
                    '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{feff}'
                ))
        {
            out.push(c);
        }
    }
    out
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(SUMMARY_CHARS).collect())
        .unwrap_or_default()
}

fn summarize(code: Option<i32>, output: &[u8]) -> Finished {
    let line = last_line(&clean(output));
    let detail = |text: String| {
        if line.is_empty() {
            text
        } else {
            format!("{text}: {line}")
        }
    };
    match code {
        Some(0) => Finished {
            ok: true,
            summary: if line.is_empty() {
                "finished".into()
            } else {
                line
            },
        },
        Some(code) => Finished::failed(detail(format!("failed with exit status {code}"))),
        None => Finished::failed(detail("stopped by a signal".into())),
    }
}

#[cfg(unix)]
fn kill_group(child: &std::process::Child) {
    if let Some(pid) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(unix)]
pub(super) fn capture(program: &Path, args: &[String], timeout: Duration) -> Finished {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let spawned = (|| {
        let (reader, writer) = std::io::pipe()?;
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(writer.try_clone()?)
            .stderr(writer)
            .process_group(0);
        let child = command.spawn()?;
        Ok::<_, std::io::Error>((child, reader))
    })();
    let (mut child, reader) = match spawned {
        Ok(spawned) => spawned,
        Err(error) => return Finished::failed(format!("could not start: {}", error.kind())),
    };
    let (sender, receiver) = mpsc::channel();
    let drain = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.take(OUTPUT_LIMIT as u64 + 1).read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });
    let deadline = Instant::now() + timeout;
    let mut output = None;
    let status = loop {
        if output.is_none() {
            output = receiver.try_recv().ok();
        }
        if output
            .as_ref()
            .is_some_and(|bytes| bytes.len() > OUTPUT_LIMIT)
        {
            break Err("stopped after 64 KiB of output".to_owned());
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                break Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => break Err(format!("could not wait: {}", error.kind())),
        }
    };
    kill_group(&child);
    let _ = child.wait();
    let _ = drain.join();
    let output = output
        .or_else(|| receiver.try_recv().ok())
        .unwrap_or_default();
    match status {
        Ok(status) => summarize(status.code(), &output[..output.len().min(OUTPUT_LIMIT)]),
        Err(reason) => Finished::failed(reason),
    }
}

#[cfg(not(unix))]
pub(super) fn capture(_: &Path, _: &[String], _: Duration) -> Finished {
    Finished::failed("unsupported on this platform")
}

#[cfg(unix)]
struct IgnoreTtou {
    previous: libc::sigaction,
    active: bool,
}

#[cfg(unix)]
impl IgnoreTtou {
    fn install() -> Self {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut action.sa_mask);
            let mut previous = std::mem::zeroed();
            let active = libc::sigaction(libc::SIGTTOU, &action, &mut previous) == 0;
            Self { previous, active }
        }
    }
}

#[cfg(unix)]
impl Drop for IgnoreTtou {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                libc::sigaction(libc::SIGTTOU, &self.previous, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(unix)]
pub(super) fn handoff(program: &Path, args: &[String]) -> Finished {
    use rustix::process::{Signal, WaitOptions, kill_process_group, waitpid};
    use std::os::unix::process::CommandExt;

    let _ignore = IgnoreTtou::install();
    let terminal = std::io::stdin();
    let ours = rustix::process::getpgrp();
    let child = match std::process::Command::new(program)
        .args(args)
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return Finished::failed(format!("could not start: {}", error.kind())),
    };
    let Some(pid) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return Finished::failed("could not start: invalid process id");
    };
    if rustix::termios::tcsetpgrp(&terminal, pid).is_err() {
        kill_group(&child);
        let _ = waitpid(Some(pid), WaitOptions::empty());
        return Finished::failed("the terminal is unavailable");
    }
    let _ = kill_process_group(pid, Signal::CONT);
    let finished = loop {
        match waitpid(Some(pid), WaitOptions::UNTRACED) {
            Ok(Some((_, status))) if status.stopped() => {
                let _ = kill_process_group(pid, Signal::CONT);
            }
            Ok(Some((_, status))) => {
                break summarize(status.exit_status(), &[]);
            }
            Ok(None) => {}
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => break Finished::failed("could not wait"),
        }
    };
    let _ = kill_process_group(pid, Signal::KILL);
    let _ = rustix::termios::tcsetpgrp(&terminal, ours);
    finished
}

#[cfg(not(unix))]
pub(super) fn handoff(_: &Path, _: &[String]) -> Finished {
    Finished::failed("unsupported on this platform")
}

pub(super) fn run_handoff(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    label: &str,
    args: &[String],
) -> std::io::Result<Finished> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use std::io::Write;

    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, event::DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    terminal.show_cursor()?;
    println!("\n$ duolane {}\n", args.join(" "));
    let finished = match program() {
        Ok(program) => handoff(&program, args),
        Err(error) => Finished::failed(format!("could not start: {}", error.kind())),
    };
    enable_raw_mode()?;
    write!(
        stdout,
        "\r\n{label}: {}{}\r\nPress Enter to return to the dashboard.\r\n",
        if finished.ok { "" } else { "failed, " },
        finished.summary
    )?;
    stdout.flush()?;
    loop {
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Enter
        {
            break;
        }
    }
    crossterm::execute!(stdout, EnterAlternateScreen, event::EnableMouseCapture)?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    Ok(finished)
}

#[derive(Default)]
pub(super) struct Runner {
    job: Option<(String, mpsc::Receiver<Finished>)>,
}

impl Runner {
    pub(super) fn running(&self) -> Option<&str> {
        self.job.as_ref().map(|(label, _)| label.as_str())
    }

    pub(super) fn start(
        &mut self,
        label: String,
        run: impl FnOnce() -> Finished + Send + 'static,
    ) -> bool {
        if self.job.is_some() {
            return false;
        }
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("account-action".into())
            .spawn(move || {
                let _ = sender.send(run());
            });
        if spawned.is_err() {
            return false;
        }
        self.job = Some((label, receiver));
        true
    }

    pub(super) fn poll(&mut self) -> Option<(String, Finished)> {
        let (_, receiver) = self.job.as_ref()?;
        let finished = match receiver.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return None,
            Ok(finished) => finished,
            Err(mpsc::TryRecvError::Disconnected) => Finished::failed("stopped unexpectedly"),
        };
        self.job.take().map(|(label, _)| (label, finished))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_loses_escape_sequences_and_control_characters() {
        let text = clean(b"\x1b[32mgreen\x1b[0m\r\nnext\x07\tok\xe2\x80\xae\n\xff");
        assert_eq!(text, "green\nnextok\n\u{fffd}");
    }

    #[cfg(unix)]
    fn shell(script: &str, timeout: Duration) -> Finished {
        capture(Path::new("/bin/sh"), &["-c".into(), script.into()], timeout)
    }

    #[cfg(unix)]
    #[test]
    fn capture_reports_success_and_failure_with_the_last_line() {
        let ok = shell("printf 'first\\nRemoved backup.\\n'", CAPTURE_TIMEOUT);
        assert_eq!(
            ok,
            Finished {
                ok: true,
                summary: "Removed backup.".into()
            }
        );
        let failed = shell(
            "printf 'out\\n'; printf 'Error: \\033[31mkept\\n' >&2; exit 3",
            CAPTURE_TIMEOUT,
        );
        assert!(!failed.ok);
        assert_eq!(failed.summary, "failed with exit status 3: Error: kept");
        let missing = capture(Path::new("/nonexistent/duolane"), &[], CAPTURE_TIMEOUT);
        assert!(!missing.ok && missing.summary.starts_with("could not start"));
    }

    #[cfg(unix)]
    #[test]
    fn capture_stops_a_command_that_hangs_or_floods() {
        let started = std::time::Instant::now();
        let hung = shell("sleep 30 & sleep 30", Duration::from_millis(200));
        assert_eq!(hung.summary, "timed out after 0s");
        assert!(!hung.ok && started.elapsed() < Duration::from_secs(10));
        let flood = shell("yes 0123456789abcdef", CAPTURE_TIMEOUT);
        assert_eq!(flood.summary, "stopped after 64 KiB of output");
        assert!(!flood.ok);
    }

    #[test]
    fn runner_runs_one_action_at_a_time_and_returns_its_result() {
        let mut runner = Runner::default();
        let (release, gate) = mpsc::channel::<()>();
        assert!(runner.start("Remove backup".into(), move || {
            let _ = gate.recv();
            Finished {
                ok: true,
                summary: "done".into(),
            }
        }));
        assert_eq!(runner.running(), Some("Remove backup"));
        assert!(!runner.start("Remove other".into(), || panic!("second action started")));
        assert!(runner.poll().is_none());
        release.send(()).unwrap();
        let finished = loop {
            if let Some(finished) = runner.poll() {
                break finished;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(finished.0, "Remove backup");
        assert!(finished.1.ok && runner.running().is_none());
    }
}
