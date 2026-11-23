pub mod api_key;
pub mod constants;
pub mod jwt;
pub mod manager;
pub mod token_store;

#[cfg(test)]
mod api_key_task_tests {
    use super::api_key::{KeyOrigin, KeyStore, OpenAiKey};
    use std::ffi::OsStr;

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn codex_api_key_store_is_private() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let store = KeyStore::new(root.clone());
        let key = OpenAiKey::parse(b"  toy-key  \n").unwrap();
        store.save(&key).unwrap();
        let (loaded, origin) = store.load(None).unwrap();
        assert_eq!(origin, KeyOrigin::Stored);
        assert!(loaded.unwrap().for_sending() == "toy-key");
        let (loaded, origin) = store.load(Some(OsStr::new("env-toy"))).unwrap();
        assert_eq!(origin, KeyOrigin::Environment);
        assert!(loaded.unwrap().for_sending() == "env-toy");
        assert!(OpenAiKey::parse(&vec![b'a'; 513]).is_err());
        assert!(OpenAiKey::parse(b"a\nb").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let dir = root.join("credentials");
            let path = dir.join("openai-api-key");
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(store.load(None).is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
            assert!(store.load(None).unwrap().0.is_some());
            std::fs::remove_file(&path).unwrap();
            let sentinel = root.join("sentinel");
            std::fs::write(&sentinel, b"untouched").unwrap();
            symlink(&sentinel, &path).unwrap();
            assert!(store.save(&key).is_err());
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"untouched");
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, vec![b'a'; 4097]).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(store.load(None).is_err());
            assert!(store.remove().unwrap());
            std::fs::write(&path, b"toy").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(store.load(None).is_err());
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::remove_file(&path).unwrap();
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(store.load(None).is_err());
            std::fs::remove_file(&path).unwrap();
            let alias = root.join("alias");
            symlink(&root, &alias).unwrap();
            assert!(KeyStore::new(alias).load(None).is_err());
            assert!(!store.remove().unwrap());
        }
    }
    #[test]
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn codex_api_key_store_environment_only() {
        let temp = tempfile::tempdir().unwrap();
        let store = KeyStore::new(temp.path().to_path_buf());
        let (key, origin) = store.load(Some(OsStr::new("toy-key"))).unwrap();
        assert_eq!(origin, KeyOrigin::Environment);
        assert!(key.unwrap().for_sending() == "toy-key");
        assert_eq!(store.load(None).unwrap().1, KeyOrigin::None);
        assert!(store.save(&OpenAiKey::parse(b"toy-key").unwrap()).is_err());
        assert!(store.remove().is_err());
    }
}
