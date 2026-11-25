use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use time::OffsetDateTime;

use super::files::{self, CA_FILE, LEAF_FILE, PASSWORD_FILE};
use super::picker;
use super::renewal::{self, Due};
use super::{INTERCEPTED_HOST, INTERCEPTED_PORT, PROXY_USER, certs, proxy_names_listener};

pub(super) const HTTPS_PROXY: [&str; 2] = ["HTTPS_PROXY", "https_proxy"];
pub(super) const NO_PROXY: [&str; 2] = ["NO_PROXY", "no_proxy"];
pub(super) const CA_VARIABLE: &str = "NODE_EXTRA_CA_CERTS";
pub(super) const GZIP_VARIABLE: &str = "CLAUDE_CODE_GZIP_REQUEST_BODIES";
const FIRST_BYTE_VARIABLE: &str = "CLAUDE_STREAM_FIRST_BYTE_TIMEOUT_MS";
const IDLE_VARIABLE: &str = "CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS";
pub(super) const BASE_URL_VARIABLE: &str = "ANTHROPIC_BASE_URL";
pub(super) const LOOPBACK_NO_PROXY: &str = "localhost,127.0.0.1,::1";
const IDLE_TIMEOUT_MS: &str = "300000";

const FIRST_BYTE_MARGIN_MS: u64 = 30_000;
const LISTENER_CHECK: Duration = Duration::from_secs(5);
const MAX_RESPONSE_HEAD: usize = 8 * 1024;
const START_FIRST: &str = "start `duolane serve --intercept` first";
const PASSWORD_SENT: &str =
    "the proxy password was sent to that process; run `duolane intercept cleanup` to replace it";

pub(super) struct Install {
    pub(super) password: String,
    pub(super) port: u16,
    pub(super) ca: PathBuf,
}

impl Install {
    pub(super) fn new(password: String) -> anyhow::Result<Self> {
        let ca = std::path::absolute(super::intercept_dir().join(CA_FILE))?;
        Ok(Self {
            password,
            port: super::port(),
            ca,
        })
    }

    pub(super) fn proxy_url(&self) -> String {
        format!(
            "http://{PROXY_USER}:{}@127.0.0.1:{}",
            self.password, self.port
        )
    }

    pub(super) fn fixed(&self) -> Vec<(&'static str, String)> {
        let url = self.proxy_url();
        vec![
            (HTTPS_PROXY[0], url.clone()),
            (HTTPS_PROXY[1], url),
            (CA_VARIABLE, self.ca.to_string_lossy().into_owned()),
            (GZIP_VARIABLE, "0".to_string()),
        ]
    }
}

pub(super) fn timeouts() -> [(&'static str, String); 2] {
    [
        (
            FIRST_BYTE_VARIABLE,
            (crate::config::codex_header_timeout_ms() + FIRST_BYTE_MARGIN_MS).to_string(),
        ),
        (IDLE_VARIABLE, IDLE_TIMEOUT_MS.to_string()),
    ]
}

pub(super) fn variable(name: &str) -> Option<String> {
    std::env::var_os(name)
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.trim().is_empty())
}

pub(super) fn same_file(value: &str, expected: &Path) -> bool {
    let value = Path::new(value);
    match (
        std::fs::canonicalize(value),
        std::fs::canonicalize(expected),
    ) {
        (Ok(value), Ok(expected)) => value == expected,
        _ => std::path::absolute(value).is_ok_and(|value| value == expected),
    }
}

pub fn run(arguments: Vec<OsString>, expected: Option<&str>) -> anyhow::Result<()> {
    let dir = super::intercept_dir();
    let required = |name: &str| {
        files::read(&dir.join(name))?
            .ok_or_else(|| anyhow::anyhow!("{name} is missing in {}; {START_FIRST}", dir.display()))
    };
    let ca = required(CA_FILE)?;
    let leaf = required(LEAF_FILE)?;
    let password = String::from_utf8(required(PASSWORD_FILE)?)
        .map(|text| text.trim().to_string())
        .ok()
        .filter(|text| files::valid_password(text))
        .ok_or_else(|| anyhow::anyhow!("{PASSWORD_FILE} is not valid; {START_FIRST}"))?;
    let leaf = CertificateDer::from_pem_slice(&leaf)?;
    let authorities = certs::authorities(&ca);
    match renewal::due(&authorities, &leaf, OffsetDateTime::now_utc()) {
        Due::Expired => anyhow::bail!(renewal::EXPIRED),
        Due::Soon => eprintln!("warning: {}", renewal::SOON),
        Due::Fine => {}
    }
    let install = Install::new(password)?;
    check_environment(&install)?;
    check_listener(&install, &authorities, &leaf)?;
    let profile = crate::claude_accounts::prepare_launch(&arguments, expected)?;
    let command = child_command(
        &arguments,
        profile.as_ref(),
        &install,
        super::settings::MANAGED_SETTINGS.map(Path::new),
    );
    if let Some(profile) = &profile {
        profile.recheck()?;
    }
    start(command)
}

