use super::limits::{Limits, Window};
use crossterm::style::Stylize;
use std::collections::BTreeMap;
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

fn span(seconds: u64) -> String {
    match seconds {
        0..60 => "<1m".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h{}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d{}h", seconds / 86400, seconds % 86400 / 3600),
    }
}

fn percent(window: &Window, observed_at: u64, length: u64, now: u64) -> u64 {
    if window.resets_at.is_some_and(|reset| reset <= now)
        || (window.used >= 1.0
            && window.resets_at.is_none()
            && observed_at.saturating_add(length) <= now)
    {
        return 0;
    }
    let value = (window.used * 100.0 + 1e-9).floor() as u64;
    if window.used < 1.0 {
        value.min(99)
    } else {
        value
    }
}

fn window_cell(window: Option<&Window>, observed_at: u64, length: u64, now: u64) -> String {
    let Some(window) = window else {
        return "-".into();
    };
    let used = percent(window, observed_at, length, now);
    match window.resets_at.filter(|reset| *reset > now) {
        Some(reset) => format!("{used}% {}", span(reset - now)),
        None => format!("{used}%"),
    }
}

fn seen_cell(limits: &Limits, now: u64) -> String {
    match now.saturating_sub(limits.observed_at) {
        0..60 => "now".into(),
        age => format!(
            "{} ago",
            span(age)
                .split_inclusive(['m', 'h', 'd'])
                .next()
                .unwrap_or("")
        ),
    }
}

fn usage<'a>(
    entries: &[Entry],
    entry: &Entry,
    limits: &'a BTreeMap<String, Limits>,
) -> Option<&'a Limits> {
    let key = entry.account.as_ref().and_then(identity);
    entries
        .iter()
        .filter(|other| {
            other.name == entry.name
                || (key.is_some() && other.account.as_ref().and_then(identity) == key)
        })
        .filter_map(|other| limits.get(&other.name))
        .max_by_key(|limits| limits.observed_at)
}

pub(super) fn total(
    entries: &[Entry],
    limits: &BTreeMap<String, Limits>,
    now: jiff::Timestamp,
) -> String {
    let now = u64::try_from(now.as_second()).unwrap_or(0);
    let accounts: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.same_as.is_none())
        .collect();
    let seen: Vec<&Limits> = accounts
        .iter()
        .filter_map(|entry| usage(entries, entry, limits))
        .collect();
    let average = |pick: fn(&Limits) -> Option<&Window>, length| {
        let used: Vec<u64> = seen
            .iter()
            .filter_map(|limits| {
                pick(limits).map(|window| percent(window, limits.observed_at, length, now))
            })
            .collect();
        match used.len() {
            0 => "-".to_string(),
            count => format!("{}%", used.iter().sum::<u64>() / count as u64),
        }
    };
    format!(
        "all accounts: 5H {} used, 7D {} used ({} of {} seen by the proxy)",
        average(|limits| limits.five_hour.as_ref(), 18_000),
        average(|limits| limits.seven_day.as_ref(), 604_800),
        seen.len(),
        accounts.len()
    )
}

