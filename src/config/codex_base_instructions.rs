pub fn codex_base_instructions() -> bool {
    let env: std::collections::HashMap<_, _> = crate::paths::environment_snapshot();
    if let Some(enabled) = env
        .get("DUOLANE_CODEX_BASE_INSTRUCTIONS")
        .and_then(|raw| parse_bool(raw))
    {
        return enabled;
    }
    let config_dir = crate::paths::config_dir();
    if let Some(file) = super::read_file_config(&config_dir)
        && let Some(codex) = file.codex
        && let Some(enabled) = codex.base_instructions
    {
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

pub(super) fn deserialize_bool<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<bool>, D::Error> {
    let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
    Ok(value.as_bool())
}

pub(super) fn env_source(env: &std::collections::HashMap<String, String>) -> Option<String> {
    env.get("DUOLANE_CODEX_BASE_INSTRUCTIONS")
        .and_then(|raw| parse_bool(raw))
        .map(|_| "DUOLANE_CODEX_BASE_INSTRUCTIONS (env)".to_string())
}
