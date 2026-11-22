# Foundation plan

**Goal:** Turn the inherited proxy into `duolane`: an independent, hardened, current Rust product with Claude and Codex backends, built and documented in the owner's style.

**Architecture:** Each slice is one pull request, or a short series, that leaves the default branch green. Remove and rename first so later work touches less code. Harvest comments before other slices rewrite the code they explain.

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
- A conversation that moves Claude to Codex to Claude keeps its reasoning, and Anthropic receives no duolane signature or encrypted content.
- A body with no block to convert stays byte-identical on the Claude route, and a converted body changes only the converted blocks.
- A rejected refresh does not delete the Codex login, and a token rotated by the Codex CLI is picked up.
- `--help`, `duolane serve`, `duolane monitor`, and `duolane codex auth status` keep working through renames, comment removal, and file splits.

## Slices

- [x] **Foundation documents.** Rules, designs, ADRs, standards, license, security policy, Markdown style configuration, and this plan.
- [x] **Remove extra backends.** Delete Kimi, Grok, Cursor, their logins, commands, config, tests, and any code and crates only they use. Accept: `git grep -inE 'kimi|moonshot|grok|x\.ai|cursor-agent|api2\.cursor|cursor\.com'` finds nothing outside `docs/adr`, `docs/plans`, `CHANGELOG.md`, and the negative tests in `tests/cli.rs` and `tests/server.rs`; elsewhere the word `cursor` remains only for terminal cursors; `/v1/models` lists only Claude and Codex models.
- [x] **Refresh the lockfile.** Run `cargo update`. Accept: full tests pass; `cargo deny check advisories` reports no vulnerability, or each remaining one has a recorded temporary ignore that the upgrade slice removes; the `cargo deny` clauses in `instructions/` no longer name this slice.
- [x] **Rename to duolane.** Crate, binary, library, `DUOLANE_` environment variables, config and state directories, the `x-duolane-key` header, `originator`, `User-Agent`, `flake.nix`, workflows, and the installer. Rewrite the README with credits to both source projects, restart `CHANGELOG.md`, and set version `0.1.0`. Accept: `git grep -nE 'claude-code-proxy|claude_code_proxy|CLAUDE_CODE_PROXY|claude-codex|claude_codex|claude-code-with-codex|fcakyon|CCP_'` finds nothing outside the README credits, the remote names in `AGENTS.md`, `docs/adr`, and `docs/plans`; `README.md` is removed from the Prettier and markdownlint ignore lists; CI is green.
- [x] **Move comments into design.** Harvest decisions from comments into `docs/design/`. Replace `clap` doc comments with attributes. Remove comments with a Rust-aware tool. Accept: token streams match before and after, apart from `doc` attributes, the `clap` attributes, and branches that clippy requires merging or removing once their comments are gone; `--help` output is unchanged; rule 4 in `AGENTS.md` no longer defers comment removal.
- [x] **Tooling and CI.** `.tool-versions`, `rust-toolchain.toml`, `Makefile`, `.githooks`, license and source rules in `deny.toml`, a CI step for `cargo deny`, a check for the 3-day crate cool-down, `.gitleaks.toml`, comment gate, Markdown checks, length gate with a ratcheting baseline, workflow pin gate, CodeQL, and Dependabot alerts. Pin actions by SHA, use least privilege in the release workflow, make every test hermetic, correct the platform names in `src/paths.rs`, and remove Nix, `just`, `checkle`, and workmux files. Set the `main` ruleset and the Actions allowlist afterward. Accept: each gate has a test that proves it fails, and a full test run writes nothing under the real home directory.
- [x] **Upgrade dependencies.** One pull request per breaking family: `reqwest`, `tokio-tungstenite` with `rustls`, `ratatui` with `crossterm`, `jiff`, and the small crates, and remove direct dependencies nothing uses. Accept: every direct dependency is on its latest stable version or has a recorded reason, the temporary advisory ignores are gone, and `cargo deny check` passes and blocks merge.
- [x] **Inbound hardening.** Implement the [inbound rules](../design/security.md#inbound-requests), including the token in `duolane monitor`. Accept: tests cover each rule with hostile and legitimate requests, including the recorded Claude Code headers.
- [x] **Reasoning portability.** Invariant 4 is enforced when a request is translated; the credential safety slice binds it through to sending. Implemented: [reasoning across backends](../design/architecture.md#reasoning-across-backends) and invariants 1 to 4. Accept: tests cover every row of the conversion table, byte preservation outside converted blocks, and a Codex to Claude to Codex conversation.
- [x] **Limits and stored data.** Implement the [limits](../design/security.md#limits), [stored data rules](../design/security.md#stored-data), and [terminal rule](../design/security.md#terminal-output). Accept: a test exercises each limit at its boundary, and multibyte truncation cannot panic.
- [x] **Credential safety.** Implement the [credential](../design/security.md#credentials) and [upstream connection](../design/security.md#upstream-connections) rules. Read the Codex login off the async workers, bind each request to the account it was translated for through to sending, and key stored compaction state by account. Bound the token refresh response body. Accept: tests cover lock timeout, re-read before refresh, re-read after rejection, redirect refusal, URL validation, and an account change.
- [x] **Stream and content fidelity.** Implement invariants 5 and 6, and fix transcript parsing. Accept: tests cover truncated streams, open tool calls at stream end, unsupported blocks, and transcripts containing JSON escapes.
- [x] **Structure.** Split oversized files by concern, one file per pull request. Accept: the length baseline is empty.
- [x] **Live verification.** With the owner's approval, run Claude and Codex through a real Claude Code session, including a mid-conversation switch, a token refresh, the new `originator` and `User-Agent`, and the [unverified behavior](../design/architecture.md#unverified-behavior). Accept: results are recorded before the first release is proposed.
- [x] **Intercept mode.** Build the [intercept design](../design/intercept.md) after the slices above. Start with a spike that confirms its unverified behavior with the owner's Claude Code session. Accept: Remote Control and a Codex model work in one session; cleanup leaves no file or setting behind.
- [x] **Overflow code on buffered WebSocket.** On the non-streaming WebSocket path, `codex_status_error` keeps only the error text, so a Codex context overflow reported by its `context_length_exceeded` code alone reaches Claude Code as `api_error` instead of `prompt is too long`. Accept: that path keeps the code, and a test covers a code-only overflow there.
- [x] **Remaining live checks.** The [live results](../design/architecture.md#live-results) left a token refresh, the `originator` and `User-Agent` values, a context window overflow, and a switch while a tool call is pending. Accept: each is checked and recorded, or moved to a design's unverified list with a reason.
- [ ] **Buffered overflow.** A non-streaming request over the context window gets HTTP 500 from Responses Lite, three retries, and a 503 instead of `prompt is too long`. Accept: record the upstream error code for buffered 5xx responses, and map a context overflow to the same 400 the streaming path returns without retrying.