fn pad(text: &str, width: usize) -> String {
    let fill = width.saturating_sub(text.chars().count());
    format!("{text}{}", " ".repeat(fill))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Status {
    Available,
    Limited(Option<u64>),
    Unknown,
}

fn status(limits: Option<&Limits>, now: u64) -> Status {
    let Some(limits) = limits else {
        return Status::Unknown;
    };
    let windows = [
        (limits.five_hour.as_ref(), 18_000),
        (limits.seven_day.as_ref(), 604_800),
    ];
    if windows.iter().all(|(window, _)| window.is_none()) {
        return Status::Unknown;
    }
    let limited: Vec<&Window> = windows
        .into_iter()
        .filter_map(|(window, length)| {
            window.filter(|window| {
                window.used >= 1.0
                    && window
                        .resets_at
                        .unwrap_or_else(|| limits.observed_at.saturating_add(length))
                        > now
            })
        })
        .collect();
    if limited.is_empty() {
        return Status::Available;
    }
    Status::Limited(
        limited
            .iter()
            .map(|window| window.resets_at)
            .collect::<Option<Vec<u64>>>()
            .and_then(|resets| resets.into_iter().max()),
    )
}

fn status_cell(status: Status, now: u64) -> String {
    match status {
        Status::Available => "available".into(),
        Status::Limited(Some(reset)) => format!("limited {}", span(reset.saturating_sub(now))),
        Status::Limited(None) => "limited".into(),
        Status::Unknown => "unknown".into(),
    }
}

fn paint(status: Status, cell: String) -> String {
    match status {
        Status::Available => cell.green().to_string(),
        Status::Limited(_) => cell.red().bold().to_string(),
        Status::Unknown => cell.dim().to_string(),
    }
}

fn heat(cell: String) -> String {
    let used = cell
        .split('%')
        .next()
        .and_then(|digits| digits.parse::<u64>().ok());
    match used {
        Some(90..) => cell.red().to_string(),
        Some(70..) => cell.yellow().to_string(),
        _ => cell,
    }
}

const COLUMNS: usize = 9;

pub(super) fn table(
    entries: &[Entry],
    selected: &str,
    limits: &BTreeMap<String, Limits>,
    now: jiff::Timestamp,
    color: bool,
) -> Table {
    let seconds = u64::try_from(now.as_second()).unwrap_or(0);
    let cells: Vec<[String; COLUMNS]> = entries
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
            let usage = usage(entries, entry, limits);
            let state = status(usage, seconds);
            let note = entry
                .same_as
                .as_ref()
                .map(|other| format!("same account as {other}"))
                .unwrap_or_default();
            [
                entry.name.clone(),
                account,
                plan,
                login,
                status_cell(state, seconds),
                window_cell(
                    usage.and_then(|limits| limits.five_hour.as_ref()),
                    usage.map_or(0, |limits| limits.observed_at),
                    18_000,
                    seconds,
                ),
                window_cell(
                    usage.and_then(|limits| limits.seven_day.as_ref()),
                    usage.map_or(0, |limits| limits.observed_at),
                    604_800,
                    seconds,
                ),
                usage
                    .map(|limits| seen_cell(limits, seconds))
                    .unwrap_or_default(),
                note,
            ]
        })
        .collect();
    let titles = [
        "PROFILE", "ACCOUNT", "PLAN", "LOGIN", "STATUS", "5H", "7D", "SEEN", "",
    ];
    let widths: Vec<usize> = (0..COLUMNS)
        .map(|column| {
            cells
                .iter()
                .map(|row| row[column].chars().count())
                .chain([titles[column].chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |marker: &str, row: [String; COLUMNS]| {
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
            let padded: [String; COLUMNS] =
                std::array::from_fn(|column| pad(&row[column], widths[column]));
            if !color {
                return line(if chosen { "●" } else { " " }, padded);
            }
            let state = status(usage(entries, entry, limits), seconds);
            let [
                name,
                account,
                plan,
                login,
                state_cell,
                five_hour,
                seven_day,
                seen,
                note,
            ] = padded;
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
                    paint(state, state_cell),
                    heat(five_hour),
                    heat(seven_day),
                    seen.dim().to_string(),
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

pub(super) fn signed_out(entry: &Entry) -> bool {
    entry
        .account
        .as_ref()
        .is_some_and(|account| !account.signed_in)
}

pub(super) fn same_account(entries: &[Entry], a: &str, b: &str) -> bool {
    let key = |name: &str| {
        entries
            .iter()
            .find(|entry| entry.name == name)
            .and_then(|entry| entry.account.as_ref())
            .and_then(identity)
    };
    key(a).is_some_and(|left| key(b).as_ref() == Some(&left))
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
    fn claude_accounts_roster_marks_limited_until_reset() {
        let now = 1_000_000;
        let limits = |five: f64, reset: u64| Limits {
            five_hour: Some(Window {
                used: five,
                resets_at: Some(reset),
            }),
            seven_day: Some(Window {
                used: 0.3,
                resets_at: None,
            }),
            observed_at: now,
        };
        let full = limits(1.0, now + 4800);
        assert_eq!(status(Some(&full), now), Status::Limited(Some(now + 4800)));
        assert_eq!(status_cell(status(Some(&full), now), now), "limited 1h20m");
        assert_eq!(status(Some(&limits(1.0, now - 1)), now), Status::Available);
        assert_eq!(
            status(Some(&limits(0.99, now + 60)), now),
            Status::Available
        );
        let decimal = limits(0.29, now + 60);
        assert_eq!(
            window_cell(decimal.five_hour.as_ref(), decimal.observed_at, 18_000, now),
            "29% 1m"
        );
        let near = limits(0.996, now + 60);
        assert_eq!(status(Some(&near), now), Status::Available);
        assert_eq!(
            window_cell(near.five_hour.as_ref(), near.observed_at, 18_000, now),
            "99% 1m"
        );
        let mut expired = limits(1.0, now + 60);
        expired.five_hour.as_mut().unwrap().resets_at = None;
        expired.observed_at = now - 18_000;
        assert_eq!(status(Some(&expired), now), Status::Available);
        assert_eq!(
            window_cell(expired.five_hour.as_ref(), expired.observed_at, 18_000, now),
            "0%"
        );
        assert_eq!(status(None, now), Status::Unknown);
    }

    #[test]
    fn claude_accounts_roster_shows_usage_per_account_and_total() {
        let now: jiff::Timestamp = "2026-01-10T00:00:00Z".parse().unwrap();
        let at = u64::try_from(now.as_second()).unwrap();
        let mut entries = vec![
            entry("default", r#"{"loggedIn":true,"email":"a@example.com"}"#),
            entry("backup", r#"{"loggedIn":true,"email":"b@example.com"}"#),
            entry("copy", r#"{"loggedIn":true,"email":"a@example.com"}"#),
            entry("idle", r#"{"loggedIn":true,"email":"c@example.com"}"#),
        ];
        mark_duplicates(&mut entries);
        let window = |used, reset| {
            Some(Window {
                used,
                resets_at: Some(reset),
            })
        };
        let limits = BTreeMap::from([
            (
                "copy".to_string(),
                Limits {
                    five_hour: window(0.615, at + 7800),
                    seven_day: window(0.2, at + 277_200),
                    observed_at: at - 300,
                },
            ),
            (
                "default".to_string(),
                Limits {
                    five_hour: window(0.1, at + 60),
                    seven_day: None,
                    observed_at: at - 4000,
                },
            ),
            (
                "backup".to_string(),
                Limits {
                    five_hour: window(0.97, at - 5),
                    seven_day: window(0.4, at + 90),
                    observed_at: at - 20,
                },
            ),
        ]);
        let table = table(&entries, "default", &limits, now, false);
        let words = |index: usize| -> Vec<&str> { table.rows[index].split_whitespace().collect() };
        assert!(table.header.contains("5H") && table.header.contains("SEEN"));
        assert_eq!(
            words(0)[2..],
            [
                "a@example.com",
                "available",
                "61%",
                "2h10m",
                "20%",
                "3d5h",
                "5m",
                "ago"
            ]
        );
        assert_eq!(
            words(1)[1..],
            ["b@example.com", "available", "0%", "40%", "1m", "now"]
        );
        assert_eq!(words(2)[2..7], ["available", "61%", "2h10m", "20%", "3d5h"]);
        assert_eq!(words(3)[1..], ["c@example.com", "unknown", "-", "-"]);
        assert_eq!(
            total(&entries, &limits, now),
            "all accounts: 5H 30% used, 7D 30% used (2 of 3 seen by the proxy)"
        );
        assert_eq!(
            total(&entries, &BTreeMap::new(), now),
            "all accounts: 5H - used, 7D - used (0 of 3 seen by the proxy)"
        );
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
