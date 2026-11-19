use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::IgnoredAny;

use crate::providers::codex::translate::model_allowlist::ALLOWED_MODELS;
use crate::providers::codex::translate::model_catalog::{self, CatalogModel, valid_slug};

use super::settings;

pub(super) const KEY: &str = "modelPicker";
const SETTINGS_FLAG: &str = "--settings";
const MAX_ROWS: usize = 20;
const MAX_BYTES: usize = 16 * 1024;
const MAX_LABEL_CHARS: usize = 64;

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Row {
    model: String,
    label: String,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(super) struct Lineup {
    options: Vec<Row>,
}

#[derive(Serialize)]
struct Settings<'a> {
    #[serde(rename = "modelPicker")]
    model_picker: &'a Lineup,
}

impl Lineup {
    fn settings(&self) -> serde_json::Result<String> {
        serde_json::to_string(&Settings { model_picker: self })
    }
}

pub(super) fn lineup() -> Option<Lineup> {
    build(&model_catalog::catalog_models())
}

fn build(catalog: &[CatalogModel]) -> Option<Lineup> {
    let mut options: Vec<Row> = if catalog.is_empty() {
        ALLOWED_MODELS
            .iter()
            .rev()
            .filter(|id| valid_slug(id))
            .map(|id| row(id, None))
            .collect()
    } else {
        let mut listed: Vec<&CatalogModel> = catalog
            .iter()
            .filter(|model| {
                model.listed
                    && valid_slug(&model.slug)
                    && (model.supported_in_api || ALLOWED_MODELS.contains(&model.slug.as_str()))
            })
            .collect();
        listed.sort_by_key(|model| (model.priority.is_none(), model.priority));
        listed
            .into_iter()
            .map(|model| row(&model.slug, model.display_name.as_deref()))
            .collect()
    };
    options.truncate(MAX_ROWS);
    let lineup = Lineup { options };
    let size = lineup.settings().ok()?.len();
    (!lineup.options.is_empty() && size <= MAX_BYTES).then_some(lineup)
}

fn row(id: &str, display_name: Option<&str>) -> Row {
    let name = display_name
        .filter(|name| {
            !name.trim().is_empty()
                && name.len() <= MAX_LABEL_CHARS
                && name
                    .bytes()
                    .all(|byte| byte == b' ' || byte.is_ascii_graphic())
        })
        .unwrap_or(id);
    Row {
        model: id.to_string(),
        label: format!("{name} (Codex)"),
    }
}

pub(super) fn arguments(owner: &[OsString]) -> Vec<OsString> {
    match offered(owner) {
        Ok(settings) => vec![OsString::from(SETTINGS_FLAG), OsString::from(settings)],
        Err(reason) => {
            eprintln!("note: not adding the Codex model lineup: {reason}");
            Vec::new()
        }
    }
}

fn offered(owner: &[OsString]) -> Result<String, String> {
    if !cfg!(unix) {
        return Err("this platform is not Unix".to_string());
    }
    if owner
        .first()
        .is_some_and(|first| !first.as_encoded_bytes().starts_with(b"-"))
    {
        return Err("the first argument is not an option".to_string());
    }
    let passed = owner
        .iter()
        .take_while(|argument| argument.as_os_str() != "--")
        .any(|argument| {
            argument.as_os_str() == SETTINGS_FLAG
                || argument
                    .as_encoded_bytes()
                    .starts_with(format!("{SETTINGS_FLAG}=").as_bytes())
        });
    if passed {
        return Err(format!("the arguments pass {SETTINGS_FLAG}"));
    }
    let user = settings::settings_path().map_err(|error| error.to_string())?;
    for path in std::iter::once(user).chain(settings::MANAGED_SETTINGS.map(PathBuf::from)) {
        if sets_picker(&path)? {
            return Err(format!("{} sets {KEY}", path.display()));
        }
    }
    lineup()
        .ok_or_else(|| "there is no Codex model lineup".to_string())?
        .settings()
        .map_err(|error| error.to_string())
}

fn sets_picker(path: &Path) -> Result<bool, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(format!("cannot read {}", path.display())),
    };
    serde_json::from_slice::<HashMap<String, IgnoredAny>>(&bytes)
        .map(|top| top.contains_key(KEY))
        .map_err(|_| format!("{} is not a JSON object", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(slug: &str, priority: i64, listed: bool, name: Option<&str>) -> CatalogModel {
        CatalogModel {
            slug: slug.to_string(),
            use_responses_lite: false,
            supported_in_api: true,
            listed,
            priority: Some(priority),
            display_name: name.map(str::to_string),
        }
    }

    #[test]
    fn lineup_follows_priority_and_distrusts_the_catalog() {
        let long = "x".repeat(MAX_LABEL_CHARS + 1);
        let mut catalog = vec![
            model("gpt-c", 3, true, Some("Gamma")),
            model("gpt-a", 1, true, Some("Alpha\u{202E}\u{200B}")),
            model("gpt-hidden", 0, false, Some("Hidden")),
            model("gpt-a-fast", 0, true, Some("Fast")),
            model("gpt-b", 2, true, Some(&long)),
        ];
        catalog.extend((0..MAX_ROWS).map(|index| model(&format!("gpt-x{index}"), 10, true, None)));
        let options = build(&catalog).unwrap().options;
        assert_eq!(options.len(), MAX_ROWS);
        let labels: Vec<(&str, &str)> = options
            .iter()
            .take(3)
            .map(|row| (row.model.as_str(), row.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            [
                ("gpt-a", "gpt-a (Codex)"),
                ("gpt-b", "gpt-b (Codex)"),
                ("gpt-c", "Gamma (Codex)")
            ]
        );
        assert!(
            options
                .iter()
                .all(|row| row.model != "gpt-hidden" && row.model != "gpt-a-fast")
        );

        let fallback = build(&[]).unwrap().options;
        assert_eq!(fallback.len(), ALLOWED_MODELS.len());
        let newest = ALLOWED_MODELS[ALLOWED_MODELS.len() - 1];
        assert_eq!(fallback[0].model, newest);
        assert_eq!(fallback[0].label, format!("{newest} (Codex)"));
    }
}
