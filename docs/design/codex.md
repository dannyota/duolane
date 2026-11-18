# Codex route

The Codex route translates an Anthropic Messages request into an OpenAI
Responses API request for the ChatGPT Codex backend, then translates the stream
back. [Architecture](architecture.md) owns routing and the reasoning formats.
[Security](security.md) owns credentials and limits.

## Models

- A built-in list of model ids is the baseline. The Codex CLI's
  `models_cache.json`, read from the same Codex home as `auth.json`, extends it
  so new models work without a duolane release.
- The cache is advisory. duolane reads no credential from it and makes no
  network request for it. The backend still decides access.
- duolane checks the cache file at most once per second and parses it only when
  its metadata changes. A missing or invalid file removes the extension, never
  the baseline.
- A catalog entry is skipped when it is malformed, or when its id could be
  mistaken for a Claude alias, a tier suffix, or provider syntax.
- Entries listed in the Codex picker and usable through the API are advertised.
  Other usable entries are accepted by exact id but not advertised.
- Each Claude alias has an ordered list of Codex targets. The first one the
  account's catalog lists wins, and the first entry applies without a catalog.

## Lanes

Codex serves two lanes: the full Responses API and Responses Lite.

- A catalog field marks a model as Lite. The field defaults to false.
- Requests with hosted `web_search` use the full lane, because Lite accepts only
  function and custom tools.
- Lite-only models, such as the Luna family, do not exist on the full lane. A
  hosted search request for one moves to its nearest full-lane model.
- Lite rejects `parallel_tool_calls: true`, so duolane sends `false` there.

## Service tiers

- The suffix `-fast` on any model requests the priority tier. `-ultrafast`
  exists only on models whose catalog entry offers that tier.
- The backend serves an unavailable tier at standard speed without an error, so
  duolane narrows a requested tier to one the model offers. Ultrafast falls back
  to priority.
- A configured tier wins over the tier implied by the model name. A tier suffix
  on an override model wins over the requested suffix.

## Request translation

- Reasoning output and encrypted reasoning are requested only when the resolved
  effort is not `none`. An explicit `none` still names the effort so it
  overrides the backend default.
- A `tool_choice` that forces `web_search` without a registered search tool is
  dropped, because the backend fails such a request.
- `pattern` keywords are removed from tool schemas. OpenAI's regular expression
  dialect differs from the clients' dialects, so duolane accepts weaker argument
  validation rather than guessing. Only schema keywords are visited, so
  defaults, examples, and property names stay intact.
- Claude Code's `DeferredToolPlaceholder` stub exists only to keep Anthropic's
  deferred loading on, and is dropped.
- On the Lite lane, a deferred tool that a `tool_reference` loaded is moved from
  the tool list to the point in the history where it loaded. This keeps the
  cached prefix identical across loads. Deferred tools with no visible load stay
  in the tool list so they remain callable.
- The `session-id` header matches the one the official Codex client sends.
- WebSocket requests omit the `stream` field.

## Compaction requests

Claude Code's conversation summary request is recognized by fixed markers in its
system prompt or final user message. Summarizing needs little reasoning, so the
request's effort is capped. `DUOLANE_COMPACT_EFFORT` sets the cap: `low` by
default, `off` to disable it, or any effort level. A request with no effort
takes the cap, and a request already below the cap keeps its effort.

## Transport and retries

- `DUOLANE_CODEX_TRANSPORT` selects `http`, `websocket`, or `auto`. `auto`
  prefers WebSocket and falls back to HTTP before a request is sent.
- Codex sends response headers only after the model produces its first output,
  so the wait depends on how much reasoning a request needs. The wait is 5
  minutes by default. A configured value below 1 second is ignored rather than
  clamped, so a typo cannot hide behind the default.
- duolane holds protocol framing until the stream produces real output. Before
  that point a retry is invisible to the client. After it, duolane never
  retries.
- The HTTP reader runs its own bounded retries. They are not multiplied by the
  WebSocket retry policy.
- An unclassified `response.failed` event is retryable, and never becomes a
  successful turn.

## Empty completions

A successful terminal event with no text, thinking, tool call, or search would
become an empty turn.

- duolane retries such a completion with full context.
- An empty completion is accepted only after one retry, and only when the last
  exchange resolved every tool call. Text in that exchange may carry new
  instructions, so it disqualifies the acceptance.
- An empty completion with a missing or failed terminal status is never
  accepted.
- A transport failure resets the count of consecutive empty completions.
- When retries run out, duolane returns HTTP 503 with `x-should-retry: false`.
  duolane already spent its retry budget, and Claude Code would otherwise repeat
  the whole loop up to ten times.

## Usage limits

Codex reports a spent subscription window as `usage_limit_reached`, with reset
times in the error body and in `X-Codex-*` headers. The limiting window is the
one whose reset time matches the error's own.

duolane answers with HTTP 429, `x-should-retry: false`, and the Anthropic
headers Claude Code reads: `anthropic-ratelimit-unified-status`,
`-unified-reset`, and `-unified-representative-claim`. It sends no
`Retry-After`, because clients sleep for its full value and the window reopens
hours later.

## Responses

- A `ping` event after the terminal event is ignored. Any other event after the
  terminal event is invalid.
- The latest `codex.rate_limits` event is attached to the final usage block as a
  duolane extension that is not part of Anthropic's schema.
- Reasoning tokens are reported as `thinking_tokens`, the name Claude Code
  records for Anthropic models.
- Live streams report an estimated input token count first, so Claude Code's
  subagent counter moves, and the backend's count replaces it at the end.

## Token counting

`count_tokens` on the Codex route uses the `o200k_base` tokenizer for text and
local estimates for images, encrypted reasoning, and protocol framing. The
result is an estimate, not a billing count.
