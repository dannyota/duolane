use crate::provider::CliHandlers;

use super::auth::token_store::file_store;

pub(crate) struct CodexCli;

impl CliHandlers for CodexCli {
    fn login(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!(
            "Codex auth is provided by the Codex CLI. Run `codex login` to create ~/.codex/auth.json; this proxy reads it directly."
        )
    }

    fn device(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!(
            "Codex auth is provided by the Codex CLI. Run `codex login` to create ~/.codex/auth.json; this proxy reads it directly."
        )
    }

    fn status(&self) -> Result<(), anyhow::Error> {
        let store = file_store();
        let stored = store.load_auth()?;
        match stored {
            Some(auth) => {
                println!(
                    "Account: {}",
                    crate::text::terminal_text(auth.account_id.as_deref().unwrap_or("(none)"))
                );
                println!("{}", format_expiry(auth.expires, now_ms()));
                println!("Storage: {} (Codex CLI)", store.auth_path());
                Ok(())
            }
            None => anyhow::bail!(
                "No Codex credentials. Run `codex login` to create {}",
                store.auth_path()
            ),
        }
    }

    fn logout(&self) -> Result<(), anyhow::Error> {
        println!(
            "Codex credentials are managed by the Codex CLI; run `codex logout` to remove them"
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
