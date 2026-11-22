use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) fn environment_snapshot() -> HashMap<String, String> {
    std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            if key.eq_ignore_ascii_case("DUOLANE_OPENAI_API_KEY")
                || key.eq_ignore_ascii_case("OPENAI_API_KEY")
            {
                return None;
            }
            Some((key, value.into_string().ok()?))
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct DirResolverEnv {
    pub platform: String,
    pub env: HashMap<String, String>,
    pub home: String,
}

impl Default for DirResolverEnv {
    fn default() -> Self {
        Self {
            platform: std::env::consts::OS.into(),
            env: environment_snapshot(),
            home: std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| "/".to_string()),
        }
    }
}

pub fn resolve_config_dir(deps: &DirResolverEnv) -> PathBuf {
    if let Some(override_dir) = deps
        .env
        .get("DUOLANE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
    {
        return Path::new(override_dir).to_path_buf();
    }

    if deps.platform == "windows" {
        let appdata = deps
            .env
            .get("APPDATA")
            .cloned()
            .unwrap_or_else(|| format!("{}\\AppData\\Roaming", deps.home));
        return join_with_sep(&appdata, &["duolane"], true);
    }

    let base = deps.env.get("XDG_CONFIG_HOME").cloned().unwrap_or_else(|| {
        join_with_sep(&deps.home, &[".config"], false)
            .to_string_lossy()
            .into_owned()
    });
    join_with_sep(&base, &["duolane"], false)
}

pub fn resolve_state_dir(deps: &DirResolverEnv) -> PathBuf {
    if let Some(override_dir) = deps
        .env
        .get("DUOLANE_STATE_DIR")
        .filter(|dir| !dir.is_empty())
    {
        return Path::new(override_dir).to_path_buf();
    }

    if deps.platform == "windows" {
        let local = deps
            .env
            .get("LOCALAPPDATA")
            .cloned()
            .unwrap_or_else(|| format!("{}\\AppData\\Local", deps.home));
        return join_with_sep(&local, &["duolane"], true);
    }

    let base = deps.env.get("XDG_STATE_HOME").cloned().unwrap_or_else(|| {
        join_with_sep(&deps.home, &[".local", "state"], false)
            .to_string_lossy()
            .into_owned()
    });
    join_with_sep(&base, &["duolane"], false)
}

pub fn config_dir() -> PathBuf {
    resolve_config_dir(&DirResolverEnv::default())
}

pub fn state_dir() -> PathBuf {
    resolve_state_dir(&DirResolverEnv::default())
}

pub fn log_file() -> PathBuf {
    resolve_state_dir(&DirResolverEnv::default()).join("duolane.log")
}

fn join_with_sep(base: &str, parts: &[&str], win32: bool) -> PathBuf {
    let sep = '/';
    let _ = win32;
    let mut out = String::new();
    for part in std::iter::once(base).chain(parts.iter().copied()) {
        if !out.is_empty() && !out.ends_with(sep) {
            out.push(sep);
        }
        out.push_str(part);
    }
    Path::new(&out).to_path_buf()
}

pub fn resolve_config_dir_for_env(
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> PathBuf {
    resolve_config_dir(&DirResolverEnv {
        platform: platform.to_string(),
        env: env.clone(),
        home: home.to_string(),
    })
}

pub fn resolve_state_dir_for_env(
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> PathBuf {
    resolve_state_dir(&DirResolverEnv {
        platform: platform.to_string(),
        env: env.clone(),
        home: home.to_string(),
    })
}

pub fn claude_config_dir() -> anyhow::Result<std::path::PathBuf> {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|home| !home.is_empty())
            .map(|home| std::path::PathBuf::from(home).join(".claude"))
            .ok_or_else(|| {
                anyhow::anyhow!("cannot find the home directory; set CLAUDE_CONFIG_DIR")
            })?,
    };
    Ok(std::path::absolute(dir)?)
}
