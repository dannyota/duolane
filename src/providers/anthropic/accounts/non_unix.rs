use super::UsageReport;
use anyhow::{Result, bail};

pub fn select(_name: &str) -> Result<()> {
    bail!("Claude accounts are not supported on this platform")
}

pub fn list() -> Result<Vec<(String, bool)>> {
    Ok(Vec::new())
}

pub async fn usage() -> Result<UsageReport> {
    bail!("Claude accounts are not supported on this platform")
}
