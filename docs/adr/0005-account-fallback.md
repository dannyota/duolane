# ADR 0005: Account fallback rotates a pool on usage exhaustion

Status: proposed 2026-11-21.

## Context

duolane routes each request to a backend by model name, and each backend uses
one login at a time. The Claude route forwards Claude Code's subscription login.
The Codex route reads the Codex CLI's `auth.json` for the selected profile.

A subscription account has a usage quota: Claude's limit and ChatGPT's five-hour
and seven-day windows. When the active account hits it, duolane returns an error
and the owner switches accounts by hand, by running
`duolane claude-accounts use` for Claude or `duolane codex auth use` for Codex.

duolane already detects Codex quota exhaustion precisely: a
`usage_limit_reached` error surfaces as a `CodexUsageLimit`, with a message and
optional reset fields. The Claude route has no equivalent.

## Decision

- Each backend keeps a named pool of accounts and a persisted current selection.
  Claude gets a new store. Codex reuses `codex-profiles.json` and its selection.
- When a response reports that the active account's quota is spent, duolane
  marks that account limited until the reset time and advances the selection to
  the next eligible account in the pool's order, persisting it. Only new
  requests use the new selection; the request that hit the limit keeps its quota
  error and is not replayed.
- The order is positional, not alphabetical: Codex is `default` first then named
  profiles in `codex-profiles.json` order, and Claude is the order the accounts
  appear in `claude-accounts.json`. A switch scans forward from the active
  account and skips accounts without a readable login or still marked limited.
- The advance is serialized under a lock and re-reads the selection first.
- Only a quota signal advances the selection. A transient rate limit, an
  overload, a transport failure, or a context overflow is retried or returned as
  before, never advanced.
- The Claude store and its selection follow the Codex profile security rules.
- The [accounts design](../design/accounts.md) owns the pools, selection, and
  fallback; the [Codex route design](../design/codex.md) owns the Codex profile
  order and eligibility.

## Consequences

- The Claude route is no longer pure passthrough when accounts are configured:
  duolane reads the selected account's session key from its config directory and
  rewrites the `Authorization` header. With no accounts configured it stays the
  byte-exact passthrough described in the
  [architecture](../design/architecture.md#claude-route).
- duolane now reads Anthropic credentials from the selected account's config
  directory, so the [security design](../design/security.md#credentials) gains a
  rule for reading and attaching them.
- A switch mid-conversation can send one account's thinking signature to
  another. This mirrors the existing Claude to Codex to Claude switch and its
  documented fallback, and stays unverified until a live check.
- Anthropic's `rate_limit_error` covers both a per-minute limit and a spent
  subscription window, so the Claude signal must separate the two. It is
  unverified and stays conservative until a live check confirms it.

## Alternatives rejected

- Transparent failover that re-sends the request on the next account in the same
  request. Replaying re-runs the reasoning the first account already spent, and
  a mid-conversation replay can send one account's thinking signature to
  another; advancing for the next request avoids both.
- A proactive local token threshold instead of an upstream error. duolane does
  not know each account's quota, so a local count cannot be accurate.
- A round-robin load balancer. The owner wants a stable account, not
  distribution.
- Automatic re-login. duolane cannot complete an OAuth flow by itself.

## History

- 2026-11-21: proposed.
