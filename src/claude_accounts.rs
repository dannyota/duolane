#[cfg(unix)]
mod auth_overrides;
#[cfg(unix)]
mod identity;
pub(crate) mod limits;
#[cfg(unix)]
mod picker;
pub(crate) mod profile_state;
#[cfg(unix)]
pub(crate) mod roster;
#[cfg(unix)]
pub(crate) mod store;
#[cfg(unix)]
mod switch;
#[cfg(unix)]
mod transcripts;

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name != "default"
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

pub(crate) fn valid_profile_tag(name: &str) -> bool {
    name == "default" || valid_name(name)
}

#[cfg(unix)]
mod unix {
    use super::{
        auth_overrides::{AUTH_OVERRIDE_PAIR, AUTH_OVERRIDES, reject_auth_overrides},
        identity::identity,
        limits, picker, roster, store, transcripts, valid_name,
    };
    use crate::paths;
    use anyhow::{Context, Result, bail, ensure};
    use std::ffi::OsString;
    use std::fs::File;
    use std::io::IsTerminal;
    use std::io::{BufRead, Read, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub(crate) struct Profile {
        path: PathBuf,
        dir: File,
        _lock: File,
        cwd: Option<PathBuf>,
    }

    impl Profile {
        pub(crate) fn path(&self) -> &Path {
            &self.path
        }

        pub(crate) fn name(&self) -> Option<&str> {
            self.path.file_name().and_then(|name| name.to_str())
        }

        pub(crate) fn settings(&self) -> Result<Option<Vec<u8>>> {
            store::settings_read(&self.dir)
        }

        pub(crate) fn recheck(&self) -> Result<()> {
            let current = store::checked_dir(&self.path, false)?.context("profile missing")?;
            let a = self.dir.metadata()?;
            let b = current.metadata()?;
            ensure!(
                a.dev() == b.dev() && a.ino() == b.ino(),
                "profile changed during launch"
            );
            ensure!(a.mode() & 0o7777 == 0o700, "unsafe profile permissions");
            if let Some(cwd) = &self.cwd {
                recheck_cwd(cwd)?;
            }
            Ok(())
        }

        #[cfg(test)]
        pub(crate) fn test_profile(path: &Path) -> Self {
            let dir = File::open(path).unwrap();
            let lock = store::try_lock(&dir).unwrap();
            Self {
                path: path.to_path_buf(),
                dir,
                _lock: lock,
                cwd: None,
            }
        }
    }
    fn input(prompt: &str) -> Result<String> {
        print!("{prompt}");
        std::io::stdout().flush()?;
        let mut bytes = Vec::new();
        let stdin = std::io::stdin();
        let mut reader = stdin.lock().take(66);
        reader.read_until(b'\n', &mut bytes)?;
        ensure!(
            bytes.len() <= 64 && bytes.last() == Some(&b'\n'),
            "invalid input"
        );
        bytes.pop();
        Ok(String::from_utf8(bytes)?)
    }
    fn config_root(create: bool) -> Result<Option<PathBuf>> {
        store::checked_canonical_dir(&paths::config_dir(), create)
    }
    fn profile_dir(name: &str, create: bool) -> Result<Option<(PathBuf, File)>> {
        ensure!(valid_name(name), "invalid profile name");
        let Some(root) = config_root(create)? else {
            return Ok(None);
        };
        let accounts = root.join("claude-accounts");
        if store::checked_dir(&accounts, create)?.is_none() {
            return Ok(None);
        }
        let path = accounts.join(name);
        let Some(dir) = store::checked_dir(&path, create)? else {
            return Ok(None);
        };
        let stat = dir.metadata()?;
        ensure!(stat.mode() & 0o7777 == 0o700, "unsafe profile permissions");
        Ok(Some((path, dir)))
    }
    fn profile(name: &str, create: bool) -> Result<Option<Profile>> {
        profile_expected(name, create, None, false)
    }

    fn profile_expected(
        name: &str,
        create: bool,
        expected: Option<&str>,
        absent: bool,
    ) -> Result<Option<Profile>> {
        ensure!(valid_name(name), "invalid profile name");
        let Some(parent) = selector_dir(create)? else {
            return Ok(None);
        };
        let lock = store::lock(&parent, &format!(".profile-{name}.lock"), false)?;
        let existing = profile_dir(name, false)?;
        if absent {
            ensure!(
                existing.is_none(),
                "profile already exists; reload and retry"
            );
        }
        if let Some(expected) = expected {
            ensure!(
                existing
                    .as_ref()
                    .and_then(|(_, dir)| identity(dir).ok().flatten())
                    .as_deref()
                    == Some(expected),
                "profile changed or identity unknown; reload and retry"
            );
        }
        let Some((path, dir)) = (if existing.is_some() {
            existing
        } else {
            profile_dir(name, create)?
        }) else {
            return Ok(None);
        };
        if !create && store::private_read(&dir, ".duolane-ready", 64)?.is_none() {
            return Ok(None);
        }
        Ok(Some(Profile {
            path,
            dir,
            _lock: lock,
            cwd: None,
        }))
    }

    fn selector_dir(create: bool) -> Result<Option<File>> {
        let Some(root) = config_root(create)? else {
            return Ok(None);
        };
        store::checked_dir(&root.join("claude-accounts"), create)
    }

    pub(super) fn claude_command(config: Option<&Path>) -> Command {
        let mut command = Command::new("claude");
        command.env_remove("DUOLANE_OPENAI_API_KEY");
        for name in AUTH_OVERRIDES.into_iter().chain(AUTH_OVERRIDE_PAIR) {
            command.env_remove(name);
        }
        if let Some(config) = config {
            command.env("CLAUDE_CONFIG_DIR", config);
        }
        command
    }

    fn inherit_login_lock(command: &mut Command, lock: &File) -> Result<()> {
        use std::os::unix::process::CommandExt;
        let fd = rustix::io::fcntl_dupfd_cloexec(lock, 3)?;
        unsafe {
            command.pre_exec(move || {
                rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).map_err(Into::into)
            });
        }
        Ok(())
    }

