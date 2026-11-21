# Accounts

duolane can hold several accounts for one backend and move to the next when the
active account's quota is spent. Each backend keeps a named pool and a persisted
current selection. The [security design](security.md#credentials) owns how the
stores and selections are written and read. The [Codex route design](codex.md)
owns the Codex profile order and eligibility.

## Pools

| Backend | Store                                    | Selection                         | Account unit                   |
| ------- | ---------------------------------------- | --------------------------------- | ------------------------------ |
| Claude  | `claude-accounts.json` in the config dir | `claude-account` in the state dir | A Claude Code config directory |
| Codex   | `codex-profiles.json` in the config dir  | `codex-profile` in the state dir  | A Codex home with `auth.json`  |

The Claude store maps a name to an absolute Claude Code config directory, the
directory `CLAUDE_CONFIG_DIR` names. Each directory holds one login, so the
owner adds an account by signing Claude Code in there
(`CLAUDE_CONFIG_DIR=<dir> claude`, then `/login`) and recording the directory.
The file holds at most 16 accounts in at most 16 KiB. A name is 1 to 32
characters from `[A-Za-z0-9_-]`.

The Codex store already exists and its rules are unchanged; this design adds
only the Claude side of the fallback below.

## Selection

The current account is persisted per backend and set by hand with
`duolane claude-accounts use` or `duolane codex auth use`, and each `list`
prints the pool and the selection. Fallback advances the selection. Without a
`claude-account` selection, duolane attaches no account and the Claude route
stays passthrough, as if no accounts were configured.

## Fallback

A request starts on the selected account. When the upstream reports that the
account's quota is spent, duolane marks that account limited until the reset
time in the error and advances the selection to the next eligible account in the
pool's order, persisting it. Only new requests use the new selection; the
request that hit the limit keeps its quota error and is not replayed.

Fallback advances only on a quota signal. A transient rate limit, an overload, a
transport failure, or a context overflow is retried or returned as before, never
advanced.

| Backend | Quota signal                                                   |
| ------- | -------------------------------------------------------------- |
| Claude  | A 429 whose reset window is the subscription window            |
| Codex   | A `usage_limit_reached` error, surfaced as a `CodexUsageLimit` |

Anthropic's `rate_limit_error` covers both a per-minute transient limit and a
spent subscription window, so a bare 429 is not enough. The Claude signal is a
429 whose reset window or message marks the subscription window, never a
per-minute reset. The exact discriminator is unverified and is confirmed live
before Claude fallback turns on, as the [unverified behavior](#unverified) list
records.

For Codex the trigger is the presence of a `CodexUsageLimit`. Its `window` and
`resets_at` fields are informational and may be absent.

The order is positional, not alphabetical. Codex is `default` first then named
profiles in `codex-profiles.json` order, and Claude is the order the accounts
appear in `claude-accounts.json`. A switch scans forward from the active account
and skips accounts with a login that cannot be read or that are still marked
limited. If no account is eligible, the selection stays and the quota error is
unchanged. Limit marks live in memory and end at their reset time or a restart.

The advance is serialized: duolane re-reads the selection under a lock before
advancing, so concurrent requests cannot regress it to a spent account.

## Claude credential attachment

With no Claude accounts configured, the Claude route is the byte-exact
passthrough the [architecture](architecture.md#claude-route) describes. With
accounts configured, duolane replaces the forwarded `Authorization` header with
`Bearer <session key>` read from the selected account's login, as
[Claude login location](#claude-login-location) defines, and leaves the body
byte-exact. `POST /v1/messages` falls back. `POST /v1/messages/count_tokens`
relays through the selected account without fallback, because counting does not
spend quota.

## Claude login location

Claude Code keeps one OAuth login per config directory. On macOS it is a
Keychain generic password whose service is `Claude Code-credentials-` followed
by the first 8 hex digits of the SHA-256 of the directory path as spelled in
`claude-accounts.json`, with any trailing slash removed; the resolved path is
not used. On other Unix systems it is `.credentials.json` inside the directory.
Both hold a JSON object whose `claudeAiOauth.accessToken` is the bearer key and
whose `claudeAiOauth.expiresAt` is its expiry in Unix milliseconds. duolane only
reads the login; Claude Code refreshes it. An expired key is reported as expired
and is never refreshed or sent by duolane.

## Usage

`duolane claude-accounts usage` prints every account in pool order with the
share of its 5-hour and 7-day windows already used and when each resets, in
local time. For each account it reads the login as above and sends
`GET https://api.anthropic.com/api/oauth/usage` with that bearer key and
`anthropic-beta: oauth-2025-04-20`. The endpoint is fixed: the relay's
`DUOLANE_ANTHROPIC_BASE_URL` override does not apply, and only tests substitute
a mock endpoint, always with toy logins.

Only `five_hour` and `seven_day`, each with a numeric `utilization` and an RFC
3339 `resets_at`, are read. Every other field is ignored, and a missing or
unparsable window prints as `?`. Output contains only account names, rounded
percentages, and reset times duolane formats itself; no upstream string is
echoed.

The command reads a snapshot of the store and is independent of the selection.
With no store or an empty one it reads no login and sends nothing. An unsafe or
invalid store fails the whole command. A valid selection marks its account with
`*`; an invalid selection only drops the marker and prints a warning to stderr.
Accounts are queried concurrently and printed in order. A failure for one
account prints one fixed reason for that account and does not stop the others:
login not found, login unreadable, login expired, Keychain access denied,
Keychain timed out, usage request failed, usage request failed with HTTP
`<status>`, or usage response invalid. Per-account failures still exit 0. On
non-Unix systems the command reports that Claude accounts are not supported.

Usage is display only: it never changes the selection, and fallback still
advances only on a quota signal. `list` stays offline.

## Reasoning and conversation

Switching accounts mid-conversation moves a thinking signature from one account
to another. For Codex the account tag in a duolane signature changes, so earlier
encrypted reasoning returns as reasoning text, per the
[reasoning table](architecture.md#reasoning-across-backends). For Claude, an
Anthropic signature from the previous account may be rejected; Claude Code then
strips earlier thinking, as it does after a backend switch.

## Unverified

- The exact Anthropic signal that separates a per-minute 429 from a spent
  subscription window.
- Whether a Claude account switch mid-conversation is rejected, and how Claude
  Code recovers.
