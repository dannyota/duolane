use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

use serde::Serialize;
use serde::de::IgnoredAny;

use crate::providers::codex::translate::model_allowlist::{
    ALLOWED_MODELS, FAMILY_ALIASES, resolve_family_in, split_tier_suffix_with,
};
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
    let override_model = crate::config::codex_model();
    build(&model_catalog::catalog_models(), override_model.as_deref())
}

fn build(catalog: &[CatalogModel], override_model: Option<&str>) -> Option<Lineup> {
    let is_allowed = |model: &str| {
        ALLOWED_MODELS.contains(&model)
            || catalog
                .iter()
                .any(|entry| entry.slug == model && entry.supported_in_api)
    };
    let override_known = override_model
        .filter(|model| !model.is_empty())
        .is_none_or(|model| {
            let base = split_tier_suffix_with(model, is_allowed).map_or(model, |(base, _)| base);
            known_context(base) && is_allowed(base)
        });
    let mut options: Vec<Row> = if catalog.is_empty() {
        ALLOWED_MODELS
            .iter()
            .rev()
            .filter(|id| valid_slug(id))
            .map(|id| row(id, None, override_known && known_context(id)))
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
            .map(|model| {
                row(
                    &model.slug,
                    model.display_name.as_deref(),
                    override_known && known_context(&model.slug),
                )
            })
            .collect()
    };
    for (i, alias) in FAMILY_ALIASES.iter().enumerate() {
        let name = match *alias {
            "terra" => "Terra",
            "sol" => "Sol",
            "luna" => "Luna",
            _ => "Astra",
        };
        let known = resolve_family_in(alias, catalog)
            .is_some_and(|resolved| known_context(&resolved.model));
        options.insert(i, row(alias, Some(name), override_known && known));
    }
    options.truncate(MAX_ROWS);
    let lineup = Lineup { options };
    let size = lineup.settings().ok()?.len();
    (!lineup.options.is_empty() && size <= MAX_BYTES).then_some(lineup)
}

fn known_context(id: &str) -> bool {
    matches!(
        id,
        "gpt-5.4"
            | "gpt-5.5"
            | "gpt-5.6-luna"
            | "gpt-5.6-sol"
            | "gpt-5.6-terra"
            | "gpt-6-astra"
            | "gpt-6-sol"
            | "gpt-6.1-sol"
            | "gpt-6.1-terra"
            | "gpt-6-luna"
            | "gpt-6.2-astra"
            | "gpt-6.2-sol"
    )
}

fn row(id: &str, display_name: Option<&str>, one_million: bool) -> Row {
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
        model: if one_million {
            format!("{id}[1m]")
        } else {
            id.to_string()
        },
        label: format!("{name} (Codex)"),
    }
}

pub(super) fn arguments(
    owner: &[OsString],
    profile: Option<&crate::claude_accounts::Profile>,
    managed: Option<&Path>,
) -> Vec<OsString> {
    match offered(owner, profile, managed) {
        Ok(settings) => vec![OsString::from(SETTINGS_FLAG), OsString::from(settings)],
        Err(reason) => {
            eprintln!("note: not adding the Codex model lineup: {reason}");
            Vec::new()
        }
    }
}

