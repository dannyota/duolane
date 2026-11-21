use serde::de::{Error, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, PathBuf};

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
    let Accounts(accounts) = Accounts::deserialize(&mut de)?;
    for (name, path) in &accounts {
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 32
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
            "invalid Claude account name"
        );
        anyhow::ensure!(
            path.is_absolute() && !path.components().any(|c| c == Component::ParentDir),
            "Claude account directory must be absolute and contain no parent components"
        );
    }
    Ok(accounts)
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{list, select};

#[cfg(not(unix))]
mod non_unix;
#[cfg(not(unix))]
pub use non_unix::{list, select};

#[cfg(all(test, unix))]
mod tests {
    use super::parse_accounts;

    #[test]
    fn account_configuration_rejects_invalid_entries() {
        assert!(parse_accounts(br#"{"work":"/home/work"}"#).is_ok());
        for input in [
            br#"{"bad name":"/home/work"}"#.as_slice(),
            br#"{"":"/home/work"}"#,
            br#"{"abcdefghijklmnopqrstuvwxyz0123456":"/home/work"}"#,
            br#"{"work":"relative"}"#,
            br#"{"work":"/home/../work"}"#,
            br#"{"work":"/home/work","work":"/home/other"}"#,
        ] {
            assert!(parse_accounts(input).is_err());
        }
        assert!(parse_accounts(&vec![b' '; 16 * 1024 + 1]).is_err());
        let accounts: std::collections::BTreeMap<_, _> =
            (0..17).map(|i| (format!("p{i}"), "/home/work")).collect();
        assert!(parse_accounts(&serde_json::to_vec(&accounts).unwrap()).is_err());
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
