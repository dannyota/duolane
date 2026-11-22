use super::{Listed, UsageReport};
use anyhow::{Result, bail};
use std::path::Path;

pub fn add(_name: &str, _dir: &Path) -> Result<Option<String>> {
    bail!("Claude accounts are not supported on this platform")
}

pub fn select(_name: &str) -> Result<()> {
    bail!("Claude accounts are not supported on this platform")
}

pub fn list() -> Result<Vec<Listed>> {
    Ok(Vec::new())
}

pub async fn usage() -> Result<UsageReport> {
    bail!("Claude accounts are not supported on this platform")
}
