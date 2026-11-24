use crossterm::style::Stylize;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const STATUS_LIMIT: u64 = 64 * 1024;
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const FIELD_CHARS: usize = 64;
const PARALLEL_QUERIES: usize = 8;

static CANCELLED: AtomicBool = AtomicBool::new(false);
static SIGNAL_LOCK: Mutex<()> = Mutex::new(());

extern "C" fn cancel_signal(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}

struct Signals {
    _lock: MutexGuard<'static, ()>,
    interrupt: libc::sigaction,
    terminate: libc::sigaction,
    hangup: libc::sigaction,
    active: bool,
}

impl Signals {
    fn install() -> anyhow::Result<Self> {
        let lock = SIGNAL_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("signal lock poisoned"))?;
        CANCELLED.store(false, Ordering::Relaxed);
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = cancel_signal as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            let mut interrupt = std::mem::zeroed();
            let mut terminate = std::mem::zeroed();
            let mut hangup = std::mem::zeroed();
            if libc::sigaction(libc::SIGINT, &action, &mut interrupt) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if libc::sigaction(libc::SIGTERM, &action, &mut terminate) != 0 {
                let error = std::io::Error::last_os_error();
                libc::sigaction(libc::SIGINT, &interrupt, std::ptr::null_mut());
                return Err(error.into());
            }
            if libc::sigaction(libc::SIGHUP, &action, &mut hangup) != 0 {
                let error = std::io::Error::last_os_error();
                libc::sigaction(libc::SIGTERM, &terminate, std::ptr::null_mut());
                libc::sigaction(libc::SIGINT, &interrupt, std::ptr::null_mut());
                return Err(error.into());
            }
            Ok(Self {
                _lock: lock,
                interrupt,
                terminate,
                hangup,
                active: true,
            })
        }
    }

    fn restore(&mut self) {
        if self.active {
            unsafe {
                libc::sigaction(libc::SIGINT, &self.interrupt, std::ptr::null_mut());
                libc::sigaction(libc::SIGTERM, &self.terminate, std::ptr::null_mut());
                libc::sigaction(libc::SIGHUP, &self.hangup, std::ptr::null_mut());
            }
            self.active = false;
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        self.restore();
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Account {
    signed_in: bool,
    email: Option<String>,
    org: Option<String>,
    plan: Option<String>,
    expires: Option<jiff::Timestamp>,
}

pub(super) struct Entry {
    pub(super) name: String,
    account: Option<Account>,
    same_as: Option<String>,
}

pub(super) struct Table {
    pub(super) header: String,
    pub(super) rows: Vec<String>,
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    c,
                    '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{feff}'
                )
        })
        .take(FIELD_CHARS)
        .collect()
}

fn parse(bytes: &[u8]) -> Option<Account> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let field = |name: &str| {
        value
            .get(name)
            .and_then(|field| field.as_str())
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    Some(Account {
        signed_in: value.get("loggedIn")?.as_bool()?,
        email: field("email"),
        org: field("orgId"),
        plan: field("subscriptionType"),
        expires: value
            .get("loginExpiresAt")
            .and_then(|field| field.as_str())
            .and_then(|text| text.parse().ok()),
    })
}

