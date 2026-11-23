use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{self, FileType, Mode, OFlags};
use rustix::io::Errno;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

fn check_directory(file: &File, leaf: bool) -> Result<()> {
    let stat = fs::fstat(file)?;
    ensure!(
        FileType::from_raw_mode(stat.st_mode) == FileType::Directory,
        "unsafe directory type"
    );
    let uid = rustix::process::getuid().as_raw();
    ensure!(
        stat.st_uid == uid || (!leaf && stat.st_uid == 0),
        "directory has wrong owner"
    );
    let mode = Mode::from_raw_mode(stat.st_mode);
    ensure!(
        !mode.intersects(Mode::WGRP | Mode::WOTH)
            || (!leaf && mode.contains(Mode::SVTX) && (stat.st_uid == 0 || stat.st_uid == uid)),
        "unsafe directory permissions"
    );
    check_acl(file)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn check_acl(_: &File) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn check_acl(file: &File) -> Result<()> {
    use std::ffi::c_void;
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn acl_get_fd_np(fd: i32, kind: i32) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, id: i32, entry: *mut *mut c_void) -> i32;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut i32) -> i32;
        fn acl_free(acl: *mut c_void) -> i32;
    }
    struct Acl(*mut c_void);
    impl Drop for Acl {
        fn drop(&mut self) {
            unsafe {
                acl_free(self.0);
            }
        }
    }
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(2) {
            return Ok(());
        }
        return Err(err.into());
    }
    let acl = Acl(acl);
    let mut id = 0;
    loop {
        let mut entry = std::ptr::null_mut();
        if unsafe { acl_get_entry(acl.0, id, &mut entry) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(22) {
                break;
            }
            return Err(err.into());
        }
        let mut tag = 0;
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        ensure!(tag == 2, "unsafe or unknown ACL entry");
        id = -1;
    }
    Ok(())
}

