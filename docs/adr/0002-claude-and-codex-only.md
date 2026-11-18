# ADR 0002: Claude and Codex are the only backends

Status: accepted 2026-11-18 by the owner.

## Context

The inherited code also translates requests for Kimi, Grok, and Cursor. Those
backends add about 18,700 lines under `src/providers`, each with its own login,
token storage, and wire format. The owner uses Claude and Codex only.

## Decision

duolane routes `claude-*` models and the Claude aliases to Anthropic, and Codex
models to the ChatGPT Codex backend. The Kimi, Grok, and Cursor backends, their
logins, their commands, and any code only they use are removed.

## Consequences

- There are fewer credentials to protect and less code to review and upgrade.
- Users of those backends need another tool.
- Provider selection can stay simple, with one translated backend.

## Alternatives rejected

- Keeping them behind Cargo features. The code would still need upgrades,
  review, and tests.
- Keeping them unchanged. They hold credentials the owner has no use for.

## History

- 2026-11-18: accepted.
