#[cfg(test)]
#[path = "../tests/support/hermetic.rs"]
mod hermetic;

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand};
use duolane::{
    config,
    inbound_auth::{InboundPolicy, PLAIN_HTTP_WARNING, PROXY_KEY_HEADER},
    intercept::InterceptListener,
    logging,
    monitor::MonitorHandle,
    paths,
    registry::{ANTHROPIC_STYLE_ALIASES, Registry},
    server::{self, ServerConfig},
    text::terminal_text,
    traffic::{TRAFFIC_CAPTURE_WARNING, traffic_capture_enabled},
    tui::{self, MonitorExit, MonitorUiConfig},
};
use std::io::IsTerminal;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[command(
    name = "duolane",
    version = VERSION,
    about = "Anthropic-compatible proxy for Claude Code provider backends",
    disable_version_flag = true
)]
struct Cli {
    #[arg(long = "version", short = 'v', action = ArgAction::SetTrue)]
    version_flag: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(about = "Print version information")]
    Version,
    #[command(about = "Start the proxy server and monitor")]
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long = "no-monitor", action = ArgAction::SetTrue)]
        no_monitor: bool,
        #[arg(
            long,
            action = ArgAction::SetTrue,
            help = "Also open the intercept proxy listener"
        )]
        intercept: bool,
    },
    #[command(about = "Attach a read-only dashboard to a running proxy")]
    Monitor {
        #[arg(long)]
        url: Option<reqwest::Url>,
    },
    #[command(about = "Open the monitor TUI with mock data and no proxy server")]
    #[command(hide = true)]
    Demo,
    #[command(about = "List supported provider models")]
    Models,
    #[command(about = "Manage Codex authentication")]
    Codex {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    #[command(
        about = "Start Claude Code through the intercept proxy",
        disable_help_flag = true
    )]
    Claude {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },
    #[command(about = "Set up or remove intercept mode for every Claude Code session")]
    Intercept {
        #[command(subcommand)]
        command: InterceptCommand,
    },
}

#[derive(Debug, Subcommand)]
enum InterceptCommand {
    #[command(about = "Write the intercept proxy settings to the Claude Code user settings")]
    Setup,
    #[command(about = "Remove the intercept proxy settings and files")]
    Cleanup,
}

#[derive(Debug, Subcommand)]
enum ProviderGroup {
    Auth {
        #[command(subcommand)]
        command: duolane::provider::AuthCommand,
    },
}

