use super::token_store::{StoredAuth, parse_auth, update_document};
use crate::auth::AuthStorage;
use rustix::fs::{self, FileType, Mode, OFlags};
use std::fs::{File, TryLockError};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
const LIMIT: u64 = 1024 * 1024;

pub struct NamedHomeAuthStore {
    home: Arc<File>,
    path: PathBuf,
}

impl NamedHomeAuthStore {
    pub fn new(home: Arc<File>, path: PathBuf) -> Self {
        Self { home, path }
    }

    fn read(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let fd = match fs::openat(
            &*self.home,
            "auth.json",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let file = File::from(fd);
        checked_file(&file)?;
        let mut bytes = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() as u64 <= LIMIT, "auth file exceeds size limit");
        Ok(Some(bytes))
    }
}

fn checked_file(file: &File) -> anyhow::Result<()> {
    let stat = fs::fstat(file)?;
    anyhow::ensure!(
        FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile,
        "unsafe file type"
    );
    anyhow::ensure!(
        stat.st_uid == rustix::process::getuid().as_raw(),
        "file has wrong owner"
    );
    anyhow::ensure!(
        !Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO),
        "unsafe file permissions"
    );
    Ok(())
}

impl AuthStorage<StoredAuth> for NamedHomeAuthStore {
    fn load(&self) -> anyhow::Result<Option<StoredAuth>> {
        match self.read()? {
            Some(bytes) => parse_auth(&bytes, &self.path),
            None => Ok(None),
        }
    }

    fn save(&self, value: StoredAuth) -> anyhow::Result<()> {
        let mut doc = match self.read()? {
            Some(bytes) => serde_json::from_slice(&bytes)?,
            None => serde_json::json!({}),
        };
        update_document(&mut doc, value);
        let bytes = serde_json::to_vec_pretty(&doc)?;
        let name = format!(
            ".auth.json.duolane-tmp.{}.{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let fd = fs::openat(
            &*self.home,
            name.as_str(),
            OFlags::WRONLY
                | OFlags::CREATE
                | OFlags::EXCL
                | OFlags::NOFOLLOW
                | OFlags::NOCTTY
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let result = (|| -> anyhow::Result<()> {
            let mut file = File::from(fd);
            fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::renameat(&*self.home, name.as_str(), &*self.home, "auth.json")?;
            self.home.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::unlinkat(&*self.home, name.as_str(), fs::AtFlags::empty());
        }
        result
    }

    fn clear(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
    fn lock_for_refresh(&self) -> anyhow::Result<Option<File>> {
        let fd = fs::openat(
            &*self.home,
            "auth.json.duolane.lock",
            OFlags::RDWR
                | OFlags::CREATE
                | OFlags::NOFOLLOW
                | OFlags::NOCTTY
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let file = File::from(fd);
        checked_file(&file)?;
        let timeout = Duration::from_secs(10);
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some(file)),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(TryLockError::WouldBlock) => {
                    anyhow::bail!("timed out after {:?} waiting for refresh lock", timeout)
                }
                Err(TryLockError::Error(err)) => return Err(err.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthStorage;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    #[test]
    fn writes_and_locks_remain_relative_to_open_home() {
        let root = tempfile::TempDir::new().unwrap();
        let home = root.path().join("home");
        let moved = root.path().join("moved");
        std::fs::create_dir(&home).unwrap();
        let store =
            NamedHomeAuthStore::new(Arc::new(File::open(&home).unwrap()), home.join("auth.json"));
        std::fs::rename(&home, &moved).unwrap();
        std::fs::create_dir(&home).unwrap();
        store
            .save(StoredAuth {
                access: "toy".into(),
                refresh: "toy".into(),
                expires: 0,
                account_id: None,
            })
            .unwrap();
        let _lock = store.lock_for_refresh().unwrap().unwrap();
        assert!(moved.join("auth.json").is_file());
        assert!(moved.join("auth.json.duolane.lock").is_file());
        assert!(!home.join("auth.json").exists());
        assert!(!home.join("auth.json.duolane.lock").exists());
    }

    #[test]
    fn descriptor_store_checks_reads_and_persists_privately() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        let store = NamedHomeAuthStore::new(
            std::sync::Arc::new(std::fs::File::open(dir.path()).unwrap()),
            path.clone(),
        );
        std::fs::write(
            &path,
            br#"{"extra":true,"tokens":{"access_token":"toy","refresh_token":"old","other":7}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.load().unwrap().is_some());
        let original_inode = std::fs::metadata(&path).unwrap().ino();
        store
            .save(StoredAuth {
                access: "new".into(),
                refresh: "rotated".into(),
                expires: 0,
                account_id: None,
            })
            .unwrap();
        assert_ne!(original_inode, std::fs::metadata(&path).unwrap().ino());
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc["extra"], true);
        assert_eq!(doc["tokens"]["other"], 7);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _lock = store.lock_for_refresh().unwrap().unwrap();
        assert!(dir.path().join("auth.json.duolane.lock").is_file());
        std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(store.load().is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(store.load().is_err());
        std::fs::remove_dir(&path).unwrap();
        symlink("missing", &path).unwrap();
        assert!(store.load().is_err());
    }
}
