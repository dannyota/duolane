use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
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
pub(super) fn capture(
    program: &Path,
    args: &[String],
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Finished {
    use std::io::{ErrorKind, Read};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let spawned = (|| {
        let (reader, writer) = std::io::pipe()?;
        rustix::fs::fcntl_setfl(&reader, rustix::fs::OFlags::NONBLOCK)?;
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
    let (mut child, mut reader) = match spawned {
        Ok(spawned) => spawned,
        Err(error) => return Finished::failed(format!("could not start: {}", error.kind())),
    };
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut eof = false;
    let mut status = None;
    let result = loop {
        if cancelled.load(Ordering::Acquire) {
            break Err("cancelled".to_owned());
        }
        if Instant::now() >= deadline {
            break Err(format!("timed out after {}s", timeout.as_secs()));
        }
        let mut buffer = [0u8; 8192];
        if !eof {
            match reader.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    output.extend_from_slice(&buffer[..n]);
                    if output.len() > OUTPUT_LIMIT {
                        break Err("stopped after 64 KiB of output".to_owned());
                    }
                }
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock
                        || error.kind() == ErrorKind::Interrupted => {}
                Err(error) => break Err(format!("could not read: {}", error.kind())),
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(error) => break Err(format!("could not wait: {}", error.kind())),
            }
        }
        if eof && let Some(status) = status {
            break Ok(status);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    kill_group(&child);
    let _ = child.wait();
    match result {
        Ok(status) => summarize(status.code(), &output),
        Err(reason) => Finished::failed(reason),
    }
}

#[cfg(not(unix))]
pub(super) fn capture(_: &Path, _: &[String], _: Duration, _: &AtomicBool) -> Finished {
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
trait TerminalOps {
    type Saved;
    fn save(&mut self) -> std::io::Result<Self::Saved>;
    fn give(&mut self) -> std::io::Result<()>;
    fn wait(&mut self) -> Finished;
    fn take(&mut self) -> std::io::Result<()>;
    fn restore(&mut self, saved: Self::Saved) -> std::io::Result<()>;
}

#[cfg(unix)]
fn terminal_handoff(ops: &mut impl TerminalOps) -> std::io::Result<Finished> {
    let saved = ops.save()?;
    let give = ops.give();
    let finished = if give.is_ok() { Some(ops.wait()) } else { None };
    let take = if give.is_ok() { ops.take() } else { Ok(()) };
    let restore = ops.restore(saved);
    give?;
    take?;
    restore?;
    Ok(finished.unwrap_or_else(|| Finished::failed("could not start")))
}

#[derive(Debug)]
pub(super) struct TerminalHandoffError(pub(super) std::io::Error);

impl std::fmt::Display for TerminalHandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "terminal handoff failed: {}", self.0)
    }
}

impl std::error::Error for TerminalHandoffError {}

pub(super) fn terminal_handoff_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            error
                .get_ref()
                .is_some_and(|inner| inner.is::<TerminalHandoffError>())
        })
    })
}

#[cfg(unix)]
pub(super) fn handoff(program: &Path, args: &[String]) -> std::io::Result<Finished> {
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
        Err(error) => {
            return Ok(Finished::failed(format!(
                "could not start: {}",
                error.kind()
            )));
        }
    };
    let Some(pid) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        kill_group(&child);
        return Err(std::io::Error::other("invalid child process id"));
    };
    struct Ops<'a> {
        terminal: &'a std::io::Stdin,
        pid: rustix::process::Pid,
        ours: rustix::process::Pid,
    }
    impl TerminalOps for Ops<'_> {
        type Saved = rustix::termios::Termios;
        fn save(&mut self) -> std::io::Result<Self::Saved> {
            rustix::termios::tcgetattr(self.terminal).map_err(Into::into)
        }
        fn give(&mut self) -> std::io::Result<()> {
            rustix::termios::tcsetpgrp(self.terminal, self.pid).map_err(|e| {
                std::io::Error::other(format!("could not give terminal foreground: {e}"))
            })
        }
        fn wait(&mut self) -> Finished {
            let _ = kill_process_group(self.pid, Signal::CONT);
            loop {
                match waitpid(Some(self.pid), WaitOptions::UNTRACED) {
                    Ok(Some((_, status))) if status.stopped() => {
                        let _ = kill_process_group(self.pid, Signal::CONT);
                    }
                    Ok(Some((_, status))) => break summarize(status.exit_status(), &[]),
                    Ok(None) | Err(rustix::io::Errno::INTR) => {}
                    Err(_) => break Finished::failed("could not wait"),
                }
            }
        }
        fn take(&mut self) -> std::io::Result<()> {
            let _ = kill_process_group(self.pid, Signal::KILL);
            rustix::termios::tcsetpgrp(self.terminal, self.ours).map_err(|e| {
                std::io::Error::other(format!("could not reclaim terminal foreground: {e}"))
            })
        }
        fn restore(&mut self, saved: Self::Saved) -> std::io::Result<()> {
            rustix::termios::tcsetattr(self.terminal, rustix::termios::OptionalActions::Now, &saved)
                .map_err(|e| {
                    std::io::Error::other(format!("could not restore terminal attributes: {e}"))
                })
        }
    }
    let result = terminal_handoff(&mut Ops {
        terminal: &terminal,
        pid,
        ours,
    });
    kill_group(&child);
    let _ = waitpid(Some(pid), WaitOptions::empty());
    result.map_err(|error| std::io::Error::other(TerminalHandoffError(error)))
}

