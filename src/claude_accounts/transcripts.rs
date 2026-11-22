use super::store;
use anyhow::{Context, Result, ensure};
use rustix::fs::{self, AtFlags, FileType};
use rustix::io::Errno;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn link_matches(dir: &File, key: &str, target: &Path) -> Result<bool> {
    let stat = match fs::statat(dir, key, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    ensure!(
        FileType::from_raw_mode(stat.st_mode) == FileType::Symlink,
        "project destination occupied"
    );
    let mut buf = [0u8; 4096];
    let len = fs::readlinkat_raw(dir, key, &mut buf[..])?;
    ensure!(
        len < buf.len() && &buf[..len] == target.as_os_str().as_bytes(),
        "project link target mismatch"
    );
    Ok(true)
}

fn source_dirs(default_dir: &Path, key: &str) -> Result<[Option<File>; 3]> {
    let default = store::checked_dir(default_dir, false)?;
    let projects = match &default {
        Some(dir) => store::checked_child(dir, "projects", false)?,
        None => None,
    };
    let source = match &projects {
        Some(dir) => store::checked_child(dir, key, false)?,
        None => None,
    };
    Ok([default, projects, source])
}

fn recheck_source(default_dir: &Path, key: &str, original: &[Option<File>; 3]) -> Result<()> {
    let current = source_dirs(default_dir, key)?;
    for (before, after) in original.iter().zip(current.iter()) {
        if let Some(before) = before {
            let after = after.as_ref().context("project source disappeared")?;
            let a = fs::fstat(before)?;
            let b = fs::fstat(after)?;
            ensure!(
                a.st_dev == b.st_dev && a.st_ino == b.st_ino,
                "project source changed"
            );
        }
    }
    Ok(())
}

pub(super) fn link_project(
    profile_dir: &File,
    default_dir: &Path,
    cwd: &Path,
    confirm: impl FnOnce(&Path) -> Result<bool>,
) -> Result<()> {
    let cwd_bytes = cwd.as_os_str().as_bytes();
    ensure!(
        cwd.is_absolute() && cwd_bytes.is_ascii() && cwd_bytes.len() <= 200,
        "unsupported working directory"
    );
    ensure!(
        default_dir.is_absolute(),
        "default directory must be absolute"
    );
    let key: String = cwd_bytes
        .iter()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                *b as char
            } else {
                '-'
            }
        })
        .collect();
    let target = default_dir.join("projects").join(&key);
    let source = source_dirs(default_dir, &key)?;
    let projects = store::checked_child(profile_dir, "projects", false)?;
    let records = store::checked_child(profile_dir, ".duolane-projects", false)?;
    let record = match &records {
        Some(dir) => store::private_read(dir, &key, 200)?,
        None => None,
    };
    if let Some(record) = &record {
        ensure!(record == cwd_bytes, "project path collision");
    }
    let linked = match &projects {
        Some(dir) => link_matches(dir, &key, &target)?,
        None => false,
    };
    if linked {
        ensure!(record.is_some(), "project link has no approval");
        source[2].as_ref().context("project source missing")?;
        recheck_source(default_dir, &key, &source)?;
        let projects = store::checked_child(profile_dir, "projects", false)?
            .context("profile projects missing")?;
        let records = store::checked_child(profile_dir, ".duolane-projects", false)?
            .context("project records missing")?;
        ensure!(
            store::private_read(&records, &key, 200)?.as_deref() == Some(cwd_bytes),
            "project approval changed"
        );
        ensure!(
            link_matches(&projects, &key, &target)?,
            "project link disappeared"
        );
        return Ok(());
    }
    ensure!(confirm(&target)?, "project sharing declined");
    recheck_source(default_dir, &key, &source)?;
    let default = store::checked_dir(default_dir, true)?.context("default directory missing")?;
    let source_projects =
        store::checked_child(&default, "projects", true)?.context("projects directory missing")?;
    let original_source =
        store::checked_child(&source_projects, &key, true)?.context("project source missing")?;
    let projects =
        store::checked_child(profile_dir, "projects", true)?.context("profile projects missing")?;
    let records = store::checked_child(profile_dir, ".duolane-projects", true)?
        .context("project records missing")?;
    ensure!(
        !link_matches(&projects, &key, &target)?,
        "project destination appeared"
    );
    recheck_source(default_dir, &key, &source)?;
    match store::private_read(&records, &key, 200)? {
        Some(record) => ensure!(record == cwd_bytes, "project path collision"),
        None => store::private_create(&records, &key, cwd_bytes)?,
    }
    fs::symlinkat(&target, &projects, key.as_str())?;
    let current_default =
        store::checked_dir(default_dir, false)?.context("default directory missing")?;
    let current_source_projects = store::checked_child(&current_default, "projects", false)?
        .context("projects directory missing")?;
    let current_source = store::checked_child(&current_source_projects, &key, false)?
        .context("project source missing")?;
    for (original, current) in [
        (&default, &current_default),
        (&source_projects, &current_source_projects),
        (&original_source, &current_source),
    ] {
        let a = fs::fstat(original)?;
        let b = fs::fstat(current)?;
        ensure!(
            a.st_dev == b.st_dev && a.st_ino == b.st_ino,
            "project source changed"
        );
    }
    let current_projects = store::checked_child(profile_dir, "projects", false)?
        .context("profile projects missing")?;
    let current_records = store::checked_child(profile_dir, ".duolane-projects", false)?
        .context("project records missing")?;
    for (original, current) in [(&projects, &current_projects), (&records, &current_records)] {
        let a = fs::fstat(original)?;
        let b = fs::fstat(current)?;
        ensure!(
            a.st_dev == b.st_dev && a.st_ino == b.st_ino,
            "profile directory changed"
        );
    }
    ensure!(
        store::private_read(&current_records, &key, 200)?.as_deref() == Some(cwd_bytes),
        "project approval changed"
    );
    ensure!(
        link_matches(&current_projects, &key, &target)?,
        "project link disappeared"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    fn key(path: &Path) -> String {
        path.as_os_str()
            .as_bytes()
            .iter()
            .map(|b| {
                if b.is_ascii_alphanumeric() {
                    *b as char
                } else {
                    '-'
                }
            })
            .collect()
    }

    #[test]
    fn claude_accounts_links_only_current_project() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let profile = root.join("profile");
        let default = root.join("default");
        let cwd = root.join("work");
        fs::create_dir(&profile).unwrap();
        fs::create_dir(&cwd).unwrap();
        let dir = File::open(&profile).unwrap();
        let source = default.join("projects").join(key(&cwd));
        link_project(&dir, &default, &cwd, |path| {
            assert_eq!(path, source);
            Ok(true)
        })
        .unwrap();
        let dest = profile.join("projects").join(key(&cwd));
        fs::write(source.join("marker"), b"a").unwrap();
        OpenOptions::new()
            .append(true)
            .open(dest.join("marker"))
            .unwrap()
            .write_all(b"b")
            .unwrap();
        assert!(fs::read(source.join("marker")).unwrap() == b"ab");
        link_project(&dir, &default, &cwd, |_| panic!("unexpected confirmation")).unwrap();
    }

    #[test]
    fn claude_accounts_leaves_conflicts_untouched() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let profile = root.join("profile");
        let default = root.join("default");
        let cwd = root.join("a-b");
        let collision = root.join("a_b");
        fs::create_dir(&profile).unwrap();
        let dir = File::open(&profile).unwrap();
        assert!(link_project(&dir, &default, &cwd, |_| Ok(false)).is_err());
        assert!(!default.exists());
        assert!(!profile.join("projects").exists());
        fs::create_dir(profile.join("projects")).unwrap();
        let dest = profile.join("projects").join(key(&cwd));
        fs::write(&dest, b"existing").unwrap();
        assert!(link_project(&dir, &default, &cwd, |_| Ok(true)).is_err());
        assert!(fs::read(&dest).unwrap() == b"existing");
        fs::remove_file(&dest).unwrap();
        link_project(&dir, &default, &cwd, |_| Ok(true)).unwrap();
        assert!(link_project(&dir, &default, &collision, |_| Ok(true)).is_err());
        assert!(
            fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn claude_accounts_refuses_invalid_cwd_and_bare_link() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let profile = root.join("profile");
        let default = root.join("default");
        fs::create_dir(&profile).unwrap();
        let dir = File::open(&profile).unwrap();
        assert!(
            link_project(&dir, &default, &root.join("é"), |_| panic!(
                "unexpected confirmation"
            ))
            .is_err()
        );
        assert!(
            link_project(&dir, &default, &root.join("x".repeat(201)), |_| panic!(
                "unexpected confirmation"
            ))
            .is_err()
        );
        assert!(!default.exists());
        fs::create_dir(profile.join("projects")).unwrap();
        symlink(
            default.join("projects").join(key(&root)),
            profile.join("projects").join(key(&root)),
        )
        .unwrap();
        assert!(
            link_project(&dir, &default, &root, |_| panic!("unexpected confirmation")).is_err()
        );
    }

    #[test]
    fn claude_accounts_rejects_source_replaced_during_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let profile = root.join("profile");
        let default = root.join("default");
        let source = default.join("projects").join(key(&root));
        fs::create_dir(&profile).unwrap();
        fs::create_dir_all(&source).unwrap();
        let dir = File::open(&profile).unwrap();
        assert!(
            link_project(&dir, &default, &root, |_| {
                fs::rename(&source, root.join("old-source")).unwrap();
                fs::create_dir(&source).unwrap();
                Ok(true)
            })
            .is_err()
        );
        assert!(!profile.join("projects").join(key(&root)).exists());
        assert!(!profile.join(".duolane-projects").join(key(&root)).exists());
    }

    #[test]
    fn claude_accounts_rechecks_after_confirmation() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let profile = root.join("profile");
        let default = root.join("default");
        fs::create_dir(&profile).unwrap();
        fs::create_dir(&default).unwrap();
        let dir = File::open(&profile).unwrap();
        assert!(
            link_project(&dir, &default, &root, |_| {
                symlink(&root, default.join("projects")).unwrap();
                Ok(true)
            })
            .is_err()
        );
        assert!(!profile.join("projects").exists());
    }

    #[test]
    fn claude_accounts_rejects_hostile_project_symlink() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let profile = root.join("profile");
        let default = root.join("default");
        fs::create_dir(&profile).unwrap();
        fs::create_dir(&default).unwrap();
        symlink(&root, default.join("projects")).unwrap();
        let dir = File::open(&profile).unwrap();
        assert!(link_project(&dir, &default, &root, |_| Ok(true)).is_err());
        assert!(!profile.join("projects").exists());
    }
}
