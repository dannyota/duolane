# Claude Accounts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Terra implements; Sol reviews Terra's changes. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user select one of several Claude Code logins and continue the same local transcript using `duolane claude`.

**Architecture:** The CLI registers named directories through Claude Code's native login and stores a name-only selector in the duolane state directory. The launcher resolves that selector, shares the current project's directory without overwriting either store, and sets the chosen profile only in the child process. Do not cherry-pick the token pool in PR #73 or the Codex-only patch in PR #36.

**Tech Stack:** Rust, clap, existing `rustix` and `uuid` dependencies, fake Claude subprocesses and private temporary directories for tests.

**Spec:** [Claude account selection](../design/claude-accounts.md)

## Global Constraints

- The public commands are `duolane accounts login`, `duolane accounts use`, and the existing `duolane claude` launcher. No alias, separate run command, or automatic fallback.
- Claude Code owns login, storage, and refresh. duolane never reads, copies, rewrites, or substitutes a Claude credential.
- No new dependencies or Codex and proxy routing changes. Use one `.worktrees/` checkout and no real logins in automated checks.
- Account commands are Unix-only. Existing startup is unchanged with an explicit `CLAUDE_CONFIG_DIR`, an absent selector, the `default` selection, or on non-Unix.
- Stage only named paths and wait for owner authorization before committing or pushing. Implementation and review agents do not commit.

## Review Focus

- Login fails or is canceled: a new name must not appear in the selector menu.
- A selector is invalid, too long, symlinked, or selected under a different config root: never load a different login or follow the selector's target.
- A profile already has distinct project history or two working paths normalize to one folder: refuse the launch and leave both trees intact.
- Explicit configuration and inherited auth overrides: preserve the explicitly chosen config; for a managed account, refuse inherited command-line environment auth overrides and leave native settings/policy evaluation to Claude Code.
- Launch overlaps a login, or the caller changes the project folder: fail before writing any link.

### Task 1: Register profiles and manage the selector

**Files:** Create `src/claude_accounts.rs` and `src/claude_accounts/store.rs`; modify `src/lib.rs`, `src/main.rs`; add focused unit tests beside the new module and one `accounts --help` test in `tests/cli.rs`.

**Produces:** `pub fn login() -> anyhow::Result<()>`, `pub fn select() -> anyhow::Result<()>`; `pub(crate) fn selected_profile() -> anyhow::Result<Option<Profile>>` returns the checked absolute directory and a temporary lock in `Profile` or `None`. `Profile::path(&self) -> &Path` exposes its path. On non-Unix, `login` and `select` report unavailable and `selected_profile` returns `None`.

- [ ] Write failing tests `claude_accounts_login_registers_only_success`, `claude_accounts_selects_default_and_named`, and `claude_accounts_rejects_unsafe_selector`. Assert a fake successful `claude auth login` sees the named `CLAUDE_CONFIG_DIR`, a fake failed login leaves no ready marker, and an unknown name cannot modify the selector. Verify `backup\n` and `default\n` are the only persisted values for those selections; a 65-byte or symlinked selector errors. Verify two config roots with the same named profile do not share a selection, and that a held profile lock blocks login and resolution. Verify the help exposes `login` and `use` without `run`.
- [ ] Run `cargo nextest run --locked -E 'test(claude_accounts)'`. Expect failure for the missing registration/selection interface before implementation.
- [ ] Implement `login` and `select` using bounded input and a sorted name menu with `default` first. Store profiles under `paths::config_dir()/claude-accounts/<name>` with `0700` permissions and mark successful `claude auth login --claudeai` with a private regular `.duolane-ready` file. Check owner, file type, and permissions; never inspect profile credential storage. Allow an existing checked name to log in again, but serialize managed login and launch preparation with a nonblocking exclusive advisory lock on an owned private regular `.duolane-lock` file. Take the lock before login or checking readiness, and hold it until login completes or the launch execs. Use close-on-exec.
- [ ] Implement `selected_profile` from `paths::state_dir()/claude-accounts/<config-root-hash>/claude-account`; hash the canonical config root's Unix path bytes with SHA-256 in lowercase hex. Read at most 64 bytes through a regular, `0600`, non-followed file in an owner-controlled state directory. Use a private create-new temporary file and same-directory rename for updates. A missing file or `default` returns `None`; malformed, stale, or unsafe selections error. A nonempty explicit `CLAUDE_CONFIG_DIR` bypasses selection before any state read.
- [ ] Run `cargo nextest run --locked -E 'test(claude_accounts)'`. Expect all new tests to pass.

