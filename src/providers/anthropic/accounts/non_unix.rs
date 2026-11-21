use anyhow::{Result, bail};

pub fn select(_name: &str) -> Result<()> {
    bail!("Claude accounts are not supported on this platform")
}

pub fn list() -> Result<Vec<(String, bool)>> {
    Ok(Vec::new())
}
