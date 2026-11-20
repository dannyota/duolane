use std::{
    collections::BTreeMap,
    fmt,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::digest;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{Error as _, MapAccess, Visitor},
};

use super::auth::token_store::{codex_auth_file, file_store_at, write_atomic};

pub(crate) const DEFAULT_PROFILE: &str = "default";
pub(crate) const SELECTION_ERROR: &str = "Codex profile selection is invalid. Run `duolane codex auth use <profile>` and restart duolane after changing the profile definitions.";
const SWITCH_HINT: &str =
    "Run `duolane codex auth use <profile>` to switch accounts, then resend the turn.";
pub(super) fn usage_limit_message(message: &str) -> String {
    if super::translate::model_catalog::has_named_profiles() {
        return format!("{message} {SWITCH_HINT}");
    }
    message.to_owned()
}

pub(super) fn add_switch_header(response: &mut axum::response::Response) {
    if response.status() == http::StatusCode::TOO_MANY_REQUESTS
        && super::translate::model_catalog::has_named_profiles()
    {
        response.headers_mut().insert(
            http::HeaderName::from_static("x-duolane-codex-profile-hint"),
            http::HeaderValue::from_static(SWITCH_HINT),
        );
    }
}

const MAX_DEFINITIONS_BYTES: u64 = 16 * 1024;
const MAX_SELECTOR_BYTES: u64 = 256;
const MAX_PROFILES: usize = 16;

pub(crate) struct Profiles {
    definitions: Definitions,
    default_auth: PathBuf,
    selector: PathBuf,
    fingerprint: digest::Digest,
}

#[derive(Default, Serialize)]
struct Definitions(BTreeMap<String, PathBuf>);

impl<'de> Deserialize<'de> for Definitions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ProfileMap;
        impl<'de> Visitor<'de> for ProfileMap {
            type Value = Definitions;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map of profile names to absolute Codex homes")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut homes = BTreeMap::new();
                while let Some(name) = map.next_key::<String>()? {
                    if homes.len() == MAX_PROFILES {
                        return Err(A::Error::custom("too many Codex profiles"));
                    }
                    if !valid_name(&name) || name == DEFAULT_PROFILE {
                        return Err(A::Error::custom("invalid or reserved Codex profile name"));
                    }
                    let home = map.next_value::<PathBuf>()?;
                    if !home.is_absolute() || home.as_os_str().as_encoded_bytes().contains(&0) {
                        return Err(A::Error::custom("invalid absolute Codex home"));
                    }
                    if homes.insert(name, home).is_some() {
                        return Err(A::Error::custom("duplicate Codex profile"));
                    }
                }
                Ok(Definitions(homes))
            }
        }
        deserializer.deserialize_map(ProfileMap)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    name: String,
    fingerprint: String,
}

impl Profiles {
    pub(crate) fn load() -> anyhow::Result<Self> {
        let config = crate::paths::config_dir().join("codex-profiles.json");
        let definitions = match read_bounded(&config, MAX_DEFINITIONS_BYTES, false)
            .context("Cannot read codex-profiles.json")?
        {
            Some(bytes) => serde_json::from_slice(&bytes).context("Invalid codex-profiles.json")?,
            None => Definitions::default(),
        };
        let fingerprint = digest::digest(&digest::SHA256, &serde_json::to_vec(&definitions)?);
        Ok(Self {
            definitions,
            default_auth: std::path::absolute(codex_auth_file())?,
            selector: crate::paths::state_dir().join("codex-profile"),
            fingerprint,
        })
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(DEFAULT_PROFILE).chain(self.definitions.0.keys().map(String::as_str))
    }

    pub(crate) fn has_named_profiles(&self) -> bool {
        !self.definitions.0.is_empty()
    }

    pub(crate) fn auth_path(&self, name: &str) -> Option<PathBuf> {
        if name == DEFAULT_PROFILE {
            return Some(self.default_auth.clone());
        }
        self.definitions
            .0
            .get(name)
            .map(|home| home.join("auth.json"))
    }

