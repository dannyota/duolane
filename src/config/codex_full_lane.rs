pub fn codex_full_lane() -> Option<bool> {
    let env: std::collections::HashMap<_, _> = crate::paths::environment_snapshot();
    if let Some(enabled) = env
        .get("DUOLANE_CODEX_FULL_LANE")
        .and_then(|raw| super::codex_base_instructions::parse_bool(raw))
    {
        return Some(enabled);
    }
    super::read_file_config(&crate::paths::config_dir())?
        .codex?
        .full_lane
}
