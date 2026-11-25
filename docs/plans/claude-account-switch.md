# Claude Account Switch Implementation Plan

> **For agentic workers:** Follow the repository's [workflow](../../instructions/workflow.md) and any local overrides. The primary owns Git.

**Goal:** Move to another Claude profile and resume the current project's latest session with one command after a login runs out of usage.

**Architecture:** `duolane accounts switch` resolves a target profile, saves it through the existing selector path, and calls the existing intercept launcher with `--continue` added when no session flag is present. Claude Code still chooses the session; duolane reads no transcript or credential.

**Tech Stack:** Rust, clap, the existing `claude_accounts` store, roster, and picker, and fake Claude subprocesses in private temporary directories.

**Spec:** [Switch and resume](../design/claude-accounts.md#switch-and-resume).

## Global Constraints

- No quota inspection, automatic trigger, proxy change, or Codex change. No new dependencies.
- Reuse the selector lock, checked write, and `prepare_launch` path. Never bypass the launch refusals or the first-link confirmation.
- Unix only, like the other accounts commands. Non-Unix reports the command as unavailable.
- Tests use fake `claude` binaries, toy accounts, and temporary config and state directories. No real login.

## Review Focus

- `--next` never lands on the current selection or a profile proven to hold the same account; an unknown status is not a match; a single-candidate roster errors without writing.
- A user session flag before `--` suppresses `--continue`; a flag after the user's own `--` does not count; `=` forms are recognized.
- `CLAUDECODE` set without a terminal refuses before any write.
- A failed launch leaves the new selection saved and names it in the error.
- An explicit `CLAUDE_CONFIG_DIR` keeps bypassing selection; `switch` must refuse rather than save a selection the launch would ignore.

## Task 1: Resolve and save the target

**Files:** Create `src/claude_accounts/switch.rs`; modify `src/claude_accounts.rs`, `src/main.rs`; add tests in `tests/claude_accounts.rs`.

- [x] Write failing tests `claude_accounts_switch_next_skips_same_account`, `claude_accounts_switch_rejects_current_selection`, and `claude_accounts_switch_refuses_inside_claude_code`. Use the existing fake status binary for accounts.
- [x] Add `AccountsCommand::Switch { name: Option<String>, next: bool, args: Vec<OsString> }` with `name` conflicting with `--next` and `args` as trailing arguments after `--`.
- [x] Implement target resolution with `roster::entries` and the `use` menu, then save through the same function `select` uses. Refuse when `CLAUDE_CONFIG_DIR` is set.
- [x] Run `cargo nextest run --locked -E 'test(claude_accounts)'`.

## Task 2: Launch with `--continue`

**Files:** `src/claude_accounts/switch.rs`, `src/intercept/wrapper.rs` only if the entry point needs a public wrapper, `README.md`, `docs/wiki/Claude-Accounts.md`, `docs/wiki/CLI.md` if it lists accounts commands.

- [x] Write failing tests `claude_accounts_switch_adds_continue` and `claude_accounts_switch_keeps_user_session_flag`. Assert the fake `claude` child's arguments.
- [x] Implement session flag detection and call `intercept::wrapper::run` with the final arguments. On launch error, add the selected profile name and the `duolane claude --continue` retry hint.
- [x] Add the README command row and a wiki section that replaces the two-command resume example with `switch`, keeping the `--resume` form for several sessions.
- [x] Run `make check`.

## Delivery

- [ ] Live check with owner approval: start a session under one profile, quit, run `duolane accounts switch <other>`, and confirm Claude Code resumes the same transcript. Record the Claude Code version in the design's evidence section and remove the unverified note.
- [ ] Ask the owner before merging. Delete this plan when the work lands.
