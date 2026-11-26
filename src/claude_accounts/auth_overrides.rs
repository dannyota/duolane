use anyhow::{Result, ensure};
pub(super) const AUTH_OVERRIDES: [&str; 7] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "ANTHROPIC_PROFILE",
];
pub(super) const AUTH_OVERRIDE_PAIR: [&str; 2] =
    ["ANTHROPIC_FEDERATION_RULE_ID", "ANTHROPIC_ORGANIZATION_ID"];

pub(super) fn reject_auth_overrides() -> Result<()> {
    let nonempty = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    for name in AUTH_OVERRIDES {
        ensure!(!nonempty(name), "authentication override set: {name}");
    }
    ensure!(
        !AUTH_OVERRIDE_PAIR.into_iter().all(nonempty),
        "authentication overrides set: ANTHROPIC_FEDERATION_RULE_ID and ANTHROPIC_ORGANIZATION_ID"
    );
    Ok(())
}
