use super::{Login, LoginReader, Reason, parse_login};
#[cfg(any(not(target_os = "macos"), test))]
use std::fs::File;
#[cfg(any(not(target_os = "macos"), test))]
use std::path::Path;

const LOGIN_LIMIT: u64 = 64 * 1024;

#[cfg(any(not(target_os = "macos"), test))]
pub(in crate::providers::anthropic::accounts) struct FileReader;

#[cfg(any(not(target_os = "macos"), test))]
impl LoginReader for FileReader {
    fn read(&self, _configured: &Path, dir: &File) -> Result<Login, Reason> {
        use crate::secure_fs::{bounded, checked};
        use rustix::fs::{self, FileType, Mode, OFlags};

        let fd = match fs::openat(
            dir,
            ".credentials.json",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Err(Reason::LoginNotFound),
            Err(_) => return Err(Reason::LoginUnreadable),
        };
        let file = File::from(fd);
        checked(
            &file,
            FileType::RegularFile,
            Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
        )
        .map_err(|_| Reason::LoginUnreadable)?;
        let data = bounded(file, LOGIN_LIMIT).map_err(|_| Reason::LoginUnreadable)?;
        parse_login(&data)
    }
}

#[cfg(any(target_os = "macos", test))]
pub(in crate::providers::anthropic::accounts) use keychain::KeychainReader;

#[cfg(any(target_os = "macos", test))]
mod keychain {
    use super::{LOGIN_LIMIT, Login, LoginReader, Reason, parse_login};
    use std::fs::File;
    use std::io::{ErrorKind, Read};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdout, Command, Stdio};
    use std::time::{Duration, Instant};

    const SECURITY_PROGRAM: &str = "/usr/bin/security";
    const SECURITY_DEADLINE: Duration = Duration::from_secs(30);
    const POLL_INTERVAL: Duration = Duration::from_millis(10);

    pub(in crate::providers::anthropic::accounts) struct KeychainReader {
        pub(in crate::providers::anthropic::accounts) program: PathBuf,
        pub(in crate::providers::anthropic::accounts) deadline: Duration,
    }

    impl KeychainReader {
        pub(in crate::providers::anthropic::accounts) fn system() -> Self {
            Self {
                program: PathBuf::from(SECURITY_PROGRAM),
                deadline: SECURITY_DEADLINE,
            }
        }
    }

    pub(in crate::providers::anthropic::accounts) fn keychain_service(configured: &Path) -> String {
        let bytes = configured.as_os_str().as_bytes();
        let mut end = bytes.len();
        while end > 1 && bytes[end - 1] == b'/' {
            end -= 1;
        }
        let digest =
            crate::secure_fs::fingerprint(Path::new(std::ffi::OsStr::from_bytes(&bytes[..end])));
        format!("Claude Code-credentials-{}", &digest[..8])
    }

    impl LoginReader for KeychainReader {
        fn read(&self, configured: &Path, _dir: &File) -> Result<Login, Reason> {
            let args = [
                "find-generic-password".to_string(),
                "-s".to_string(),
                keychain_service(configured),
                "-w".to_string(),
            ];
            parse_login(&run_child(&self.program, &args, self.deadline)?)
        }
    }

    struct Reaper(Child);

    impl Drop for Reaper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    enum Drained {
        Open,
        Closed,
    }

    fn drain(stdout: &mut ChildStdout, data: &mut Vec<u8>) -> Result<Drained, Reason> {
        let mut chunk = [0u8; 4096];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => return Ok(Drained::Closed),
                Ok(read) => {
                    data.extend_from_slice(&chunk[..read]);
                    if data.len() as u64 > LOGIN_LIMIT {
                        return Err(Reason::LoginUnreadable);
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(Drained::Open),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Err(Reason::LoginUnreadable),
            }
        }
    }

    fn run_child(program: &Path, args: &[String], deadline: Duration) -> Result<Vec<u8>, Reason> {
        let mut child = Reaper(
            Command::new(program)
                .args(args)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|_| Reason::LoginUnreadable)?,
        );
        let mut stdout = child.0.stdout.take().ok_or(Reason::LoginUnreadable)?;
        let flags = rustix::fs::fcntl_getfl(&stdout).map_err(|_| Reason::LoginUnreadable)?;
        rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|_| Reason::LoginUnreadable)?;
        let started = Instant::now();
        let mut data = Vec::new();
        let status = loop {
            drain(&mut stdout, &mut data)?;
            match child.0.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(_) => return Err(Reason::LoginUnreadable),
            }
            if started.elapsed() >= deadline {
                return Err(Reason::KeychainTimedOut);
            }
            std::thread::sleep(POLL_INTERVAL);
        };
        match status.code() {
            Some(0) => {}
            Some(128 | 51) => return Err(Reason::KeychainDenied),
            Some(44) => return Err(Reason::LoginNotFound),
            _ => return Err(Reason::LoginUnreadable),
        }
        drain(&mut stdout, &mut data)?;
        Ok(data)
    }
}

#[cfg(test)]
pub(in crate::providers::anthropic::accounts) use keychain::keychain_service;
