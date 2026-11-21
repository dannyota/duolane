use super::usage as usage_query;
use super::{MAX_CONFIG_BYTES, SELECTION_WARNING, UsageReport, parse_accounts};
use crate::secure_fs::{bounded, checked, fingerprint, open_checked_directory, write_atomic};
use anyhow::{Context, Result, ensure};
use rustix::fs::{self, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};

const SELECTOR_LIMIT: u64 = 256;
const SELECTOR: &str = "claude-account";

pub struct AccountSource {
    path: PathBuf,
    configured: PathBuf,
    dir: File,
}

impl AccountSource {
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn into_login_location(self) -> (PathBuf, File) {
        (self.configured, self.dir)
    }
}

fn accounts() -> Result<Vec<(String, PathBuf)>> {
    let Some((dir, _)) = open_checked_directory(&crate::paths::config_dir(), false, false)? else {
        return Ok(Vec::new());
    };
    let fd = match fs::openat(
        &dir,
        "claude-accounts.json",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let file = File::from(fd);
    checked(
        &file,
        FileType::RegularFile,
        Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
    )?;
    parse_accounts(&bounded(file, MAX_CONFIG_BYTES as u64)?)
}

fn open_source(name: &str, accounts: &[(String, PathBuf)]) -> Result<Option<AccountSource>> {
    let path = accounts
        .iter()
        .find(|(account, _)| account == name)
        .map(|(_, path)| path)
        .context("unknown Claude account")?;
    Ok(
        open_checked_directory(path, false, true)?.map(|(dir, resolved)| AccountSource {
            path: resolved,
            configured: path.clone(),
            dir,
        }),
    )
}

fn source(name: &str, accounts: &[(String, PathBuf)]) -> Result<AccountSource> {
    open_source(name, accounts)?.context("Claude account directory does not exist")
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

fn selected_name(dir: Option<&File>, snapshot: &[(String, PathBuf)]) -> Result<Option<String>> {
    let Some(dir) = dir else {
        return Ok(None);
    };
    let fd = match fs::openat(
        dir,
        SELECTOR,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let file = File::from(fd);
    checked(&file, FileType::RegularFile, Mode::RWXG | Mode::RWXO)?;
    let selected: Selection = serde_json::from_slice(&bounded(file, SELECTOR_LIMIT)?)?;
    let source = source(&selected.name, snapshot)?;
    ensure!(
        selected.fingerprint == fingerprint(source.path()),
        "Claude account path changed"
    );
    Ok(Some(selected.name))
}

pub fn select(name: &str) -> Result<()> {
    let accounts = accounts()?;
    let source = source(name, &accounts)?;
    let data = serde_json::to_vec(&Selection {
        name: name.into(),
        fingerprint: fingerprint(source.path()),
    })?;
    ensure!(
        data.len() <= SELECTOR_LIMIT as usize,
        "selection exceeds size limit"
    );
    let dir = state_dir(true)
        .context(
            "unsafe state directory; run `duolane claude-accounts use <name>` after fixing it",
        )?
        .context("state directory missing")?;
    write_atomic(&dir, SELECTOR, ".claude-account-", &data)
}

pub fn list() -> Result<Vec<(String, bool)>> {
    let dir = state_dir(false)
        .context("Claude account selection invalid; run `duolane claude-accounts use <name>`")?;
    let accounts = accounts()?;
    let selected = selected_name(dir.as_ref(), &accounts)
        .context("Claude account selection invalid; run `duolane claude-accounts use <name>`")?;
    Ok(accounts
        .into_iter()
        .map(|(name, _)| {
            let is_selected = selected.as_deref() == Some(name.as_str());
            (name, is_selected)
        })
        .collect())
}

pub async fn usage() -> Result<UsageReport> {
    usage_with(usage_query::Query::production()).await
}

async fn usage_with(query: usage_query::Query) -> Result<UsageReport> {
    let accounts = accounts()?;
    if accounts.is_empty() {
        return Ok(UsageReport {
            lines: Vec::new(),
            warning: None,
        });
    }
    let (selected, selection_invalid) =
        match state_dir(false).and_then(|dir| selected_name(dir.as_ref(), &accounts)) {
            Ok(selected) => (selected, false),
            Err(_) => (None, true),
        };
    let targets = accounts
        .iter()
        .map(|(name, _)| usage_query::Target {
            name: name.clone(),
            location: match open_source(name, &accounts) {
                Ok(Some(source)) => Ok(source.into_login_location()),
                Ok(None) => Err(usage_query::Reason::LoginNotFound),
                Err(_) => Err(usage_query::Reason::LoginUnreadable),
            },
        })
        .collect();
    let rows = query.collect(targets).await;
    Ok(UsageReport {
        lines: usage_query::render(&rows, selected.as_deref(), &jiff::Zoned::now()),
        warning: selection_invalid.then_some(SELECTION_WARNING),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingReader(AtomicUsize);

    impl usage_query::LoginReader for CountingReader {
        fn read(
            &self,
            _configured: &Path,
            _dir: &File,
        ) -> Result<usage_query::Login, usage_query::Reason> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(usage_query::Reason::LoginNotFound)
        }
    }

    fn offline_query(reader: Arc<CountingReader>) -> usage_query::Query {
        usage_query::Query {
            reader,
            client: usage_query::build_client(Vec::new()),
            endpoint: "http://127.0.0.1:9/".to_string(),
        }
    }

    fn write_accounts(names: &[(&str, PathBuf)]) {
        let config = crate::paths::config_dir();
        let entries: Vec<String> = names
            .iter()
            .map(|(name, path)| format!("{:?}:{:?}", name, path))
            .collect();
        let json = format!("{{{}}}", entries.join(","));
        std::fs::write(config.join("claude-accounts.json"), json).unwrap();
        std::fs::set_permissions(
            config.join("claude-accounts.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    #[test]
    fn select_then_list_reports_the_selection_in_file_order() {
        let config = crate::paths::config_dir();
        let work = config.join("work");
        let personal = config.join("personal");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&personal).unwrap();
        write_accounts(&[("work", work), ("personal", personal)]);

        select("work").unwrap();
        assert_eq!(
            list().unwrap(),
            vec![("work".to_string(), true), ("personal".to_string(), false)]
        );
    }

    #[test]
    fn accounts_file_readable_by_others_is_rejected() {
        let config = crate::paths::config_dir();
        let work = config.join("work");
        std::fs::create_dir_all(&work).unwrap();
        write_accounts(&[("work", work)]);
        let store = config.join("claude-accounts.json");

        for mode in [0o644, 0o640, 0o604, 0o620] {
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(list().is_err());
            assert!(select("work").is_err());
        }

        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(select("work").is_ok());
        assert_eq!(list().unwrap(), vec![("work".to_string(), true)]);
    }

    #[test]
    fn select_rejects_an_unknown_account() {
        let config = crate::paths::config_dir();
        let work = config.join("work");
        std::fs::create_dir_all(&work).unwrap();
        write_accounts(&[("work", work)]);

        assert!(select("missing").is_err());
    }

    #[test]
    fn select_rejects_a_symlinked_account_directory() {
        let config = crate::paths::config_dir();
        let real = config.join("real");
        let link = config.join("link");
        std::fs::create_dir_all(&real).unwrap();
        symlink(&real, &link).unwrap();
        write_accounts(&[("work", link)]);

        assert!(select("work").is_err());
    }

    #[test]
    fn list_rejects_a_selection_with_a_wrong_fingerprint() {
        let config = crate::paths::config_dir();
        let work = config.join("work");
        std::fs::create_dir_all(&work).unwrap();
        write_accounts(&[("work", work)]);
        select("work").unwrap();

        let selector = crate::paths::state_dir().join(SELECTOR);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&selector).unwrap()).unwrap();
        value["fingerprint"] = serde_json::Value::String("wrong".into());
        std::fs::write(&selector, serde_json::to_vec(&value).unwrap()).unwrap();

        assert!(list().is_err());
    }

    #[tokio::test]
    async fn usage_reads_nothing_without_accounts_and_survives_an_invalid_selection() {
        let config = crate::paths::config_dir();
        let reader = Arc::new(CountingReader(AtomicUsize::new(0)));
        let _ = std::fs::remove_file(config.join("claude-accounts.json"));

        let report = usage_with(offline_query(reader.clone())).await.unwrap();
        assert!(report.lines.is_empty() && report.warning.is_none());
        assert_eq!(reader.0.load(Ordering::SeqCst), 0);

        let work = config.join("work");
        let personal = config.join("personal");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&personal).unwrap();
        write_accounts(&[("work", work), ("personal", personal)]);
        select("work").unwrap();
        let selector = crate::paths::state_dir().join(SELECTOR);
        let valid = std::fs::read(&selector).unwrap();
        std::fs::write(&selector, b"{").unwrap();
        std::fs::set_permissions(&selector, std::fs::Permissions::from_mode(0o600)).unwrap();

        let report = usage_with(offline_query(reader.clone())).await.unwrap();
        assert_eq!(report.warning, Some(SELECTION_WARNING));
        assert_eq!(report.lines.len(), 2);
        assert!(report.lines[0].starts_with("  work "));
        assert!(report.lines[1].starts_with("  personal "));
        assert!(
            report
                .lines
                .iter()
                .all(|line| line.ends_with("login not found"))
        );
        assert_eq!(reader.0.load(Ordering::SeqCst), 2);

        std::fs::write(&selector, valid).unwrap();
        let report = usage_with(offline_query(reader.clone())).await.unwrap();
        assert!(report.warning.is_none());
        assert!(report.lines[0].starts_with("* work "));
        assert!(report.lines[1].starts_with("  personal "));

        std::fs::set_permissions(
            config.join("claude-accounts.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(usage_with(offline_query(reader)).await.is_err());
    }
}
