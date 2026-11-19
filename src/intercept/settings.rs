use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::de::{Error as _, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use time::OffsetDateTime;

use super::files::{self, MANIFEST_FILE, OWNED_FILES};
use super::manifest::{self, Entry, Manifest};
use super::wrapper::{
    BASE_URL_VARIABLE, CA_VARIABLE, GZIP_VARIABLE, HTTPS_PROXY, Install, LOOPBACK_NO_PROXY,
    NO_PROXY, same_file, timeouts, variable,
};
use super::{PROXY_USER, lock};

const ENV: &str = "env";
const SETTINGS_FILE: &str = "settings.json";
const DEFAULT_INDENT: &[u8] = b"  ";
const PROJECT_FILES: [&str; 2] = [".claude/settings.json", ".claude/settings.local.json"];
#[cfg(target_os = "macos")]
const MANAGED_SETTINGS: Option<&str> =
    Some("/Library/Application Support/ClaudeCode/managed-settings.json");
#[cfg(target_os = "linux")]
const MANAGED_SETTINGS: Option<&str> = Some("/etc/claude-code/managed-settings.json");
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const MANAGED_SETTINGS: Option<&str> = None;

struct Ordered<V>(Vec<(String, V)>);

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Ordered<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Entries<V>(PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for Entries<V> {
            type Value = Ordered<V>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered<V>, A::Error> {
                let mut seen = HashSet::new();
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, V>()? {
                    if !seen.insert(key.clone()) {
                        return Err(A::Error::custom("duplicate key"));
                    }
                    entries.push((key, value));
                }
                Ok(Ordered(entries))
            }
        }
        deserializer.deserialize_map(Entries(PhantomData))
    }
}

impl<V: Serialize> Serialize for Ordered<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

enum Slot {
    Raw(Box<RawValue>),
    Env(Ordered<Box<RawValue>>),
}

impl Serialize for Slot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Raw(value) => value.serialize(serializer),
            Self::Env(env) => env.serialize(serializer),
        }
    }
}

struct Document {
    entries: Ordered<Slot>,
    newline: bool,
    indent: Vec<u8>,
}

impl Document {
    fn parse(bytes: Option<&[u8]>, path: &Path) -> anyhow::Result<Self> {
        let Some(bytes) = bytes else {
            return Ok(Self {
                entries: Ordered(Vec::new()),
                newline: true,
                indent: DEFAULT_INDENT.to_vec(),
            });
        };
        let top: Ordered<Box<RawValue>> = serde_json::from_slice(bytes).map_err(|error| {
            anyhow::anyhow!(
                "{} is not a JSON object (line {}, column {})",
                path.display(),
                error.line(),
                error.column()
            )
        })?;
        let mut entries = Vec::with_capacity(top.0.len());
        for (key, value) in top.0 {
            let slot = if key == ENV {
                Slot::Env(serde_json::from_str(value.get()).map_err(|_| {
                    anyhow::anyhow!("{ENV} in {} is not a JSON object", path.display())
                })?)
            } else {
                Slot::Raw(value)
            };
            entries.push((key, slot));
        }
        Ok(Self {
            entries: Ordered(entries),
            newline: bytes.ends_with(b"\n"),
            indent: indentation(bytes),
        })
    }

    fn env(&self) -> Option<&Vec<(String, Box<RawValue>)>> {
        self.entries.0.iter().find_map(|(key, slot)| match slot {
            Slot::Env(env) if key == ENV => Some(&env.0),
            _ => None,
        })
    }

    fn pairs(&self) -> Vec<(String, String)> {
        self.env().map_or_else(Vec::new, |env| {
            env.iter()
                .map(|(name, value)| (name.clone(), text(value)))
                .collect()
        })
    }

