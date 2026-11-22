use serde::de::{Error, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};

const MAX_CONFIG_BYTES: usize = 16 * 1024;
const MAX_ACCOUNTS: usize = 16;

struct Accounts(Vec<(String, PathBuf)>);

impl<'de> Deserialize<'de> for Accounts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AccountVisitor;
        impl<'de> Visitor<'de> for AccountVisitor {
            type Value = Accounts;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map of Claude accounts")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Accounts, M::Error> {
                let mut accounts = Vec::new();
                let mut seen = BTreeSet::new();
                while let Some((name, path)) = map.next_entry::<String, PathBuf>()? {
                    if accounts.len() >= MAX_ACCOUNTS {
                        return Err(M::Error::custom("too many Claude accounts"));
                    }
                    if !seen.insert(name.clone()) {
                        return Err(M::Error::custom("duplicate Claude account"));
                    }
                    accounts.push((name, path));
                }
                Ok(Accounts(accounts))
            }
        }
        deserializer.deserialize_map(AccountVisitor)
    }
}

pub fn parse_accounts(bytes: &[u8]) -> anyhow::Result<Vec<(String, PathBuf)>> {
    anyhow::ensure!(
        bytes.len() <= MAX_CONFIG_BYTES,
        "Claude accounts file exceeds 16 KiB"
    );
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let Ok(Accounts(accounts)) = Accounts::deserialize(&mut de) else {
        anyhow::bail!("invalid Claude accounts file");
    };
    anyhow::ensure!(de.end().is_ok(), "invalid Claude accounts file");
    for (name, path) in &accounts {
        anyhow::ensure!(valid_name(name), "invalid Claude account name");
        anyhow::ensure!(
            valid_directory(path),
            "Claude account directory must be absolute and contain no parent components"
        );
    }
    Ok(accounts)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn valid_directory(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|c| c == Component::ParentDir)
}

#[cfg(unix)]
fn render_accounts(accounts: &[(String, PathBuf)]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(accounts.len() <= MAX_ACCOUNTS, "too many Claude accounts");
    let mut entries = Vec::with_capacity(accounts.len());
    for (name, path) in accounts {
        let path = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Claude account directory must be valid UTF-8"))?;
        entries.push(format!(
            "  {}: {}",
            serde_json::to_string(name)?,
            serde_json::to_string(path)?
        ));
    }
    let data = if entries.is_empty() {
        b"{}\n".to_vec()
    } else {
        format!("{{\n{}\n}}\n", entries.join(",\n")).into_bytes()
    };
    parse_accounts(&data)?;
    Ok(data)
}

pub struct Listed {
    pub name: String,
    pub selected: bool,
    pub identity: Option<String>,
}

pub struct UsageReport {
    pub lines: Vec<String>,
    pub warning: Option<&'static str>,
}

pub const SELECTION_WARNING: &str =
    "warning: Claude account selection invalid; run duolane claude-accounts use <name>";

#[cfg(unix)]
mod identity;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{add, list, select, usage};

#[cfg(unix)]
mod usage;

#[cfg(not(unix))]
mod non_unix;
#[cfg(not(unix))]
pub use non_unix::{add, list, select, usage};

#[cfg(all(test, unix))]
mod tests {
    use super::{parse_accounts, render_accounts};
    use std::path::PathBuf;

    #[test]
    fn account_configuration_rejects_invalid_entries() {
        assert!(parse_accounts(br#"{"work":"/home/work"}"#).is_ok());
        assert!(parse_accounts(b"{\"work\":\"/home/work\"}\n  \n").is_ok());
        for input in [
            br#"{"bad name":"/home/work"}"#.as_slice(),
            br#"{"":"/home/work"}"#,
            br#"{"abcdefghijklmnopqrstuvwxyz0123456":"/home/work"}"#,
            br#"{"work":"relative"}"#,
            br#"{"work":"/home/../work"}"#,
            br#"{"work":"/home/work","work":"/home/other"}"#,
            br#"{"work":"/home/work"}{"personal":"/home/personal"}"#,
            br#"{"work":"/home/work"} x"#,
        ] {
            assert!(parse_accounts(input).is_err());
        }
        assert!(parse_accounts(&vec![b' '; 16 * 1024 + 1]).is_err());
        let accounts: std::collections::BTreeMap<_, _> =
            (0..17).map(|i| (format!("p{i}"), "/home/work")).collect();
        assert!(parse_accounts(&serde_json::to_vec(&accounts).unwrap()).is_err());
    }

    #[test]
    fn account_configuration_errors_never_echo_file_contents() {
        for input in [
            br#""secret-value""#.as_slice(),
            br#"{"work":["secret-value"]}"#,
            br#"{"work":"/home/work"} secret-value"#,
            br#"["secret-value"]"#,
        ] {
            let error = parse_accounts(input).unwrap_err();
            assert!(!format!("{error:#}").contains("secret-value"));
        }
    }

    #[test]
    fn rendered_accounts_round_trip_in_order() {
        let accounts = vec![
            ("zebra".to_string(), PathBuf::from("/z \"quoted\"")),
            ("alpha".to_string(), PathBuf::from("/a")),
        ];
        let data = render_accounts(&accounts).unwrap();
        assert_eq!(parse_accounts(&data).unwrap(), accounts);
        assert_eq!(
            parse_accounts(&render_accounts(&[]).unwrap()).unwrap(),
            vec![]
        );
    }

    #[test]
    fn account_configuration_preserves_file_order() {
        let accounts = parse_accounts(br#"{"zebra":"/z","alpha":"/a","mike":"/m"}"#).unwrap();
        assert_eq!(
            accounts
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["zebra", "alpha", "mike"]
        );
    }
}
