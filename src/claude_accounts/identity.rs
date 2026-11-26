use anyhow::Result;
use std::fs::File;
use std::os::unix::fs::MetadataExt;

pub(super) fn identity(dir: &File) -> Result<Option<String>> {
    let stat = dir.metadata()?;
    let Some(birth) = stat
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
    else {
        return Ok(None);
    };
    Ok(Some(format!(
        "{}:{}:{}:{}",
        stat.dev(),
        stat.ino(),
        birth.as_secs(),
        birth.subsec_nanos()
    )))
}
