# Security

The [security design](../docs/design/security.md) owns the requirements. These rules govern the work.

## Credentials

- Never read credential values into a conversation, log, fixture, or report.
- Use toy tokens in tests. Check them by comparison, not by printing.
- The Claude login belongs to Claude Code and stays in transit. The Codex login belongs to the Codex CLI.

## Dependencies

- Stay on the latest stable Rust, crates, tools, and GitHub Actions. Move major versions one at a time, each with the full test suite.
- Before adding a crate, check its latest version, license, maintenance, and transitive dependencies, and record the reason in the owning design or plan.
- `cargo deny check advisories` findings block merge once the plan's lockfile slice lands, and the full `cargo deny check` blocks merge once `deny.toml` exists. Suppress only a verified false positive, with its reason.

## Workflows

- Pin every action by full commit SHA with a version comment.
- Give each job the least `permissions` it needs. Only the job that publishes a release gets `contents: write`.
- Never interpolate untrusted event data into a shell command.

## Public repository

Commits, pull requests, and CI logs are public. Report a suspected vulnerability in shipped code privately, as [SECURITY.md](../SECURITY.md) says, and keep exploit steps out of public text.
