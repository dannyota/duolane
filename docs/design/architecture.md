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
- Session history never moves a Claude model to Codex. A Codex request earlier
  in a session cannot pull the Claude aliases off the Anthropic backend.
- Routing ignores login state, so signing in later needs no restart. A backend
  without a login is shown differently on each surface:
  - `duolane models` and the unknown-model error show a hint on how to sign in
    instead of its models.
  - `GET /v1/models` lists its models without a hint.
- `GET /v1/models` answers in Anthropic's format for Claude Code's gateway model
  discovery, which `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` turns on
  ([gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol)).
  Claude Code adds only ids that contain `claude` or `anthropic`, so the Claude
  aliases are what reach its `/model` picker. Codex ids are still listed for
  other clients. Discovery needs a gateway credential, so with a claude.ai login
  it does not run: Claude Code 2.1.328 logged
  `[gatewayDiscovery] skipped: no credential` on 2026-11-19.
- Codex models reach the `/model` picker through a
  [`modelPicker`](https://code.claude.com/docs/en/settings-reference#modelpicker)
  lineup, which takes any model id and was seen listing `gpt-6.2-sol` on
  2026-11-19. The lineup has one row per Codex catalog entry with `visibility`
  `list` that duolane advertises, in catalog `priority` order, without tier
  variants such as `-fast`. A row is `model` and `label` only: the catalog
  display name followed by `(Codex)`, or the id when the display name holds a
  control character or is longer than 64 characters. The catalog is untrusted:
  ids must pass the catalog's slug check, the lineup holds at most 20 rows, and
  duolane drops it when its JSON exceeds 16 KiB. Without a catalog, the rows
  come from the built-in Codex list, labeled with the id. duolane never writes
  an empty lineup, never sets `behavesAs`, and never edits `availableModels`,
  which can still hide these rows. Intercept mode supplies the lineup, as the
  [intercept design](intercept.md#model-picker) describes. With
  `ANTHROPIC_BASE_URL`, the owner adds it to the user settings.
- Every response a `POST` handler produces carries a `request-id` header, which
  Claude Code records as the transcript `requestId`. duolane keeps an upstream
  value and otherwise sets its own, before the body streams.
- `/monitor` returns wall-clock timestamps, elapsed durations, and computed
  rates, never monotonic clock readings.

## Claude route

duolane relays the original body and headers to `api.anthropic.com` and streams
the reply back. It stores no Anthropic credential. Claude Code keeps using its
subscription login when only `ANTHROPIC_BASE_URL` is set. An
`ANTHROPIC_AUTH_TOKEN` or approved `ANTHROPIC_API_KEY` replaces that login
([gateway docs](https://code.claude.com/docs/en/llm-gateway),
[connection docs](https://code.claude.com/docs/en/llm-gateway-connect.md)).

The relay keeps the original path, query, and body bytes. It removes these
request headers:

- `host`, hop-by-hop headers, and `content-length`, which the client recomputes;
- `accept-encoding`, so Anthropic answers without compression, because duolane
  never decompresses and must not relay a body under a stale encoding; and
- the inbound token header.

It removes hop-by-hop headers, `content-length`, and `content-encoding` from the
response. `anthropic-*`, `request-id`, and rate limit headers reach Claude Code.

## Codex route

duolane translates Messages requests into Responses API requests and translates
the stream back. It uses the login that the Codex CLI stored, and has no Codex
login of its own. A token refresh writes the rotated tokens back so the Codex
CLI keeps working. The [security design](security.md#credentials) owns these
rules, and the [Codex route design](codex.md) owns translation, lanes, retries,
and errors.

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

An empty block that would become reasoning text is removed. On the Claude route,
if that empties an assistant message, the message keeps one reasoning text block
with empty text, so the conversation stays valid.

## Invariants

1. **The Claude route is byte-exact.** A body with no block to convert is
   forwarded unchanged. A conversion replaces only the bytes of the affected
   blocks. Anthropic binds thinking signatures to earlier content, and rewriting
   earlier messages can cause a rejection, after which Claude Code removes
   earlier thinking from the session
   ([gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol)).
2. **Conversions are deterministic.** The same history produces the same bytes
   on every turn, so the prompt cache and Anthropic's prefix check keep working.
3. **Encrypted Codex reasoning in a duolane signature never reaches the
   Anthropic Messages API.** The Claude route refuses a body it cannot parse, a
   body with duplicate keys in a message or block, and any block that still
   carries a duolane signature after conversion. The transcript that Remote
   Control mirrors in
   [intercept mode](intercept.md#requests-on-the-intercepted-host) is outside
   this rule.
4. **Encrypted reasoning returns only to the account that produced it.**
5. **Stream failures are visible.** An interrupted or incomplete upstream stream
   is never reported as a successful turn, and a tool call with incomplete
   arguments is never reported as complete. A stop at `max_tokens` is reported
   as that stop.
6. **Unsupported content is never dropped silently.** A message content block
   the backend cannot carry produces an explicit marker or an error. Removing
   empty thinking is the only exception. Tool definitions follow the
   [Codex route rules](codex.md#request-translation) instead.

## Live results

Checked on 2026-11-20 with Claude Code 2.1.328 against the owner's accounts:

- A Codex model (`gpt-6.2-sol`) answers through `ANTHROPIC_BASE_URL` and through
  intercept mode.
- One conversation switched from Opus to Sol and back, with a `Read` tool call
  on each turn. Each model saw the other's turns and tool results.
- Prompt caches hit on both sides: the Sol turn read 21,632 cached tokens from
  Codex, and the return to Opus read 34,393 from Anthropic.
- Claude Code sends effort for a Codex model: `--effort low` and `high` reached
  the Codex route.
- A `modelPicker` lineup lists the Codex models in `/model` in the terminal. The
  Remote Control model list on a phone does not show them; `/model <id>` from
  the phone works.
- Not checked: a token refresh, the `originator` and `User-Agent` values, a
  context window overflow, and a switch while a tool call is pending.

## Unverified behavior

These inherited assumptions need a live check:

- Switching backends while a tool call is pending.
- Claude Code recording `request-id` as `requestId`.
- Claude Code running web search through a client-side `WebSearch` tool and an
  isolated inner request, which would keep hosted search blocks out of the outer
  transcript. The Claude route logs `hosted_web_search_in_history` if such
  blocks appear.
