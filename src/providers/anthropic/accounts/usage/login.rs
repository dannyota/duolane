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
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{self, TryRecvError};
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

    fn reap(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    fn run_child(program: &Path, args: &[String], deadline: Duration) -> Result<Vec<u8>, Reason> {
        let mut child = Command::new(program)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|_| Reason::LoginUnreadable)?;
        let Some(stdout) = child.stdout.take() else {
            reap(&mut child);
            return Err(Reason::LoginUnreadable);
        };
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let result = stdout.take(LOGIN_LIMIT + 1).read_to_end(&mut data);
            let _ = sender.send(result.map(|_| data));
        });
        let started = Instant::now();
        let mut output: Option<Vec<u8>> = None;
        let status = loop {
            if output.is_none() {
                match receiver.try_recv() {
                    Ok(Ok(data)) if data.len() as u64 > LOGIN_LIMIT => {
                        reap(&mut child);
                        return Err(Reason::LoginUnreadable);
                    }
                    Ok(Ok(data)) => output = Some(data),
                    Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                        reap(&mut child);
                        return Err(Reason::LoginUnreadable);
                    }
                    Err(TryRecvError::Empty) => {}
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(_) => {
                    reap(&mut child);
                    return Err(Reason::LoginUnreadable);
                }
            }
            if started.elapsed() >= deadline {
                reap(&mut child);
                return Err(Reason::KeychainTimedOut);
            }
            std::thread::sleep(POLL_INTERVAL);
        };
        match status.code() {
            Some(0) => {}
            Some(128) | Some(51) => return Err(Reason::KeychainDenied),
            Some(44) => return Err(Reason::LoginNotFound),
            _ => return Err(Reason::LoginUnreadable),
        }
        let data = match output {
            Some(data) => data,
            None => match receiver.recv_timeout(deadline.saturating_sub(started.elapsed())) {
                Ok(Ok(data)) => data,
                Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Reason::LoginUnreadable);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(Reason::KeychainTimedOut),
            },
        };
        if data.len() as u64 > LOGIN_LIMIT {
            return Err(Reason::LoginUnreadable);
        }
        Ok(data)
    }
}

#[cfg(test)]
pub(in crate::providers::anthropic::accounts) use keychain::keychain_service;