#[cfg(not(unix))]
pub(super) fn handoff(_: &Path, _: &[String]) -> std::io::Result<Finished> {
    Ok(Finished::failed("unsupported on this platform"))
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
        Ok(program) => handoff(&program, args)?,
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

struct Job {
    label: String,
    receiver: mpsc::Receiver<Finished>,
    cancel: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

#[derive(Default)]
pub(super) struct Runner {
    job: Option<Job>,
}

impl Runner {
    pub(super) fn running(&self) -> Option<&str> {
        self.job.as_ref().map(|job| job.label.as_str())
    }

    pub(super) fn start(
        &mut self,
        label: String,
        run: impl FnOnce(Arc<AtomicBool>) -> Finished + Send + 'static,
    ) -> bool {
        if self.job.is_some() {
            return false;
        }
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        let Ok(thread) = std::thread::Builder::new()
            .name("account-action".into())
            .spawn(move || {
                let _ = sender.send(run(signal));
            })
        else {
            return false;
        };
        self.job = Some(Job {
            label,
            receiver,
            cancel,
            thread,
        });
        true
    }

    pub(super) fn poll(&mut self) -> Option<(String, Finished)> {
        let job = self.job.as_ref()?;
        let finished = match job.receiver.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return None,
            Ok(finished) => finished,
            Err(mpsc::TryRecvError::Disconnected) => Finished::failed("stopped unexpectedly"),
        };
        self.job.take().map(|job| {
            let _ = job.thread.join();
            (job.label, finished)
        })
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Release);
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !job.thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if job.thread.is_finished() {
                let _ = job.thread.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_errors_remain_identifiable_when_wrapped_in_io_error() {
        let error = anyhow::Error::from(std::io::Error::other(TerminalHandoffError(
            std::io::Error::other("foreground unavailable"),
        )));
        assert!(terminal_handoff_error(&error));
        assert!(!terminal_handoff_error(&anyhow::anyhow!("other error")));
    }

    #[cfg(unix)]
    #[test]
    fn handoff_restores_attributes_before_returning_or_reporting_foreground_failure() {
        struct Fake {
            steps: Vec<&'static str>,
            fail_take: bool,
        }
        impl TerminalOps for Fake {
            type Saved = ();
            fn save(&mut self) -> std::io::Result<()> {
                self.steps.push("save");
                Ok(())
            }
            fn give(&mut self) -> std::io::Result<()> {
                self.steps.push("give");
                Ok(())
            }
            fn wait(&mut self) -> Finished {
                self.steps.push("wait");
                Finished::failed("done")
            }
            fn take(&mut self) -> std::io::Result<()> {
                self.steps.push("take");
                if self.fail_take {
                    Err(std::io::Error::other("take failed"))
                } else {
                    Ok(())
                }
            }
            fn restore(&mut self, _: ()) -> std::io::Result<()> {
                self.steps.push("restore");
                Ok(())
            }
        }
        for fail_take in [false, true] {
            let mut terminal = Fake {
                steps: Vec::new(),
                fail_take,
            };
            let result = terminal_handoff(&mut terminal);
            assert_eq!(result.is_err(), fail_take);
            assert_eq!(terminal.steps, ["save", "give", "wait", "take", "restore"]);
        }
    }

    #[test]
    fn output_loses_escape_sequences_and_control_characters() {
        let text = clean(b"\x1b[32mgreen\x1b[0m\r\nnext\x07\tok\xe2\x80\xae\n\xff");
        assert_eq!(text, "green\nnextok\n\u{fffd}");
    }

    #[cfg(unix)]
    fn shell(script: &str, timeout: Duration) -> Finished {
        capture(
            Path::new("/bin/sh"),
            &["-c".into(), script.into()],
            timeout,
            &AtomicBool::new(false),
        )
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
        let missing = capture(
            Path::new("/nonexistent/duolane"),
            &[],
            CAPTURE_TIMEOUT,
            &AtomicBool::new(false),
        );
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

    #[cfg(unix)]
    #[test]
    fn capture_times_out_when_an_escaped_descendant_holds_output() {
        let started = std::time::Instant::now();
        let result = shell(
            "/usr/bin/perl -MPOSIX -e 'pipe(R,W); if (fork) { sysread(R,$x,1); exit } POSIX::setsid(); syswrite(W,chr(120)); sleep 2'",
            Duration::from_millis(200),
        );
        assert!(!result.ok);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn capture_rejects_excess_output_from_exited_child() {
        let result = shell("head -c 65537 /dev/zero", CAPTURE_TIMEOUT);
        assert!(!result.ok);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_runner_stops_and_reaps_child() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pid");
        let script = format!("printf '%s' $$ > '{}'; exec sleep 30", path.display());
        let mut runner = Runner::default();
        assert!(runner.start("sleep".into(), move |cancel| capture(
            Path::new("/bin/sh"),
            &["-c".into(), script],
            CAPTURE_TIMEOUT,
            &cancel
        )));
        let start = std::time::Instant::now();
        while !path.exists() && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid: i32 = std::fs::read_to_string(path).unwrap().parse().unwrap();
        let started = std::time::Instant::now();
        drop(runner);
        assert!(started.elapsed() < Duration::from_secs(2));
        let exists = unsafe { libc::kill(pid, 0) } == 0;
        if exists {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        assert!(!exists);
    }

    #[test]
    fn runner_runs_one_action_at_a_time_and_returns_its_result() {
        let mut runner = Runner::default();
        let (release, gate) = mpsc::channel::<()>();
        assert!(runner.start("Remove backup".into(), move |_| {
            let _ = gate.recv();
            Finished {
                ok: true,
                summary: "done".into(),
            }
        }));
        assert_eq!(runner.running(), Some("Remove backup"));
        assert!(!runner.start("Remove other".into(), |_| panic!("second action started")));
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
