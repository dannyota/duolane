# ADR 0002: Claude and Codex are the only backends

Status: accepted 2026-11-18 by the owner, amended 2026-11-22.

## Context

The inherited code also translates requests for Kimi, Grok, and Cursor. Those
backends add about 18,700 lines under `src/providers`, each with its own login,
token storage, and wire format. The owner uses Claude and Codex only.

Some users pay for OpenAI models with an API key instead of, or as well as, a
ChatGPT plan. The OpenAI API serves the same Codex families through its
Responses API ([models](https://developers.openai.com/api/docs/models)). The
owner asked that users choose the API key, the ChatGPT login, or both in an
order they set (2026-11-22).

## Decision

- duolane routes `claude-*` models and the Claude aliases to Anthropic, and
  Codex models to the ChatGPT Codex backend or to the OpenAI API with an API
  key. The Kimi, Grok, and Cursor backends, their logins, their commands, and
  any code only they use are removed.
- The user chooses which Codex credentials duolane uses and in which order.
  duolane never starts spending API credit without that choice.
- duolane stores a key the user enters in a private file.
  [OpenAI API key](../design/openai-api-key.md) owns the details.

## Consequences

- There are fewer credentials to protect and less code to review and upgrade.
- Users of those backends need another tool.
- Provider selection stays simple: one translated backend family, with one or
  two credential sources.
- An API key is a second Codex credential to protect. Unlike the ChatGPT login,
  it bills at API rates and does not expire.
- The two sources differ in endpoints, errors, and features, so the Codex route
  carries per-source rules.

## Alternatives rejected

- Keeping them behind Cargo features. The code would still need upgrades,
  review, and tests.
- Keeping them unchanged. They hold credentials the owner has no use for.
- A separate OpenAI provider chosen by model name. The user would switch models
  by hand when a plan limit is reached, with no order.
- Reading the key from the Codex CLI's API-key login. The Codex CLI holds one
  login at a time, so the two sources could not be combined.
- The operating system keychain. Linux hosts without a desktop session have no
  Secret Service, so a file would still be needed, and the ChatGPT tokens
  already live in a private file.

## History

- 2026-11-18: accepted.
- 2026-11-22: Codex models may also go to the OpenAI API with an API key, in an
  order the user chooses.