fn child_command(
    arguments: &[OsString],
    profile: Option<&crate::claude_accounts::Profile>,
    install: &Install,
    managed: Option<&Path>,
) -> Command {
    let mut command = Command::new("claude");
    command
        .args(picker::arguments(arguments, profile, managed))
        .args(arguments)
        .env_remove(BASE_URL_VARIABLE)
        .env_remove("DUOLANE_OPENAI_API_KEY")
        .envs(child_variables(install));
    if let Some(profile) = profile {
        command.env("CLAUDE_CONFIG_DIR", profile.path());
    }
    command
}

fn child_variables(install: &Install) -> Vec<(&'static str, String)> {
    let mut variables = install.fixed();
    for name in NO_PROXY {
        let value = match variable(name) {
            Some(existing) => format!("{existing},{LOOPBACK_NO_PROXY}"),
            None => LOOPBACK_NO_PROXY.to_string(),
        };
        variables.push((name, value));
    }
    variables.extend(
        timeouts()
            .into_iter()
            .filter(|(name, _)| variable(name).is_none()),
    );
    variables
}

fn check_environment(install: &Install) -> anyhow::Result<()> {
    for name in super::PROXY_VARIABLES {
        if variable(name).is_some_and(|value| !proxy_names_listener(&value, install.port)) {
            anyhow::bail!(
                "refusing to start: {name} names another proxy; unset it or point it at the intercept listener"
            );
        }
    }
    if variable(CA_VARIABLE).is_some_and(|value| !same_file(&value, &install.ca)) {
        anyhow::bail!(
            "refusing to start: {CA_VARIABLE} names another file than {}",
            install.ca.display()
        );
    }
    for name in NO_PROXY {
        if variable(name).is_some_and(|value| bypasses_intercept(&value)) {
            anyhow::bail!(
                "refusing to start: {name} would send {INTERCEPTED_HOST} around the intercept proxy"
            );
        }
    }
    Ok(())
}

fn bypasses_intercept(no_proxy: &str) -> bool {
    no_proxy
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            let entry = entry.to_ascii_lowercase();
            if entry == "*" {
                return true;
            }
            let host = entry
                .rsplit_once(':')
                .filter(|(_, port)| port.bytes().all(|byte| byte.is_ascii_digit()))
                .map_or(entry.as_str(), |(host, _)| host);
            let host = host.trim_start_matches('*').trim_start_matches('.');
            !host.is_empty()
                && (host == INTERCEPTED_HOST
                    || INTERCEPTED_HOST
                        .strip_suffix(host)
                        .is_some_and(|prefix| prefix.ends_with('.')))
        })
}

