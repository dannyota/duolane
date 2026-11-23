use anyhow::{Result, ensure};
use serde_json::{Map, Value};
use std::ffi::OsStr;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    ChatGpt,
    ApiKey,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderOrigin {
    Default,
    File,
    Environment,
}
#[derive(Debug)]
pub struct AuthOrder {
    sources: Vec<AuthSource>,
    origin: OrderOrigin,
}
impl AuthOrder {
    pub fn sources(&self) -> &[AuthSource] {
        &self.sources
    }
    pub fn origin(&self) -> OrderOrigin {
        self.origin
    }
}
fn parse<'a>(values: impl IntoIterator<Item = &'a str>, name: &str) -> Result<Vec<AuthSource>> {
    let mut sources = Vec::new();
    for value in values {
        let item = match value {
            "chatgpt" => AuthSource::ChatGpt,
            "api-key" => AuthSource::ApiKey,
            _ => anyhow::bail!("invalid {name}"),
        };
        ensure!(!sources.contains(&item), "invalid {name}");
        sources.push(item);
    }
    ensure!(!sources.is_empty(), "invalid {name}");
    Ok(sources)
}
pub(crate) fn read_config_snapshot(dir: &Path) -> Result<Value> {
    match std::fs::read(dir.join("config.json")) {
        Ok(bytes) => {
            Ok(serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid config.json"))?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(e.into()),
    }
}
pub fn load_order(config_dir: &Path, env: Option<&OsStr>) -> Result<AuthOrder> {
    let snapshot = if env.is_none() {
        Some(read_config_snapshot(config_dir)?)
    } else {
        None
    };
    order_from_snapshot(snapshot.as_ref(), env)
}

pub(crate) fn order_from_snapshot(
    config: Option<&Value>,
    env: Option<&OsStr>,
) -> Result<AuthOrder> {
    if let Some(value) = env {
        let value = value
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid DUOLANE_CODEX_AUTH_ORDER"))?;
        return Ok(AuthOrder {
            sources: parse(value.split(','), "DUOLANE_CODEX_AUTH_ORDER")?,
            origin: OrderOrigin::Environment,
        });
    }
    let Some(codex) = config
        .and_then(|value| value.get("codex"))
        .filter(|value| !value.is_null())
    else {
        return Ok(AuthOrder {
            sources: vec![AuthSource::ChatGpt],
            origin: OrderOrigin::Default,
        });
    };
    let codex = codex
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("invalid codex.authOrder"))?;
    let Some(order) = codex.get("authOrder") else {
        return Ok(AuthOrder {
            sources: vec![AuthSource::ChatGpt],
            origin: OrderOrigin::Default,
        });
    };
    let array = order
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("invalid codex.authOrder"))?;
    let values = array
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| anyhow::anyhow!("invalid codex.authOrder"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(AuthOrder {
        sources: parse(values, "codex.authOrder")?,
        origin: OrderOrigin::File,
    })
}

pub fn save_order(config_dir: &Path, order: Option<&[AuthSource]>) -> Result<()> {
    if let Some(order) = order {
        ensure!(
            !order.is_empty() && !(order.len() == 2 && order[0] == order[1]) && order.len() <= 2,
            "invalid codex.authOrder"
        );
    }
    let dir = platform::ConfigDir::open(config_dir)?;
    let _lock = dir.lock()?;
    let original = dir.read()?;
    let mut config = match &original {
        Some(raw) => serde_json::from_slice::<Value>(raw)
            .map_err(|_| anyhow::anyhow!("invalid config.json"))?,
        None => Value::Object(Map::new()),
    };
    let root = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("invalid config.json"))?;
    if root.get("codex").is_some_and(Value::is_null) && order.is_some() {
        root.insert("codex".into(), Value::Object(Map::new()));
    }
    if order.is_some() || root.get("codex").is_some_and(|value| !value.is_null()) {
        let codex = root
            .entry("codex")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid codex.authOrder"))?;
        match order {
            Some(order) => {
                codex.insert(
                    "authOrder".into(),
                    Value::Array(
                        order
                            .iter()
                            .map(|s| {
                                Value::String(
                                    match s {
                                        AuthSource::ChatGpt => "chatgpt",
                                        AuthSource::ApiKey => "api-key",
                                    }
                                    .into(),
                                )
                            })
                            .collect(),
                    ),
                );
            }
            None => {
                codex.remove("authOrder");
            }
        }
    }
    let bytes = serde_json::to_vec_pretty(&config)?;
    dir.replace(original.as_deref(), &bytes)
}

fn resolve_openai_base_url(env: Option<&OsStr>, file: Option<&Value>) -> Result<url::Url> {
    let (name, value) = if let Some(env) = env {
        (
            "DUOLANE_OPENAI_BASE_URL",
            env.to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid DUOLANE_OPENAI_BASE_URL"))?,
        )
    } else if let Some(file) = file {
        (
            "codex.apiBaseUrl",
            file.as_str()
                .ok_or_else(|| anyhow::anyhow!("invalid codex.apiBaseUrl"))?,
        )
    } else {
        ("codex.apiBaseUrl", "https://api.openai.com/v1")
    };
    if let Some(problem) = crate::upstream_url::upstream_url_problem(value) {
        anyhow::bail!("{name} {problem}");
    }
    url::Url::parse(value).map_err(|_| anyhow::anyhow!("invalid {name}"))
}

