# Architecture

duolane is a local HTTP proxy for Claude Code. Claude Code speaks the Anthropic
Messages API, so duolane accepts that API and picks a backend for each request
from the model name. One session can use a Claude subscription and a ChatGPT
subscription and switch between them mid-conversation.

## Routing

| Requested model                   | Backend   | Behavior                                 |
| --------------------------------- | --------- | ---------------------------------------- |
| `claude-*` and the Claude aliases | Anthropic | Transparent passthrough                  |
| Codex model ids                   | Codex     | Translate to the OpenAI Responses API    |
| Anything else                     | None      | HTTP 400 that lists the supported models |

Codex model ids come from a built-in list plus the listed, API-capable entries
in the Codex CLI's `models_cache.json`.

## Claude route

duolane relays the original body and headers to `api.anthropic.com` and streams
the reply back. It stores no Anthropic credential. Claude Code forwards its own
subscription login only when `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` are
unset, so setting either one breaks this route.

## Codex route

duolane translates Messages requests into Responses API requests and translates
the stream back. It uses the login that the Codex CLI stored in
`~/.codex/auth.json`, and has no Codex login of its own. A token refresh writes
the rotated tokens back to that file so the Codex CLI keeps working.

## Invariants

1. **The Claude route is byte-exact for normal traffic.** Re-serializing a body
   changes the prompt cache key. duolane rewrites a body only when it contains
   a thinking block that Anthropic did not sign.
2. **Reasoning stays portable.** Only a thinking block with an Anthropic-issued
   signature reaches Anthropic as a thinking block. A block with an empty
   signature, or a signature duolane issued for Codex reasoning, becomes text
   wrapped in the shared reasoning tags. Encrypted Codex reasoning never reaches
   Anthropic.
3. **Rewrites are deterministic.** The same history produces the same bytes on
   every turn, so the rewritten prefix stays cacheable.
4. **Codex replay stays within Codex.** duolane may replay encrypted reasoning
   to Codex only from a signature it issued, and only for the account that
   produced it.
5. **Stream failures are visible.** An interrupted or incomplete upstream stream
   is never reported as a successful turn, and a tool call is never emitted with
   truncated arguments.
6. **Unsupported content is never dropped silently.** A block the backend cannot
   carry produces an explicit marker or an error.

## Known limitation

Switching backends in the middle of an active tool call can fail. Anthropic
requires a leading signed thinking block in that position, and no valid
signature exists for reasoning from another backend.

Claude Code runs web search through a client-side `WebSearch` tool and an
isolated inner request, so hosted search blocks are not expected in the outer
transcript. The Claude route logs `hosted_web_search_in_history` if that changes.
