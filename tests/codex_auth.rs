#[path = "support/hermetic.rs"]
mod hermetic;

use assert_cmd::Command;
use tempfile::TempDir;

const ACCESS_EXP_2100: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjQxMDI0NDQ4MDB9.sig";
const ACCESS_EXP_2000: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjk0NjY4NDgwMH0.sig";
const ID_TOKEN_ACCT: &str =
    "eyJhbGciOiJub25lIn0.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2N0X2lkdG9rIn0.sig";

fn codex_cmd() -> (Command, TempDir, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let auth_path = temp.path().join(".codex").join("auth.json");
    let mut cmd = Command::cargo_bin("duolane").unwrap();
    cmd.args(["codex", "auth", "status"]);
    cmd.env("DUOLANE_CONFIG_DIR", temp.path());
    cmd.env("DUOLANE_CODEX_AUTH_FILE", &auth_path);
    (cmd, temp, auth_path)
}

fn write_auth(
    path: &std::path::Path,
    access: &str,
    account_id: Option<&str>,
    id_token: Option<&str>,
) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let account_field = account_id
        .map(|a| format!(r#","account_id":"{a}""#))
        .unwrap_or_default();
    let id_field = id_token
        .map(|t| format!(r#""id_token":"{t}","#))
        .unwrap_or_default();
    let body = format!(
        r#"{{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{{{id_field}"access_token":"{access}","refresh_token":"r"{account_field}}},"last_refresh":"2026-07-13T08:00:00Z"}}"#
    );
    std::fs::write(path, body).unwrap();
}

#[test]
fn codex_auth_status_reads_stored_auth() -> Result<(), Box<dyn std::error::Error>> {
    let (mut cmd, _temp, auth_path) = codex_cmd();
    write_auth(&auth_path, ACCESS_EXP_2100, Some("acct_1"), Some("id.t.s"));
    let output = cmd.assert().success().get_output().stdout.clone();
    let out = String::from_utf8(output)?;
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert_eq!(lines[0], "Account: acct_1");
    assert!(
        lines[1].starts_with("Expires: 2100-01-01T00:00:00.000Z (in "),
        "{out}"
    );
    assert!(lines[1].ends_with("s)"), "{out}");
    assert!(lines[2].starts_with("Storage: "), "{out}");
    assert!(lines[2].contains("(Codex CLI)"), "{out}");
    Ok(())
}

#[test]
fn codex_auth_status_falls_back_to_id_token_account_id() -> Result<(), Box<dyn std::error::Error>> {
    let (mut cmd, _temp, auth_path) = codex_cmd();
    write_auth(&auth_path, ACCESS_EXP_2100, None, Some(ID_TOKEN_ACCT));
    let output = cmd.assert().success().get_output().stdout.clone();
    let out = String::from_utf8(output)?;
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines[0], "Account: acct_idtok", "{out}");
    Ok(())
}

#[test]
fn codex_auth_status_no_auth() -> Result<(), Box<dyn std::error::Error>> {
    let (mut cmd, _temp, _auth_path) = codex_cmd();
    let output = cmd.output()?;
    assert_eq!(output.status.code(), Some(1));
    let out = String::from_utf8(output.stdout)?;
    assert!(out.contains("No Codex credentials"), "{out}");
    Ok(())
}

#[test]
fn codex_auth_status_shows_storage_path() -> Result<(), Box<dyn std::error::Error>> {
    let (mut cmd, _temp, auth_path) = codex_cmd();
    write_auth(&auth_path, ACCESS_EXP_2100, Some("acct_3"), None);
    let output = cmd.assert().success().get_output().stdout.clone();
    let out = String::from_utf8(output)?;
    assert!(out.contains("Storage:"), "{out}");
    assert!(out.contains("(Codex CLI)"), "{out}");
    assert!(!out.contains("Auth path:"), "{out}");
    Ok(())
}

#[test]
fn codex_auth_status_no_account_id_shows_none() -> Result<(), Box<dyn std::error::Error>> {
    let (mut cmd, _temp, auth_path) = codex_cmd();
    write_auth(&auth_path, ACCESS_EXP_2100, None, None);
    let output = cmd.output()?;
    let out = String::from_utf8(output.stdout)?;
    assert!(output.status.success(), "{out}");
    assert!(out.contains("Account: (none)"), "{out}");
    Ok(())
}

#[test]
fn codex_auth_status_expired_auth_shows_negative_seconds() -> Result<(), Box<dyn std::error::Error>>
{
    let (mut cmd, _temp, auth_path) = codex_cmd();
    write_auth(&auth_path, ACCESS_EXP_2000, Some("acct_1"), None);
    let output = cmd.assert().success().get_output().stdout.clone();
    let out = String::from_utf8(output)?;
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].starts_with("Account:"), "{out}");
    assert!(
        lines[1].starts_with("Expires: 2000-01-01T00:00:00.000Z (in -"),
        "{out}"
    );
    assert!(lines[2].starts_with("Storage: "), "{out}");
    Ok(())
}
