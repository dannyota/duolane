use crate::{provider::CliHandlers, registry::Registry};

use std::sync::{Arc, Mutex};

use super::{
    auth::token_store::file_store_at,
    client::CodexHttpClient,
    profiles::{DEFAULT_PROFILE, Profiles, SELECTION_ERROR},
    translate::model_catalog::{REQUEST_CATALOG, RequestCatalog},
};

pub fn list_profiles() -> anyhow::Result<()> {
    let profiles = Profiles::load()?;
    let active = profiles.active();
    for name in profiles.names() {
        if active.as_deref().is_ok_and(|active| name == active) {
            println!("{name} (active)");
        } else {
            println!("{name}");
        }
    }
    active.map(|_| ())
}

pub fn use_profile(name: &str) -> anyhow::Result<()> {
    Profiles::load()?.select(name)?;
    println!("Active Codex profile: {name}");
    Ok(())
}

pub fn list_models(registry: &Registry) -> anyhow::Result<String> {
    let profiles = Profiles::load()?;
    let active = profiles.active()?;
    let auth_path = profiles
        .auth_path(&active)
        .ok_or_else(|| anyhow::anyhow!(SELECTION_ERROR))?;
    let catalog = RequestCatalog::load(
        &auth_path.with_file_name("models_cache.json"),
        &Mutex::default(),
        profiles.has_named_profiles(),
    );
    let scope = Profiles::account_scope(&active, &auth_path);
    let client = Arc::new(CodexHttpClient::with_store(file_store_at(auth_path), scope));
    let registry = registry.with_codex_client(client);
    Ok(REQUEST_CATALOG.sync_scope(catalog, || {
        if let Some(hint) = registry.signed_out().get("codex") {
            return format!("codex: not signed in, {hint}");
        }
        let models = registry.supported_models_for("codex").join(", ");
        format!("codex: {}", crate::text::terminal_text(&models))
    }))
}

pub(crate) struct CodexCli;

impl CliHandlers for CodexCli {
    fn login(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!(
            "Codex auth is provided by the Codex CLI. Run `codex login` in the selected Codex home."
        )
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!(
            "Codex auth is provided by the Codex CLI. Run `codex login` in the selected Codex home."
        )
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        let profiles = Profiles::load()?;
        let active = profiles.active()?;
        let path = profiles
            .auth_path(&active)
            .ok_or_else(|| anyhow::anyhow!(SELECTION_ERROR))?;
        if profiles.has_named_profiles() || active != DEFAULT_PROFILE {
            println!("Profile: {active}");
        }
        let store = file_store_at(path);
        let stored = store.load_profile_auth(active != DEFAULT_PROFILE)?;
        let path = crate::text::terminal_text(&store.auth_path());
        match stored {
            Some(auth) => {
                println!(
                    "Account: {}",
                    crate::text::terminal_text(auth.account_id.as_deref().unwrap_or("(none)"))
                );
                println!("{}", format_expiry(auth.expires, now_ms()));
                println!("Storage: {path} (Codex CLI)");
                Ok(())
            }
            None => anyhow::bail!(
                "No Codex credentials. Run `codex login` in the selected Codex home to create {path}"
            ),
        }
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        let profiles = Profiles::load()?;
        let selected = profiles.active()?;
        let auth_path = profiles
            .auth_path(&selected)
            .ok_or_else(|| anyhow::anyhow!(SELECTION_ERROR))?;
        let auth_path = crate::text::terminal_text(&auth_path.to_string_lossy());
        println!(
            "Codex profile {selected} uses {auth_path}. Run `codex logout` in that Codex home to remove its credentials."
        );
        Ok(())
    }
}

pub(crate) static CODEX_CLI: CodexCli = CodexCli;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn format_expiry(expires: u64, now: u64) -> String {
    let remaining = (i128::from(expires) - i128::from(now)).div_euclid(1000);
    let iso = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(expires) * 1_000_000)
        .ok()
        .and_then(|dt| {
            let fmt = time::format_description::parse_borrowed::<2>(
                "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
            )
            .ok()?;
            dt.format(&fmt).ok()
        })
        .unwrap_or_else(|| "invalid".to_string());
    format!("Expires: {iso} (in {remaining}s)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_expiry_with_future_expiry() {
        let expires = 4102444800000;
        let now = 4102444790000;
        let output = format_expiry(expires, now);
        assert!(output.starts_with("Expires: 2100-01-01T00:00:00.000Z (in "));
        assert!(output.ends_with("s)"));
    }

    #[test]
    fn format_expiry_with_past_expiry() {
        let expires = 946684800000;
        let now = 946684810000;
        let output = format_expiry(expires, now);
        assert!(output.starts_with("Expires: 2000-01-01T00:00:00.000Z (in -"));
    }
}