    pub fn login(name: Option<String>, expected: Option<String>, absent: bool) -> Result<String> {
        reject_auth_overrides()?;
        let name = match name {
            Some(name) => name,
            None => input("Profile name (lowercase letters, digits, - or _): ")?,
        };
        let profile = profile_expected(&name, true, expected.as_deref(), absent)?
            .context("profile unavailable")?;
        let mut command = Command::new("claude");
        command
            .args(["auth", "login", "--claudeai"])
            .env_remove("DUOLANE_OPENAI_API_KEY")
            .env("CLAUDE_CONFIG_DIR", &profile.path);
        inherit_login_lock(&mut command, &profile._lock)?;
        let result = (|| -> Result<()> {
            let status = command.status()?;
            drop(command);
            ensure!(status.success(), "Claude login failed");
            let dir = store::checked_dir(&profile.path, false)?.context("profile missing")?;
            let original = profile.dir.metadata()?;
            let current = dir.metadata()?;
            ensure!(
                original.dev() == current.dev() && original.ino() == current.ino(),
                "profile changed during login"
            );
            ensure!(
                profile.dir.metadata()?.mode() & 0o7777 == 0o700,
                "unsafe profile permissions"
            );
            store::private_replace(&profile.dir, ".duolane-ready", b"ready\n")?;
            Ok(())
        })();
        let unlock = rustix::fs::flock(&profile._lock, rustix::fs::FlockOperation::Unlock);
        result.and(unlock.map_err(Into::into))?;
        Ok(name)
    }

    pub(super) fn registered() -> Result<Vec<(String, PathBuf)>> {
        registered_with_warnings(true)
    }

    fn registered_with_warnings(warn: bool) -> Result<Vec<(String, PathBuf)>> {
        let mut profiles = Vec::new();
        let Some(root) = config_root(false)? else {
            return Ok(profiles);
        };
        let dir = root.join("claude-accounts");
        if store::checked_dir(&dir, false)?.is_none() {
            return Ok(profiles);
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !valid_name(&name) {
                continue;
            }
            let ready = (|| -> Result<Option<PathBuf>> {
                let Some((path, profile_dir)) = profile_dir(&name, false)? else {
                    return Ok(None);
                };
                Ok(store::private_read(&profile_dir, ".duolane-ready", 64)?.map(|_| path))
            })();
            match ready {
                Ok(Some(path)) => profiles.push((name, path)),
                Ok(None) => {}
                Err(_) if warn => eprintln!("warning: skipping unsafe profile {name}"),
                Err(_) => {}
            }
        }
        profiles.sort();
        Ok(profiles)
    }

