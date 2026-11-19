use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::files::{self, MANIFEST_FILE, OWNED_FILES};

const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Manifest {
    version: u32,
    #[serde(rename = "settingsFile")]
    pub(super) settings_file: PathBuf,
    pub(super) settings: Vec<Entry>,
    pub(super) files: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Entry {
    pub(super) name: String,
    pub(super) sha256: String,
}

impl Entry {
    pub(super) fn new(name: &str, value: &[u8]) -> Self {
        Self {
            name: name.to_string(),
            sha256: sha256(value),
        }
    }
}

impl Manifest {
    pub(super) fn new(settings_file: PathBuf, settings: Vec<Entry>, files: Vec<Entry>) -> Self {
        Self {
            version: VERSION,
            settings_file,
            settings,
            files,
        }
    }
}

pub(super) fn sha256(value: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, value)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn load(dir: &Path) -> anyhow::Result<Option<Manifest>> {
    let Some(bytes) = files::read(&dir.join(MANIFEST_FILE))? else {
        return Ok(None);
    };
    match serde_json::from_slice::<Manifest>(&bytes) {
        Ok(manifest) if manifest.version == VERSION => Ok(Some(manifest)),
        _ => anyhow::bail!("{MANIFEST_FILE} in {} is not valid", dir.display()),
    }
}

pub(super) fn save(dir: &Path, manifest: &Manifest) -> anyhow::Result<bool> {
    let path = dir.join(MANIFEST_FILE);
    let bytes = serde_json::to_vec_pretty(manifest)?;
    if files::read(&path)?.is_some_and(|existing| existing == bytes) {
        return Ok(false);
    }
    files::write(&path, &bytes)?;
    Ok(true)
}

pub(super) fn file_entries(dir: &Path) -> anyhow::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for name in OWNED_FILES {
        if let Some(bytes) = files::read(&dir.join(name))? {
            entries.push(Entry::new(name, &bytes));
        }
    }
    Ok(entries)
}

pub(super) fn refresh_files(dir: &Path) -> anyhow::Result<bool> {
    let Ok(Some(mut manifest)) = load(dir) else {
        return Ok(false);
    };
    let entries = file_entries(dir)?;
    if manifest.files == entries {
        return Ok(false);
    }
    manifest.files = entries;
    save(dir, &manifest)
}