### Task 2: Share the transcript and integrate with the launcher

**Files:** Create `src/claude_accounts/transcripts.rs`; modify `src/claude_accounts.rs`, `src/intercept/wrapper.rs`, `src/intercept/picker.rs`, `README.md`, `docs/design/README.md`, and `docs/design/intercept.md`.

**Consumes:** `selected_profile() -> anyhow::Result<Option<Profile>>`. **Produces:** `pub(crate) fn prepare_launch(args: &[OsString]) -> anyhow::Result<Option<Profile>>`, where `Some(profile)` means the project has been linked and the caller must set `CLAUDE_CONFIG_DIR` in the child and keep the profile guard alive until `exec`. Extend `picker::arguments` to take `profile: Option<&Path>`.

- [ ] Write failing tests `claude_accounts_links_only_current_project`, `claude_accounts_leaves_conflicts_untouched`, and `claude_accounts_preserves_explicit_config`. Assert a fake child appends to the original `.jsonl` through the link, and no link is created without approval. Verify repetition is idempotent, a slug collision cannot reuse a link, and other history and unexpected links stay untouched. Assert worktree flags, a configured project name, an unsupported project path, and a subscription override error before linking. Assert unset, default, and explicit configuration preserve the launcher's current behavior. Verify the named account's own regular picker settings win and symlinked settings are not read.
- [ ] Run `cargo nextest run --locked -E 'test(claude_accounts)'`. Expect the new tests to fail for the missing launch interface.
- [ ] Implement `prepare_launch`: resolve the locked profile, check the canonical working directory, and ask on a terminal before new linkage, displaying the selected profile name and entire source folder. A refusal or unavailable terminal leaves no link; render filesystem paths with escaped control characters in confirmation and errors. Create or verify only `<profile>/projects/<project>` pointing to `~/.claude/projects/<project>`. Store the canonical working path in a private `<profile>/.duolane-projects/<project>` file via create-new or verify the existing bytes. Verify directory ownership and non-writable-by-others permissions; reject symlinked source directories and destination collisions. Never rename, merge, read, or delete any transcript. Reject `CLAUDE_CODE_PROJECT_DIR_NAME`, `-w`, and `--worktree` forms before `--` on managed launches.
- [ ] Refuse nonempty `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `CLAUDE_CODE_USE_FOUNDRY`, and `ANTHROPIC_PROFILE`, or the pair `ANTHROPIC_FEDERATION_RULE_ID` and `ANTHROPIC_ORGANIZATION_ID`, during named login or launch. Error text includes variable names only.
- [ ] Call `prepare_launch` from the intercept wrapper after its existing preflight checks and before creating the child. Supply the selected profile settings path to the picker and preserve all intercept variables and passthrough arguments. Update README and design links with scope, default reset, and the resume/privacy caveats.
- [ ] Run `cargo nextest run --locked -E 'test(claude_accounts) | test(intercept)'`, followed by `make check`. Expect both to pass. No credential or transcript from the owner's machine is required for these checks.

### Delivery

- [ ] Get an independent design pass before coding and an independent Sol review after Terra's implementation; verify the no-credential boundary, selection errors, symlink collisions, and unchanged no-selection startup.
- [ ] Report code, test, and platform coverage. The standalone Claude Code live check already proved same-file append; run live end-to-end through a built wrapper only with separate owner approval.
