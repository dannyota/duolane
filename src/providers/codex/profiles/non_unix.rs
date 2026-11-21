use crate::providers::codex::auth::token_store::{
    CodexCliAuthStore, CodexTokenStore, codex_auth_file,
};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

pub enum AuthSource {
    Default { path: PathBuf },
}
impl AuthSource {
    pub fn path(&self) -> &Path {
        match self {
            Self::Default { path } => path,
        }
    }
    pub(crate) fn load_auth(
        &self,
    ) -> Result<Option<crate::providers::codex::auth::token_store::StoredAuth>> {
        CodexTokenStore::new(CodexCliAuthStore::new(self.path().to_path_buf())).load_auth()
    }
    pub fn login_exists(&self) -> Result<bool> {
        Ok(
            CodexTokenStore::new(CodexCliAuthStore::new(self.path().to_path_buf()))
                .load_auth()?
                .is_some(),
        )
    }
}
pub fn resolve_selected() -> Result<(String, AuthSource)> {
    Ok((
        "default".into(),
        AuthSource::Default {
            path: codex_auth_file(),
        },
    ))
}
pub fn select(_name: &str) -> Result<()> {
    bail!("Codex profiles are not supported on this platform")
}
pub fn list() -> Result<Vec<(String, bool, bool)>> {
    let (name, auth) = resolve_selected()?;
    Ok(vec![(name, auth.login_exists()?, true)])
}
