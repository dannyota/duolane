use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod certs;
mod dispatch;
mod files;
mod listener;
mod lock;
mod manifest;
mod relay;
mod renewal;
mod settings;
mod tunnel;
mod wrapper;

pub use listener::InterceptListener;
pub use renewal::renew;
pub use settings::{cleanup, setup};
pub use wrapper::run as run_claude;

const PROXY_USER: &str = "duolane";
const INTERCEPTED_HOST: &str = "api.anthropic.com";
const INTERCEPTED_PORT: u16 = 443;
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const TCP_KEEPALIVE_RETRIES: u32 = 4;
const OPEN_FILE_TARGET: u64 = 4096;
const PROXY_VARIABLES: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"];

pub const DEFAULT_PORT: u16 = 18766;

pub fn enabled() -> bool {
    match std::env::var("DUOLANE_INTERCEPT") {
        Ok(raw) => matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => crate::config::intercept_file_settings().0.unwrap_or(false),
    }
}

pub fn port() -> u16 {
    match std::env::var("DUOLANE_INTERCEPT_PORT") {
        Ok(raw) => raw.parse().unwrap_or(DEFAULT_PORT),
        Err(_) => crate::config::intercept_file_settings()
            .1
            .unwrap_or(DEFAULT_PORT),
    }
}

pub fn intercept_dir() -> PathBuf {
    crate::paths::config_dir().join("intercept")
}

pub fn ca_path() -> PathBuf {
    intercept_dir().join(files::CA_FILE)
}

fn prepare(now: time::OffsetDateTime) -> anyhow::Result<(String, renewal::Loaded)> {
    let dir = intercept_dir();
    let _lock = lock::acquire(&dir, lock::WAIT)?;
    prepare_locked(&dir, now)
}

fn prepare_locked(
    dir: &Path,
    now: time::OffsetDateTime,
) -> anyhow::Result<(String, renewal::Loaded)> {
    files::ensure_dir(dir)?;
    let password = files::load_or_create_password(dir)?;
    let loaded = renewal::load(dir, now)?;
    manifest::refresh_files(dir)?;
    Ok((password, loaded))
}

pub fn check_proxy_environment(port: u16) -> anyhow::Result<()> {
    for name in PROXY_VARIABLES {
        if let Some(raw) = std::env::var_os(name)
            && proxy_names_listener(&raw.to_string_lossy(), port)
        {
            anyhow::bail!(
                "{name} points at the intercept proxy on port {port}, so upstream requests would loop through it; unset {name} for duolane serve"
            );
        }
    }
    Ok(())
}

fn proxy_names_listener(raw: &str, port: u16) -> bool {
    let raw = raw.trim();
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let Ok(url) = url::Url::parse(&candidate) else {
        return false;
    };
    let local = match url.host() {
        Some(url::Host::Domain(host)) => {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse()
                    .is_ok_and(is_local)
        }
        Some(url::Host::Ipv4(ip)) => is_local(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => is_local(IpAddr::V6(ip)),
        None => false,
    };
    local && url.port_or_known_default() == Some(port)
}

fn is_local(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    ip.is_loopback() || ip.is_unspecified()
}

fn set_keepalive(socket: &tokio::net::TcpStream) {
    let keepalive = socket2::TcpKeepalive::new().with_time(TCP_KEEPALIVE);
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    ))]
    let keepalive = keepalive
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_RETRIES);
    let _ = socket2::SockRef::from(socket).set_tcp_keepalive(&keepalive);
}

#[cfg(unix)]
pub fn raise_open_file_limit() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    let target = limit
        .maximum
        .map_or(OPEN_FILE_TARGET, |hard| hard.min(OPEN_FILE_TARGET));
    if limit.current.is_none_or(|soft| soft >= target) {
        return;
    }
    if let Err(error) = setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(target),
            maximum: limit.maximum,
        },
    ) {
        crate::logging::create_logger("intercept").warn(
            "could not raise the open file limit",
            Some(serde_json::Map::from_iter([
                ("target".to_string(), serde_json::json!(target)),
                ("error".to_string(), serde_json::json!(error.to_string())),
            ])),
        );
    }
}

#[cfg(not(unix))]
pub fn raise_open_file_limit() {}
