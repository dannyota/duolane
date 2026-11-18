# Security

The [security design](../docs/design/security.md) owns the requirements. These rules govern the work.

## Credentials

- Never read credential values into a conversation, log, fixture, or report.
- Use toy tokens in tests. Check them by comparison, not by printing.
- The Claude login belongs to Claude Code and stays in transit. The Codex login belongs to the Codex CLI.

## Dependencies

- Stay on the latest stable Rust, crates, tools, and GitHub Actions. Move major versions one at a time, each with the full test suite.
- Wait 72 hours after a crate version is published before adopting it, unless it fixes a vulnerability that affects this product. `make crate-age` enforces this on pull requests; exempt a vulnerability fix with a `name@version reason` line in the `CRATE_AGE_EXEMPT` repository variable.
- Before adding a crate, check its latest version, license, maintenance, and transitive dependencies, and record the reason in the owning design or plan.
- `cargo deny --locked check` findings block merge. Suppress only a verified false positive, with its reason.
- The high npm advisories reached through markdownlint-cli2, picomatch ReDoS (GHSA-fc5q-39xc-vjpf) and KaTeX (GHSA-238p-pmpm-9mq7), are accepted because the Markdown tools read only repository Markdown in a job with a read-only token. Revisit them when a fixed release exists. CI runs `npm ci` without `--no-audit` so they stay visible.

## Workflows

- Pin every action by full commit SHA with a version comment.
- Give each job the least `permissions` it needs. Only the job that publishes a release gets `contents: write`.
- Never interpolate untrusted event data into a shell command.

## Public repository

Commits, pull requests, and CI logs are public. Report a suspected vulnerability in shipped code privately, as [SECURITY.md](../SECURITY.md) says, and keep exploit steps out of public text.
