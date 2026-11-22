#[cfg(unix)]
mod store;
#[cfg(unix)]
mod transcripts;

#[cfg(unix)]
mod unix {
    use super::{store, transcripts};
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
        let Some((path, dir)) = profile_dir(name, create)? else {
            return Ok(None);
        };
        let lock = store::try_lock(&dir)?;
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

    fn reject_auth_overrides() -> Result<()> {
        let vars = [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "ANTHROPIC_PROFILE",
        ];
        let nonempty = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        for name in vars {
            ensure!(!nonempty(name), "authentication override set: {name}");
        }
        ensure!(
            !(nonempty("ANTHROPIC_FEDERATION_RULE_ID") && nonempty("ANTHROPIC_ORGANIZATION_ID")),
            "authentication overrides set: ANTHROPIC_FEDERATION_RULE_ID and ANTHROPIC_ORGANIZATION_ID"
        );
        Ok(())
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

    pub fn login(name: Option<String>) -> Result<()> {
        reject_auth_overrides()?;
        let name = match name {
            Some(name) => name,
            None => input("Profile name: ")?,
        };
        let profile = profile(&name, true)?.context("profile unavailable")?;
        let mut command = Command::new("claude");
        command
            .args(["auth", "login", "--claudeai"])
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
        result.and(unlock.map_err(Into::into))
    }

    pub fn select(name: Option<String>) -> Result<()> {
        if let Some(name) = &name {
            ensure!(
                name == "default" || valid_name(name),
                "invalid profile name"
            );
        }
        let mut names = Vec::new();
        if let Some(root) = config_root(false)? {
            let dir = root.join("claude-accounts");
            if store::checked_dir(&dir, false)?.is_some() {
                for entry in std::fs::read_dir(dir)? {
                    let entry = entry?;
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if !valid_name(&name) {
                        continue;
                    }
                    let ready = (|| -> Result<bool> {
                        let Some((_, profile_dir)) = profile_dir(&name, false)? else {
                            return Ok(false);
                        };
                        Ok(store::private_read(&profile_dir, ".duolane-ready", 64)?.is_some())
                    })();
                    match ready {
                        Ok(true) => names.push(name),
                        Ok(false) => {}
                        Err(_) => eprintln!("warning: skipping unsafe profile {name}"),
                    }
                }
            }
        }
        names.sort();
        names.insert(0, "default".into());
        let name = if let Some(name) = name {
            ensure!(names.contains(&name), "unknown profile");
            name
        } else {
            println!(
                "Selection applies to the next launch. Named profiles may share approved project history."
            );
            for (i, name) in names.iter().enumerate() {
                println!("{}: {}", i + 1, name);
            }
            let choice = input("Choice: ")?
                .parse::<usize>()
                .context("invalid choice")?;
            choice
                .checked_sub(1)
                .and_then(|i| names.get(i))
                .context("invalid choice")?
                .clone()
        };
        read_selection()?;
        let dir = selector_dir(true)?.context("selector unavailable")?;
        store::private_replace(&dir, ".selected", format!("{name}\n").as_bytes())
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

    pub(crate) fn prepare_launch(args: &[OsString]) -> Result<Option<Profile>> {
        let Some(mut profile) = selected_profile()? else {
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
        resolve_selection()
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
    fn resolve_selection() -> Result<Option<Profile>> {
        let Some(name) = read_selection()? else {
            return Ok(None);
        };
        if name == "default" {
            return Ok(None);
        }
        Ok(Some(
            profile(&name, false)?.context("selected profile is not registered")?,
        ))
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
pub(crate) use unix::{Profile, prepare_launch};
#[cfg(unix)]
pub use unix::{login, select};

#[cfg(not(unix))]
pub(crate) struct Profile;
#[cfg(not(unix))]
impl Profile {
    pub(crate) fn path(&self) -> &std::path::Path {
        unreachable!()
    }
    pub(crate) fn settings(&self) -> anyhow::Result<Option<Vec<u8>>> {
        unreachable!()
    }
    pub(crate) fn recheck(&self) -> anyhow::Result<()> {
        unreachable!()
    }
}
#[cfg(not(unix))]
pub(crate) fn prepare_launch(_: &[std::ffi::OsString]) -> anyhow::Result<Option<Profile>> {
    Ok(None)
}

#[cfg(not(unix))]
pub fn login(_: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("Claude accounts are unavailable on this platform")
}
#[cfg(not(unix))]
pub fn select(_: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("Claude accounts are unavailable on this platform")
}
