use anyhow::{Result, ensure};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::{Component, Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle, MoveFileExW, OPEN_ALWAYS, OPEN_EXISTING, ReplaceFileW,
};

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn open(
    path: &Path,
    access: u32,
    share: u32,
    disposition: u32,
    flags: u32,
    private: bool,
) -> Result<File> {
    open_with_sddl(
        path,
        access,
        share,
        disposition,
        flags,
        private,
        "D:P(A;;FA;;;OW)(A;;FA;;;SY)",
    )
}

fn open_with_sddl(
    path: &Path,
    access: u32,
    share: u32,
    disposition: u32,
    flags: u32,
    private: bool,
    sddl: &str,
) -> Result<File> {
    let mut descriptor = null_mut();
    let sddl = wide(OsStr::new(sddl));
    if private
        && unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateFileW(
            wide(path.as_os_str()).as_ptr(),
            access,
            share,
            if private { &attrs } else { null() },
            disposition,
            flags | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    let error = if handle == INVALID_HANDLE_VALUE {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    if !descriptor.is_null() {
        unsafe {
            LocalFree(descriptor);
        }
    }
    if let Some(error) = error {
        return Err(error.into());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}
fn check(file: &File, directory: bool) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
            && (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == directory,
        "unsafe config path"
    );
    if !directory {
        ensure!(info.nNumberOfLinks == 1, "unsafe config file links");
    }
    Ok(())
}

pub(super) struct ConfigDir {
    path: PathBuf,
    _parents: Vec<File>,
}
impl ConfigDir {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let path = std::path::absolute(path)?;
        let mut current = PathBuf::new();
        let mut parents = Vec::new();
        for component in path.components() {
            ensure!(
                !matches!(component, Component::ParentDir),
                "invalid config directory"
            );
            current.push(component.as_os_str());
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            if !current.exists() {
                std::fs::create_dir(&current)?;
            }
            let dir = open(
                &current,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                false,
            )?;
            check(&dir, true)?;
            parents.push(dir);
        }
        Ok(Self {
            path,
            _parents: parents,
        })
    }
    pub(super) fn lock(&self) -> Result<File> {
        let file = open(
            &self.path.join(".codex-auth-order.lock"),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            true,
        )?;
        check(&file, false)?;
        file.try_lock()
            .map_err(|_| anyhow::anyhow!("config lock is busy"))?;
        Ok(file)
    }
    pub(super) fn read(&self) -> Result<Option<Vec<u8>>> {
        let file = match open(
            &self.path.join("config.json"),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            false,
        ) {
            Ok(file) => file,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        check(&file, false)?;
        let mut bytes = Vec::new();
        file.take(1_048_577).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1_048_576, "config file exceeds size limit");
        Ok(Some(bytes))
    }
    pub(super) fn replace(&self, original: Option<&[u8]>, bytes: &[u8]) -> Result<()> {
        let temp = self.path.join(format!(".config-{}", uuid::Uuid::new_v4()));
        let mut file = open(
            &temp,
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            true,
        )?;
        let written = (|| -> Result<()> {
            check(&file, false)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        let result = written.and_then(|()| {
            ensure!(
                self.read()?.as_deref() == original,
                "config.json changed while editing"
            );
            let target_path = self.path.join("config.json");
            let backup_path = self
                .path
                .join(format!(".config-backup-{}", uuid::Uuid::new_v4()));
            let target = wide(target_path.as_os_str());
            let source = wide(temp.as_os_str());
            if original.is_some() {
                let backup = wide(backup_path.as_os_str());
                if unsafe {
                    ReplaceFileW(
                        target.as_ptr(),
                        source.as_ptr(),
                        backup.as_ptr(),
                        0,
                        null(),
                        null(),
                    )
                } == 0
                {
                    let error = std::io::Error::last_os_error();
                    let recovery = recover_backup(&target_path, &backup_path);
                    anyhow::bail!("cannot replace config.json: {error}; {recovery}");
                }
                std::fs::remove_file(&backup_path).map_err(|error| {
                    anyhow::anyhow!(
                        "config.json was replaced but backup cleanup failed at {}: {error}",
                        backup_path.display()
                    )
                })?;
            } else if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        });
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

fn recover_backup(target: &Path, backup: &Path) -> String {
    let file = match open(
        backup,
        GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        false,
    ) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return "no backup was created".into();
        }
        Err(_) => return format!("backup retained at {}", backup.display()),
    };
    if check(&file, false).is_err() {
        return format!("backup retained at {}", backup.display());
    }
    drop(file);
    if unsafe {
        MoveFileExW(
            wide(backup.as_os_str()).as_ptr(),
            wide(target.as_os_str()).as_ptr(),
            0,
        )
    } != 0
    {
        "original restored".into()
    } else {
        format!("backup retained at {}", backup.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dacl(file: &File) -> (u16, Vec<u16>) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl,
        };
        let mut descriptor = null_mut();
        let mut dacl = null_mut();
        assert_eq!(
            unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    &mut dacl,
                    null_mut(),
                    &mut descriptor,
                )
            },
            0
        );
        let mut control = 0;
        let mut revision = 0;
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
            0
        );
        let mut text = null_mut();
        let mut length = 0;
        assert_ne!(
            unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION,
                    &mut text,
                    &mut length,
                )
            },
            0
        );
        let result = unsafe { std::slice::from_raw_parts(text, length as usize) }.to_vec();
        unsafe {
            LocalFree(text.cast());
            LocalFree(descriptor);
        }
        (control, result)
    }

    #[test]
    fn codex_auth_order_windows_preserves_restrictive_dacl() {
        use windows_sys::Win32::Security::SE_DACL_PROTECTED;
        let root = tempfile::tempdir().unwrap();
        let dir = ConfigDir::open(root.path()).unwrap();
        let path = root.path().join("config.json");
        let mut file = open_with_sddl(
            &path,
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            true,
            "D:P(D;;GR;;;AN)(A;;FA;;;OW)(A;;FA;;;SY)",
        )
        .unwrap();
        file.write_all(b"{}").unwrap();
        file.sync_all().unwrap();
        let before = dacl(&file);
        let stage_path = root.path().join("stage");
        let stage = open(
            &stage_path,
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            true,
        )
        .unwrap();
        assert_ne!(before.1, dacl(&stage).1);
        drop(stage);
        std::fs::remove_file(stage_path).unwrap();
        assert_ne!(before.0 & SE_DACL_PROTECTED, 0);
        drop(file);
        dir.replace(Some(b"{}"), b"{\"saved\":true}").unwrap();
        let file = open(
            &path,
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            false,
        )
        .unwrap();
        let after = dacl(&file);
        assert_ne!(after.0 & SE_DACL_PROTECTED, 0);
        assert_eq!(before.1, after.1);
    }

    #[test]
    fn codex_auth_order_windows_backup_recovery_never_overwrites() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("config.json");
        let backup = root.path().join("backup");
        std::fs::write(&backup, b"original").unwrap();
        assert_eq!(recover_backup(&target, &backup), "original restored");
        assert!(std::fs::read(&target).unwrap() == b"original");
        std::fs::write(&backup, b"original").unwrap();
        std::fs::write(&target, b"concurrent").unwrap();
        assert!(recover_backup(&target, &backup).contains("backup retained"));
        assert!(std::fs::read(&target).unwrap() == b"concurrent");
        assert!(std::fs::read(&backup).unwrap() == b"original");
    }

    #[test]
    fn codex_auth_order_windows_private_temp_and_stale_edit() {
        let root = tempfile::tempdir().unwrap();
        let dir = ConfigDir::open(root.path()).unwrap();
        let _lock = dir.lock().unwrap();
        dir.replace(None, b"{}").unwrap();
        let original = dir.read().unwrap();
        std::fs::write(root.path().join("config.json"), b"{\"edited\":true}").unwrap();
        assert!(dir.replace(original.as_deref(), b"{}").is_err());
        assert!(std::fs::read(root.path().join("config.json")).unwrap() == b"{\"edited\":true}");
    }
}