fn openai_base_url_for(config_dir: &Path, env: Option<&OsStr>) -> Result<url::Url> {
    let snapshot = if env.is_none() {
        Some(read_config_snapshot(config_dir)?)
    } else {
        None
    };
    openai_base_url_from_snapshot(snapshot.as_ref(), env)
}

pub(crate) fn openai_base_url_from_snapshot(
    config: Option<&Value>,
    env: Option<&OsStr>,
) -> Result<url::Url> {
    resolve_openai_base_url(
        env,
        config
            .and_then(|v| v.get("codex"))
            .and_then(|v| v.get("apiBaseUrl")),
    )
}

fn openai_base_url_with(
    env: Option<&OsStr>,
    config_dir: impl FnOnce() -> std::path::PathBuf,
) -> Result<url::Url> {
    if let Some(env) = env {
        return resolve_openai_base_url(Some(env), None);
    }
    openai_base_url_for(&config_dir(), None)
}

pub fn openai_base_url() -> Result<url::Url> {
    openai_base_url_with(
        std::env::var_os("DUOLANE_OPENAI_BASE_URL").as_deref(),
        crate::paths::config_dir,
    )
}

#[cfg(windows)]
mod windows;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;
#[cfg(unix)]
mod unix {
    use crate::claude_accounts::store;
    use anyhow::{Context, Result, ensure};
    use rustix::fs::{self, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};

    pub(super) struct ConfigDir {
        dir: File,
        path: PathBuf,
    }
    impl ConfigDir {
        pub(super) fn open(path: &Path) -> Result<Self> {
            let dir = store::checked_dir(path, true)?.context("config directory missing")?;
            Ok(Self {
                dir,
                path: path.to_path_buf(),
            })
        }
        pub(super) fn lock(&self) -> Result<File> {
            let file = File::from(fs::openat(
                &self.dir,
                ".codex-auth-order.lock",
                OFlags::RDWR
                    | OFlags::CREATE
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::CLOEXEC
                    | OFlags::NOCTTY,
                Mode::RUSR | Mode::WUSR,
            )?);
            store::check_private(&file)?;
            file.try_lock()
                .map_err(|_| anyhow::anyhow!("config lock is busy"))?;
            Ok(file)
        }
        pub(super) fn read(&self) -> Result<Option<Vec<u8>>> {
            let file = match fs::openat(
                &self.dir,
                "config.json",
                OFlags::RDONLY
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::CLOEXEC
                    | OFlags::NOCTTY,
                Mode::empty(),
            ) {
                Ok(fd) => File::from(fd),
                Err(Errno::NOENT) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            let stat = fs::fstat(&file)?;
            ensure!(
                FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                    && stat.st_uid == rustix::process::getuid().as_raw()
                    && stat.st_nlink == 1
                    && stat.st_mode & 0o7022 == 0,
                "unsafe config file"
            );
            store::check_acl(&file)?;
            let mut bytes = Vec::new();
            file.take(1_048_577).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 1_048_576, "config file exceeds size limit");
            Ok(Some(bytes))
        }
        pub(super) fn replace(&self, original: Option<&[u8]>, bytes: &[u8]) -> Result<()> {
            let temp = format!(".config-{}", uuid::Uuid::new_v4());
            let mut file = File::from(fs::openat(
                &self.dir,
                temp.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?);
            let result = (|| -> Result<()> {
                fs::fchmod(&file, Mode::RUSR | Mode::WUSR)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                let current =
                    store::checked_dir(&self.path, false)?.context("config directory changed")?;
                let a = fs::fstat(&self.dir)?;
                let b = fs::fstat(&current)?;
                ensure!(
                    a.st_dev == b.st_dev && a.st_ino == b.st_ino,
                    "config directory changed"
                );
                ensure!(
                    self.read()?.as_deref() == original,
                    "config.json changed while editing"
                );
                if original.is_none() {
                    fs::renameat_with(
                        &self.dir,
                        temp.as_str(),
                        &self.dir,
                        "config.json",
                        fs::RenameFlags::NOREPLACE,
                    )?;
                } else {
                    fs::renameat(&self.dir, temp.as_str(), &self.dir, "config.json")?;
                }
                fs::fsync(&self.dir)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = fs::unlinkat(&self.dir, temp.as_str(), fs::AtFlags::empty());
            }
            result
        }
    }
}

#[cfg(test)]
mod codex_auth_task_tests {
    use super::{AuthSource, OrderOrigin, load_order, save_order};
    use std::ffi::OsStr;

    #[test]
    fn codex_auth_order_env_url_skips_config_dir() {
        let url =
            super::openai_base_url_with(Some(OsStr::new("https://example.invalid/v1")), || {
                panic!("config dir must not be resolved")
            })
            .unwrap();
        assert_eq!(url.host_str(), Some("example.invalid"));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let error = super::openai_base_url_with(Some(OsStr::from_bytes(b"\xff")), || {
                panic!("config dir must not be resolved")
            })
            .unwrap_err()
            .to_string();
            assert_eq!(error, "invalid DUOLANE_OPENAI_BASE_URL");
        }
    }