fn components(path: &Path) -> VecDeque<OsString> {
    path.components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_os_string()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

pub(crate) fn checked_dir(path: &Path, create: bool) -> Result<Option<File>> {
    Ok(walk(path, create, false, false)?.0)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn checked_strict_dir(path: &Path, create: bool) -> Result<Option<File>> {
    let dir = walk(path, create, false, true)?.0;
    if let Some(dir) = &dir {
        ensure!(
            fs::fstat(dir)?.st_mode & 0o7777 == 0o700,
            "unsafe directory permissions"
        );
    }
    Ok(dir)
}

pub(crate) fn checked_canonical_dir(path: &Path, create: bool) -> Result<Option<PathBuf>> {
    let (dir, canonical) = walk(path, create, true, false)?;
    Ok(dir.map(|_| canonical))
}

pub(crate) fn canonical_missing(path: &Path) -> Result<PathBuf> {
    Ok(walk(path, false, false, false)?.1)
}

#[cfg(target_os = "macos")]
fn checked_path(file: &File, _: PathBuf) -> Result<PathBuf> {
    let path = PathBuf::from(OsString::from_vec(fs::getpath(file)?.into_bytes()));
    let reopened = File::from(fs::open(
        &path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let a = fs::fstat(file)?;
    let b = fs::fstat(&reopened)?;
    ensure!(
        a.st_dev == b.st_dev && a.st_ino == b.st_ino,
        "directory changed during path resolution"
    );
    Ok(path)
}

#[cfg(not(target_os = "macos"))]
fn checked_path(_: &File, path: PathBuf) -> Result<PathBuf> {
    Ok(path)
}

fn walk(
    path: &Path,
    create: bool,
    allow_final_symlink: bool,
    strict: bool,
) -> Result<(Option<File>, PathBuf)> {
    let mut queue: VecDeque<Option<OsString>> = components(&std::path::absolute(path)?)
        .into_iter()
        .map(Some)
        .collect();
    let mut resolving = 0;
    let root = File::from(fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    check_directory(&root, false)?;
    let mut stack = vec![root];
    let mut canonical = PathBuf::from("/");
    let mut links = 0;
    while let Some(part) = queue.pop_front() {
        let Some(name) = part else {
            resolving -= 1;
            continue;
        };
        if name == ".." {
            if stack.len() > 1 {
                stack.pop();
                canonical.pop();
            }
            continue;
        }
        let parent = stack.last().context("missing root")?;
        let mut created = false;
        let stat = match fs::statat(parent, &name, fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) if resolving > 0 => bail!("directory alias target does not exist"),
            Err(Errno::NOENT) if !create => {
                canonical = checked_path(parent, canonical)?;
                canonical.push(&name);
                for part in queue {
                    let part = part.context("invalid missing directory path")?;
                    ensure!(part != "..", "invalid missing directory path");
                    canonical.push(part);
                }
                return Ok((None, canonical));
            }
            Err(Errno::NOENT) => {
                match fs::mkdirat(parent, &name, Mode::RWXU) {
                    Ok(()) => created = true,
                    Err(Errno::EXIST) => {}
                    Err(err) => return Err(err.into()),
                }
                fs::statat(parent, &name, fs::AtFlags::SYMLINK_NOFOLLOW)?
            }
            Err(err) => return Err(err.into()),
        };
        if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
            ensure!(!strict, "unsafe directory symlink");
            ensure!(
                allow_final_symlink || queue.iter().any(Option::is_some),
                "unsafe final directory symlink"
            );
            ensure!(
                stat.st_uid == 0 || stat.st_uid == rustix::process::getuid().as_raw(),
                "symlink has wrong owner"
            );
            links += 1;
            ensure!(links <= 40, "too many directory symlinks");
            let mut buf = [0u8; 4096];
            let len = fs::readlinkat_raw(parent, &name, &mut buf[..])?;
            ensure!(len < buf.len(), "directory symlink too long");
            let target = PathBuf::from(OsString::from_vec(buf[..len].to_vec()));
            if target.is_absolute() {
                stack.truncate(1);
                canonical = PathBuf::from("/");
            }
            resolving += 1;
            queue.push_front(None);
            for part in components(&target).into_iter().rev() {
                queue.push_front(Some(part));
            }
            continue;
        }
        let child = match fs::openat(
            parent,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(err) => {
                if created {
                    cleanup_new_directory(parent, &name, &stat)?;
                    bail!("cannot safely open newly created directory: {err}");
                }
                return Err(err.into());
            }
        };
        check_directory(&child, false)?;
        if created {
            fs::fchmod(&child, Mode::RWXU)?;
        }
        stack.push(child);
        canonical.push(name);
    }
    let dir = stack.pop().context("missing root")?;
    check_directory(&dir, true)?;
    let canonical = checked_path(&dir, canonical)?;
    Ok((Some(dir), canonical))
}

fn cleanup_new_directory(dir: &File, name: &std::ffi::OsStr, stat: &fs::Stat) -> Result<()> {
    let current = fs::statat(dir, name, fs::AtFlags::SYMLINK_NOFOLLOW);
    if current.is_ok_and(|current| {
        current.st_dev == stat.st_dev
            && current.st_ino == stat.st_ino
            && FileType::from_raw_mode(current.st_mode) == FileType::Directory
    }) {
        fs::unlinkat(dir, name, fs::AtFlags::REMOVEDIR)
            .context("cannot clean up inaccessible new directory")?;
    }
    Ok(())
}

pub(super) fn checked_child(dir: &File, name: &str, create: bool) -> Result<Option<File>> {
    check_name(name)?;
    check_directory(dir, true)?;
    let created = if create {
        match fs::mkdirat(dir, name, Mode::RWXU) {
            Ok(()) => Some(fs::statat(dir, name, fs::AtFlags::SYMLINK_NOFOLLOW)?),
            Err(Errno::EXIST) => None,
            Err(err) => return Err(err.into()),
        }
    } else {
        None
    };
    let child = match fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) if created.is_none() => return Ok(None),
        Err(err) => {
            if let Some(stat) = &created {
                cleanup_new_directory(dir, name.as_ref(), stat)?;
            }
            return Err(err.into());
        }
    };
    check_directory(&child, true)?;
    Ok(Some(child))
}

pub(super) fn private_create(dir: &File, name: &str, contents: &[u8]) -> Result<()> {
    private_create_with(dir, name, |file| {
        file.write_all(contents)?;
        Ok(())
    })
}

fn private_create_with(
    dir: &File,
    name: &str,
    write: impl FnOnce(&mut File) -> Result<()>,
) -> Result<()> {
    check_name(name)?;
    let temp = format!(".duolane-{}", uuid::Uuid::new_v4());
    let mut file = File::from(fs::openat(
        dir,
        temp.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    let mut published = false;
    let result = (|| -> Result<()> {
        fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
        check_private(&file)?;
        write(&mut file)?;
        file.sync_all()?;
        publish_new(dir, &temp, name)?;
        published = true;
        fs::fsync(dir)?;
        Ok(())
    })();
    if !published {
        let _ = fs::unlinkat(dir, temp.as_str(), fs::AtFlags::empty());
    }
    result
}

#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
fn publish_new(dir: &File, temp: &str, name: &str) -> Result<()> {
    fs::renameat_with(dir, temp, dir, name, fs::RenameFlags::NOREPLACE)?;
    Ok(())
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
fn publish_new(_: &File, _: &str, _: &str) -> Result<()> {
    bail!("atomic no-replace publication unavailable on this platform")
}

fn check_name(name: &str) -> Result<()> {
    let mut parts = Path::new(name).components();
    ensure!(
        matches!(parts.next(), Some(Component::Normal(part)) if part == name)
            && parts.next().is_none(),
        "invalid file name"
    );
    Ok(())
}

fn check_private_mode(file: &File, exact: bool) -> Result<()> {
    let stat = fs::fstat(file)?;
    ensure!(
        FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile,
        "unsafe file type"
    );
    ensure!(
        stat.st_uid == rustix::process::getuid().as_raw(),
        "file has wrong owner"
    );
    ensure!(
        if exact {
            stat.st_mode & 0o7777 == 0o600
        } else {
            stat.st_mode & 0o7077 == 0
        },
        "unsafe file permissions"
    );
    ensure!(stat.st_nlink == 1, "unsafe file links");
    check_acl(file)?;
    Ok(())
}

pub(crate) fn check_private(file: &File) -> Result<()> {
    check_private_mode(file, true)
}

fn open_private_mode(dir: &File, name: &str, exact: bool) -> Result<Option<File>> {
    check_name(name)?;
    let file = match fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    check_private_mode(&file, exact)?;
    Ok(Some(file))
}

fn open_private(dir: &File, name: &str) -> Result<Option<File>> {
    open_private_mode(dir, name, true)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn key_read(dir: &File, name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
    let Some(file) = open_private_mode(dir, name, false)? else {
        return Ok(None);
    };
    let mut data = Vec::new();
    file.take(limit.checked_add(1).context("invalid size limit")?)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "file exceeds size limit");
    Ok(Some(data))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn key_remove(dir: &File, name: &str) -> Result<bool> {
    if open_private_mode(dir, name, false)?.is_none() {
        return Ok(false);
    }
    fs::unlinkat(dir, name, fs::AtFlags::empty())?;
    fs::fsync(dir)?;
    Ok(true)
}

pub(super) fn settings_read(dir: &File) -> Result<Option<Vec<u8>>> {
    let file = match fs::openat(
        dir,
        "settings.json",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(Errno::NOENT) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let stat = fs::fstat(&file)?;
    ensure!(
        FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
            && stat.st_uid == rustix::process::getuid().as_raw()
            && stat.st_nlink == 1
            && stat.st_mode & 0o7022 == 0,
        "unsafe settings file"
    );
    check_acl(&file)?;
    let mut data = Vec::new();
    file.take(1_048_577).read_to_end(&mut data)?;
    ensure!(data.len() <= 1_048_576, "settings file exceeds size limit");
    Ok(Some(data))
}

pub(crate) fn private_read(dir: &File, name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
    let Some(file) = open_private(dir, name)? else {
        return Ok(None);
    };
    let mut data = Vec::new();
    file.take(limit.checked_add(1).context("invalid size limit")?)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "file exceeds size limit");
    Ok(Some(data))
}

pub(crate) fn private_replace(dir: &File, name: &str, contents: &[u8]) -> Result<()> {
    private_replace_mode(dir, name, contents, true)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn key_replace(dir: &File, name: &str, contents: &[u8]) -> Result<()> {
    private_replace_mode(dir, name, contents, false)
}

fn private_replace_mode(dir: &File, name: &str, contents: &[u8], exact: bool) -> Result<()> {
    check_name(name)?;
    ensure!(name != ".duolane-lock", "reserved lock file name");
    open_private_mode(dir, name, exact)?;
    let temp = format!(".duolane-{}", uuid::Uuid::new_v4());
    let mut file = File::from(fs::openat(
        dir,
        temp.as_str(),
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    let mut renamed = false;
    let result = (|| -> Result<()> {
        fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
        check_private(&file)?;
        file.write_all(contents)?;
        file.sync_all()?;
        open_private_mode(dir, name, exact)?;
        fs::renameat(dir, temp.as_str(), dir, name)?;
        renamed = true;
        fs::fsync(dir)?;
        Ok(())
    })();
    if result.is_err() && !renamed {
        let _ = fs::unlinkat(dir, temp.as_str(), fs::AtFlags::empty());
    }
    result
}

pub(crate) fn try_lock(dir: &File) -> Result<File> {
    let flags =
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY;
    let file = match fs::openat(
        dir,
        ".duolane-lock",
        flags | OFlags::CREATE | OFlags::EXCL,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(fd) => {
            let file = File::from(fd);
            fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
            file
        }
        Err(Errno::EXIST) => File::from(fs::openat(dir, ".duolane-lock", flags, Mode::empty())?),
        Err(err) => return Err(err.into()),
    };
    check_private(&file)?;
    loop {
        match fs::flock(&file, fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(file),
            Err(Errno::INTR) => continue,
            Err(Errno::WOULDBLOCK) => bail!("private store lock is busy"),
            Err(err) => return Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[cfg(target_os = "macos")]
    #[test]
    fn claude_accounts_case_alias_resolves_same_path() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("Config");
        let alias = root.path().join("config");
        std::fs::create_dir(&config).unwrap();
        if !alias.exists() {
            return;
        }
        assert_eq!(
            canonical_missing(&config).unwrap(),
            canonical_missing(&alias).unwrap()
        );
        assert_eq!(
            canonical_missing(&config.join("missing")).unwrap(),
            canonical_missing(&alias.join("missing")).unwrap()
        );
    }

    #[test]
    fn claude_accounts_bounded_read_and_unsafe_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = File::open(root.path()).unwrap();
        private_replace(&dir, "state", b"hello").unwrap();
        assert!(private_read(&dir, "state", 5).unwrap().unwrap() == b"hello");
        assert!(private_read(&dir, "state", 4).is_err());
        symlink("state", root.path().join("link")).unwrap();
        assert!(private_read(&dir, "link", 10).is_err());
        assert!(private_replace(&dir, "link", b"other").is_err());
        std::fs::hard_link(root.path().join("state"), root.path().join("hard")).unwrap();
        assert!(private_read(&dir, "state", 10).is_err());
        assert!(private_replace(&dir, "state", b"other").is_err());
        assert!(private_read(&dir, "../state", 10).is_err());
    }

    #[test]
    fn claude_accounts_private_create_is_atomic_and_no_clobber() {
        let root = tempfile::tempdir().unwrap();
        let dir = File::open(root.path()).unwrap();
        let result = private_create_with(&dir, "record", |file| {
            file.write_all(b"partial")?;
            assert!(!root.path().join("record").exists());
            anyhow::bail!("simulated write failure")
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let result = private_create_with(&dir, "record", |file| {
            file.write_all(b"replacement")?;
            std::fs::write(root.path().join("record"), b"original")?;
            std::fs::set_permissions(
                root.path().join("record"),
                std::fs::Permissions::from_mode(0o600),
            )?;
            Ok(())
        });
        assert!(result.is_err());
        assert!(private_read(&dir, "record", 20).unwrap().unwrap() == b"original");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        std::fs::remove_file(root.path().join("record")).unwrap();
        private_create(&dir, "record", b"original").unwrap();
        assert!(private_create(&dir, "record", b"replacement").is_err());
        assert!(private_read(&dir, "record", 20).unwrap().unwrap() == b"original");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn claude_accounts_lock_excludes_until_release() {
        let root = tempfile::tempdir().unwrap();
        let dir = File::open(root.path()).unwrap();
        let held = try_lock(&dir).unwrap();
        assert!(private_replace(&dir, ".duolane-lock", b"replacement").is_err());
        assert!(try_lock(&dir).is_err());
        drop(held);
        try_lock(&dir).unwrap();
    }

    #[test]
    fn claude_accounts_canonical_missing_suffix_checks_aliases() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        symlink("real", root.path().join("alias")).unwrap();
        assert_eq!(
            canonical_missing(&root.path().join("alias/missing/deeper")).unwrap(),
            real.canonicalize().unwrap().join("missing/deeper")
        );
        symlink("absent", root.path().join("dangling")).unwrap();
        assert!(canonical_missing(&root.path().join("dangling/missing")).is_err());
        assert!(canonical_missing(&root.path().join("missing/../other")).is_err());
    }

    #[test]
    fn claude_accounts_dangling_alias_does_not_create_target() {
        let root = tempfile::tempdir().unwrap();
        symlink("missing", root.path().join("alias")).unwrap();
        assert!(checked_dir(&root.path().join("alias/state"), true).is_err());
        assert!(!root.path().join("missing").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn claude_accounts_acl_rejects_allow_and_accepts_deny() {
        use std::process::Command;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state");
        std::fs::create_dir(&path).unwrap();
        let chmod = |spec: &str, path: &Path| {
            assert!(
                Command::new("/bin/chmod")
                    .env_clear()
                    .args(["+a", spec])
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        chmod("everyone deny delete", &path);
        assert!(checked_dir(&path, false).unwrap().is_some());
        chmod("everyone allow write", &path);
        assert!(checked_dir(&path, false).is_err());
        let file = root.path().join("file");
        std::fs::write(&file, b"data").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        chmod("everyone allow write", &file);
        let dir = File::open(root.path()).unwrap();
        assert!(private_read(&dir, "file", 10).is_err());
        let inheriting = root.path().join("inheriting");
        std::fs::create_dir(&inheriting).unwrap();
        chmod("everyone allow write,file_inherit", &inheriting);
        let dir = File::open(&inheriting).unwrap();
        assert!(private_replace(&dir, "state", b"data").is_err());
        assert!(!inheriting.join("state").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn claude_accounts_restrictive_umask_cleans_new_directory() {
        use std::process::Command;
        let status = Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .args([
                "--exact",
                "--ignored",
                "claude_accounts::store::tests::claude_accounts_umask_probe",
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn claude_accounts_umask_probe() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("new");
        let dir = File::open(root.path()).unwrap();
        let previous = rustix::process::umask(Mode::from_raw_mode(0o777));
        let result = checked_dir(&path, true);
        let child_result = checked_child(&dir, "child", true);
        rustix::process::umask(previous);
        assert!(result.is_err());
        assert!(!path.exists());
        assert!(child_result.is_err());
        assert!(!root.path().join("child").exists());
        let previous = rustix::process::umask(Mode::from_raw_mode(0o200));
        let restricted = checked_child(&dir, "restricted", true);
        rustix::process::umask(previous);
        let restricted = restricted.unwrap().unwrap();
        assert_eq!(
            restricted.metadata().unwrap().permissions().mode() & 0o777,
            0o500
        );
        assert!(checked_child(&dir, "child", true).unwrap().is_some());
        std::fs::set_permissions(
            root.path().join("child"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        assert!(checked_child(&dir, "child", true).is_err());
        assert!(root.path().join("child").exists());
        std::fs::set_permissions(
            root.path().join("child"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }

    #[test]
    fn claude_accounts_checked_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("new");
        assert!(checked_dir(&path, false).unwrap().is_none());
        assert!(checked_dir(&path, true).unwrap().is_some());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(checked_dir(&path, false).is_err());
        symlink(&path, root.path().join("link")).unwrap();
        assert!(checked_dir(&root.path().join("link"), false).is_err());
    }
}
