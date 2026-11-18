# Architecture

duolane is a local HTTP proxy for Claude Code. Claude Code speaks the Anthropic
Messages API, so duolane accepts that API and picks a backend for each request
from the model name. One session can use a Claude subscription and a ChatGPT
subscription and switch between them mid-conversation.

## Routes

| Route                                                  | Purpose                     | Availability   |
| ------------------------------------------------------ | --------------------------- | -------------- |
| `POST /v1/messages`, `POST /v1/messages/count_tokens`  | Claude Code traffic         | Always         |
| `GET /v1/models`                                       | Model discovery             | Always         |
| `GET /healthz`                                         | Liveness                    | Always         |
| `GET /monitor`                                         | Local dashboard snapshot    | Always         |
| `POST /v1/responses`, `POST /v1/chat/completions`      | OpenAI-compatible, Codex    | Off by default |
| `POST /v1/images/generations`, `POST /v1/images/edits` | Images through Codex        | Off by default |
| `POST /v1/audio/transcriptions`                        | Transcription through Codex | Off by default |

The off-by-default routes serve Codex models only and answer HTTP 400 for any
other model.

## Routing

| Requested model                   | Backend   | Behavior                                 |
| --------------------------------- | --------- | ---------------------------------------- |
| `claude-*` and the Claude aliases | Anthropic | Transparent passthrough                  |
| Codex model ids                   | Codex     | Translate to the OpenAI Responses API    |
| Anything else                     | None      | HTTP 400 that lists the supported models |

- Codex model ids come from a built-in list plus the API-capable entries in the
  Codex CLI's `models_cache.json`. Unlisted catalog entries are accepted when
  requested by exact id but are not advertised.
- A `[1m]` suffix on a model id is ignored.
- A setting can send `claude-*` and the Claude aliases to Codex instead.
- A setting can override the model of Claude Code's auto-review requests. Those
  requests use the Codex small model when they would go to Codex and no override
  is set.
- A request that continues a message thread is rejected on the Codex route,
  because only Anthropic supports threads.

## Claude route

duolane relays the original body and headers to `api.anthropic.com` and streams
the reply back. It stores no Anthropic credential. Claude Code keeps using its
subscription login when only `ANTHROPIC_BASE_URL` is set. An
`ANTHROPIC_AUTH_TOKEN` or approved `ANTHROPIC_API_KEY` replaces that login
([gateway docs](https://code.claude.com/docs/en/llm-gateway),
[connection docs](https://code.claude.com/docs/en/llm-gateway-connect.md)).

## Codex route

duolane translates Messages requests into Responses API requests and translates
the stream back. It uses the login that the Codex CLI stored, and has no Codex
login of its own. A token refresh writes the rotated tokens back so the Codex
CLI keeps working. The [security design](security.md#credentials) owns these
rules.

## Reasoning across backends

Two formats live in Claude Code transcripts, outside duolane:

| Format          | Value                                                                    |
| --------------- | ------------------------------------------------------------------------ |
| Codex signature | `duolane:codex:v1:<account tag>:<base64url item id>:<encrypted content>` |
| Reasoning text  | `<previous_reasoning>\n<text>\n</previous_reasoning>`                    |

The account tag is the first 16 hexadecimal characters of the SHA-256 of the
ChatGPT account id. Without a known account id, duolane issues no Codex
signature. Both formats are frozen from the first release. A new version adds a
new prefix, and duolane keeps recognizing every released one.

An Anthropic signature is any non-empty signature without a released duolane
prefix.

| Thinking block                 | Sent to Anthropic | Sent to Codex               |
| ------------------------------ | ----------------- | --------------------------- |
| Anthropic signature            | Unchanged         | Reasoning text if non-empty |
| Codex signature, same account  | Reasoning text    | Encrypted reasoning item    |
| Codex signature, other account | Reasoning text    | Reasoning text              |
| No signature                   | Reasoning text    | Reasoning text              |

An empty block that would become reasoning text is removed. If that empties an
assistant message, the message keeps one reasoning text block with empty text,
so the conversation stays valid.

## Invariants

1. **The Claude route is byte-exact.** A body with no block to convert is
   forwarded unchanged. A conversion replaces only the bytes of the affected
   blocks. Anthropic binds thinking signatures to earlier content, and rewriting
   earlier messages can cause a rejection, after which Claude Code removes
   earlier thinking from the session
   ([gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol)).
2. **Conversions are deterministic.** The same history produces the same bytes
   on every turn, so the prompt cache and Anthropic's prefix check keep working.
3. **Encrypted Codex reasoning in a duolane signature never reaches Anthropic.**
4. **Encrypted reasoning returns only to the account that produced it.**
5. **Stream failures are visible.** An interrupted or incomplete upstream stream
   is never reported as a successful turn, and a tool call with incomplete
   arguments is never reported as complete. A stop at `max_tokens` is reported
   as that stop.
6. **Unsupported content is never dropped silently.** A block the backend cannot
   carry produces an explicit marker or an error. Removing empty thinking is the
   only exception.

## Unverified behavior

These inherited assumptions need a live check:

- Switching backends while a tool call is pending.
- Claude Code running web search through a client-side `WebSearch` tool and an
  isolated inner request, which would keep hosted search blocks out of the outer
  transcript. The Claude route logs `hosted_web_search_in_history` if such
  blocks appear.
