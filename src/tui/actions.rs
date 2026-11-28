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
    if let Err(error) = child.wait() {
        eprintln!("could not reap account action child: {}", error.kind());
        return Finished::failed(format!("could not reap child: {}", error.kind()));
    }
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
pub(super) fn handoff(
    program: &Path,
    args: &[String],
    terminal: &std::fs::File,
) -> std::io::Result<Finished> {
    use rustix::process::{Signal, WaitOptions, kill_process_group, waitpid};
    use std::os::unix::process::CommandExt;

    let _ignore = IgnoreTtou::install();
    let ours = rustix::process::getpgrp();
    let child = match std::process::Command::new(program)
        .args(args)
        .stdin(terminal.try_clone()?)
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
        terminal: &'a std::fs::File,
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
        terminal,
        pid,
        ours,
    });
    kill_group(&child);
    let _ = waitpid(Some(pid), WaitOptions::empty());
    result.map_err(|error| std::io::Error::other(TerminalHandoffError(error)))
}

#[cfg(not(unix))]
pub(super) fn handoff(_: &Path, _: &[String], _: &std::fs::File) -> std::io::Result<Finished> {
    Ok(Finished::failed("unsupported on this platform"))
}

#[cfg(unix)]
fn open_terminal() -> std::io::Result<std::fs::File> {
    let terminal = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    rustix::termios::tcgetattr(&terminal)?;
    if rustix::termios::tcgetpgrp(&terminal)? != rustix::process::getpgrp() {
        return Err(std::io::Error::other("terminal is not in foreground"));
    }
    Ok(terminal_device(terminal))
}

#[cfg(unix)]
fn terminal_device(alias: std::fs::File) -> std::fs::File {
    use rustix::fs::OFlags;
    use std::os::fd::AsFd;
    let Ok(session) = rustix::termios::tcgetsid(&alias) else {
        return alias;
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    [stdin.as_fd(), stdout.as_fd(), stderr.as_fd()]
        .into_iter()
        .find(|fd| {
            rustix::fs::fcntl_getfl(fd).is_ok_and(|flags| flags & OFlags::RWMODE != OFlags::WRONLY)
                && rustix::termios::tcgetsid(fd).ok() == Some(session)
        })
        .and_then(|fd| fd.try_clone_to_owned().ok())
        .map_or(alias, std::fs::File::from)
}

#[cfg(not(unix))]
fn open_terminal() -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "terminal unavailable",
    ))
}

pub(super) fn run_handoff(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    label: &str,
    args: &[String],
) -> std::io::Result<Finished> {
    with_terminal(open_terminal, |tty| {
        run_handoff_ready(terminal, label, args, tty)
    })
}

fn with_terminal<T>(
    open: impl FnOnce() -> std::io::Result<T>,
    run: impl FnOnce(T) -> std::io::Result<Finished>,
) -> std::io::Result<Finished> {
    match open() {
        Ok(tty) => run(tty),
        Err(error) => Ok(Finished::failed(format!(
            "terminal unavailable: {}",
            error.kind()
        ))),
    }
}

fn run_handoff_ready(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    label: &str,
    args: &[String],
    tty: std::fs::File,
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
        Ok(program) => handoff(&program, args, &tty)?,
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

impl Runner {
    pub(super) fn stop(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Release);
            if job.thread.join().is_err() {
                eprintln!("account action thread failed during shutdown");
            }
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
