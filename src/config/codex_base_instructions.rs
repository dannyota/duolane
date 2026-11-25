pub fn codex_base_instructions() -> bool {
    let env: std::collections::HashMap<_, _> = crate::paths::environment_snapshot();
    if let Some(enabled) = env
        .get("DUOLANE_CODEX_BASE_INSTRUCTIONS")
        .and_then(|raw| parse_bool(raw))
    {
        return enabled;
    }
    if let Some(enabled) = file_value(&crate::paths::config_dir()) {
        return enabled;
    }
    true
}

pub(super) fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub(super) fn file_value(config_dir: &std::path::Path) -> Option<bool> {
    let raw = std::fs::read_to_string(config_dir.join("config.json")).ok()?;
    serde_json::from_str::<super::FileConfig>(&raw).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.get("codex")?.get("baseInstructions")?.as_bool()
}

pub(super) fn env_source(env: &std::collections::HashMap<String, String>) -> Option<String> {
    env.get("DUOLANE_CODEX_BASE_INSTRUCTIONS")
        .and_then(|raw| parse_bool(raw))
        .map(|_| "DUOLANE_CODEX_BASE_INSTRUCTIONS (env)".to_string())
}
