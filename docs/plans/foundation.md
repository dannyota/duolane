# Foundation plan

**Goal:** Turn the inherited proxy into `duolane`: an independent, hardened, current Rust product with Claude and Codex backends, built and documented in the owner's style.

**Architecture:** Each slice is one pull request, or a short series, that leaves `main` green. Remove and rename first so later work touches less code. Tooling comes before upgrades, and documentation of comments comes before their removal.

**Tech stack:** Rust stable, axum, tokio, reqwest with rustls, ratatui, GitHub Actions, cargo-deny, gitleaks.

**Spec:** [Architecture](../design/architecture.md), [security](../design/security.md), and the [ADRs](../adr/README.md).

## Global constraints

- Follow `AGENTS.md`, `instructions/`, and the engineering standard.
- Every slice: failing test first where behavior changes, full gates, independent review for code, green CI, merge commit.
- Tests never touch the home directory or a live backend.
- Keep each slice to one purpose. Do not mix mechanical changes with behavior changes.
- Tags, releases, and crates.io publishing wait for the owner.

## Review focus

- Claude Code's real requests keep working after inbound checks: `Host: localhost:<port>` or `127.0.0.1:<port>`, no `Origin`, and a JSON content type.
- A conversation that moves Claude to Codex to Claude keeps its reasoning, and Anthropic receives no Codex signature or encrypted content.
- A body with no unsigned or duolane-signed thinking stays byte-identical on the Claude route.
- The Codex CLI and duolane running together do not lose a rotated refresh token.
- `--help`, `duolane serve`, `duolane monitor`, and `duolane codex status` keep working through renames, comment removal, and file splits.

## Slices

- [ ] **Foundation documents.** Rules, designs, ADRs, standards, license, security policy, and this plan.
- [ ] **Remove extra backends.** Delete Kimi, Grok, Cursor, their logins, commands, config, tests, docs, and any code and crates only they use. Accept: no reference to those names remains outside ADRs, and `/v1/models` lists only Claude and Codex models.
- [ ] **Rename to duolane.** Crate, binary, library, `DUOLANE_` environment variables, config and state directories, inbound token header, `originator`, and `User-Agent`. Rewrite the README with credits to both source projects, restart `CHANGELOG.md`, set version `0.1.0`, and repoint the installer. Accept: no inherited product name remains outside `LICENSE`, the README credits, ADRs, and history.
- [ ] **Tooling and CI.** `.tool-versions`, `rust-toolchain.toml`, `Makefile`, `.githooks`, `deny.toml`, `.gitleaks.toml`, length gate with a ratcheting baseline, workflow pin gate, tool freshness check, CodeQL, and Dependabot alerts. Pin actions by SHA, use least privilege in the release workflow, make every test hermetic, and remove Nix, `just`, `checkle`, and workmux files. Set the `main` ruleset and the Actions allowlist afterward.
- [ ] **Upgrade dependencies.** `cargo update` first. Then one pull request per breaking family: `reqwest`, `tokio-tungstenite` with `rustls`, `ratatui` with `crossterm`, `jiff`, and the small crates. Accept: `cargo deny check` passes with no unexplained exception.
- [ ] **Move comments into design.** Harvest decisions from comments into `docs/design/`. Replace `clap` doc comments with attributes. Remove comments with a Rust-aware tool, then enable a gate that rejects new ones. Accept: token streams match before and after, apart from the `clap` attributes, and `--help` output is unchanged.
- [ ] **Inbound hardening.** Implement the [inbound rules](../design/security.md#inbound-requests).
- [ ] **Reasoning portability.** Implement invariants 1 to 4 of the [architecture](../design/architecture.md#invariants), with a Codex to Claude to Codex test.
- [ ] **Limits and panic safety.** Implement the [limits](../design/security.md#limits), safe truncation, and the [terminal rule](../design/security.md#terminal-output).
- [ ] **Credential safety.** Implement the [credential rules](../design/security.md#credentials) for refresh locking, durable writes, redirects, base URL validation, and account-bound reuse.
- [ ] **Stream and content fidelity.** Implement invariants 5 and 6, fix transcript parsing, and close redaction gaps.
- [ ] **Structure.** Split oversized files by concern, one file per pull request, until the length baseline is empty.
- [ ] **Live verification.** With the owner's approval, run Claude and Codex through a real Claude Code session, including a mid-conversation switch and a token refresh. Record what passed before proposing the first release.