fn main() -> Result<()> {
    let mut raw = std::env::args_os().skip(1);
    if raw.next().is_some_and(|command| command == "claude") {
        return duolane::intercept::run_claude(raw.collect());
    }
    let cli = Cli::parse();

    if cli.version_flag {
        println!("duolane {}", VERSION);
        return Ok(());
    }

    let commands = cli.command.unwrap_or(Commands::Serve {
        port: None,
        no_monitor: false,
        intercept: false,
    });

    match commands {
        Commands::Version => {
            println!("duolane {}", VERSION);
            Ok(())
        }
        Commands::Serve {
            port,
            no_monitor,
            intercept,
        } => {
            let bind_address = config::bind_address();
            let effective_port = port.unwrap_or_else(config::port);
            let intercept_port =
                (intercept || duolane::intercept::enabled()).then(duolane::intercept::port);
            if let Some(port) = intercept_port {
                duolane::intercept::check_proxy_environment(port)?;
                duolane::intercept::raise_open_file_limit();
            }
            let exposed = match bind_address.parse::<std::net::IpAddr>() {
                Ok(ip) => !InboundPolicy::from_config(ip)?.is_loopback(),
                Err(_) => false,
            };
            if exposed {
                eprintln!("{PLAIN_HTTP_WARNING}");
            }
            if traffic_capture_enabled() {
                eprintln!("{TRAFFIC_CAPTURE_WARNING}");
            }
            let registry = Registry::with_default_alias();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            match select_serve_mode(std::io::stdout().is_terminal(), no_monitor) {
                ServeMode::Plain => {
                    print_server_banner(&bind_address, effective_port, intercept_port, &registry);
                    runtime
                        .block_on(run_service(ServerConfig {
                            bind_address,
                            port: effective_port,
                            monitor: Some(MonitorHandle::default()),
                            intercept_port,
                        }))
                        .map_err(|err| anyhow::anyhow!(err))
                }
                ServeMode::Monitor => {
                    let _stderr_guard = logging::suppress_stderr();
                    let monitor = MonitorHandle::default();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    let (shutdown_complete_tx, shutdown_complete_rx) = std::sync::mpsc::channel();
                    let listener = runtime
                        .block_on(server::bind_proxy_listener(&bind_address, effective_port))?;
                    let intercept = intercept_port
                        .map(|port| runtime.block_on(InterceptListener::bind(port)))
                        .transpose()?;
                    let local_addr = listener.local_addr()?;
                    let mut monitor_listen_url =
                        listen_url(&local_addr.ip().to_string(), local_addr.port());
                    if let Some(intercept) = &intercept {
                        let address = intercept.local_addr()?;
                        monitor_listen_url.push_str(&format!("  intercept proxy {address}"));
                    }
                    if exposed {
                        monitor_listen_url.push_str("  plain HTTP: use an encrypted path");
                    }
                    if traffic_capture_enabled() {
                        monitor_listen_url
                            .push_str("  traffic capture on: stores conversation content");
                    }
                    let server_monitor = monitor.clone();
                    let server_task = runtime.spawn(async move {
                        let result = server::serve_listeners(
                            listener,
                            intercept,
                            Some(server_monitor),
                            async move {
                                let _ = shutdown_rx.await;
                            },
                        )
                        .await;
                        let _ = shutdown_complete_tx.send(());
                        result
                    });
                    let ui_result = tui::run_monitor(
                        monitor,
                        MonitorUiConfig {
                            listen_url: monitor_listen_url,
                            port: effective_port,
                            registry: &registry,
                            shutdown: Some(shutdown_tx),
                            shutdown_complete: Some(shutdown_complete_rx),
                        },
                    );
                    if matches!(&ui_result, Ok(MonitorExit::ForceQuit)) {
                        server_task.abort();
                        let _ = runtime.block_on(server_task);
                        std::process::exit(130);
                    }
                    let server_result = runtime.block_on(server_task)?;
                    ui_result?;
                    server_result.map_err(|err| anyhow::anyhow!(err))
                }
            }
        }
        Commands::Demo => {
            let registry = Registry::with_default_alias();
            tui::run_mock_monitor(config::port(), &registry)
        }
        Commands::Monitor { url } => {
            let url = url.unwrap_or_else(|| {
                format!("http://127.0.0.1:{}", config::port())
                    .parse()
                    .expect("local proxy URL")
            });
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let mut headers = reqwest::header::HeaderMap::new();
            if let Some(token) = config::auth_token().filter(|_| token_may_travel(&url)) {
                let mut value = reqwest::header::HeaderValue::from_str(&token)?;
                value.set_sensitive(true);
                headers.insert(PROXY_KEY_HEADER, value);
            }
            let client = duolane::tls::client_builder()
                .default_headers(headers)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(2))
                .build()?;
            let monitor = runtime
                .block_on(duolane::monitor::remote::RemoteMonitor::connect(
                    client,
                    url.clone(),
                ))
                .map_err(|err| anyhow::anyhow!(terminal_text(&format!("{err:#}"))))?;
            tui::run_attached_monitor(|| monitor.snapshot(), url.to_string())?;
            Ok(())
        }
        Commands::Models => {
            print_models(&Registry::with_default_alias());
            Ok(())
        }
        Commands::Codex { command } => run_provider_cli("codex", command),
        Commands::Claude { args } => duolane::intercept::run_claude(args),
        Commands::Intercept { command } => match command {
            InterceptCommand::Setup => duolane::intercept::setup(),
            InterceptCommand::Cleanup => duolane::intercept::cleanup(),
        },
    }
}

async fn run_service(config: ServerConfig) -> Result<()> {
    let mut signals = ServiceShutdownSignals::new()?;
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let server = server::serve_with_shutdown(config, async {
        let _ = stopped.await;
    });
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        signal = signals.recv() => {
            signal?;
            let _ = shutdown.send(());
            tokio::select! {
                result = &mut server => result,
                signal = signals.recv() => {
                    signal?;
                    std::process::exit(130);
                }
            }
        }
    }
}

#[cfg(unix)]
struct ServiceShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ServiceShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }

    async fn recv(&mut self) -> std::io::Result<()> {
        tokio::select! {
            _ = self.interrupt.recv() => Ok(()),
            _ = self.terminate.recv() => Ok(()),
        }
    }
}