fn offered(
    owner: &[OsString],
    profile: Option<&crate::claude_accounts::Profile>,
    managed: Option<&Path>,
) -> Result<String, String> {
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
    if let Some(profile) = profile {
        let bytes = profile
            .settings()
            .map_err(|_| "cannot safely read named profile settings".to_string())?;
        if let Some(bytes) = bytes {
            let top = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes)
                .map_err(|_| "named profile settings are not a JSON object".to_string())?;
            if top.contains_key(KEY) {
                return Err(format!("named profile settings set {KEY}"));
            }
        }
    } else {
        let user = settings::settings_path().map_err(|error| error.to_string())?;
        if sets_picker(&user)? {
            return Err(format!("{} sets {KEY}", user.display()));
        }
    }
    if let Some(path) = managed
        && sets_picker(path)?
    {
        return Err(format!("{} sets {KEY}", path.display()));
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

    #[cfg(unix)]
    #[test]
    #[ignore]
    fn intercept_settings_precedence_probe() {
        unsafe {
            std::env::set_var(
                "CLAUDE_CONFIG_DIR",
                std::env::var_os("TEST_AMBIENT").unwrap(),
            );
        }
        let root = std::env::var_os("TEST_PROFILE").unwrap();
        let profile = crate::claude_accounts::Profile::test_profile(Path::new(&root));
        let managed = Path::new(&root).join("managed.json");
        assert!(offered(&[], None, Some(&managed)).is_err());
        assert!(offered(&[], Some(&profile), Some(&managed)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn intercept_named_settings_replace_ambient_settings() {
        let root = tempfile::tempdir().unwrap();
        let ambient = root.path().join("ambient");
        let profile = root.path().join("profile");
        std::fs::create_dir(&ambient).unwrap();
        std::fs::create_dir(&profile).unwrap();
        std::fs::write(ambient.join("settings.json"), br#"{"modelPicker":{}}"#).unwrap();
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .stdin(std::process::Stdio::null())
                .env("TEST_AMBIENT", ambient)
                .env("TEST_PROFILE", profile)
                .args([
                    "--exact",
                    "--ignored",
                    "intercept::picker::tests::intercept_settings_precedence_probe"
                ])
                .status()
                .unwrap()
                .success()
        );
    }

    #[cfg(unix)]
    #[test]
    fn intercept_named_settings_are_bounded_and_do_not_follow_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let profile = crate::claude_accounts::Profile::test_profile(root.path());
        let settings = root.path().join("settings.json");
        let managed = root.path().join("managed.json");
        std::fs::write(&settings, br#"{"modelPicker":{}}"#).unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            offered(&[], Some(&profile), Some(&managed)).unwrap_err(),
            "named profile settings set modelPicker"
        );
        std::fs::write(&settings, b"{}").unwrap();
        assert!(offered(&[], Some(&profile), Some(&managed)).is_ok());
        std::fs::write(&managed, br#"{"modelPicker":{}}"#).unwrap();
        assert_eq!(
            offered(&[], Some(&profile), Some(&managed)).unwrap_err(),
            format!("{} sets modelPicker", managed.display())
        );
        std::fs::remove_file(&managed).unwrap();
        std::fs::write(&settings, b"{\"other\":\"\xff\"}").unwrap();
        assert_eq!(
            offered(&[], Some(&profile), Some(&managed)).unwrap_err(),
            "named profile settings are not a JSON object"
        );
        std::fs::write(&settings, vec![b' '; 1_048_577]).unwrap();
        assert_eq!(
            offered(&[], Some(&profile), Some(&managed)).unwrap_err(),
            "cannot safely read named profile settings"
        );
        std::fs::remove_file(&settings).unwrap();
        symlink("elsewhere", &settings).unwrap();
        assert_eq!(
            offered(&[], Some(&profile), Some(&managed)).unwrap_err(),
            "cannot safely read named profile settings"
        );
    }

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
        let options = build(&catalog, None).unwrap().options;
        assert_eq!(options.len(), MAX_ROWS);
        let labels: Vec<(&str, &str)> = options
            .iter()
            .take(7)
            .map(|row| (row.model.as_str(), row.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            [
                ("terra[1m]", "Terra (Codex)"),
                ("sol[1m]", "Sol (Codex)"),
                ("astra[1m]", "Astra (Codex)"),
                ("luna[1m]", "Luna (Codex)"),
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

        let fallback = build(&[], None).unwrap().options;
        assert_eq!(fallback.len(), ALLOWED_MODELS.len() + FAMILY_ALIASES.len());
        let newest = ALLOWED_MODELS[ALLOWED_MODELS.len() - 1];
        assert_eq!(fallback[4].model, format!("{newest}[1m]"));
        assert_eq!(fallback[4].label, format!("{newest} (Codex)"));
    }
    #[test]
    fn unknown_newest_family_does_not_advertise_older_context() {
        let options = build(
            &[
                model("gpt-6.2-sol", 1, true, None),
                model("gpt-6.99-sol", 2, true, None),
            ],
            None,
        )
        .unwrap()
        .options;
        assert_eq!(options[1].model, "sol");
        assert_eq!(options[4].model, "gpt-6.2-sol[1m]");
        assert_eq!(options[5].model, "gpt-6.99-sol");
    }

    #[test]
    fn override_must_also_have_known_context() {
        let catalog = [
            model("gpt-6.2-sol", 1, true, None),
            model("gpt-6.99-sol", 2, true, None),
        ];
        let smaller = build(&catalog, Some("gpt-5.4-mini")).unwrap();
        assert_eq!(smaller.options[4].model, "gpt-6.2-sol");
        let large = build(&catalog, Some("gpt-5.4-fast")).unwrap();
        assert_eq!(large.options[4].model, "gpt-6.2-sol[1m]");
        assert_eq!(large.options[5].model, "gpt-6.99-sol");
    }
    #[test]
    fn catalog_override_requires_supported_entry() {
        let row = model("gpt-6.1-sol", 1, true, None);
        let mut override_entry = model("gpt-6.2-astra", 2, false, None);
        override_entry.supported_in_api = false;
        for catalog in [vec![row.clone()], vec![row.clone(), override_entry.clone()]] {
            let lineup = build(&catalog, Some("gpt-6.2-astra")).unwrap();
            assert_eq!(lineup.options[4].model, "gpt-6.1-sol");
        }
        override_entry.supported_in_api = true;
        for name in ["gpt-6.2-astra", "gpt-6.2-astra-fast"] {
            let lineup = build(&[row.clone(), override_entry.clone()], Some(name)).unwrap();
            assert_eq!(lineup.options[4].model, "gpt-6.1-sol[1m]");
        }
    }
}
