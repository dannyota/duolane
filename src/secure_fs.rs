use anyhow::{Context, Result, ensure};
use rustix::fs::{self, FileType, Mode, OFlags};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

pub(crate) fn checked(fd: &File, kind: FileType, forbidden: Mode) -> Result<()> {
    let stat = fs::fstat(fd)?;
    ensure!(
        FileType::from_raw_mode(stat.st_mode) == kind,
        "unsafe file type"
    );
    ensure!(
        stat.st_uid == rustix::process::getuid().as_raw(),
        "file has wrong owner"
    );
    ensure!(
        !Mode::from_raw_mode(stat.st_mode).intersects(forbidden),
        "unsafe file permissions"
    );
    Ok(())
}

pub(crate) fn bounded(mut file: File, limit: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "file exceeds size limit");
    Ok(data)
}

pub(crate) fn check_ancestor(fd: &File) -> Result<()> {
    let stat = fs::fstat(fd)?;
    ensure!(
        FileType::from_raw_mode(stat.st_mode) == FileType::Directory,
        "unsafe directory type"
    );
    ensure!(
        stat.st_uid == 0 || stat.st_uid == rustix::process::getuid().as_raw(),
        "directory has wrong owner"
    );
    let mode = Mode::from_raw_mode(stat.st_mode);
    ensure!(
        !mode.intersects(Mode::WGRP | Mode::WOTH) || mode.contains(Mode::SVTX),
        "unsafe ancestor permissions"
    );
    Ok(())
}

pub(crate) fn check_symlink_owner(stat: &fs::Stat) -> Result<()> {
    ensure!(
        stat.st_uid == 0 || stat.st_uid == rustix::process::getuid().as_raw(),
        "symlink has wrong owner"
    );
    Ok(())
}

pub(crate) fn path_components(path: &Path) -> VecDeque<std::ffi::OsString> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name.to_os_string()),
            std::path::Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

pub(crate) fn open_checked_directory(
    path: &Path,
    create: bool,
    strict_leaf: bool,
) -> Result<Option<(File, PathBuf)>> {
    walk_path(path, create, strict_leaf)
}

fn walk_path(path: &Path, create: bool, strict_leaf: bool) -> Result<Option<(File, PathBuf)>> {
    let path = std::path::absolute(path)?;
    let mut queue = path_components(&path);
    let root = File::from(fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    check_ancestor(&root)?;
    let mut stack = vec![root];
    let mut resolved = PathBuf::from("/");
    let mut symlinks = 0;
    while let Some(name) = queue.pop_front() {
        if name == ".." {
            if stack.len() > 1 {
                stack.pop();
                resolved.pop();
            }
            continue;
        }
        let dir = stack.last().context("missing root directory")?;
        let stat = match fs::statat(dir, &name, fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) if !create => return Ok(None),
            Err(rustix::io::Errno::NOENT) => {
                match fs::mkdirat(dir, &name, Mode::RWXU) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
                fs::statat(dir, &name, fs::AtFlags::SYMLINK_NOFOLLOW)?
            }
            Err(e) => return Err(e.into()),
        };
        if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
            ensure!(!queue.is_empty(), "unsafe final directory symlink");
            check_symlink_owner(&stat)?;
            symlinks += 1;
            ensure!(symlinks <= 40, "too many directory symlinks");
            let mut buf = [0u8; 4096];
            let len = fs::readlinkat_raw(dir, &name, &mut buf[..])?;
            ensure!(len < buf.len(), "directory symlink too long");
            let target = PathBuf::from(std::ffi::OsString::from_vec(buf[..len].to_vec()));
            if target.is_absolute() {
                stack.truncate(1);
                resolved = PathBuf::from("/");
            }
            for component in path_components(&target).into_iter().rev() {
                queue.push_front(component);
            }
            continue;
        }
        let child = File::from(fs::openat(
            dir,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        check_ancestor(&child)?;
        resolved.push(&name);
        stack.push(child);
    }
    let dir = stack.pop().context("missing root directory")?;
    if strict_leaf {
        checked(&dir, FileType::Directory, Mode::WGRP | Mode::WOTH)?;
    }
    Ok(Some((dir, resolved)))
}

pub(crate) fn fingerprint(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let hash = ring::digest::digest(&ring::digest::SHA256, path.as_os_str().as_bytes());
    hash.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn write_atomic(dir: &File, name: &str, temp_prefix: &str, data: &[u8]) -> Result<()> {
    let temp = format!("{temp_prefix}{}", uuid::Uuid::new_v4());
    let fd = fs::openat(
        dir,
        temp.as_str(),
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::NOFOLLOW
            | OFlags::NOCTTY
            | OFlags::NONBLOCK
            | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut file = File::from(fd);
    let result = (|| -> Result<()> {
        fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::renameat(dir, temp.as_str(), dir, name)?;
        fs::fsync(dir)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::unlinkat(dir, temp.as_str(), fs::AtFlags::empty());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_owner_must_be_root_or_current_user() {
        let temp = tempfile::tempfile().unwrap();
        let mut stat = fs::fstat(&temp).unwrap();
        stat.st_uid = 0;
        assert!(check_symlink_owner(&stat).is_ok());
        let current = rustix::process::getuid().as_raw();
        stat.st_uid = current;
        assert!(check_symlink_owner(&stat).is_ok());
        stat.st_uid = if current == 12345 { 12346 } else { 12345 };
        assert!(check_symlink_owner(&stat).is_err());
    }
}
