# Account fallback plan

Rotate a backend's account pool when the active account's quota is spent. The [ADR 0005](../adr/0005-account-fallback.md) records the choice and the [accounts design](../design/accounts.md) owns the rules.

## Global constraints

- Follow `AGENTS.md`, `instructions/`, and the engineering standard.
- Every slice: failing test first where behavior changes, full gates, independent review for code, green CI, merge commit.
- Tests never touch the home directory or a live backend.

## Slices

- [ ] **Documents.** The ADR, the accounts design, and the security and architecture updates land together and pass an independent review.
- [ ] **Claude account store and CLI.** `claude-accounts.json` maps a name to an absolute Claude Code config directory, and `duolane claude-accounts list|use` manages the `claude-account` selection. The owner adds an account by signing Claude Code in with `CLAUDE_CONFIG_DIR` set and recording the directory. Mirror the Codex profile security: ownership and mode checks, bounded reads, no symlink following, atomic selection writes. Tests cover hostile and legitimate files and selection writes.
- [ ] **Claude usage.** `duolane claude-accounts usage` reads each account's login from the macOS Keychain or `.credentials.json` and prints its 5-hour and 7-day usage from the fixed `/api/oauth/usage` endpoint, as the [accounts design](../design/accounts.md#usage) defines. Tests never run the real Keychain: the login reader and the Keychain child sit behind a seam that tests replace, and the endpoint is a loopback mock used with toy logins. Cases: a used share prints per account in order; a missing, expired, or unreadable login and a non-success status report per account without stopping the others, and an expired login sends nothing; the intercept proxy is skipped while another proxy is kept; an oversized login, child output, or response is rejected; a child past its deadline is killed; the production endpoint stays fixed when `DUOLANE_ANTHROPIC_BASE_URL` is set; a redirect to a second listener is not followed and that listener gets no connection; and no token appears on stdout or stderr.
- [ ] **Claude fallback.** Read the selected account's bearer key from its config directory, attach it, detect the quota signal, mark the account limited, and advance the selection for the next request. Tests: no accounts keeps passthrough; a spent account advances the selection and the current request keeps its 429; every account spent leaves the selection unchanged; a non-quota 429 does not advance.
- [ ] **Codex fallback.** The `feat/codex-profiles` branch already switches in order on `usage_limit_reached` without replaying. This slice verifies it against the accounts design and closes any gap: mark the account limited and advance the selection for the next request.
- [ ] **Monitor and live check.** Show the active account and record switches, then confirm the Claude discriminator and the mid-conversation switch with the owner's account.