fn query(config: Option<&Path>) -> Option<Account> {
    if CANCELLED.load(Ordering::Relaxed) {
        return None;
    }
    let mut child = super::claude_command(config)
        .args(["auth", "status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let deadline = Instant::now() + STATUS_TIMEOUT;
    let mut reaped = false;
    let result = (|| {
        let mut stdout = child.stdout.take()?;
        rustix::fs::fcntl_setfl(&stdout, rustix::fs::OFlags::NONBLOCK).ok()?;
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            if CANCELLED.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return None;
            }
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    bytes.extend_from_slice(&buffer[..n]);
                    if bytes.len() as u64 > STATUS_LIMIT {
                        return None;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
        loop {
            if CANCELLED.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return None;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    reaped = true;
                    let account = parse(&bytes)?;
                    return (status.code() == Some(if account.signed_in { 0 } else { 1 }))
                        .then_some(account);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return None,
            }
        }
    })();
    if !reaped {
        if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = child.wait();
    }
    result
}

fn identity(account: &Account) -> Option<(String, Option<&str>)> {
    account
        .signed_in
        .then_some(account.email.as_ref())
        .flatten()
        .map(|email| (email.to_lowercase(), account.org.as_deref()))
}

fn mark_duplicates(entries: &mut [Entry]) {
    for index in 1..entries.len() {
        let (earlier, rest) = entries.split_at_mut(index);
        let Some(key) = rest[0].account.as_ref().and_then(identity) else {
            continue;
        };
        rest[0].same_as = earlier
            .iter()
            .find(|entry| entry.account.as_ref().and_then(identity).as_ref() == Some(&key))
            .map(|entry| entry.name.clone());
    }
}

pub(super) fn entries(profiles: &[(String, PathBuf)]) -> anyhow::Result<Vec<Entry>> {
    let mut signals = Signals::install()?;
    let mut targets: Vec<(String, Option<&Path>)> = vec![("default".into(), None)];
    targets.extend(
        profiles
            .iter()
            .map(|(name, path)| (name.clone(), Some(path.as_path()))),
    );
    let mut entries = Vec::with_capacity(targets.len());
    for chunk in targets.chunks(PARALLEL_QUERIES) {
        if CANCELLED.load(Ordering::Relaxed) {
            break;
        }
        let accounts: Vec<Option<Account>> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|(_, config)| scope.spawn(|| query(*config)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().ok().flatten())
                .collect()
        });
        for ((name, _), account) in chunk.iter().zip(accounts) {
            entries.push(Entry {
                name: name.clone(),
                account,
                same_as: None,
            });
        }
    }
    signals.restore();
    anyhow::ensure!(
        !CANCELLED.load(Ordering::Relaxed),
        "account status interrupted"
    );
    mark_duplicates(&mut entries);
    Ok(entries)
}

fn remaining(expires: jiff::Timestamp, now: jiff::Timestamp) -> String {
    let seconds = expires.as_second().saturating_sub(now.as_second());
    match seconds {
        ..=0 => "expired".into(),
        1..3600 => "<1h left".into(),
        3600..86400 => format!("{}h left", seconds / 3600),
        _ => format!("{}d left", seconds / 86400),
    }
}

fn pad(text: &str, width: usize) -> String {
    let fill = width.saturating_sub(text.chars().count());
    format!("{text}{}", " ".repeat(fill))
}

