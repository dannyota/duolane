use std::path::Path;
use std::time::Duration;

pub(super) const WAIT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(super) struct Lock {
    #[cfg(unix)]
    _file: std::fs::File,
}

#[cfg(unix)]
const RETRY: Duration = Duration::from_millis(100);
#[cfg(unix)]
const BUSY: &str = "another duolane command is changing the intercept files";

#[cfg(unix)]
pub(super) fn acquire(dir: &Path, wait: Duration) -> anyhow::Result<Lock> {
    use rustix::fs::{FlockOperation, flock};
    use rustix::io::Errno;
    let parent = dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| anyhow::anyhow!("cannot create {}: {error}", parent.display()))?;
    let file = std::fs::File::open(parent)
        .map_err(|error| anyhow::anyhow!("cannot open {}: {error}", parent.display()))?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Lock { _file: file }),
            Err(Errno::WOULDBLOCK | Errno::INTR) => {}
            Err(error) => anyhow::bail!("cannot lock {}: {error}", parent.display()),
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            anyhow::bail!(BUSY);
        }
        std::thread::sleep(left.min(RETRY));
    }
}

#[cfg(not(unix))]
pub(super) fn acquire(_dir: &Path, _wait: Duration) -> anyhow::Result<Lock> {
    Ok(Lock {})
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_fails_until_the_first_releases_the_lock() {
        let root = tempfile::TempDir::new().unwrap();
        let dir = root.path().join("intercept");
        let held = acquire(&dir, WAIT).unwrap();
        let refused = acquire(&dir, Duration::ZERO).unwrap_err();
        assert_eq!(refused.to_string(), BUSY);
        drop(held);
        acquire(&dir, Duration::ZERO).unwrap();
    }
}
