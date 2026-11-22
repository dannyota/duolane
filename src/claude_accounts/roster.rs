use crossterm::style::Stylize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const STATUS_LIMIT: u64 = 64 * 1024;
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const FIELD_CHARS: usize = 64;
const PARALLEL_QUERIES: usize = 8;

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
            .map(clean)
            .filter(|text| !text.is_empty())
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
    let mut child = super::claude_command(config)
        .args(["auth", "status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + STATUS_TIMEOUT;
    let (sender, receiver) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let read = stdout.take(STATUS_LIMIT + 1).read_to_end(&mut bytes);
            let _ = sender.send(read.ok().map(|_| bytes));
        });
    }
    let bytes = receiver
        .recv_timeout(STATUS_TIMEOUT)
        .ok()
        .flatten()
        .filter(|bytes| bytes.len() as u64 <= STATUS_LIMIT);
    while bytes.is_some() && Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    parse(&bytes?)
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

pub(super) fn entries(profiles: &[(String, PathBuf)]) -> Vec<Entry> {
    let mut targets: Vec<(String, Option<&Path>)> = vec![("default".into(), None)];
    targets.extend(
        profiles
            .iter()
            .map(|(name, path)| (name.clone(), Some(path.as_path()))),
    );
    let mut entries = Vec::with_capacity(targets.len());
    for chunk in targets.chunks(PARALLEL_QUERIES) {
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
    mark_duplicates(&mut entries);
    entries
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
                    account.email.clone().unwrap_or_else(|| "signed in".into()),
                    account.plan.clone().unwrap_or_default(),
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
    let summary = account.email.as_ref().map(|email| match &account.plan {
        Some(plan) => format!("{email} ({plan})"),
        None => email.clone(),
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
    fn claude_accounts_roster_reports_login_time_left() {
        let now: jiff::Timestamp = "2026-01-10T00:00:00Z".parse().unwrap();
        let left = |text: &str| remaining(text.parse().unwrap(), now);
        assert_eq!(left("2026-01-13T12:00:00Z"), "3d left");
        assert_eq!(left("2026-01-10T05:30:00Z"), "5h left");
        assert_eq!(left("2026-01-10T00:10:00Z"), "<1h left");
        assert_eq!(left("2026-01-09T00:00:00Z"), "expired");
    }
}
