use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::Context;

use crate::providers::codex::translate::model_allowlist::FAMILY_ALIASES;

fn content(family: &str) -> String {
    let label = match family {
        "terra" => "Terra",
        "sol" => "Sol",
        "astra" => "Astra",
        "luna" => "Luna",
        _ => family,
    };
    format!(
        "---\nname: {family}\ndescription: General-purpose {label} agent.\nmodel: {family}\n---\n\nFollow the task brief and project instructions.\n"
    )
}

enum Existing {
    Missing,
    Symlink,
    Other,
    File(Vec<u8>),
}

fn existing(path: &Path, limit: usize) -> anyhow::Result<Existing> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Existing::Missing),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Ok(Existing::Symlink);
    }
    if !metadata.is_file() {
        return Ok(Existing::Other);
    }
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open};
        match open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fs::File::from(fd),
            Err(rustix::io::Errno::LOOP) => return Ok(Existing::Symlink),
            Err(error) => return Err(error.into()),
        }
    };
    #[cfg(not(unix))]
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Ok(Existing::Other);
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    Ok(Existing::File(bytes))
}

fn open_new(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    options.open(path)
}

fn write_file(mut file: fs::File, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    file.write_all(bytes)?;
    file.sync_all()
}

fn write_atomic(path: &Path, bytes: &[u8], replace: bool) -> std::io::Result<bool> {
    if !replace {
        let file = match open_new(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(error),
        };
        if let Err(error) = write_file(file, bytes) {
            let _ = fs::remove_file(path);
            return Err(error);
        }
        return Ok(true);
    }
    let names = (0..8).map(|_| uuid::Uuid::new_v4());
    replace_with_names(path, bytes, names).map(|()| true)
}

fn replace_with_names(
    path: &Path,
    bytes: &[u8],
    names: impl Iterator<Item = uuid::Uuid>,
) -> std::io::Result<()> {
    for name in names {
        let staging = path.with_file_name(format!(".duolane-agent-{name}"));
        let file = match open_new(&staging) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = write_file(file, bytes).and_then(|()| fs::rename(&staging, path));
        if result.is_err() {
            let _ = fs::remove_file(&staging);
        }
        return result;
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "cannot create a unique staging file",
    ))
}

fn kept_status(state: Existing, expected: &[u8]) -> &'static str {
    match state {
        Existing::Missing => "absent",
        Existing::Symlink => "skipped (symlink)",
        Existing::Other => "kept (not a regular file)",
        Existing::File(bytes) if bytes == expected => "unchanged",
        Existing::File(_) => "kept (differs; use --force)",
    }
}

pub fn setup(force: bool) -> anyhow::Result<()> {
    let dir = crate::paths::claude_config_dir()?.join("agents");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for family in FAMILY_ALIASES {
        let path = dir.join(format!("{family}.md"));
        let expected = content(family);
        let inspect = || {
            existing(&path, expected.len()).with_context(|| format!("reading {}", path.display()))
        };
        let state = inspect()?;
        let replace =
            matches!(&state, Existing::File(bytes) if bytes != expected.as_bytes()) && force;
        let create = matches!(state, Existing::Missing);
        let status = if create || replace {
            match write_atomic(&path, expected.as_bytes(), replace) {
                Ok(true) => {
                    if create {
                        "created"
                    } else {
                        "replaced"
                    }
                }
                Ok(false) => kept_status(inspect()?, expected.as_bytes()),
                Err(error) => {
                    return Err(error).with_context(|| format!("writing {}", path.display()));
                }
            }
        } else {
            kept_status(state, expected.as_bytes())
        };
        println!("{}.md: {status}", family);
    }
    println!(
        "Agents reach Codex only when Claude Code routes through duolane (ANTHROPIC_BASE_URL or intercept mode: duolane intercept setup or duolane claude). Set effort per Agent call or with an effort: line."
    );
    Ok(())
}

pub fn remove() -> anyhow::Result<()> {
    let dir = crate::paths::claude_config_dir()?.join("agents");
    for family in FAMILY_ALIASES {
        let path = dir.join(format!("{family}.md"));
        let expected = content(family);
        let status = match existing(&path, expected.len())
            .with_context(|| format!("reading {}", path.display()))?
        {
            Existing::Missing => "absent",
            Existing::Symlink => "skipped (symlink)",
            Existing::Other => "kept (not a regular file)",
            Existing::File(bytes) if bytes == expected.as_bytes() => {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
                "removed"
            }
            Existing::File(_) => "kept (differs)",
        };
        println!("{}.md: {status}", family);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_does_not_overwrite_a_file_that_appeared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terra.md");
        assert!(matches!(existing(&path, 3).unwrap(), Existing::Missing));
        fs::write(&path, b"other").unwrap();
        assert!(!write_atomic(&path, b"new", false).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"other");
    }
    #[test]
    fn replacement_retries_staging_collision_without_removing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terra.md");
        let first = uuid::Uuid::new_v4();
        let second = uuid::Uuid::new_v4();
        let staging = dir.path().join(format!(".duolane-agent-{first}"));
        fs::write(&path, b"old").unwrap();
        fs::write(&staging, b"other").unwrap();
        replace_with_names(&path, b"new", [first, second].into_iter()).unwrap();
        assert_eq!(fs::read(&staging).unwrap(), b"other");
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn new_file_does_not_need_a_staging_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terra.md");
        assert!(write_atomic(&path, b"new", false).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