#[cfg(windows)]
struct ServiceShutdownSignals {
    ctrl_c: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl ServiceShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            ctrl_c: tokio::signal::windows::ctrl_c()?,
        })
    }

    async fn recv(&mut self) -> std::io::Result<()> {
        let _ = self.ctrl_c.recv().await;
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
struct ServiceShutdownSignals;

#[cfg(not(any(unix, windows)))]
impl ServiceShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> std::io::Result<()> {
        tokio::signal::ctrl_c().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeMode {
    Monitor,
    Plain,
}

fn select_serve_mode(stdout_is_tty: bool, no_monitor: bool) -> ServeMode {
    if stdout_is_tty && !no_monitor {
        ServeMode::Monitor
    } else {
        ServeMode::Plain
    }
}

fn run_provider_cli(name: &str, command: ProviderGroup) -> Result<()> {
    let registry = Registry::with_default_alias();
    let provider = registry
        .provider(name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
    let handlers = provider.cli();
    match command {
        ProviderGroup::Auth { command } => match command {
            duolane::provider::AuthCommand::Login => {
                if let Err(err) = handlers.login() {
                    eprintln!("{err}");
                    std::process::exit(2);
                }
                Ok(())
            }
            duolane::provider::AuthCommand::Device => {
                if let Err(err) = handlers.device() {
                    eprintln!("{err}");
                    std::process::exit(2);
                }
                Ok(())
            }
            duolane::provider::AuthCommand::Status => {
                if let Err(err) = handlers.status() {
                    println!("{}", terminal_text(&err.to_string()));
                    std::process::exit(1);
                }
                Ok(())
            }
            duolane::provider::AuthCommand::Logout => {
                handlers.logout()?;
                Ok(())
            }
        },
    }
}

fn print_models(registry: &Registry) {
    let provider = "codex";
    if let Some(hint) = registry.signed_out().get(provider) {
        println!("{provider}: not signed in, {hint}");
    } else if let Some(models) = registry.grouped_models().get(provider) {
        println!("{provider}: {}", terminal_text(&models.join(", ")));
    }
}

fn token_may_travel(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        || match url.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.to_canonical().is_loopback(),
            None => false,
        }
}

fn listen_url(bind_address: &str, port: u16) -> String {
    match bind_address.parse::<std::net::IpAddr>() {
        Ok(ip) => format!("http://{}", std::net::SocketAddr::new(ip, port)),
        Err(_) => format!("http://{bind_address}:{port}"),
    }
}

fn print_server_banner(
    bind_address: &str,
    port: u16,
    intercept_port: Option<u16>,
    registry: &Registry,
) {
    println!("duolane listening on {}", listen_url(bind_address, port));
    if let Some(intercept_port) = intercept_port {
        println!(
            "Intercept proxy on {}",
            listen_url("127.0.0.1", intercept_port)
        );
        println!("Intercept CA: {}", duolane::intercept::ca_path().display());
    }
    println!("Logs: {}", paths::log_file().display());
    let cfg = paths::config_dir();
    if cfg.exists() {
        println!("Config: {}", cfg.display());
    }
    print_models(registry);
    println!();
    println!("Configure Claude Code:");
    println!("  export ANTHROPIC_BASE_URL=\"http://localhost:{port}\"");
    println!("  # Leave ANTHROPIC_AUTH_TOKEN and ANTHROPIC_API_KEY unset: Claude Code");
    println!("  # forwards your Claude subscription login for claude-* models, and any");
    println!("  # token set here overrides it and breaks the Claude route (401).");
    println!("  export ANTHROPIC_DEFAULT_OPUS_MODEL=\"claude-opus-5-5\"   # opus slot -> Claude");
    println!(
        "  export ANTHROPIC_DEFAULT_SONNET_MODEL=\"gpt-5.6-terra\"  # sonnet slot -> a backend above"
    );
    println!(
        "  export ANTHROPIC_DEFAULT_HAIKU_MODEL=\"claude-haiku-5-5\"  # small model -> Claude"
    );
}

#[allow(dead_code)]
fn alias_names() -> usize {
    ANTHROPIC_STYLE_ALIASES.len()
}

#[cfg(test)]
mod tests {
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
}
