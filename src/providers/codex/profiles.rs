use serde::de::{Error, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, PathBuf};

const MAX_CONFIG_BYTES: usize = 16 * 1024;
const MAX_PROFILES: usize = 16;

struct Profiles(BTreeMap<String, PathBuf>);

impl<'de> Deserialize<'de> for Profiles {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ProfileVisitor;
        impl<'de> Visitor<'de> for ProfileVisitor {
            type Value = Profiles;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map of Codex profiles")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Profiles, M::Error> {
                let mut profiles = BTreeMap::new();
                while let Some((name, path)) = map.next_entry::<String, PathBuf>()? {
                    if profiles.len() >= MAX_PROFILES {
                        return Err(M::Error::custom("too many Codex profiles"));
                    }
                    if profiles.insert(name, path).is_some() {
                        return Err(M::Error::custom("duplicate Codex profile"));
                    }
                }
                Ok(Profiles(profiles))
            }
        }
        deserializer.deserialize_map(ProfileVisitor)
    }
}

pub fn parse_profiles(bytes: &[u8]) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    anyhow::ensure!(
        bytes.len() <= MAX_CONFIG_BYTES,
        "Codex profiles file exceeds 16 KiB"
    );
    let Profiles(profiles) = serde_json::from_slice(bytes)?;
    for (name, path) in &profiles {
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 32
                && name != "default"
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
            "invalid Codex profile name"
        );
        anyhow::ensure!(
            path.is_absolute() && !path.components().any(|c| c == Component::ParentDir),
            "Codex profile home must be absolute and contain no parent components"
        );
    }
    Ok(profiles)
}

#[cfg(all(test, unix))]
mod tests {
    use super::parse_profiles;

    #[test]
    fn profile_configuration_rejects_invalid_entries() {
        assert!(parse_profiles(br#"{"work":"/home/work"}"#).is_ok());
        for input in [
            br#"{"default":"/home/work"}"#.as_slice(),
            br#"{"bad name":"/home/work"}"#,
            br#"{"":"/home/work"}"#,
            br#"{"abcdefghijklmnopqrstuvwxyz0123456":"/home/work"}"#,
            br#"{"work":"relative"}"#,
            br#"{"work":"/home/../work"}"#,
            br#"{"work":"/home/work","work":"/home/other"}"#,
        ] {
            assert!(parse_profiles(input).is_err());
        }
        assert!(parse_profiles(&vec![b' '; 16 * 1024 + 1]).is_err());
        let profiles: std::collections::BTreeMap<_, _> =
            (0..17).map(|i| (format!("p{i}"), "/home/work")).collect();
        assert!(parse_profiles(&serde_json::to_vec(&profiles).unwrap()).is_err());
    }
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{AuthSource, list, resolve_selected, select};

#[cfg(not(unix))]
mod non_unix;
#[cfg(not(unix))]
pub use non_unix::{AuthSource, list, resolve_selected, select};