    fn get(&self, name: &str) -> Option<String> {
        self.env()?
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| text(value))
    }

    fn set(&mut self, name: &str, value: &str) -> anyhow::Result<()> {
        let value = RawValue::from_string(serde_json::to_string(value)?)?;
        for (key, slot) in &mut self.entries.0 {
            if let Slot::Env(env) = slot
                && key == ENV
            {
                match env.0.iter_mut().find(|(key, _)| key == name) {
                    Some((_, existing)) => *existing = value,
                    None => env.0.push((name.to_string(), value)),
                }
                return Ok(());
            }
        }
        let env = Ordered(vec![(name.to_string(), value)]);
        self.entries.0.push((ENV.to_string(), Slot::Env(env)));
        Ok(())
    }

    fn remove(&mut self, names: &[String]) {
        for (key, slot) in &mut self.entries.0 {
            if let Slot::Env(env) = slot
                && key == ENV
            {
                env.0.retain(|(key, _)| !names.contains(key));
            }
        }
        self.entries.0.retain(|(key, slot)| {
            !(key == ENV && matches!(slot, Slot::Env(env) if env.0.is_empty()))
        });
    }

    fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(&self.indent);
        let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, formatter);
        self.entries.serialize(&mut serializer)?;
        if self.newline {
            bytes.push(b'\n');
        }
        Ok(bytes)
    }
}

fn indentation(bytes: &[u8]) -> Vec<u8> {
    bytes
        .split(|byte| *byte == b'\n')
        .find_map(|line| {
            let width = line
                .iter()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            let content = line.get(width..)?;
            let indented = width > 0 && !content.iter().all(u8::is_ascii_whitespace);
            line.get(..width).filter(|_| indented)
        })
        .unwrap_or(DEFAULT_INDENT)
        .to_vec()
}

fn text(value: &RawValue) -> String {
    serde_json::from_str::<String>(value.get()).unwrap_or_else(|_| value.get().to_string())
}

fn settings_path() -> anyhow::Result<PathBuf> {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(".claude"))
            .ok_or_else(|| {
                anyhow::anyhow!("cannot find the home directory; set CLAUDE_CONFIG_DIR")
            })?,
    };
    Ok(std::path::absolute(dir.join(SETTINGS_FILE))?)
}

fn read_settings(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => anyhow::bail!("cannot inspect {}: {error}", path.display()),
    };
    if metadata.file_type().is_symlink() {
        anyhow::bail!("refusing {}: it is a symlink", path.display());
    }
    if !metadata.is_file() {
        anyhow::bail!("refusing {}: it is not a regular file", path.display());
    }
    std::fs::read(path)
        .map(Some)
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))
}

fn replace(path: &Path, original: Option<&[u8]>, bytes: &[u8]) -> anyhow::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?
        .to_string_lossy();
    let staging = path.with_file_name(format!(".{name}.duolane-staging.{}", std::process::id()));
    files::write(&staging, bytes)?;
    let unchanged = read_settings(path).is_ok_and(|current| current.as_deref() == original);
    let replaced = if unchanged {
        std::fs::rename(&staging, path)
            .map_err(|error| anyhow::anyhow!("cannot replace {}: {error}", path.display()))
    } else {
        Err(anyhow::anyhow!(
            "{} changed while duolane was editing it; nothing was written, so run the command again",
            path.display()
        ))
    };
    if replaced.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    replaced
}

struct Place {
    name: String,
    pairs: Vec<(String, String)>,
}