    pub(crate) struct MonitorRoster {
        pub names: Vec<String>,
        pub identities: std::collections::BTreeMap<String, String>,
        pub selected: String,
        pub overridden: bool,
    }

    pub(crate) fn monitor_roster() -> Result<MonitorRoster> {
        let mut names = vec!["default".to_owned()];
        names.extend(
            registered_with_warnings(false)?
                .into_iter()
                .map(|(name, _)| name),
        );
        let identities = names
            .iter()
            .filter_map(|name| {
                if name == "default" {
                    return Some((name.clone(), "default".into()));
                }
                profile_dir(name, false)
                    .ok()
                    .flatten()
                    .and_then(|(_, dir)| identity(&dir).ok().flatten())
                    .map(|id| (name.clone(), id))
            })
            .collect();
        Ok(MonitorRoster {
            names,
            identities,
            selected: current_selection()?,
            overridden: std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty()),
        })
    }

    pub(crate) fn monitor_details(
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Option<super::profile_state::Roster> {
        let profiles = registered_with_warnings(false).ok()?;
        super::profile_state::Roster::check(&profiles, cancelled)
    }

    fn color() -> bool {
        std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
    }

    pub(super) fn current_selection() -> Result<String> {
        Ok(read_selection()?.unwrap_or_else(|| "default".into()))
    }

    pub fn announce(name: &str) -> Result<()> {
        let entries = roster::entries(&registered()?)?;
        let (summary, twin) = roster::describe(&entries, name);
        match summary {
            Some(summary) => println!("Signed in to {name} as {summary}."),
            None => println!("Signed in to {name}."),
        }
        if let Some(twin) = twin {
            eprintln!(
                "warning: {name} is the same account as {twin}. To add another account, sign out of claude.ai in the browser or open the sign-in link in a private window, then run `duolane accounts login {name}` again."
            );
        } else if current_selection().ok().as_deref() != Some(name) {
            println!("Run `duolane accounts use {name}` to select it.");
        }
        Ok(())
    }

    pub fn list() -> Result<()> {
        let selected = current_selection()?;
        let entries = roster::entries(&registered()?)?;
        let usage = limits::load();
        let now = jiff::Timestamp::now();
        let table = roster::table(&entries, &selected, &usage, now, color());
        println!("{}", table.header);
        for row in table.rows {
            println!("{row}");
        }
        println!("\n   {}", roster::total(&entries, &usage, now));
        println!(" ● selected for the next duolane claude launch");
        if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty()) {
            println!(" CLAUDE_CONFIG_DIR is set, so duolane claude ignores the selection");
        }
        Ok(())
    }

    pub fn remove(name: String, yes: bool, expected: Option<String>) -> Result<()> {
        ensure!(valid_name(&name), "invalid profile name");
        let selector = selector_dir(false)?.context("unknown profile")?;
        let lock = store::lock(&selector, &format!(".profile-{name}.lock"), false)?;
        let Some((path, dir)) = profile_dir(&name, false)? else {
            bail!("unknown profile");
        };
        if let Some(expected) = expected {
            ensure!(
                identity(&dir)?.as_deref() == Some(expected.as_str()),
                "profile changed or identity unknown; reload and retry"
            );
        }
        read_selection()?;
        if !yes {
            ensure!(
                std::io::stdin().is_terminal(),
                "removal requires terminal confirmation or --yes"
            );
            eprint!(
                "Sign out and delete profile {name:?}? Project folders shared from the default profile stay in place. Type yes to confirm: "
            );
            std::io::stderr().flush()?;
            let mut response = String::new();
            std::io::stdin().lock().take(65).read_line(&mut response)?;
            if response != "yes\n" && response != "yes\r\n" {
                println!("Kept {name}.");
                return Ok(());
            }
        }
        let mut command = claude_command(Some(&path));
        command.args(["auth", "logout"]);
        inherit_login_lock(&mut command, &lock)?;
        let result = (|| -> Result<()> {
            let status = command.status()?;
            drop(command);
            ensure!(status.success(), "Claude logout failed; profile kept");
            let selector_lock = store::lock(&selector, ".selector.lock", true)?;
            let selected = read_selection()?;
            let current = store::checked_dir(&path, false)?.context("profile missing")?;
            let a = dir.metadata()?;
            let b = current.metadata()?;
            ensure!(
                a.dev() == b.dev() && a.ino() == b.ino(),
                "profile changed during removal"
            );
            if selected.as_deref() == Some(name.as_str()) {
                store::private_replace(&selector, ".selected", b"default\n")?;
            }
            std::fs::remove_dir_all(&path)?;
            drop(selector_lock);
            Ok(())
        })();
        let unlock = rustix::fs::flock(&lock, rustix::fs::FlockOperation::Unlock);
        result.and(unlock.map_err(Into::into))?;
        println!("Removed {name}.");
        Ok(())
    }

    fn choose(profiles: &[(String, PathBuf)], selected: &str) -> Result<Option<String>> {
        let entries = roster::entries(profiles)?;
        let table = roster::table(
            &entries,
            selected,
            &limits::load(),
            jiff::Timestamp::now(),
            false,
        );
        let start = entries
            .iter()
            .position(|entry| entry.name == selected)
            .unwrap_or(0);
        println!(
            "Selection applies to the next launch. Named profiles may share approved project history."
        );
        let choice = picker::interactive(&table.header, &table.rows, start, color())?;
        Ok(choice.and_then(|index| entries.get(index).map(|entry| entry.name.clone())))
    }

    pub(super) fn profile_names(profiles: &[(String, PathBuf)]) -> Vec<String> {
        let mut names: Vec<String> = profiles.iter().map(|(name, _)| name.clone()).collect();
        names.insert(0, "default".into());
        names
    }

    pub(super) fn checked_target(name: &str, names: &[String]) -> Result<()> {
        ensure!(
            name == "default" || valid_name(name),
            "invalid profile name"
        );
        ensure!(names.iter().any(|known| known == name), "unknown profile");
        Ok(())
    }

    pub(super) fn prompt_name(
        profiles: &[(String, PathBuf)],
        selected: &str,
    ) -> Result<Option<String>> {
        let names = profile_names(profiles);
        let interactive = std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && picker::fits(names.len());
        if interactive {
            return choose(profiles, selected);
        }
        println!(
            "Selection applies to the next launch. Named profiles may share approved project history."
        );
        for (i, name) in names.iter().enumerate() {
            println!("{}: {}", i + 1, name);
        }
        let choice = input("Choice: ")?
            .parse::<usize>()
            .context("invalid choice")?;
        Ok(Some(
            choice
                .checked_sub(1)
                .and_then(|i| names.get(i))
                .context("invalid choice")?
                .clone(),
        ))
    }

    pub fn select(name: Option<String>) -> Result<()> {
        if let Some(name) = &name {
            ensure!(
                name == "default" || valid_name(name),
                "invalid profile name"
            );
        }
        let profiles = registered()?;
        let name = if let Some(name) = name {
            checked_target(&name, &profile_names(&profiles))?;
            name
        } else {
            let Some(name) = prompt_name(&profiles, &current_selection()?)? else {
                println!("Selection unchanged.");
                return Ok(());
            };
            name
        };
        save_selection(&name, None)?;
        println!("The next duolane claude launch uses {name}.");
        Ok(())
    }

    pub(crate) fn save_selection(name: &str, expected: Option<&str>) -> Result<()> {
        save_selection_identity(name, expected, None)
    }

    pub(super) fn save_selection_identity(
        name: &str,
        expected: Option<&str>,
        identity_expected: Option<&str>,
    ) -> Result<()> {
        let _profile = if name == "default" {
            None
        } else {
            Some(
                profile_expected(name, false, identity_expected, false)?
                    .context("unknown profile")?,
            )
        };
        if name == "default" {
            ensure!(
                identity_expected.is_none_or(|value| value == "default"),
                "profile changed or identity unknown; reload and retry"
            );
        }
        let dir = selector_dir(true)?.context("selector unavailable")?;
        let _selector_lock = store::lock(&dir, ".selector.lock", expected.is_none())?;
        let current = current_selection()?;
        if let Some(expected) = expected {
            ensure!(current == expected, "selection changed; reload and retry");
            ensure!(
                !std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|value| !value.is_empty()),
                "CLAUDE_CONFIG_DIR overrides selection"
            );
        }
        store::private_replace(&dir, ".selected", format!("{name}\n").as_bytes())?;
        Ok(())
    }
    fn recheck_cwd(expected: &Path) -> Result<()> {
        ensure!(
            std::env::current_dir()?.canonicalize()? == expected,
            "working directory changed during launch"
        );
        Ok(())
    }
    fn link_launch(
        profile: &Profile,
        default: &Path,
        cwd: &Path,
        confirm: impl FnOnce(&Path) -> Result<bool>,
    ) -> Result<()> {
        transcripts::link_project(&profile.dir, default, cwd, |source| {
            let approved = confirm(source)?;
            recheck_cwd(cwd)?;
            Ok(approved)
        })?;
        recheck_cwd(cwd)
    }

    pub(crate) fn prepare_launch(
        args: &[OsString],
        expected: Option<&str>,
    ) -> Result<Option<Profile>> {
        let Some(mut profile) = selected_profile_expected(expected)? else {
            return Ok(None);
        };
        reject_auth_overrides()?;
        ensure!(
            !std::env::var_os("CLAUDE_CODE_PROJECT_DIR_NAME").is_some_and(|v| !v.is_empty()),
            "CLAUDE_CODE_PROJECT_DIR_NAME is unsupported"
        );
        for arg in args.iter().take_while(|arg| *arg != "--") {
            let bytes = arg.as_os_str().as_bytes();
            ensure!(
                bytes != b"--worktree"
                    && !bytes.starts_with(b"--worktree=")
                    && !bytes.starts_with(b"-w"),
                "worktree arguments are unsupported"
            );
        }
        let cwd = std::env::current_dir()?.canonicalize()?;
        let default = paths::claude_config_dir()?;
        let default = store::canonical_missing(&default)?;
        let name = profile
            .path
            .file_name()
            .context("profile name missing")?
            .to_string_lossy();
        link_launch(&profile, &default, &cwd, |source| {
            ensure!(
                std::io::stdin().is_terminal(),
                "project sharing requires terminal confirmation"
            );
            eprint!(
                "Share the whole folder {:?} with profile {:?}, including auto memory? History may be sent to this profile. Type yes to confirm: ",
                source, name
            );
            std::io::stderr().flush()?;
            let mut response = String::new();
            std::io::stdin().lock().take(65).read_line(&mut response)?;
            Ok(response == "yes\n" || response == "yes\r\n")
        })?;
        profile.cwd = Some(cwd);
        Ok(Some(profile))
    }

    pub(crate) fn selected_profile() -> Result<Option<Profile>> {
        if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty()) {
            return Ok(None);
        }
        resolve_selection(None)
    }
    fn selected_profile_expected(expected: Option<&str>) -> Result<Option<Profile>> {
        if expected.is_none() {
            return selected_profile();
        }
        resolve_selection(expected)
    }
    fn read_selection() -> Result<Option<String>> {
        let Some(dir) = selector_dir(false)? else {
            return Ok(None);
        };
        let Some(value) = store::private_read(&dir, ".selected", 64)? else {
            return Ok(None);
        };
        let value = std::str::from_utf8(&value)?;
        let Some(name) = value.strip_suffix('\n') else {
            bail!("invalid selection");
        };
        ensure!(name == "default" || valid_name(name), "invalid selection");
        Ok(Some(name.to_owned()))
    }
    fn resolve_selection(expected: Option<&str>) -> Result<Option<Profile>> {
        let selected = read_selection()?;
        if let Some(expected) = expected {
            ensure!(
                selected.as_deref().unwrap_or("default") == expected,
                "selection changed; expected {expected}, nothing was launched"
            );
        }
        let Some(name) = selected else {
            return Ok(None);
        };
        if name == "default" {
            return Ok(None);
        }
        resolve_named_selection(&name).map(Some)
    }
    fn resolve_named_selection(name: &str) -> Result<Profile> {
        let profile = profile(name, false)?.context("selected profile is not registered")?;
        ensure!(
            read_selection()?.as_deref() == Some(name),
            "selection changed; expected {name}, nothing was launched"
        );
        Ok(profile)
    }
    #[cfg(test)]
    mod tests {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/claude_accounts/tests.rs"
        ));
    }
}

#[cfg(unix)]
pub use switch::switch;
#[cfg(unix)]
use unix::claude_command;
#[cfg(unix)]
pub(crate) use unix::{Profile, monitor_details, monitor_roster, prepare_launch, save_selection};
#[cfg(unix)]
pub use unix::{announce, list, login, remove, select};

#[cfg(not(unix))]
mod unsupported;
#[cfg(not(unix))]
pub use unsupported::*;
