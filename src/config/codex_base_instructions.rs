pub fn codex_base_instructions() -> bool {
    let env: std::collections::HashMap<_, _> = crate::paths::environment_snapshot();
    if let Some(raw) = env.get("DUOLANE_CODEX_BASE_INSTRUCTIONS") {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return true,
            "0" | "false" | "no" | "off" => return false,
            _ => {}
        }
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