fn places(user: &Document, user_path: &Path, names: &[&str]) -> anyhow::Result<Vec<Place>> {
    let shell = names
        .iter()
        .chain([&BASE_URL_VARIABLE])
        .filter_map(|name| variable(name).map(|value| (name.to_string(), value)))
        .collect();
    let mut places = vec![
        Place {
            name: "the environment".to_string(),
            pairs: shell,
        },
        Place {
            name: user_path.display().to_string(),
            pairs: user.pairs(),
        },
    ];
    let current = std::env::current_dir()?;
    let others = PROJECT_FILES
        .iter()
        .map(|file| current.join(file))
        .chain(MANAGED_SETTINGS.map(PathBuf::from));
    for path in others {
        match std::fs::read(&path) {
            Ok(bytes) => places.push(Place {
                name: path.display().to_string(),
                pairs: Document::parse(Some(&bytes), &path)?.pairs(),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => anyhow::bail!("cannot read {}: {error}", path.display()),
        }
    }
    Ok(places)
}

fn check_conflicts(wanted: &[(&str, String)], places: &[Place]) -> anyhow::Result<()> {
    for place in places {
        for (name, value) in &place.pairs {
            if value.trim().is_empty() {
                continue;
            }
            if name == BASE_URL_VARIABLE {
                anyhow::bail!(
                    "refusing setup: {} sets {BASE_URL_VARIABLE}, which sends Claude Code past the intercept proxy",
                    place.name
                );
            }
            let differs = wanted.iter().any(|(wanted, ours)| {
                wanted == name
                    && if name == CA_VARIABLE {
                        !same_file(value, Path::new(ours))
                    } else {
                        value != ours
                    }
            });
            if differs {
                anyhow::bail!(
                    "refusing setup: {name} already has another value in {}",
                    place.name
                );
            }
        }
    }
    Ok(())
}

fn finish(report: &[String], reminders: &[&str]) {
    if report.is_empty() {
        println!("Nothing changed.");
    }
    for line in report
        .iter()
        .map(String::as_str)
        .chain(reminders.iter().copied())
    {
        println!("{line}");
    }
}

pub fn setup() -> anyhow::Result<()> {
    let dir = super::intercept_dir();
    let _lock = lock::acquire(&dir, lock::WAIT)?;
    let path = settings_path()?;
    let previous = manifest::load(&dir).ok().flatten();
    if let Some(previous) = &previous
        && previous.settings_file != path
    {
        anyhow::bail!(
            "refusing setup: {MANIFEST_FILE} records intercept settings in {}; run `duolane intercept cleanup` with the same CLAUDE_CONFIG_DIR first",
            previous.settings_file.display()
        );
    }
    let before = manifest::file_entries(&dir)?;
    let (password, loaded) =
        super::prepare_locked(&dir, OffsetDateTime::now_utc(), Some(super::port()))?;
    let mut report = Vec::new();
    for entry in manifest::file_entries(&dir)? {
        match before.iter().find(|old| old.name == entry.name) {
            None => report.push(format!("created {}", entry.name)),
            Some(old) if old.sha256 != entry.sha256 => {
                report.push(format!("changed {}", entry.name));
            }
            Some(_) => {}
        }
    }
    let install = Install::new(password)?;
    let original = read_settings(&path)?;
    let mut document = Document::parse(original.as_deref(), &path)?;
    let recorded_before = |entry: &Entry| {
        previous
            .as_ref()
            .is_some_and(|previous| previous.settings.contains(entry))
    };
    let mut wanted = install.fixed();
    wanted.extend(NO_PROXY.map(|name| (name, LOOPBACK_NO_PROXY.to_string())));
    let mut kept = Vec::new();
    for (name, value) in timeouts() {
        match document.get(name).filter(|existing| !existing.is_empty()) {
            None => wanted.push((name, value)),
            Some(existing) => kept.push(Entry::new(name, existing.as_bytes())),
        }
    }
    let names: Vec<&str> = wanted.iter().map(|(name, _)| *name).collect();
    check_conflicts(&wanted, &places(&document, &path, &names)?)?;
    let mut settings = Vec::new();
    let mut changed = false;
    for (name, value) in &wanted {
        let entry = Entry::new(name, value.as_bytes());
        if document.get(name).as_deref() != Some(value.as_str()) {
            document.set(name, value)?;
            changed = true;
            report.push(format!("set {name}"));
            settings.push(entry);
        } else if recorded_before(&entry) || HTTPS_PROXY.contains(name) || *name == CA_VARIABLE {
            settings.push(entry);
        }
    }
    if changed {
        replace(&path, original.as_deref(), &document.to_bytes()?)?;
        report.push(format!("wrote {}", path.display()));
    }
    settings.extend(kept.into_iter().filter(|entry| recorded_before(entry)));
    let record = Manifest::new(path, settings, manifest::file_entries(&dir)?);
    if manifest::save(&dir, &record)? {
        report.push(format!("wrote {MANIFEST_FILE}"));
    }
    let mut reminders = vec![
        "Restart running Claude Code sessions so they read the new settings.",
        "Run `duolane serve --intercept` and keep it running: every Claude Code session now goes through it.",
    ];
    if loaded.replaced {
        reminders.push(
            "The intercept certificate was replaced: restart `duolane serve --intercept`, then restart Claude Code sessions.",
        );
    }
    finish(&report, &reminders);
    Ok(())
}

pub fn cleanup() -> anyhow::Result<()> {
    let dir = super::intercept_dir();
    let _lock = lock::acquire(&dir, lock::WAIT)?;
    let mut report = Vec::new();
    let recorded = manifest::load(&dir).unwrap_or_else(|_| {
        report.push(format!("ignored {MANIFEST_FILE}: it is not valid"));
        None
    });
    let edited = match &recorded {
        Some(recorded) => Ok(recorded.settings_file.clone()),
        None => settings_path(),
    }
    .and_then(|path| {
        clean_settings(&path, recorded.as_ref(), &dir, &mut report)
            .map_err(|error| settings_error(&path, &error))
    });
    for name in OWNED_FILES.into_iter().chain([MANIFEST_FILE]) {
        if files::remove(&dir.join(name))? {
            report.push(format!("removed {name}"));
        }
    }
    match std::fs::remove_dir(&dir) {
        Ok(()) => report.push(format!("removed {}", dir.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => report.push(format!("left {}: it holds other files", dir.display())),
    }
    finish(
        &report,
        &["Restart running Claude Code sessions so they stop using the intercept proxy."],
    );
    edited
}

fn settings_error(path: &Path, error: &anyhow::Error) -> anyhow::Error {
    let reason = format!("{error:#}");
    let shown = path.display().to_string();
    if reason.contains(&shown) {
        anyhow::anyhow!("could not remove the intercept settings: {reason}")
    } else {
        anyhow::anyhow!("could not remove the intercept settings from {shown}: {reason}")
    }
}

fn clean_settings(
    path: &Path,
    recorded: Option<&Manifest>,
    dir: &Path,
    report: &mut Vec<String>,
) -> anyhow::Result<()> {
    let Some(original) = read_settings(path)? else {
        return Ok(());
    };
    let mut document = Document::parse(Some(&original), path)?;
    let removals = match recorded {
        Some(recorded) => recorded_settings(&document, recorded, report),
        None => duolane_settings(&document, dir),
    };
    if !removals.is_empty() {
        document.remove(&removals);
        replace(path, Some(&original), &document.to_bytes()?)?;
        report.extend(removals.iter().map(|name| format!("removed {name}")));
        report.push(format!("wrote {}", path.display()));
    }
    Ok(())
}

fn recorded_settings(
    document: &Document,
    recorded: &Manifest,
    report: &mut Vec<String>,
) -> Vec<String> {
    let mut removals = Vec::new();
    for entry in &recorded.settings {
        match document.get(&entry.name) {
            Some(value) if manifest::sha256(value.as_bytes()) == entry.sha256 => {
                removals.push(entry.name.clone());
            }
            Some(_) => report.push(format!("left {}: it changed since setup", entry.name)),
            None => {}
        }
    }
    removals
}

fn duolane_settings(document: &Document, dir: &Path) -> Vec<String> {
    let dir = std::path::absolute(dir).ok();
    let pairs = document.pairs();
    let anchors: Vec<String> = pairs
        .iter()
        .filter(|(name, value)| {
            if HTTPS_PROXY.contains(&name.as_str()) {
                duolane_proxy(value)
            } else if name == CA_VARIABLE {
                std::path::absolute(value)
                    .ok()
                    .is_some_and(|path| path.parent().map(Path::to_path_buf) == dir)
            } else {
                false
            }
        })
        .map(|(name, _)| name.clone())
        .collect();
    if anchors.is_empty() {
        return anchors;
    }
    let mut companions = vec![(GZIP_VARIABLE, "0".to_string())];
    companions.extend(NO_PROXY.map(|name| (name, LOOPBACK_NO_PROXY.to_string())));
    companions.extend(timeouts());
    let companions = pairs
        .iter()
        .filter(|(name, value)| companions.iter().any(|(n, v)| n == name && v == value))
        .map(|(name, _)| name.clone());
    anchors.into_iter().chain(companions).collect()
}

fn duolane_proxy(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.username() == PROXY_USER
        && match url.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        }
}