pub(super) fn table(entries: &[Entry], selected: &str, now: jiff::Timestamp, color: bool) -> Table {
    let cells: Vec<[String; 5]> = entries
        .iter()
        .map(|entry| {
            let (account, plan, login) = match &entry.account {
                None => ("unknown".into(), String::new(), String::new()),
                Some(account) if !account.signed_in => {
                    ("not signed in".into(), String::new(), String::new())
                }
                Some(account) => (
                    account
                        .email
                        .as_deref()
                        .map(clean)
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "signed in".into()),
                    account.plan.as_deref().map(clean).unwrap_or_default(),
                    account
                        .expires
                        .map(|expires| remaining(expires, now))
                        .unwrap_or_default(),
                ),
            };
            let note = entry
                .same_as
                .as_ref()
                .map(|other| format!("same account as {other}"))
                .unwrap_or_default();
            [entry.name.clone(), account, plan, login, note]
        })
        .collect();
    let titles = ["PROFILE", "ACCOUNT", "PLAN", "LOGIN", ""];
    let widths: Vec<usize> = (0..titles.len())
        .map(|column| {
            cells
                .iter()
                .map(|row| row[column].chars().count())
                .chain([titles[column].chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |marker: &str, row: [String; 5]| {
        let joined = row.join("   ");
        format!(" {marker} {}", joined.trim_end())
    };
    let header = line(
        " ",
        std::array::from_fn(|column| pad(titles[column], widths[column])),
    );
    let rows = cells
        .iter()
        .zip(entries)
        .map(|(row, entry)| {
            let chosen = entry.name == selected;
            let healthy = entry
                .account
                .as_ref()
                .is_some_and(|account| account.signed_in);
            let padded: [String; 5] =
                std::array::from_fn(|column| pad(&row[column], widths[column]));
            if !color {
                return line(if chosen { "●" } else { " " }, padded);
            }
            let [name, account, plan, login, note] = padded;
            let marker = if chosen {
                "●".green().to_string()
            } else {
                " ".into()
            };
            line(
                &marker,
                [
                    if chosen {
                        name.bold().to_string()
                    } else {
                        name
                    },
                    if healthy {
                        account
                    } else {
                        account.yellow().to_string()
                    },
                    plan,
                    login.dim().to_string(),
                    note.yellow().to_string(),
                ],
            )
        })
        .collect();
    Table {
        header: if color {
            header.dim().to_string()
        } else {
            header
        },
        rows,
    }
}

pub(super) fn describe(entries: &[Entry], name: &str) -> (Option<String>, Option<String>) {
    let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
        return (None, None);
    };
    let Some(account) = entry.account.as_ref().filter(|account| account.signed_in) else {
        return (None, None);
    };
    let summary = account
        .email
        .as_deref()
        .map(clean)
        .filter(|email| !email.is_empty())
        .map(|email| match &account.plan {
            Some(plan) => format!("{email} ({})", clean(plan)),
            None => email,
        });
    let key = identity(account);
    let twin = key.as_ref().and_then(|key| {
        entries
            .iter()
            .find(|other| {
                other.name != name
                    && other.account.as_ref().and_then(identity).as_ref() == Some(key)
            })
            .map(|other| other.name.clone())
    });
    (summary, twin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, json: &str) -> Entry {
        Entry {
            name: name.into(),
            account: parse(json.as_bytes()),
            same_as: None,
        }
    }

    #[test]
    fn claude_accounts_roster_matches_accounts_by_email_and_organization() {
        let mut entries = vec![
            entry(
                "default",
                r#"{"loggedIn":true,"email":"a@example.com","orgId":"one"}"#,
            ),
            entry(
                "other-org",
                r#"{"loggedIn":true,"email":"a@example.com","orgId":"two"}"#,
            ),
            entry(
                "copy",
                r#"{"loggedIn":true,"email":"A@Example.com","orgId":"one"}"#,
            ),
            entry(
                "signed-out",
                r#"{"loggedIn":false,"email":"a@example.com"}"#,
            ),
            entry("malformed", r#"{"loggedIn":"yes"}"#),
        ];
        mark_duplicates(&mut entries);
        let same: Vec<_> = entries
            .iter()
            .map(|entry| entry.same_as.as_deref())
            .collect();
        assert_eq!(same, [None, None, Some("default"), None, None]);
        assert!(entries[4].account.is_none());
        assert_eq!(
            describe(&entries, "default"),
            (Some("a@example.com".into()), Some("copy".into()))
        );
    }

    #[test]
    fn claude_accounts_roster_distinguishes_long_emails() {
        let prefix = "a".repeat(64);
        let first = format!(r#"{{"loggedIn":true,"email":"{prefix}1"}}"#);
        let second = format!(r#"{{"loggedIn":true,"email":"{prefix}2"}}"#);
        let mut entries = vec![entry("one", &first), entry("two", &second)];
        mark_duplicates(&mut entries);
        assert!(entries[1].same_as.is_none());
    }

    #[test]
    fn claude_accounts_roster_reports_login_time_left() {
        let now: jiff::Timestamp = "2026-01-10T00:00:00Z".parse().unwrap();
        let left = |text: &str| remaining(text.parse().unwrap(), now);
        assert_eq!(left("2026-01-13T12:00:00Z"), "3d left");
        assert_eq!(left("2026-01-10T05:30:00Z"), "5h left");
        assert_eq!(left("2026-01-10T00:10:00Z"), "<1h left");
        assert_eq!(left("2026-01-09T00:00:00Z"), "expired");
    }
}