    #[test]
    fn codex_auth_order_base_url_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(super::openai_base_url_for(dir.path(), None).is_ok());
        std::fs::write(dir.path().join("config.json"), b"{").unwrap();
        assert!(super::openai_base_url_for(dir.path(), None).is_err());
        assert!(
            super::openai_base_url_for(dir.path(), Some(OsStr::new("https://example.invalid/v1")))
                .is_ok()
        );
        std::fs::remove_file(dir.path().join("config.json")).unwrap();
        std::fs::create_dir(dir.path().join("config.json")).unwrap();
        assert!(super::openai_base_url_for(dir.path(), None).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn codex_auth_order_refuses_symlinks_and_stale_original() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().canonicalize().unwrap();
        let target = path.join("target");
        let config = path.join("config.json");
        std::fs::write(&target, b"sentinel").unwrap();
        symlink(&target, &config).unwrap();
        assert!(save_order(&path, Some(&[AuthSource::ApiKey])).is_err());
        assert!(std::fs::read(&target).unwrap() == b"sentinel");
        std::fs::remove_file(&config).unwrap();
        std::fs::write(&config, b"{}").unwrap();
        let dir = super::unix::ConfigDir::open(&path).unwrap();
        let snapshot = dir.read().unwrap();
        std::fs::write(&config, b"{\"external\":1}").unwrap();
        assert!(dir.replace(snapshot.as_deref(), b"{}").is_err());
        assert!(std::fs::read(&config).unwrap() == b"{\"external\":1}");
    }

    #[test]
    fn codex_auth_order_null_codex_and_url_resolution() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{"codex":null}"#).unwrap();
        assert_eq!(
            load_order(dir.path(), None).unwrap().origin(),
            OrderOrigin::Default
        );
        save_order(dir.path(), Some(&[AuthSource::ApiKey])).unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"port":1234,"codex":{"apiBaseUrl":42}}"#,
        )
        .unwrap();
        assert_eq!(
            super::super::read_file_config(dir.path()).unwrap().port,
            Some(1234)
        );
        let invalid = serde_json::json!(42);
        assert!(super::resolve_openai_base_url(None, Some(&invalid)).is_err());
        let value = serde_json::json!("http://not-loopback.invalid/private");
        let err = super::resolve_openai_base_url(None, Some(&value))
            .unwrap_err()
            .to_string();
        assert!(err.contains("codex.apiBaseUrl") && !err.contains("not-loopback.invalid"));
        let url =
            super::resolve_openai_base_url(Some(OsStr::new("http://127.0.0.1/v1")), Some(&invalid))
                .unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert!(
            super::resolve_openai_base_url(
                None,
                Some(&serde_json::json!("https://example.invalid/v1"))
            )
            .is_ok()
        );
    }

    #[test]
    fn codex_auth_order_is_explicit_and_strict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let default = load_order(path, None).unwrap();
        assert_eq!(default.sources(), &[AuthSource::ChatGpt]);
        assert_eq!(default.origin(), OrderOrigin::Default);
        let env = load_order(path, Some(OsStr::new("api-key,chatgpt"))).unwrap();
        assert_eq!(env.sources(), &[AuthSource::ApiKey, AuthSource::ChatGpt]);
        assert_eq!(env.origin(), OrderOrigin::Environment);
        for invalid in ["", "chatgpt,chatgpt", "other"] {
            let error = load_order(path, Some(OsStr::new(invalid)))
                .unwrap_err()
                .to_string();
            assert!(error.contains("DUOLANE_CODEX_AUTH_ORDER"));
            assert!(!error.contains(invalid) || invalid.is_empty());
        }
        for value in [
            "null",
            "{}",
            "[]",
            "[\"chatgpt\",\"chatgpt\"]",
            "[\"other\"]",
        ] {
            std::fs::write(
                path.join("config.json"),
                format!("{{\"codex\":{{\"authOrder\":{value}}}}}"),
            )
            .unwrap();
            assert!(
                load_order(path, None)
                    .unwrap_err()
                    .to_string()
                    .contains("codex.authOrder")
            );
        }
        std::fs::write(
            path.join("config.json"),
            r#"{"other":1,"codex":{"extra":2}}"#,
        )
        .unwrap();
        save_order(path, Some(&[AuthSource::ApiKey])).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["other"], 1);
        assert_eq!(config["codex"]["extra"], 2);
        assert_eq!(load_order(path, None).unwrap().origin(), OrderOrigin::File);
        save_order(path, None).unwrap();
        assert_eq!(
            load_order(path, None).unwrap().origin(),
            OrderOrigin::Default
        );
        std::fs::write(path.join("config.json"), b"{").unwrap();
        assert!(save_order(path, Some(&[AuthSource::ChatGpt])).is_err());
        assert_eq!(std::fs::read(path.join("config.json")).unwrap(), b"{");
    }
}
