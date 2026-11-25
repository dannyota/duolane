# Codex route

The Codex route translates an Anthropic Messages request into an OpenAI
Responses API request for the ChatGPT Codex backend, then translates the stream
back. [Architecture](architecture.md#routing) owns which model ids route here
and the reasoning formats. [Security](security.md) owns credentials and limits.

## Model catalog

- The Codex CLI's `models_cache.json`, in the same Codex home as `auth.json`,
  extends the built-in model list. duolane reads no credential from it and makes
  no network request for it, and the backend still decides access.
- duolane checks the file at most once per second and parses it only when its
  metadata changes. A missing or invalid file removes the extension, never the
  built-in list.
- A catalog entry is skipped when it is malformed, or when its id could be
  mistaken for a Claude or Codex family alias, a tier suffix, or provider
  syntax.
- Each Claude alias has an ordered list of Codex targets. The first one the
  account's catalog lists wins, and the first entry applies without a catalog.
- A Codex family alias selects the highest numeric version of the same family
  among listed, API-capable entries. A candidate id is `gpt-<version>-<family>`
  with one to three numeric version components. Missing components count as
  zero. Ties select the id that sorts first. If there is no candidate for that
  family, use its highest built-in version. Select once per request and hold the
  selection across retries. Model overrides keep their existing precedence. A
  [tier suffix](#service-tiers) on a family alias applies to the selected model.

## Lanes

Codex serves two lanes: the full Responses API and Responses Lite.

- A built-in list says which built-in models use Lite. For any other id, the
  catalog entry's Lite field decides, and it defaults to false.
- `DUOLANE_CODEX_FULL_LANE` moves the built-in Lite models that are known to
  exist on the full lane off Lite. It never overrides the catalog for other ids,
  because their full-lane availability is unknown. On Chat Completions, only the
  Codex family aliases honor this setting.
- Requests with hosted `web_search` use the full lane, because Lite accepts only
  function and custom tools.
- The Luna family exists only on Lite. A hosted search request for a Luna model
  moves to its nearest full-lane model.
- Lite rejects `parallel_tool_calls: true`, so duolane sends `false` there.

Standalone search uses the validated conversation identity for its owner. Main
keeps its session id; each Agent gets a stable SHA-256 owner from its session
and Agent ids. Missing or invalid identity gets a fresh UUID. The body id and
`session-id`, `x-client-request-id`, and `x-codex-window-id` headers share this
owner, with `:0` appended to the window id. Agent ids are not sent upstream.

## Service tiers

- The suffix `-fast` on an allowed exact Codex model id or on a family alias
  requests the priority tier. On an exact id, `-ultrafast` exists only on a
  built-in list of models. A family alias always accepts `-ultrafast`, because
  its target changes with the catalog. The Claude aliases take no tier suffix.
- duolane narrows a requested tier to one the model offers, because the backend
  serves an unavailable tier at standard speed without an error. Ultrafast falls
  back to priority.
- A configured tier wins over the tier implied by the model name. A tier suffix
  on an override model wins over the requested suffix.

## Request translation

- A content block the translation cannot carry becomes the text
  `[unsupported content block omitted: <type>]`, so the loss stays visible.
- For auto-review classifiers routed to Codex, `DUOLANE_AUTO_REVIEW_EFFORT` or
  `autoReviewEffort` overrides normal effort resolution. The environment wins;
  invalid values are ignored. Claude routes and token counting are unchanged.
- Reasoning output and encrypted reasoning are requested only when the resolved
  effort is not `none`. An explicit `none` still names the effort so it
  overrides the backend default.
- A `tool_choice` that forces `web_search`, directly or through an allowed-tools
  list, without a registered search tool becomes `auto`, because the backend
  fails such a request.
- `pattern` keywords are removed from tool schemas, because OpenAI's regular
  expression dialect differs from the clients' dialects. duolane accepts weaker
  argument validation rather than guessing. Only schema keywords are visited, so
  defaults, examples, and property names stay intact.
- Claude Code's `DeferredToolPlaceholder` stub exists only to keep Anthropic's
  deferred loading on, and is dropped.
- On the Lite lane, a deferred tool that a `tool_reference` loaded moves from
  the tool list to the point in the history where it loaded. The cached prefix
  then stays identical across loads. Deferred tools with no visible load stay in
  the tool list so they remain callable.
- The `session-id` header matches the one the official Codex client sends.
- WebSocket requests omit the `stream` field.

## Continuation

When `DUOLANE_CODEX_PREVIOUS_RESPONSE_ID` is on and a WebSocket request extends
the transcript of a stored response, duolane sends `previous_response_id` with
only the new items.

- `function_call.arguments` are compared as JSON values. The stored transcript
  keeps the text the model streamed, while the next request re-serializes the
  client's parsed input.
- Stale continuation state is dropped before a retry picks another transport or
  connection.

## Compaction requests

Claude Code's conversation summary request is recognized by fixed markers in its
system prompt or final user message, ignoring trailing system messages.
Summarizing needs little reasoning, so the request's effort is capped.
`DUOLANE_COMPACT_EFFORT` sets the cap: `low` by default, `off` to disable it, or
any effort level. A request with no effort takes the cap, and a request already
below the cap keeps its effort. With Auto transport, summary requests use HTTP
before any WebSocket send, regardless of compression. Explicit WebSocket and
native server compaction retain their configured transport.

## Transport and retries

- A WebSocket whose response completed returns to an idle pool, with or without
  [continuation](#continuation). The pool keeps one socket per conversation,
  account, and continuation setting, so a request with continuation off neither
  takes nor overwrites the slot of a socket that a stored response needs. The
  conversation's next request takes the socket out of the pool, so a socket
  carries one response at a time. Without continuation the reused socket carries
  the full context and no `previous_response_id`, which OpenAI's
  [WebSocket mode](https://developers.openai.com/api/docs/guides/websocket-mode)
  defines as the start of a new chain.
- A socket is bound to the credential that authenticated its handshake and to
  the handshake headers that carry identity, lane, and protocol. The access
  token, the WebSocket key, and the headers that only serve the HTTP upgrade are
  not compared. A request reuses a socket only when the credential and those
  headers equal its own, so a socket never serves another login, and a socket
  opened on the Lite lane never serves the full lane. A socket that does not
  match is closed.
- A pooled socket is closed instead of reused when it was opened more than 30
  minutes ago or does not answer a ping. The request then counts as a WebSocket
  attempt that failed before it was sent, and the rules below choose a new
  socket, HTTP, or an error.
- Reuse never changes how a request ends: errors, limits, and incomplete
  responses keep the results the rules below give them. It only decides what
  happens to the socket. A socket returns to the pool when its stream showed a
  `response.created` and then a terminal event that carries the same response id
  and reports a completed response. In every other case the socket is closed
  when the request ends, including a cancelled request and a stream without a
  terminal event.
- On a reused socket, a response starts only at a `response.created` whose id
  the socket has not completed before. An output event or a completed terminal
  event before that point, and a terminal event with another id after it, belong
  to an earlier response. duolane passes nothing from them to the client, closes
  the socket, and fails the attempt as a transport failure at that point, which
  the rules below retry only before output. A stale event that reports a hosted
  tool still counts as evidence that the tool ran, so the request is not
  replayed. Before `response.created`, an `error` event and a terminal event
  that reports a failure keep their own result, such as a usage limit, along
  with the evidence that the request was sent; the socket is closed.
- The pool holds at most the [idle sockets](security.md#limits) the limits
  allow, counted over all conversations. A socket that returns to the pool first
  closes the pooled sockets opened more than 30 minutes ago, and then, if the
  pool is full, the idle socket used least recently.
- Fresh WebSocket handshakes share a process-wide queue, with at most four in
  flight and a 30-second queue deadline. Starts are spaced by one second
  initially and at minimum. An origin upgrade 403 doubles spacing up to eight
  seconds and starts one three-second cooldown; further 403s during it do not
  stack. The origin's 403 headers start the cooldown even if its error body
  stalls. Each successful handshake halves spacing toward the floor. Proxy 403s
  do not affect pacing. Cancelled waiters release their place. A queue timeout
  returns a non-retryable overload error, unless a prior origin 403 must be
  preserved. Auto can still fall back to HTTP before send. The 403 retry and
  HTTP fallback spend the request's recovery budget. Lowering the floor awaits a
  live A/B check.
- `DUOLANE_CODEX_TRANSPORT` selects `http`, `websocket`, or `auto`. `auto`
  prefers WebSocket and falls back to HTTP only when WebSocket failed before the
  request was sent.
- `DUOLANE_CODEX_REQUEST_COMPRESSION` overrides `codex.requestCompression`. Use
  `gzip` to compress Responses and search requests sent over HTTP, or `off`
  (default) to send raw JSON. Invalid values fall through to the next source.
  Media endpoints and traffic capture are unchanged.
- With gzip and HTTP fallback enabled, Auto routes full-context requests whose
  serialized JSON exceeds `DUOLANE_CODEX_AUTO_WS_MAX_BYTES` or
  `codex.autoWebSocketMaxBytes` to HTTP before a WebSocket send. The default is
  4 MiB; `0` disables this rule. Live deltas stay on WebSocket. Buffered and
  streaming requests also apply the rule when a missing or dead continuation
  socket forces a pre-send full-context retry.
- Gzip reuses one encoded body across retries, with at most two compression jobs
  off the async workers. Serialization stops at 64 MiB, the largest existing
  request body limit, and an oversized request receives HTTP 413. `flate2`
  provides gzip through its pure-Rust backend without enabling response
  decompression.
- Codex sends response headers only after the model produces its first output,
  so the wait depends on how much reasoning a request needs.
  `DUOLANE_CODEX_HEADER_TIMEOUT_MS` sets the wait; the default is in
  [limits](security.md#limits). A value below 1 second is ignored rather than
  clamped, so a typo cannot hide behind the default.
- A header timeout is not retried. The request reached Codex, so a new attempt
  restarts the reasoning and times out the same way.
- Recovery shares at most three retries per request and
  [credential source](openai-api-key.md#moving-to-the-next-source), including at
  most two for empty completions. Its 60-second window starts at the first retry
  decision; a retry is not started if its backoff would end after the deadline.
  An in-flight attempt is not interrupted by this window.
- duolane holds protocol framing until the stream produces real output. Before
  that point a retry is invisible to the client. After it, duolane never
  retries.
- On HTTP, function-call starts and argument deltas stay private to the attempt.
  An `output_item.done` matched by `call_id` commits the whole tool block, even
  without an output index. A call commits once, with a completed status if one
  is present and a bounded JSON object for arguments. Empty arguments become
  `{}`. Unmatched argument events, uncommitted calls at a successful terminal
  event, and exceeded [buffer limits](security.md#limits) fail the stream. A
  drop before the commit can be retried if no other output was sent and Codex
  has not reported hosted-tool execution. HTTP assigns deferred block indexes
  when it emits the blocks. A tool commit closes any live text block first. If
  that text continues, it starts a new block.
- An unexpected EOF from the HTTP or WebSocket transport, including a TLS close
  without `close_notify`, is retryable before output. Streaming requests and
  buffered WebSockets also stop retrying once Codex reports that a hosted web
  search is in progress or has completed, even before client output. With an
  OpenAI API key, a hosted-tool request with an unknown outcome is not retried
  at all, as [replay eligibility](openai-api-key.md#moving-to-the-next-source)
  defines.
- After a full-context WebSocket retry, a repeated missing origin fails fast;
  transient failures still use the normal retry budget subject to the same
  no-replay rules.
- A streaming WebSocket send has a deadline and stops when the downstream
  disconnects. The socket is removed from the pool and closed.
- A failed read of an HTTP error body preserves the status from the headers,
  which still governs retries.
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
- When retries run out, duolane returns HTTP 503 with `x-should-retry: false`,
  because duolane already spent its retry budget.

## Usage limits

Codex reports a spent subscription window as `usage_limit_reached`, with reset
times in the error body and in `X-Codex-*` headers. The limiting window is the
one whose reset time matches the error's own. duolane does not retry it, because
the window reopens hours later.

duolane answers with HTTP 429, `x-should-retry: false`, and the Anthropic
headers Claude Code reads: `anthropic-ratelimit-unified-status`,
`-unified-reset`, and `-unified-representative-claim`. It sends no
`Retry-After`, because clients sleep for its full value.

A `codex.rate_limits` event that reports a spent window is not an error. The
account may still have credits, and the model then completes the response.

## Responses

- A `response.incomplete` with reason `max_output_tokens` ends the turn with
  `stop_reason: max_tokens`, or `finish_reason: length` on the chat route. Any
  other incomplete reason is a failure.
- A stream that ends while a tool call is still open, or that ends without a
  terminal event, is reported as an error, never as a finished turn. The one
  exception is a `Read` call whose complete arguments are followed by runaway
  whitespace: duolane ends the turn with that call once its arguments parse.
- The latest usable `codex.rate_limits` event is attached to the final usage
  block, as a duolane extension outside Anthropic's schema.
- Reasoning tokens are reported as `thinking_tokens`, the name Claude Code
  records for Anthropic models.
- Live streams first report an estimated input token count, so Claude Code's
  subagent counter moves, and the backend's count replaces it at the end.
- A `ping` event after the terminal event is ignored. Any other event after the
  terminal event is invalid.
- A context window overflow from Codex becomes HTTP 400 `invalid_request_error`.
  On the Messages route its message starts with `prompt is too long`, the
  wording the Anthropic API uses
  ([context windows](https://platform.claude.com/docs/en/build-with-claude/context-windows))
  and the one Claude Code compacts on
  ([errors](https://code.claude.com/docs/en/errors#prompt-is-too-long)),
  followed by the backend's text. HTTP 413 `request_too_large` stays for request
  bodies over duolane's size limits, as on the Anthropic API.

## Token counting

`count_tokens` on the Codex route uses the `o200k_base` tokenizer for text and
local estimates for images, encrypted reasoning, and protocol framing. The
result is an estimate, not a billing count.

## Unverified behavior

These statements come from inherited code and earlier observation. No
first-party source confirms them, so recheck them during live verification.

- The Codex backend: Lite's tool and `parallel_tool_calls` limits, the Luna
  family (the full lane answered 404 for a `-free` variant), standard-speed
  service for an unavailable tier, failure on a forced unregistered
  `web_search`, the regular expression dialect, the `session-id` header,
  response headers after first output, `usage_limit_reached` and its headers,
  and `codex.rate_limits` with remaining credits.
- Whether the live Codex endpoints accept gzip request bodies.
- Claude Code: the compaction markers, `DeferredToolPlaceholder`, repeating a
  `thinking_tokens`, the subagent counter, the `anthropic-ratelimit-unified-*`
  headers it reads, and sleeping for the full `Retry-After` value.
- Claude Code compacting on duolane's `prompt is too long` error.
- The context window the backend accepts for each model. Codex documents a
  default of 500,000 tokens for `gpt-6.2-sol` and 272,000 for `gpt-6.2-astra`,
  both raisable to 1 million
  ([Codex models](https://learn.chatgpt.com/docs/models)), and the Codex CLI
  catalog lists an 872,000-token maximum for both.
- The tolerated `ping` event was observed from another Responses provider.
- How often the WebSocket closes after a completed tool call but before the
  terminal event. duolane reports that as an error; if Claude Code runs a tool
  before the turn ends, its retry could run the tool twice. Count these cases
  before deciding on an exception.
