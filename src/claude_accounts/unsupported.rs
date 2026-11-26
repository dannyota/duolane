pub fn switch(
    _: Option<String>,
    _: bool,
    _: Vec<std::ffi::OsString>,
    _: Option<String>,
) -> anyhow::Result<()> {
    anyhow::bail!("Claude accounts are unavailable on this platform")
}

pub(crate) struct Profile;
impl Profile {
    pub(crate) fn path(&self) -> &std::path::Path {
        unreachable!()
    }
    pub(crate) fn name(&self) -> Option<&str> {
        unreachable!()
    }
    pub(crate) fn settings(&self) -> anyhow::Result<Option<Vec<u8>>> {
        unreachable!()
    }
    pub(crate) fn recheck(&self) -> anyhow::Result<()> {
        unreachable!()
    }
}
pub(crate) fn prepare_launch(
    _: &[std::ffi::OsString],
    _: Option<(&str, Option<&str>)>,
) -> anyhow::Result<Option<Profile>> {
    Ok(None)
}

pub fn login(_: Option<String>, _: Option<String>, _: bool) -> anyhow::Result<String> {
    anyhow::bail!("Claude accounts are unavailable on this platform")
}
pub fn announce(_: &str) -> anyhow::Result<()> {
    anyhow::bail!("Claude accounts are unavailable on this platform")
}
pub fn list() -> anyhow::Result<()> {
    anyhow::bail!("Claude accounts are unavailable on this platform")
}
pub fn remove(_: String, _: bool, _: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("Claude accounts are unavailable on this platform")
}
pub fn select(_: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("Claude accounts are unavailable on this platform")
}
