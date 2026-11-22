use super::identity::{self, Identity};
use super::usage as usage_query;
use super::{
    Listed, MAX_ACCOUNTS, MAX_CONFIG_BYTES, SELECTION_WARNING, UsageReport, parse_accounts,
    render_accounts, valid_directory, valid_name,
};
use crate::secure_fs::{bounded, checked, fingerprint, open_checked_directory, write_atomic};
use anyhow::{Context, Result, ensure};
use rustix::fs::{self, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};

const SELECTOR_LIMIT: u64 = 256;
const SELECTOR: &str = "claude-account";
const STORE: &str = "claude-accounts.json";

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

fn account_identity(name: &str, accounts: &[(String, PathBuf)]) -> Option<Identity> {
    identity::read(&open_source(name, accounts).ok()??.dir)
}

fn accounts() -> Result<Vec<(String, PathBuf)>> {
    let Some((dir, _)) = open_checked_directory(&crate::paths::config_dir(), false, false)? else {
        return Ok(Vec::new());
    };
    let fd = match fs::openat(
        &dir,
        STORE,
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

pub fn add(name: &str, dir: &Path) -> Result<Option<String>> {
    ensure!(valid_name(name), "invalid Claude account name");
    let configured: PathBuf = std::path::absolute(dir)?.components().collect();
    ensure!(
        valid_directory(&configured),
        "Claude account directory must contain no parent components"
    );
    let (account_dir, _) = open_checked_directory(&configured, false, true)?
        .context("Claude account directory does not exist")?;
    let added = identity::read(&account_dir);
    let mut accounts = accounts()?;
    ensure!(
        !accounts.iter().any(|(account, _)| account == name),
        "Claude account name already exists"
    );
    ensure!(
        !accounts.iter().any(|(_, path)| path == &configured),
        "Claude account directory is already added"
    );
    ensure!(accounts.len() < MAX_ACCOUNTS, "too many Claude accounts");
    if let Some(added) = &added
        && let Some((existing, _)) = accounts.iter().find(|(existing, _)| {
            account_identity(existing, &accounts).is_some_and(|other| other.same_login(added))
        })
    {
        anyhow::bail!("this Claude login is already added as {existing}");
    }
    accounts.push((name.into(), configured));
    let data = render_accounts(&accounts)?;
    let config = open_checked_directory(&crate::paths::config_dir(), true, true)?
        .context("config directory missing")?
        .0;
    write_atomic(&config, STORE, ".claude-accounts-", &data)?;
    Ok(added.map(|identity| identity.to_string()))
}

pub fn list() -> Result<Vec<Listed>> {
    let dir = state_dir(false)
        .context("Claude account selection invalid; run `duolane claude-accounts use <name>`")?;
    let accounts = accounts()?;
    let selected = selected_name(dir.as_ref(), &accounts)
        .context("Claude account selection invalid; run `duolane claude-accounts use <name>`")?;
    Ok(accounts
        .iter()
        .map(|(name, _)| Listed {
            selected: selected.as_deref() == Some(name.as_str()),
            identity: account_identity(name, &accounts).map(|identity| identity.to_string()),
            name: name.clone(),
        })
        .collect())
}

pub async fn usage() -> Result<UsageReport> {
    usage_with(usage_query::Query::production).await
}

async fn usage_with(make_query: impl FnOnce() -> usage_query::Query) -> Result<UsageReport> {
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
            label: account_identity(name, &accounts).map(|identity| identity.email),
            location: match open_source(name, &accounts) {
                Ok(Some(source)) => Ok(source.into_login_location()),
                Ok(None) => Err(usage_query::Reason::LoginNotFound),
                Err(_) => Err(usage_query::Reason::LoginUnreadable),
            },
        })
        .collect();
    let rows = make_query().collect(targets).await;
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
            names(list().unwrap()),
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
        assert_eq!(names(list().unwrap()), vec![("work".to_string(), true)]);
    }

    fn names(listed: Vec<Listed>) -> Vec<(String, bool)> {
        listed
            .into_iter()
            .map(|account| (account.name, account.selected))
            .collect()
    }

    fn write_identity(dir: &Path, email: &str) {
        let path = dir.join(".claude.json");
        let body = format!(
            r#"{{"oauthAccount":{{"emailAddress":"{email}","organizationType":"claude_pro"}}}}"#
        );
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn add_appends_in_order_and_writes_a_private_store() {
        let config = crate::paths::config_dir();
        let _ = std::fs::remove_file(crate::paths::state_dir().join(SELECTOR));
        let _ = std::fs::remove_file(config.join(STORE));
        let work = config.join("add-work");
        let personal = config.join("add-personal");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&personal).unwrap();
        write_identity(&work, "work@example.com");

        assert_eq!(
            add("work", &work).unwrap().as_deref(),
            Some("work@example.com (Pro)")
        );
        let mut trailing = personal.clone().into_os_string();
        trailing.push("/");
        assert_eq!(add("personal", Path::new(&trailing)).unwrap(), None);

        let listed = list().unwrap();
        assert_eq!(
            listed[0].identity.as_deref(),
            Some("work@example.com (Pro)")
        );
        assert_eq!(listed[1].identity, None);
        assert_eq!(
            names(listed),
            vec![("work".to_string(), false), ("personal".to_string(), false)]
        );
        let store = config.join(STORE);
        let mode = std::fs::metadata(&store).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let saved = parse_accounts(&std::fs::read(&store).unwrap()).unwrap();
        assert_eq!(saved[1].1, personal);
        select("personal").unwrap();
        assert_eq!(
            names(list().unwrap()),
            vec![("work".to_string(), false), ("personal".to_string(), true)]
        );
    }

    #[test]
    fn add_rejects_bad_names_duplicates_and_unsafe_directories() {
        let config = crate::paths::config_dir();
        let _ = std::fs::remove_file(crate::paths::state_dir().join(SELECTOR));
        let work = config.join("add-work");
        let other = config.join("add-other");
        let twin = config.join("add-twin");
        let real = config.join("add-real");
        let link = config.join("add-link");
        let shared = config.join("add-shared");
        for dir in [&work, &other, &twin, &real, &shared] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let _ = std::fs::remove_file(&link);
        symlink(&real, &link).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        write_identity(&work, "same@example.com");
        write_identity(&twin, "SAME@example.com");
        write_accounts(&[("work", work.clone())]);
        let before = std::fs::read(config.join(STORE)).unwrap();

        assert!(add("bad name", &other).is_err());
        assert!(add("", &other).is_err());
        assert!(add("work", &other).is_err());
        assert!(add("again", &work).is_err());
        assert!(add("again", &work.join("../add-work")).is_err());
        assert!(add("twin", &twin).is_err());
        assert!(add("missing", &config.join("add-missing")).is_err());
        assert!(add("link", &link).is_err());
        assert!(add("shared", &shared).is_err());
        assert_eq!(std::fs::read(config.join(STORE)).unwrap(), before);
    }

    #[test]
    fn add_stops_at_the_account_limit() {
        let config = crate::paths::config_dir();
        let dirs: Vec<(String, PathBuf)> = (0..=MAX_ACCOUNTS)
            .map(|i| (format!("a{i}"), config.join(format!("add-limit-{i}"))))
            .collect();
        for (_, dir) in &dirs {
            std::fs::create_dir_all(dir).unwrap();
        }
        let full: Vec<(&str, PathBuf)> = dirs[..MAX_ACCOUNTS]
            .iter()
            .map(|(name, dir)| (name.as_str(), dir.clone()))
            .collect();
        write_accounts(&full);
        let (name, dir) = &dirs[MAX_ACCOUNTS];
        assert!(add(name, dir).is_err());
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

        let built = std::sync::atomic::AtomicBool::new(false);
        let report = usage_with(|| {
            built.store(true, Ordering::SeqCst);
            offline_query(reader.clone())
        })
        .await
        .unwrap();
        assert!(report.lines.is_empty() && report.warning.is_none());
        assert!(!built.load(Ordering::SeqCst));
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

        let report = usage_with(|| offline_query(reader.clone())).await.unwrap();
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
        let report = usage_with(|| offline_query(reader.clone())).await.unwrap();
        assert!(report.warning.is_none());
        assert!(report.lines[0].starts_with("* work "));
        assert!(report.lines[1].starts_with("  personal "));

        std::fs::set_permissions(
            config.join("claude-accounts.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        built.store(false, Ordering::SeqCst);
        let result = usage_with(|| {
            built.store(true, Ordering::SeqCst);
            offline_query(reader)
        })
        .await;
        assert!(result.is_err());
        assert!(!built.load(Ordering::SeqCst));
    }
}
