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
fn stopping_runner_stops_and_reaps_child() {
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
    runner.stop();
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

#[test]
fn unavailable_terminal_refuses_before_leaving_dashboard() {
    let result = with_terminal(
        || Err::<(), _>(std::io::Error::other("no terminal")),
        |_| panic!("dashboard left"),
    )
    .unwrap();
    assert!(!result.ok && result.summary.contains("terminal unavailable"));
}