    pub(crate) fn account_scope(name: &str, auth_path: &Path) -> Option<digest::Digest> {
        if name == DEFAULT_PROFILE {
            return None;
        }
        let mut hash = digest::Context::new(&digest::SHA256);
        hash.update(name.as_bytes());
        hash.update(&[0]);
        hash.update(auth_path.as_os_str().as_encoded_bytes());
        Some(hash.finish())
    }

    pub(crate) fn active(&self) -> anyhow::Result<String> {
        let invalid = |_| anyhow!(SELECTION_ERROR);
        let Some(bytes) =
            read_bounded(&self.selector, MAX_SELECTOR_BYTES, true).map_err(invalid)?
        else {
            return Ok(DEFAULT_PROFILE.to_owned());
        };
        let selection: Selection =
            serde_json::from_slice(&bytes).map_err(|_| anyhow!(SELECTION_ERROR))?;
        ensure!(
            self.auth_path(&selection.name).is_some()
                && selection.fingerprint == self.fingerprint(&selection.name),
            "{SELECTION_ERROR}"
        );
        Ok(selection.name)
    }

    pub(crate) fn select(&self, name: &str) -> anyhow::Result<()> {
        let Some(path) = self.auth_path(name) else {
            bail!(
                "Unknown Codex profile. Run `duolane codex auth list` to see configured profiles."
            );
        };
        if name != DEFAULT_PROFILE {
            let auth = file_store_at(path).load_profile_auth(true)?;
            ensure!(
                auth.is_some(),
                "No Codex credentials. Run `codex login` in that Codex home."
            );
        }
        let selection = Selection {
            name: name.to_owned(),
            fingerprint: self.fingerprint(name),
        };
        write_atomic(&self.selector, &serde_json::to_vec(&selection)?)
    }

    fn fingerprint(&self, name: &str) -> String {
        let mut hash = digest::Context::new(&digest::SHA256);
        hash.update(self.fingerprint.as_ref());
        if name == DEFAULT_PROFILE {
            hash.update(self.default_auth.as_os_str().as_encoded_bytes());
        }
        URL_SAFE_NO_PAD.encode(hash.finish())
    }
}

fn valid_name(name: &str) -> bool {
    name.len() <= 32
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

fn read_bounded(path: &Path, limit: u64, private: bool) -> io::Result<Option<Vec<u8>>> {
    let file = match open_file(path, private) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit || (private && !owner_only(&metadata)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid profile file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "profile file is too large",
        ));
    }
    Ok(Some(bytes))
}

#[cfg(unix)]
fn open_file(path: &Path, private: bool) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, open};
    let mut flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if private {
        flags |= OFlags::NOFOLLOW;
    }
    Ok(File::from(open(path, flags, Mode::empty())?))
}

#[cfg(not(unix))]
fn open_file(path: &Path, _private: bool) -> io::Result<File> {
    File::open(path)
}

#[cfg(unix)]
fn owner_only(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.mode() & 0o077 == 0 && metadata.uid() == rustix::process::geteuid().as_raw()
}

#[cfg(not(unix))]
fn owner_only(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::codex::translate::reasoning_signature::{
        ReasoningAccount, ReasoningReplay,
    };

    #[test]
    fn named_reasoning_signature_binds_the_profile_name_path_and_account() {
        let auth_path = Path::new("/example/codex/auth.json");
        let account = |name, path, account| {
            let scope = Profiles::account_scope(name, path);
            ReasoningAccount::for_profile(Some(account), scope.as_ref())
        };
        let original = account("backup", auth_path, "acct");
        let replay = ReasoningReplay {
            id: "rs_1".into(),
            encrypted_content: "gAAAAopaque".into(),
        };
        let signature = original.sign(&replay).unwrap();
        assert_eq!(
            signature,
            "duolane:codex:v2:mN3KVdXvn-7SBT7ZUiN8mgeCPgUiPZjIn4gBOR6mAgA:cnNfMQ:gAAAAopaque"
        );
        assert_eq!(original.replay(Some(&signature)), Some(replay));
        for unrelated in [
            account("backup", auth_path, "other-acct"),
            account("renamed", auth_path, "acct"),
            account("backup", Path::new("/elsewhere/codex/auth.json"), "acct"),
            ReasoningAccount::new(Some("acct")),
        ] {
            assert_eq!(unrelated.replay(Some(&signature)), None);
        }
    }
}