fn check_listener(
    install: &Install,
    authorities: &[CertificateDer<'static>],
    leaf: &CertificateDer<'static>,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + LISTENER_CHECK;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, install.port));
    let mut roots = rustls::RootCertStore::empty();
    for authority in authorities {
        roots.add(authority.clone())?;
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let name = ServerName::try_from(INTERCEPTED_HOST)?;
    let mut tls = rustls::ClientConnection::new(Arc::new(config), name)?;
    let down = || anyhow::anyhow!("the intercept listener on {address} is down; {START_FIRST}");
    let remaining = remaining(deadline).ok_or_else(down)?;
    let mut stream = TcpStream::connect_timeout(&address, remaining).map_err(|_| down())?;
    let credentials = STANDARD.encode(format!("{PROXY_USER}:{}", install.password));
    let target = format!("{INTERCEPTED_HOST}:{INTERCEPTED_PORT}");
    let head = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {credentials}\r\n\r\n"
    );
    exchange(&mut stream, &mut tls, head.as_bytes(), deadline, leaf)
        .map_err(|reason| anyhow::anyhow!("the listener on {address} {reason}; {PASSWORD_SENT}"))
}

fn exchange(
    stream: &mut TcpStream,
    tls: &mut rustls::ClientConnection,
    head: &[u8],
    deadline: Instant,
    leaf: &CertificateDer<'static>,
) -> Result<(), String> {
    let unanswered = || "did not answer CONNECT".to_string();
    arm(stream, deadline)
        .and_then(|()| stream.write_all(head))
        .map_err(|_| unanswered())?;
    match read_status(stream, deadline).ok_or_else(unanswered)? {
        200 => {}
        407 => {
            return Err(
                "refused the proxy password; restart `duolane serve --intercept`".to_string(),
            );
        }
        status => return Err(format!("answered CONNECT with {status}")),
    }
    let other =
        || "presented another certificate than duolane's; check what holds that port".to_string();
    while tls.is_handshaking() {
        if let Err(error) = arm(stream, deadline).and_then(|()| tls.complete_io(stream)) {
            let rejected = error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>())
                .is_some_and(|inner| matches!(inner, rustls::Error::InvalidCertificate(_)));
            return Err(if rejected {
                other()
            } else {
                "did not finish the TLS handshake".to_string()
            });
        }
    }
    let presented = tls.peer_certificates().and_then(|chain| chain.first());
    if presented != Some(leaf) {
        return Err(other());
    }
    Ok(())
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
}

fn arm(stream: &TcpStream, deadline: Instant) -> std::io::Result<()> {
    let left = remaining(deadline).ok_or(std::io::ErrorKind::TimedOut)?;
    stream.set_read_timeout(Some(left))?;
    stream.set_write_timeout(Some(left))
}

fn read_status(stream: &mut TcpStream, deadline: Instant) -> Option<u16> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_RESPONSE_HEAD {
            return None;
        }
        arm(stream, deadline).ok()?;
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    std::str::from_utf8(&head)
        .ok()?
        .split(' ')
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(unix)]
fn start(mut command: Command) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    Err(launch_error(command.exec()))
}

#[cfg(not(unix))]
fn start(mut command: Command) -> anyhow::Result<()> {
    let status = command.status().map_err(launch_error)?;
    std::process::exit(status.code().unwrap_or(1))
}

#[cfg(all(test, unix))]
mod account_tests {
    use super::*;

    #[test]
    fn intercept_child_removes_openai_key() {
        let install = Install {
            password: "toy".into(),
            port: 1234,
            ca: PathBuf::new(),
        };
        let command = child_command(&[], None, &install, None);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "DUOLANE_OPENAI_API_KEY" && value.is_none())
        );
    }

    #[test]
    fn intercept_child_keeps_arguments_and_sets_only_child_profile() {
        assert!(
            Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .stdin(std::process::Stdio::null())
                .args([
                    "--exact",
                    "--ignored",
                    "intercept::wrapper::account_tests::intercept_child_profile_probe",
                ])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    #[ignore]
    fn intercept_child_profile_probe() {
        let root = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", root.path()) }
        let profile = crate::claude_accounts::Profile::test_profile(root.path());
        let install = Install {
            password: "toy".into(),
            port: 1234,
            ca: root.path().join("ca"),
        };
        let args = vec![OsString::from("--resume"), OsString::from("fixture")];
        let managed = root.path().join("managed.json");
        let command = child_command(&args, Some(&profile), &install, Some(&managed));
        let actual: Vec<_> = command.get_args().collect();
        assert_eq!(
            &actual[actual.len() - args.len()..],
            args.iter().map(|arg| arg.as_os_str()).collect::<Vec<_>>()
        );
        assert!(command.get_envs().any(
            |(key, value)| key == "CLAUDE_CONFIG_DIR" && value == Some(root.path().as_os_str())
        ));
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == HTTPS_PROXY[0] && value.is_some())
        );
        let unmanaged = child_command(&args, None, &install, Some(&managed));
        assert!(
            !unmanaged
                .get_envs()
                .any(|(key, _)| key == "CLAUDE_CONFIG_DIR")
        );
    }
}

fn launch_error(error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!("claude is not on PATH; install Claude Code or add it to PATH")
    } else {
        anyhow::anyhow!("cannot start claude: {error}")
    }
}
