use super::*;

#[test]
fn default_serve_selects_monitor_on_tty() {
    assert_eq!(select_serve_mode(true, false), ServeMode::Monitor);
}

#[test]
fn no_monitor_selects_plain_mode() {
    assert_eq!(select_serve_mode(true, true), ServeMode::Plain);
}

#[test]
fn non_tty_stdout_selects_plain_mode() {
    assert_eq!(select_serve_mode(false, false), ServeMode::Plain);
}

#[test]
fn demo_command_parses_without_server_options() {
    let cli = Cli::try_parse_from(["duolane", "demo"]).unwrap();

    assert!(matches!(cli.command, Some(Commands::Demo)));
}

#[tokio::test]
async fn shutdown_signal_setup_and_receive_preserve_io_results() {
    fn assert_constructor(_: fn() -> std::io::Result<ServiceShutdownSignals>) {}
    fn assert_io_future<F: std::future::Future<Output = std::io::Result<()>>>(_: &F) {}

    assert_constructor(ServiceShutdownSignals::new);
    let mut signals = ServiceShutdownSignals::new().unwrap();
    let receive = signals.recv();
    assert_io_future(&receive);
}

#[cfg(unix)]
#[test]
fn shutdown_handles_hangup_and_termination() {
    let signals = shutdown_signal_kinds();
    assert!(signals.contains(&tokio::signal::unix::SignalKind::hangup()));
    assert!(signals.contains(&tokio::signal::unix::SignalKind::terminate()));
}

#[test]
fn monitor_token_travels_only_to_loopback_or_https() {
    for (url, expected) in [
        ("http://127.0.0.1:18765/", true),
        ("http://LOCALHOST:18765/", true),
        ("http://[::1]:18765/", true),
        ("https://proxy.example/", true),
        ("http://proxy.example:18765/", false),
        ("http://192.0.2.1:18765/", false),
    ] {
        assert_eq!(token_may_travel(&url.parse().unwrap()), expected, "{url}");
    }
}

#[test]
fn listen_url_brackets_ipv6_addresses() {
    assert_eq!(listen_url("::1", 18765), "http://[::1]:18765");
}
