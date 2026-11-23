#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::Context;
use anyhow::{Result, ensure};
use std::ffi::OsStr;
use std::path::PathBuf;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::claude_accounts::store;

pub struct OpenAiKey(String);

impl OpenAiKey {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let raw = raw.trim_ascii();
        ensure!(
            !raw.is_empty() && raw.len() <= 512 && raw.iter().all(|b| (0x20..=0x7e).contains(b)),
            "invalid OpenAI API key"
        );
        Ok(Self(String::from_utf8(raw.to_vec())?))
    }
    pub fn for_sending(&self) -> &str {
        &self.0
    }
    pub fn for_hashing(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOrigin {
    Stored,
    Environment,
    None,
}

pub struct KeyStore {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    config_dir: PathBuf,
}

impl KeyStore {
    pub fn new(config_dir: PathBuf) -> Self {
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = config_dir;
        Self {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            config_dir,
        }
    }
    pub fn load(&self, env: Option<&OsStr>) -> Result<(Option<OpenAiKey>, KeyOrigin)> {
        if let Some(value) = env {
            let value = value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid OpenAI API key"))?;
            return Ok((
                Some(OpenAiKey::parse(value.as_bytes())?),
                KeyOrigin::Environment,
            ));
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let Some(dir) = store::checked_strict_dir(&self.config_dir.join("credentials"), false)
                .context("check credentials directory ownership, permissions and symlinks")?
            else {
                return Ok((None, KeyOrigin::None));
            };
            match store::key_read(&dir, "openai-api-key", 4096)
                .context("check key file ownership, permissions and type")?
            {
                Some(raw) => Ok((Some(OpenAiKey::parse(&raw)?), KeyOrigin::Stored)),
                None => Ok((None, KeyOrigin::None)),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Ok((None, KeyOrigin::None))
    }
    pub fn save(&self, key: &OpenAiKey) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let dir = store::checked_strict_dir(&self.config_dir.join("credentials"), true)
                .context("check credentials directory ownership, permissions and symlinks")?
                .ok_or_else(|| anyhow::anyhow!("credentials directory missing"))?;
            let _lock = store::try_lock(&dir)?;
            let mut contents = key.for_hashing().to_vec();
            contents.push(b'\n');
            store::key_replace(&dir, "openai-api-key", &contents)
                .context("check key file ownership, permissions and type")
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = key;
            anyhow::bail!("stored OpenAI API keys are unsupported on this platform")
        }
    }
    pub fn remove(&self) -> Result<bool> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let Some(dir) = store::checked_strict_dir(&self.config_dir.join("credentials"), false)
                .context("check credentials directory ownership, permissions and symlinks")?
            else {
                return Ok(false);
            };
            let _lock = store::try_lock(&dir)?;
            store::key_remove(&dir, "openai-api-key")
                .context("check key file ownership, permissions and type")
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        anyhow::bail!("stored OpenAI API keys are unsupported on this platform")
    }
}
