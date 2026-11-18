# Foundation plan

**Goal:** Turn the inherited proxy into `duolane`: an independent, hardened, current Rust product with Claude and Codex backends, built and documented in the owner's style.

**Architecture:** Each slice is one pull request, or a short series, that leaves `main` green. Remove and rename first so later work touches less code. Harvest comments before other slices rewrite the code they explain.

**Tech stack:** Rust stable, axum, tokio, reqwest with rustls, ratatui, GitHub Actions, cargo-deny, gitleaks, Prettier, markdownlint.

**Spec:** [Architecture](../design/architecture.md), [security](../design/security.md), and the [ADRs](../adr/README.md).

## Global constraints

- Follow `AGENTS.md`, `instructions/`, and the engineering standard.
- Every slice: failing test first where behavior changes, full gates, independent review for code, green CI, merge commit.
- Every slice leaves the README and documents accurate and in the repository's style.
- Tests never touch the home directory or a live backend.
- Keep each slice to one purpose. Do not mix mechanical changes with behavior changes.
- Tags, releases, and crates.io publishing wait for the owner.

## Review focus

- Claude Code's real requests keep working after inbound checks. Record their header names as a fixture first. `HEAD /api/hello` may be rejected, because Claude Code treats it as optional.
- A conversation that moves Claude to Codex to Claude keeps its reasoning, and Anthropic receives no Codex signature or encrypted content.
- A body with no block to convert stays byte-identical on the Claude route, and a converted body changes only the converted blocks.
- A rejected refresh does not delete the Codex login, and a token rotated by the Codex CLI is picked up.
- `--help`, `duolane serve`, `duolane monitor`, and `duolane codex status` keep working through renames, comment removal, and file splits.

## Slices

- [ ] **Foundation documents.** Rules, designs, ADRs, standards, license, security policy, Markdown style configuration, and this plan.
- [ ] **Remove extra backends.** Delete Kimi, Grok, Cursor, their logins, commands, config, tests, and any code and crates only they use. Accept: `git grep -inE 'kimi|moonshot|grok|x\.ai|cursor-agent|api2\.cursor|cursor\.com'` finds nothing outside `docs/adr`, `docs/plans`, and `CHANGELOG.md`; the word `cursor` remains only for terminal cursors; `/v1/models` lists only Claude and Codex models.
- [ ] **Refresh the lockfile.** Run `cargo update`. Accept: full tests pass and `cargo deny check advisories` reports no vulnerability.
- [ ] **Rename to duolane.** Crate, binary, library, `DUOLANE_` environment variables, config and state directories, the `x-duolane-key` header, `originator`, `User-Agent`, `flake.nix`, workflows, and the installer. Rewrite the README with credits to both source projects, restart `CHANGELOG.md`, and set version `0.1.0`. Accept: `git grep -nE 'claude-code-proxy|claude_code_proxy|CLAUDE_CODE_PROXY|claude-codex|claude_codex|claude-code-with-codex|fcakyon|CCP_'` finds nothing outside the README credits, `docs/adr`, and `docs/plans`; CI is green.
- [ ] **Move comments into design.** Harvest decisions from comments into `docs/design/`. Replace `clap` doc comments with attributes. Remove comments with a Rust-aware tool. Accept: token streams match before and after, apart from `doc` attributes and the `clap` attributes; `--help` output is unchanged.
- [ ] **Tooling and CI.** `.tool-versions`, `rust-toolchain.toml`, `Makefile`, `.githooks`, `deny.toml`, `.gitleaks.toml`, comment gate, Markdown checks, length gate with a ratcheting baseline, workflow pin gate, tool freshness check, CodeQL, and Dependabot alerts. Pin actions by SHA, use least privilege in the release workflow, make every test hermetic, and remove Nix, `just`, `checkle`, and workmux files. Set the `main` ruleset and the Actions allowlist afterward. Accept: each gate has a test that proves it fails, and a full test run writes nothing under the real home directory.
- [ ] **Upgrade dependencies.** One pull request per breaking family: `reqwest`, `tokio-tungstenite` with `rustls`, `ratatui` with `crossterm`, `jiff`, and the small crates. Accept: every direct dependency is on its latest stable version or has a recorded reason, and `cargo deny check` passes and blocks merge.
- [ ] **Inbound hardening.** Implement the [inbound rules](../design/security.md#inbound-requests), including the token in `duolane monitor`. Accept: tests cover each rule with hostile and legitimate requests, including the recorded Claude Code headers.
- [ ] **Reasoning portability.** Implement [reasoning across backends](../design/architecture.md#reasoning-across-backends) and invariants 1 to 4. Accept: tests cover every row of the conversion table, byte preservation outside converted blocks, and a Codex to Claude to Codex conversation.
- [ ] **Limits and stored data.** Implement the [limits](../design/security.md#limits), [stored data rules](../design/security.md#stored-data), and [terminal rule](../design/security.md#terminal-output). Accept: a test exercises each limit at its boundary, and multibyte truncation cannot panic.
- [ ] **Credential safety.** Implement the [credential](../design/security.md#credentials) and [upstream connection](../design/security.md#upstream-connections) rules. Accept: tests cover lock timeout, re-read before refresh, re-read after rejection, redirect refusal, URL validation, and an account change.
- [ ] **Stream and content fidelity.** Implement invariants 5 and 6, and fix transcript parsing. Accept: tests cover truncated streams, open tool calls at stream end, unsupported blocks, and transcripts containing JSON escapes.
- [ ] **Structure.** Split oversized files by concern, one file per pull request. Accept: the length baseline is empty.
- [ ] **Live verification.** With the owner's approval, run Claude and Codex through a real Claude Code session, including a mid-conversation switch, a token refresh, the new `originator` and `User-Agent`, and the [unverified behavior](../design/architecture.md#unverified-behavior). Accept: results are recorded before the first release is proposed.
