use super::limits::Limits;
use std::collections::BTreeMap;

pub(crate) const STALE_SECONDS: u64 = 900;
pub(crate) const LOGIN_WARNING_SECONDS: i64 = 3 * 86_400;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Tone {
    Good,
    Warn,
    Bad,
    #[default]
    Dim,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ProfileState {
    pub(crate) status: String,
    pub(crate) status_tone: Tone,
    pub(crate) seen: Option<String>,
    pub(crate) stale: bool,
    pub(crate) limits: Option<Limits>,
    pub(crate) account: Option<String>,
    pub(crate) login: Option<(String, Tone)>,
    pub(crate) same_as: Option<String>,
    pub(crate) suggested: bool,
}

#[derive(Default)]
pub(crate) struct Roster {
    #[cfg(unix)]
    entries: Vec<super::roster::Entry>,
}

#[cfg(not(unix))]
impl Roster {
    pub(crate) fn unknown(_: &[String]) -> Self {
        Self {}
    }

    pub(crate) fn aligned(&self, _: &[String]) -> Self {
        Self {}
    }

    pub(crate) fn states(&self, _: &BTreeMap<String, Limits>, _: u64) -> Vec<ProfileState> {
        Vec::new()
    }

    pub(crate) fn total(&self, _: &BTreeMap<String, Limits>, _: u64) -> String {
        String::new()
    }
}

#[cfg(unix)]
mod unix {
    use super::super::roster::{self, Entry, Status};
    use super::*;

    fn timestamp(now: u64) -> jiff::Timestamp {
        i64::try_from(now)
            .ok()
            .and_then(|seconds| jiff::Timestamp::from_second(seconds).ok())
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
    }

    fn login_usable(entry: &Entry, now: jiff::Timestamp) -> bool {
        entry.account.as_ref().is_none_or(|account| {
            account.signed_in && account.expires.is_none_or(|expires| expires > now)
        })
    }

    fn fuller_window(limits: &Limits, now: u64) -> Option<u64> {
        [
            (limits.five_hour.as_ref(), 18_000),
            (limits.seven_day.as_ref(), 604_800),
        ]
        .into_iter()
        .filter_map(|(window, length)| {
            window.map(|window| roster::percent(window, limits.observed_at, length, now))
        })
        .max()
    }

    impl Roster {
        pub(crate) fn unknown(names: &[String]) -> Self {
            Self {
                entries: names
                    .iter()
                    .map(|name| Entry {
                        name: name.clone(),
                        account: None,
                        same_as: None,
                    })
                    .collect(),
            }
        }

        pub(crate) fn check(
            profiles: &[(String, std::path::PathBuf)],
            cancelled: &std::sync::atomic::AtomicBool,
        ) -> Option<Self> {
            let entries = roster::collect(profiles, cancelled);
            (!cancelled.load(std::sync::atomic::Ordering::Relaxed)).then_some(Self { entries })
        }

        pub(crate) fn aligned(&self, names: &[String]) -> Self {
            let mut entries: Vec<Entry> = names
                .iter()
                .map(|name| Entry {
                    name: name.clone(),
                    account: self
                        .entries
                        .iter()
                        .find(|entry| entry.name == *name)
                        .and_then(|entry| entry.account.clone()),
                    same_as: None,
                })
                .collect();
            roster::mark_duplicates(&mut entries);
            Self { entries }
        }

        #[cfg(test)]
        pub(crate) fn from_status(profiles: &[(&str, &str)]) -> Self {
            let mut entries: Vec<Entry> = profiles
                .iter()
                .map(|(name, json)| Entry {
                    name: (*name).to_owned(),
                    account: roster::parse(json.as_bytes()),
                    same_as: None,
                })
                .collect();
            roster::mark_duplicates(&mut entries);
            Self { entries }
        }

        fn suggested(&self, limits: &BTreeMap<String, Limits>, now: u64) -> Option<usize> {
            if self.entries.len() < 2 {
                return None;
            }
            let time = timestamp(now);
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.same_as.is_none() && login_usable(entry, time))
                .filter_map(|(index, entry)| {
                    let usage = roster::usage(&self.entries, entry, limits)?;
                    (roster::status(Some(usage), now) == Status::Available)
                        .then(|| fuller_window(usage, now))
                        .flatten()
                        .map(|used| (used, index))
                })
                .min()
                .map(|(_, index)| index)
        }

        pub(crate) fn states(
            &self,
            limits: &BTreeMap<String, Limits>,
            now: u64,
        ) -> Vec<ProfileState> {
            let time = timestamp(now);
            let suggested = self.suggested(limits, now);
            self.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let usage = roster::usage(&self.entries, entry, limits);
                    let status = roster::status(usage, now);
                    let (account, login) = match &entry.account {
                        None => (None, None),
                        Some(account) if !account.signed_in => {
                            (None, Some(("not signed in".to_owned(), Tone::Bad)))
                        }
                        Some(account) => {
                            let email = account
                                .email
                                .as_deref()
                                .map(roster::clean)
                                .filter(|email| !email.is_empty())
                                .unwrap_or_else(|| "signed in".into());
                            let plan = account
                                .plan
                                .as_deref()
                                .map(roster::clean)
                                .filter(|plan| !plan.is_empty());
                            let login = account.expires.map(|expires| {
                                let left = expires.as_second().saturating_sub(time.as_second());
                                let tone = if left <= 0 {
                                    Tone::Bad
                                } else if left < LOGIN_WARNING_SECONDS {
                                    Tone::Warn
                                } else {
                                    Tone::Dim
                                };
                                (roster::remaining(expires, time), tone)
                            });
                            (
                                Some(match plan {
                                    Some(plan) => format!("{email} ({plan})"),
                                    None => email,
                                }),
                                login,
                            )
                        }
                    };
                    ProfileState {
                        status: roster::status_cell(status, now),
                        status_tone: match status {
                            Status::Available => Tone::Good,
                            Status::Limited(_) => Tone::Bad,
                            Status::Unknown => Tone::Dim,
                        },
                        seen: usage.map(|limits| roster::seen_cell(limits, now)),
                        stale: usage.is_some_and(|limits| {
                            now.saturating_sub(limits.observed_at) >= STALE_SECONDS
                        }),
                        limits: usage.copied(),
                        account,
                        login,
                        same_as: entry.same_as.clone(),
                        suggested: suggested == Some(index),
                    }
                })
                .collect()
        }

        pub(crate) fn total(&self, limits: &BTreeMap<String, Limits>, now: u64) -> String {
            roster::total(&self.entries, limits, timestamp(now))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::super::limits::Window;
        use super::*;

        fn limits(five: f64, week: f64, observed_at: u64) -> Limits {
            Limits {
                five_hour: Some(Window {
                    used: five,
                    resets_at: Some(observed_at + 10_000),
                }),
                seven_day: Some(Window {
                    used: week,
                    resets_at: Some(observed_at + 500_000),
                }),
                observed_at,
            }
        }

        fn names(names: &[&str]) -> Vec<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        }

        #[test]
        fn profile_state_reports_status_age_and_staleness() {
            let roster = Roster::unknown(&names(&["default", "full", "idle", "new"]));
            let now = 1_000_000;
            let usage = BTreeMap::from([
                ("default".to_owned(), limits(0.2, 0.3, now - 30)),
                ("full".to_owned(), limits(1.0, 0.3, now - 120)),
                ("idle".to_owned(), limits(0.1, 0.1, now - 900)),
            ]);
            let states = roster.states(&usage, now);
            assert_eq!(states[0].status, "available");
            assert_eq!(states[0].status_tone, Tone::Good);
            assert_eq!(states[0].seen.as_deref(), Some("now"));
            assert!(!states[0].stale);
            assert!(states[1].status.starts_with("limited "));
            assert_eq!(states[1].status_tone, Tone::Bad);
            assert_eq!(states[2].seen.as_deref(), Some("15m ago"));
            assert!(states[2].stale);
            assert_eq!(states[3].status, "unknown");
            assert_eq!(states[3].status_tone, Tone::Dim);
            assert_eq!(states[3].seen, None);
            assert!(!states[3].stale);
            assert!(roster.total(&usage, now).starts_with("all accounts: 5H "));
        }

        #[test]
        fn suggestion_picks_the_available_profile_with_the_emptiest_fuller_window() {
            let roster = Roster::unknown(&names(&["default", "full", "roomy", "tie", "new"]));
            let now = 1_000_000;
            let usage = BTreeMap::from([
                ("default".to_owned(), limits(0.1, 0.6, now)),
                ("full".to_owned(), limits(1.0, 0.0, now)),
                ("roomy".to_owned(), limits(0.3, 0.2, now)),
                ("tie".to_owned(), limits(0.2, 0.3, now)),
            ]);
            let marks: Vec<bool> = roster
                .states(&usage, now)
                .iter()
                .map(|state| state.suggested)
                .collect();
            assert_eq!(marks, [false, false, true, false, false]);
        }

        #[test]
        fn account_details_show_email_plan_login_and_shared_accounts() {
            let now = 1_700_000_000;
            let at = |seconds: i64| {
                jiff::Timestamp::from_second(now as i64 + seconds)
                    .unwrap()
                    .to_string()
            };
            let signed = |email: &str, left: i64| {
                format!(
                    r#"{{"loggedIn":true,"email":"{email}","orgId":"org","subscriptionType":"team","loginExpiresAt":"{}"}}"#,
                    at(left)
                )
            };
            let checked = Roster::from_status(&[
                ("default", &signed("First@Example.com", 30 * 86_400)),
                ("twin", &signed("first@example.com", 2 * 86_400)),
                ("old", &signed("old@example.com\\u001b[31m", -5)),
                ("out", r#"{"loggedIn":false}"#),
                ("gone", &signed("gone@example.com", 86_400)),
            ]);
            let roster = checked.aligned(&names(&["default", "twin", "old", "out", "new"]));
            let usage = BTreeMap::from([
                ("twin".to_owned(), limits(0.5, 0.5, now)),
                ("old".to_owned(), limits(0.0, 0.0, now)),
                ("out".to_owned(), limits(0.0, 0.0, now)),
            ]);
            let states = roster.states(&usage, now);
            assert_eq!(
                states[0].account.as_deref(),
                Some("First@Example.com (team)")
            );
            assert_eq!(states[0].login, Some(("30d left".into(), Tone::Dim)));
            assert_eq!(states[0].limits, usage.get("twin").copied());
            assert_eq!(states[1].same_as.as_deref(), Some("default"));
            assert_eq!(states[1].login, Some(("2d left".into(), Tone::Warn)));
            assert_eq!(
                states[2].account.as_deref(),
                Some("old@example.com[31m (team)")
            );
            assert_eq!(states[2].login, Some(("expired".into(), Tone::Bad)));
            assert_eq!(states[3].account, None);
            assert_eq!(states[3].login, Some(("not signed in".into(), Tone::Bad)));
            assert_eq!(
                states[4],
                ProfileState {
                    status: "unknown".into(),
                    ..Default::default()
                }
            );
            let marks: Vec<bool> = states.iter().map(|state| state.suggested).collect();
            assert_eq!(marks, [true, false, false, false, false]);
        }

        #[test]
        fn cancelled_check_returns_nothing_and_starts_no_status_process() {
            let cancelled = std::sync::atomic::AtomicBool::new(true);
            assert!(crate::claude_accounts::monitor_details(&cancelled).is_none());
        }

        #[test]
        fn suggestion_is_absent_without_a_choice_or_an_available_profile() {
            let now = 1_000_000;
            let alone = Roster::unknown(&names(&["default"]));
            let usage = BTreeMap::from([("default".to_owned(), limits(0.1, 0.1, now))]);
            assert!(!alone.states(&usage, now)[0].suggested);
            let exhausted = Roster::unknown(&names(&["default", "backup"]));
            let usage = BTreeMap::from([("default".to_owned(), limits(1.0, 0.1, now))]);
            assert!(
                exhausted
                    .states(&usage, now)
                    .iter()
                    .all(|state| !state.suggested)
            );
        }
    }
}
