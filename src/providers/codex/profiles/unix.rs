use super::{MAX_CONFIG_BYTES, parse_profiles};
use crate::providers::codex::auth::token_store::{access_token, codex_auth_file};
use anyhow::{Context, Result, ensure};
use rustix::fs::{self, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

const AUTH_LIMIT: u64 = 1024 * 1024;
const SELECTOR_LIMIT: u64 = 256;
const SELECTOR: &str = "codex-profile";

pub enum AuthSource {
    Default {
        path: PathBuf,
    },
    Named {
        home: File,
        auth_name: &'static str,
        path: PathBuf,
    },
}

impl AuthSource {
    pub fn path(&self) -> &Path {
        match self {
            Self::Default { path } | Self::Named { path, .. } => path,
        }
    }
    pub(crate) fn load_auth(
        &self,
    ) -> Result<Option<crate::providers::codex::auth::token_store::StoredAuth>> {
        use crate::providers::codex::auth::token_store::{
            CodexCliAuthStore, CodexTokenStore, parse_auth,
        };
        match self {
            Self::Default { path } => {
                CodexTokenStore::new(CodexCliAuthStore::new(path.clone())).load_auth()
            }
            Self::Named {
                home,
                auth_name,
                path,
            } => match named_auth(home, auth_name)? {
                Some(data) => parse_auth(&data, path),
                None => Ok(None),
            },
        }
    }
    pub fn login_exists(&self) -> Result<bool> {
        match self {
            Self::Default { path } => default_login_exists(path),
            Self::Named {
                home, auth_name, ..
            } => match named_auth(home, auth_name)? {
                Some(data) => Ok(access_token(&serde_json::from_slice(&data)?).is_some()),
                None => Ok(false),
            },
        }
    }
}

fn checked(fd: &File, kind: FileType, forbidden: Mode) -> Result<()> {
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

fn bounded(mut file: File, limit: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "file exceeds size limit");
    Ok(data)
}

fn check_ancestor(fd: &File) -> Result<()> {
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

fn check_symlink_owner(stat: &fs::Stat) -> Result<()> {
    ensure!(
        stat.st_uid == 0 || stat.st_uid == rustix::process::getuid().as_raw(),
        "symlink has wrong owner"
    );
    Ok(())
}

fn path_components(path: &Path) -> VecDeque<std::ffi::OsString> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name.to_os_string()),
            std::path::Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

fn open_checked_directory(
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

fn profiles() -> Result<BTreeMap<String, PathBuf>> {
    let Some((dir, _)) = open_checked_directory(&crate::paths::config_dir(), false, false)? else {
        return Ok(BTreeMap::new());
    };
    let fd = match fs::openat(
        &dir,
        "codex-profiles.json",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    let file = File::from(fd);
    checked(&file, FileType::RegularFile, Mode::WGRP | Mode::WOTH)?;
    parse_profiles(&bounded(file, MAX_CONFIG_BYTES as u64)?)
}

fn default_login_exists(path: &Path) -> Result<bool> {
    let fd = match fs::open(
        path,
        OFlags::RDONLY | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let file = File::from(fd);
    ensure!(
        FileType::from_raw_mode(fs::fstat(&file)?.st_mode) == FileType::RegularFile,
        "unsafe file type"
    );
    let data = bounded(file, AUTH_LIMIT)?;
    Ok(access_token(&serde_json::from_slice(&data)?).is_some())
}

fn named_auth(home: &File, name: &str) -> Result<Option<Vec<u8>>> {
    let fd = match fs::openat(
        home,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let file = File::from(fd);
    checked(&file, FileType::RegularFile, Mode::RWXG | Mode::RWXO)?;
    Ok(Some(bounded(file, AUTH_LIMIT)?))
}

fn source(name: &str, profiles: &BTreeMap<String, PathBuf>) -> Result<AuthSource> {
    if name == "default" {
        return Ok(AuthSource::Default {
            path: codex_auth_file(),
        });
    }
    let path = profiles.get(name).context("unknown Codex profile")?;
    let (home, resolved) =
        open_checked_directory(path, false, true)?.context("Codex home does not exist")?;
    named_auth(&home, "auth.json")?;
    Ok(AuthSource::Named {
        home,
        auth_name: "auth.json",
        path: resolved.join("auth.json"),
    })
}

fn state_dir(create: bool) -> Result<Option<File>> {
    Ok(open_checked_directory(&crate::paths::state_dir(), create, true)?.map(|(dir, _)| dir))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    name: String,
    fingerprint: String,
}

fn default_path_components(path: &Path) -> VecDeque<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt;
    let mut components = path_components(path);
    let bytes = path.as_os_str().as_bytes();
    if bytes.ends_with(b"/") || bytes.ends_with(b"/.") {
        components.push_back(".".into());
    }
    components
}

fn resolve_default_path(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut queue = default_path_components(&absolute);
    use std::os::unix::ffi::OsStrExt;
    if path.as_os_str().as_bytes().ends_with(b"/.") {
        queue.push_back(".".into());
    }
    let mut resolved = PathBuf::from("/");
    let mut missing = false;
    let mut symlinks = 0;
    while let Some(name) = queue.pop_front() {
        if missing {
            resolved.push(name);
            continue;
        }
        if name == "." {
            continue;
        }
        if name == ".." {
            resolved.pop();
            continue;
        }
        let next = resolved.join(&name);
        match std::fs::symlink_metadata(&next) {
            Ok(stat) if stat.file_type().is_symlink() => {
                symlinks += 1;
                ensure!(symlinks <= 40, "too many default auth symlinks");
                let target = std::fs::read_link(&next)?;
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                for component in default_path_components(&target).into_iter().rev() {
                    queue.push_front(component);
                }
            }
            Ok(stat) => {
                ensure!(
                    queue.is_empty() || stat.is_dir(),
                    "default auth path component is not a directory"
                );
                resolved.push(name);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                missing = true;
                resolved.push(name);
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(resolved)
}

fn resolved_auth_path(auth: &AuthSource) -> Result<PathBuf> {
    match auth {
        AuthSource::Default { path } => resolve_default_path(path),
        AuthSource::Named { path, .. } => Ok(path.clone()),
    }
}

fn fingerprint(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let hash = ring::digest::digest(&ring::digest::SHA256, path.as_os_str().as_bytes());
    hash.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn verify_source(auth: AuthSource, expected: &str) -> Result<AuthSource> {
    let resolved = resolved_auth_path(&auth)?;
    ensure!(expected == fingerprint(&resolved), "profile path changed");
    Ok(match auth {
        AuthSource::Default { .. } => AuthSource::Default { path: resolved },
        named => named,
    })
}

fn selection(
    dir: Option<&File>,
    snapshot: Option<&BTreeMap<String, PathBuf>>,
) -> Result<(String, AuthSource)> {
    let Some(dir) = dir else {
        return Ok(("default".into(), source("default", &BTreeMap::new())?));
    };
    let fd = match fs::openat(
        dir,
        SELECTOR,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => {
            return Ok(("default".into(), source("default", &BTreeMap::new())?));
        }
        Err(e) => return Err(e.into()),
    };
    let file = File::from(fd);
    checked(&file, FileType::RegularFile, Mode::RWXG | Mode::RWXO)?;
    let selected: Selection = serde_json::from_slice(&bounded(file, SELECTOR_LIMIT)?)?;
    let auth = if selected.name == "default" {
        source("default", &BTreeMap::new())?
    } else if let Some(profiles) = snapshot {
        source(&selected.name, profiles)?
    } else {
        source(&selected.name, &profiles()?)?
    };
    let auth = verify_source(auth, &selected.fingerprint)?;
    Ok((selected.name, auth))
}

pub fn resolve_selected() -> Result<(String, AuthSource)> {
    (|| {
        let dir = state_dir(false)?;
        selection(dir.as_ref(), None)
    })()
    .context("Codex profile selection invalid; run `duolane codex auth use <name>`")
}

pub fn select(name: &str) -> Result<()> {
    let profiles = if name == "default" {
        BTreeMap::new()
    } else {
        profiles()?
    };
    let auth = source(name, &profiles)?;
    let data = serde_json::to_vec(&Selection {
        name: name.into(),
        fingerprint: fingerprint(&resolved_auth_path(&auth)?),
    })?;
    ensure!(
        data.len() <= SELECTOR_LIMIT as usize,
        "selection exceeds size limit"
    );
    let dir = state_dir(true)
        .context("unsafe state directory; run `duolane codex auth use <name>` after fixing it")?
        .context("state directory missing")?;
    let temp = format!(".codex-profile-{}", uuid::Uuid::new_v4());
    let fd = fs::openat(
        &dir,
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
        file.write_all(&data)?;
        file.sync_all()?;
        fs::renameat(&dir, temp.as_str(), &dir, SELECTOR)?;
        fs::fsync(&dir)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::unlinkat(&dir, temp.as_str(), fs::AtFlags::empty());
    }
    result
}

pub fn list() -> Result<Vec<(String, bool, bool)>> {
    let dir = state_dir(false)
        .context("Codex profile selection invalid; run `duolane codex auth use <name>`")?;
    let profiles = profiles()?;
    let (selected, selected_source) = selection(dir.as_ref(), Some(&profiles))
        .context("Codex profile selection invalid; run `duolane codex auth use <name>`")?;
    let mut result = Vec::new();
    for name in std::iter::once("default").chain(profiles.keys().map(String::as_str)) {
        let login = if name == selected {
            selected_source.login_exists()?
        } else {
            source(name, &profiles)?.login_exists()?
        };
        result.push((name.to_string(), login, name == selected));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn named_source_remains_anchored_when_home_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let home = root.join("home");
        std::fs::create_dir(&home).unwrap();
        let file = home.join("auth.json");
        std::fs::write(&file, b"original").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let profiles = BTreeMap::from([("work".to_string(), home.clone())]);
        let AuthSource::Named {
            home: handle,
            auth_name,
            ..
        } = source("work", &profiles).unwrap()
        else {
            panic!()
        };
        std::fs::rename(&home, root.join("old")).unwrap();
        std::fs::create_dir(root.join("replacement")).unwrap();
        symlink(root.join("replacement"), home).unwrap();
        let fd = fs::openat(
            &handle,
            auth_name,
            OFlags::RDONLY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        let data = bounded(File::from(fd), AUTH_LIMIT).unwrap();
        assert!(data == b"original");
    }
    #[test]
    fn verified_default_source_keeps_symlink_target() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let link = temp.path().join("auth.json");
        std::fs::write(&first, br#"{"tokens":{"access_token":"toy"}}"#).unwrap();
        std::fs::write(&second, b"{}").unwrap();
        symlink(&first, &link).unwrap();
        let original = AuthSource::Default { path: link.clone() };
        let expected = fingerprint(&resolved_auth_path(&original).unwrap());
        let verified = verify_source(original, &expected).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();
        assert!(verified.login_exists().unwrap());
    }
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
    #[test]
    fn selected_profile_uses_supplied_config_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let home = root.join("home");
        std::fs::create_dir(&home).unwrap();
        let profiles = BTreeMap::from([("work".to_string(), home.clone())]);
        let state = File::open(&root).unwrap();
        let selection_data = Selection {
            name: "work".into(),
            fingerprint: fingerprint(&home.join("auth.json")),
        };
        let selector = root.join(SELECTOR);
        std::fs::write(&selector, serde_json::to_vec(&selection_data).unwrap()).unwrap();
        std::fs::set_permissions(&selector, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (name, _) = selection(Some(&state), Some(&profiles)).unwrap();
        assert_eq!(name, "work");
    }
}
