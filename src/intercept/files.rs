use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom, SystemRandom};

pub(super) use crate::providers::codex::auth::token_store::write_atomic as write;

pub(super) const CA_FILE: &str = "ca.pem";
pub(super) const LEAF_FILE: &str = "leaf.pem";
pub(super) const KEY_FILE: &str = "leaf.key";
pub(super) const PASSWORD_FILE: &str = "proxy-password";

const PASSWORD_BYTES: usize = 32;
const PASSWORD_CHARS: usize = 43;

pub(super) fn ensure_dir(dir: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| anyhow::anyhow!("cannot create {}: {error}", parent.display()))?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => anyhow::bail!("cannot create {}: {error}", dir.display()),
    }
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|error| anyhow::anyhow!("cannot inspect {}: {error}", dir.display()))?;
    if !metadata.file_type().is_dir() {
        anyhow::bail!(
            "refusing {}: it must be a directory, not a symlink or file",
            dir.display()
        );
    }
    restrict(dir, &metadata, 0o700)
}

pub(super) fn read(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => anyhow::bail!("cannot inspect {}: {error}", path.display()),
    };
    if !metadata.file_type().is_file() {
        anyhow::bail!(
            "refusing {}: it must be a regular file, not a symlink",
            path.display()
        );
    }
    restrict(path, &metadata, 0o600)?;
    Ok(std::fs::read(path).ok())
}

pub(super) fn load_or_create_password(dir: &Path) -> anyhow::Result<String> {
    let path = dir.join(PASSWORD_FILE);
    if let Some(existing) = read(&path)?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| valid_password(text))
    {
        return Ok(existing);
    }
    let mut bytes = [0u8; PASSWORD_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cannot generate the proxy password"))?;
    let password = URL_SAFE_NO_PAD.encode(bytes);
    write(&path, password.as_bytes())?;
    Ok(password)
}

fn valid_password(text: &str) -> bool {
    text.len() == PASSWORD_CHARS
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(unix)]
fn restrict(path: &Path, metadata: &std::fs::Metadata, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o777 == mode {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| anyhow::anyhow!("cannot restrict {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _metadata: &std::fs::Metadata, _mode: u32) -> anyhow::Result<()> {
    Ok(())
}
