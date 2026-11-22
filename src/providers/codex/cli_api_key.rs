use std::io::{self, BufRead, IsTerminal, Read, Write};

use anyhow::{Result, ensure};
use clap::{Subcommand, ValueEnum};
#[cfg(unix)]
use crossterm::terminal;

use super::{
    api::{ApiClient, ApiFailureKind},
    auth::api_key::{KeyOrigin, KeyStore, OpenAiKey},
};
use crate::{
    config::codex_auth::{self, OrderOrigin},
    paths,
};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AuthSource {
    Chatgpt,
    ApiKey,
}

impl From<AuthSource> for codex_auth::AuthSource {
    fn from(source: AuthSource) -> Self {
        match source {
            AuthSource::Chatgpt => Self::ChatGpt,
            AuthSource::ApiKey => Self::ApiKey,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    Set {
        #[arg(long)]
        no_verify: bool,
    },
    Remove,
}

pub enum CodexApiAuthCommand {
    Key(KeyCommand),
    Order {
        sources: Vec<AuthSource>,
        reset: bool,
    },
}

fn store() -> KeyStore {
    KeyStore::new(paths::config_dir())
}
fn order() -> Result<codex_auth::AuthOrder> {
    codex_auth::load_order(
        &paths::config_dir(),
        std::env::var_os("DUOLANE_CODEX_AUTH_ORDER").as_deref(),
    )
}
fn print_order(order: &codex_auth::AuthOrder) {
    let sources: Vec<_> = order
        .sources()
        .iter()
        .map(|s| match s {
            codex_auth::AuthSource::ChatGpt => "chatgpt",
            codex_auth::AuthSource::ApiKey => "api-key",
        })
        .collect();
    let origin = match order.origin() {
        OrderOrigin::Default => "default",
        OrderOrigin::File => "config",
        OrderOrigin::Environment => "environment",
    };
    println!("Order: {} ({origin})", sources.join(", "));
}

pub(crate) fn status() -> Result<bool> {
    let order = order()?;
    let enabled = order.sources().contains(&codex_auth::AuthSource::ApiKey);
    let env = std::env::var_os("DUOLANE_OPENAI_API_KEY");
    let (key, origin) = match store().load(env.as_deref()) {
        Ok(value) => value,
        Err(error) if enabled || env.is_some() => return Err(error),
        Err(_) => {
            println!("API key: unavailable (stored key could not be read)");
            print_order(&order);
            println!("Order uses API key: no");
            return Ok(false);
        }
    };
    println!(
        "API key: {}",
        match origin {
            KeyOrigin::Stored => "stored",
            KeyOrigin::Environment => "environment",
            KeyOrigin::None => "none",
        }
    );
    print_order(&order);
    println!("Order uses API key: {}", if enabled { "yes" } else { "no" });
    if key.is_some() && !enabled {
        println!("API key is inactive; configure `duolane codex auth order` to use it.");
    }
    Ok(enabled && key.is_some())
}

#[cfg(unix)]
struct RawMode;
#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = rustix::termios::tcflush(io::stdin(), rustix::termios::QueueSelector::IFlush);
        let _ = terminal::disable_raw_mode();
    }
}
#[cfg(unix)]
fn prompt(prompt: &str, hidden: bool, flush_input: bool) -> Result<Option<Vec<u8>>> {
    use rustix::termios::{QueueSelector, tcflush};
    terminal::enable_raw_mode()?;
    let guard = RawMode;
    let stdin = io::stdin();
    if flush_input {
        tcflush(&stdin, QueueSelector::IFlush)?;
    }
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut bytes = Vec::new();
    let result = loop {
        let mut byte = [0u8; 1];
        match rustix::io::read(&stdin, &mut byte) {
            Ok(0) => break None,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
        match byte[0] {
            3 | 27 => break None,
            b'\r' | b'\n' => break Some(bytes),
            8 | 127 => {
                bytes.pop();
            }
            value => {
                ensure!(bytes.len() < 513, "input too long");
                bytes.push(value);
                if !hidden && value.is_ascii_graphic() {
                    eprint!("{}", char::from(value));
                    io::stderr().flush()?;
                }
            }
        }
    };
    drop(guard);
    eprintln!();
    Ok(result)
}

#[cfg(not(unix))]
fn prompt(_prompt: &str, _hidden: bool, _flush_input: bool) -> Result<Option<Vec<u8>>> {
    anyhow::bail!("Stored keys are unsupported on this platform; use DUOLANE_OPENAI_API_KEY")
}
fn read_key(tty: bool) -> Result<Option<OpenAiKey>> {
    let bytes = if tty {
        match prompt("OpenAI API key (Esc to cancel): ", true, false)? {
            Some(v) => v,
            None => return Ok(None),
        }
    } else {
        let mut bytes = Vec::new();
        io::BufReader::new(io::stdin().lock().take(4097)).read_until(b'\n', &mut bytes)?;
        ensure!(bytes.len() <= 4096, "input too long");
        bytes
    };
    Ok(Some(OpenAiKey::parse(&bytes)?))
}
fn restart() {
    println!("Restart the running proxy to apply this change.");
}
fn save_order(sources: Option<&[codex_auth::AuthSource]>) -> Result<()> {
    codex_auth::save_order(&paths::config_dir(), sources)?;
    if std::env::var_os("DUOLANE_CODEX_AUTH_ORDER").is_some() {
        println!("DUOLANE_CODEX_AUTH_ORDER overrides the saved order.");
    }
    restart();
    Ok(())
}
fn choose_order() -> Result<()> {
    use codex_auth::AuthSource::{ApiKey, ChatGpt};
    let choice = prompt(
        "Choose order: 1) api-key  2) chatgpt, api-key  3) api-key, chatgpt  4) chatgpt\r\nChoice (Esc to cancel): ",
        false,
        true,
    )?;
    let sources: &[codex_auth::AuthSource] = match choice.as_deref() {
        Some(b"1") => &[ApiKey],
        Some(b"2") => &[ChatGpt, ApiKey],
        Some(b"3") => &[ApiKey, ChatGpt],
        Some(b"4") => &[ChatGpt],
        _ => {
            println!("Order unchanged; use `duolane codex auth order` to configure it later.");
            return Ok(());
        }
    };
    save_order(Some(sources))
}

pub fn run(command: CodexApiAuthCommand) -> Result<()> {
    match command {
        CodexApiAuthCommand::Key(KeyCommand::Set { no_verify }) => {
            if !cfg!(any(target_os = "macos", target_os = "linux")) {
                anyhow::bail!(
                    "Stored keys are unsupported on this platform; use DUOLANE_OPENAI_API_KEY"
                );
            }
            let tty = io::stdin().is_terminal();
            let Some(key) = read_key(tty)? else {
                println!("Cancelled.");
                return Ok(());
            };
            if !no_verify {
                let base = codex_auth::openai_base_url()?;
                let client = ApiClient::new(base, OpenAiKey::parse(key.for_hashing())?)?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                if let Err(failure) = runtime.block_on(client.verify()) {
                    let message = match failure.kind {
                        ApiFailureKind::Rejected => "credential rejected",
                        ApiFailureKind::Quota => "quota exhausted",
                        ApiFailureKind::Transient => "service unavailable or rate limited",
                        ApiFailureKind::HeaderTimeout => "request timed out",
                        ApiFailureKind::Invalid => "request rejected",
                        _ => "response could not be verified",
                    };
                    anyhow::bail!(
                        "OpenAI API verification failed: {message}. Use --no-verify for offline setup."
                    );
                }
            }
            store().save(&key)?;
            println!("API key saved.");
            restart();
            if std::env::var_os("DUOLANE_OPENAI_API_KEY").is_some() {
                println!("DUOLANE_OPENAI_API_KEY overrides the stored key.");
            }
            if tty && order()?.origin() == OrderOrigin::Default {
                choose_order()?;
            } else {
                println!("Use `duolane codex auth order` to configure API key use.");
            }
            Ok(())
        }
        CodexApiAuthCommand::Key(KeyCommand::Remove) => {
            store().remove()?;
            println!("Stored API key removed.");
            if std::env::var_os("DUOLANE_OPENAI_API_KEY").is_some() {
                println!(
                    "The environment key remains in use after restart whenever the order names api-key."
                );
            }
            restart();
            Ok(())
        }
        CodexApiAuthCommand::Order { sources, reset } => {
            if reset {
                save_order(None)?;
            } else if !sources.is_empty() {
                let sources: Vec<_> = sources.into_iter().map(Into::into).collect();
                save_order(Some(&sources))?;
            }
            print_order(&order()?);
            Ok(())
        }
    }
}
