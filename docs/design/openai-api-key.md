# OpenAI API key

A Codex request can use an OpenAI API key instead of, or as well as, the ChatGPT
login. [ADR 0002](../adr/0002-claude-and-codex-only.md) records the decision.
This design owns the credential sources, their order, the key commands, key
storage, and moving a request from one source to the next. The
[Codex route](codex.md) owns translation and retries within one source, and
[Security](security.md) owns the rules every credential follows.

## Sources

| Source    | Credential                             | Upstream                    | Billing                            |
| --------- | -------------------------------------- | --------------------------- | ---------------------------------- |
| `chatgpt` | Codex login in `$CODEX_HOME/auth.json` | ChatGPT Codex backend       | ChatGPT plan limits                |
| `api-key` | OpenAI API key                         | `https://api.openai.com/v1` | OpenAI Platform account, API rates |

## Order

- `DUOLANE_CODEX_AUTH_ORDER`, or `codex.authOrder` in `config.json`, lists the
  sources a Codex request tries, first to last. The environment variable is a
  comma-separated list; the config key is a JSON array. Each value is `chatgpt`
  or `api-key`, at most once. The environment wins.
- An invalid value stops `duolane serve` with an error that names the setting.
  Other settings fall through on invalid values, but a wrong order could spend
  money or block every Codex request, so it must not pass silently.
- Without an order, the order is `chatgpt`. The key is used only when an order
  the user saved or exported names `api-key`, whatever credentials exist, so
  logging out of ChatGPT never turns API billing on. When a key exists but the
  order leaves it out, startup and `codex auth status` say so.
- A source in the order without a credential is skipped, with a startup warning.
  When no source is usable, a Codex request fails as it does today, and the
  message names the login, key, and order commands.
- duolane reads the order and the key at startup, and fetches `GET /v1/models`
  only when the order names `api-key`. The commands below say that a running
  proxy must restart to apply a change.

## Routes that switch sources

Only `POST /v1/messages` and `POST /v1/chat/completions` use the order. duolane
translates those per request and can translate again for another source.

`POST /v1/responses`, images, and transcription stay with the `chatgpt` source
in this version. A native Responses request carries the caller's
`previous_response_id`, `store`, and encrypted items, which belong to one
account. When the order has no usable `chatgpt` source, these routes answer HTTP
400 with a message that says they need the ChatGPT login.

## Commands

- `duolane codex auth key set` reads the key from a hidden terminal prompt, or
  from standard input when that is not a terminal. It never accepts the key as
  an argument, which would leave it in shell history and process lists.
- `key set` trims surrounding whitespace and rejects an empty value, a value
  over 512 bytes, or any character outside printable ASCII. It then sends
  `GET /v1/models` with the key and saves only after HTTP 200. It reads at most
  1 MiB of that response. A 401, 403, 429, 5xx, timeout, or oversized answer
  saves nothing, leaves any stored key unchanged, prints a local message for
  that class of failure, and points to `--no-verify`, which skips the check for
  offline setup.
- When `key set` runs in a terminal and no order is set, it asks which order to
  save: API key only, ChatGPT then API key, API key then ChatGPT, or ChatGPT
  only. Outside a terminal it saves no order and prints the order command to
  run.
- `duolane codex auth key remove` deletes the stored key and leaves the order.
  If `DUOLANE_OPENAI_API_KEY` is still set, it says that the environment key
  stays in use after a restart whenever the order names `api-key`.
- `duolane codex auth order` prints the effective order and where it came from.
  `duolane codex auth order <source>...` saves `codex.authOrder`, and `--reset`
  removes it. Saving keeps every other key in `config.json`, writes atomically,
  and warns when `DUOLANE_CODEX_AUTH_ORDER` overrides the file.
- `duolane codex auth status` adds the key's origin (`stored`, `environment`, or
  `none`), the effective order, and whether the order uses the key. It never
  prints any part of the key.

## Key storage

- `key set` writes the key, alone on one line, to
  `<config dir>/credentials/openai-api-key` on macOS and Linux. It uses the
  checked-directory pattern of the [Claude accounts store](claude-accounts.md):
  every directory on the path is opened without following symlinks and verified,
  and `credentials` must be owned by the user with mode `0700`. The file is
  created next to its final name with mode `0600` and renamed into place
  relative to that open directory.
- duolane reads the file only through the verified directory, without following
  a symlink, and only when it is a regular file owned by the user with no
  permission bits for group or others. It reads at most 4 KiB. A file that fails
  a check is refused with a message that says how to fix it.
- Windows has no stored key in this version, because the checked-directory store
  exists only for macOS and Linux. `DUOLANE_OPENAI_API_KEY` is the only source
  there, and `key set` says so.
- `DUOLANE_OPENAI_API_KEY` overrides the stored key. duolane ignores
  `OPENAI_API_KEY` on purpose: a shell may export it for another tool or
  organization, and duolane must not bill a key the user did not give it.
- The key goes only to the configured API base URL, in the `Authorization`
  header, which existing redaction covers.

## Errors from the API

The API can echo request data, including a key, in its error text. duolane never
forwards, logs, captures, or shows API error text. It classifies an API failure
by HTTP status and an allowlist of structured `error.code` and `error.type`
values, then builds its own message. The same rule applies to `key set`. Traffic
capture records only the status and the allowlisted code for API errors.

## Requests with the API key

- `DUOLANE_OPENAI_BASE_URL` or `codex.apiBaseUrl` sets the base URL, default
  `https://api.openai.com/v1`, with the same checks as the Codex base URL.
- Requests send `Authorization: Bearer <key>` and none of the ChatGPT-only
  headers (`ChatGPT-Account-Id`, `originator`, and the Codex session and window
  headers). `store` is always `false`; the API then returns encrypted reasoning
  without asking
  ([reasoning](https://developers.openai.com/api/docs/guides/reasoning#preserve-reasoning-without-stored-responses)).
- On the Messages route, the client's `max_tokens` becomes `max_output_tokens`,
  because API output is billed per token. The ChatGPT source keeps dropping it,
  as the Codex route does today. The Chat Completions route keeps rejecting
  `max_tokens` and `max_completion_tokens` before dispatch for both sources, so
  a request is valid or invalid the same way on each.
- The API has one Responses endpoint, so the Lite lane rules do not apply:
  parallel tool calls stay as requested, deferred tools stay in the tool list,
  and a Luna search request keeps its Luna model.
- Transport is HTTP. WebSocket mode and `previous_response_id` continuation stay
  with the `chatgpt` source until a live check confirms them on the API.
- The `api-key` catalog is the ids from `GET /v1/models` that are built-in Codex
  ids or match the [family candidate rule](codex.md#model-catalog),
  `gpt-<version>-<family>`. Other API models are ignored, because the list does
  not say which ones serve Responses. duolane fetches it at startup and at most
  once per hour, and a failed fetch keeps the last list. Only before any list
  has arrived do the built-in Codex versions stand in, as tentative defaults; a
  built-in id absent from a successful list is unavailable.
- A Codex family alias resolves against the catalog of the source that sends the
  request, and again after a switch; an exact id stays fixed. A source whose
  known catalog lacks an exact id is skipped before sending; a source with no
  catalog yet is tried. Model discovery lists only ids a source in the order can
  serve.
- A `-fast` suffix asks for priority processing. `-ultrafast` narrows to
  priority.
- Hosted web search uses the API's `web_search` tool in the same Responses
  request.
- Claude Code's standalone search becomes a Responses request that forces
  exactly one `web_search` call, carries the allowed and blocked domains, and
  asks for `web_search_call.action.sources`
  ([web search sources](https://developers.openai.com/api/docs/guides/tools-web-search#sources)).
  The sources become the same Anthropic search result blocks the `chatgpt`
  source returns. A response without a search call is an error.

## Moving to the next source

A request tries the sources in order. duolane moves to the next source only
before any client output, only while the request is still safe to replay, and
only for these failures:

- a spent plan window (`usage_limit_reached`) on `chatgpt`, or exhausted credit
  (`insufficient_quota`) on `api-key`;
- a rejected credential: a refresh token the token endpoint rejected, or HTTP
  401 for the API key. A refresh that failed on the network, timed out, could
  not take the lock, or got HTTP 408, 429, or 5xx from the token endpoint is
  transient, as the Codex login manager classifies it today;
- a transient failure after the source spent its retry budget, for a request
  without hosted tools. The same-source retry rules already replay such a
  request after a post-send drop. The first source may still have finished work
  the client never saw, so a fallback the user enabled can repeat model work
  and, on the `api-key` source, incur charges.

Replay eligibility belongs to the whole request:

- Once any attempt reports hosted-tool execution, neither a same-source retry
  nor a switch happens, even if a later error reports a spent window or a
  rejected credential.
- A request with a hosted tool, including every standalone search, that was sent
  and whose outcome is unknown (a dropped connection, a timeout, or HTTP 5xx)
  never switches sources. On the `api-key` source, where a lost search is
  billed, it is not retried either. On the `chatgpt` source the
  [Codex retry rules](codex.md#transport-and-retries) apply.
- A request with a hosted tool can still move on for a spent window, credit, or
  rejected credential that the upstream reported before any execution, or for a
  failure before it was sent.

A request never moves on for an invalid request, a context window overflow, a
header timeout, empty completions after their retries, or any failure after
output, because the next source would fail the same way or repeat work the
client already saw.

- The Codex route reports a typed failure with its replay eligibility to the
  source selector, which never infers either from the final HTTP status.
- Each source has its own [recovery budget](codex.md#transport-and-retries):
  three retries within its own 60-second window. One request makes at most two
  budgets of attempts.
- The request is translated again for the next source, as for an account change.
- A spent window or exhausted credit skips that credential until its reset time,
  or for one hour when no reset time is known. A rejected credential is skipped
  until it changes: a new ChatGPT login or refresh token, or a different key.
  Marks belong to the credential, so a change clears them. Transient failures
  leave no mark.
- When every source fails, the client receives the error of the last source
  tried. When every source is already skipped, duolane answers as the first
  skipped source would: HTTP 429 with the
  [usage limit headers](codex.md#usage-limits) and the earliest reset time for a
  spent window or credit, or HTTP 401 that names the login and key commands for
  a rejected credential.

## Reasoning

The `api-key` source is its own account for
[reasoning portability](architecture.md#reasoning-across-backends), which
defines its account tag. Encrypted reasoning never crosses sources: it reaches
the other source as reasoning text, whatever either upstream would accept. A new
key is a new account. Connection reuse and compaction state stay bound to the
account that created them.

## Monitor

Each Codex request shows its source, `chatgpt` or `api key`. A request that
moved on also shows the source it left and why, for example
`from chatgpt: usage limit`. duolane does not estimate cost.

## Non-goals

- Several keys, OpenAI organization or project headers, and Azure OpenAI.
- A different order per model or per session.
- Spending limits. The OpenAI Platform's own budget settings apply.
- The API key on native Responses, images, transcription, or WebSocket, a stored
  key on Windows, and reading the key from the Codex CLI's API-key login.

## Testing

- Hermetic tests use a mock API upstream, toy keys, and a temporary config
  directory. They cover order parsing and the default, the startup rules, the
  file checks, each move-on failure and each failure that must not move on,
  hosted-tool requests, the skip marks and their reset on a credential change,
  error text that is never forwarded, and that no test output contains the key.
- Live checks use the owner's key with the owner's approval for each run: one
  Messages request per family alias, hosted search, a standalone search,
  `max_tokens`, and a move from an invalid `DUOLANE_OPENAI_API_KEY` to `chatgpt`
  with the order `api-key,chatgpt`.

## Live results

Checked on 2026-11-23 with the owner's key against `api.openai.com`, on a test
proxy with its own config and state directories:

- With the order `api-key`, `GET /v1/models` returned the account's catalog, and
  one Messages request each for `sol`, `terra`, `luna`, and `astra` returned
  HTTP 200 with duolane's Responses body unchanged.
- A streamed Messages request completed, and `max_tokens: 16` ended with
  `stop_reason: max_tokens`.
- A hosted web search and a standalone search with `allowed_domains` each
  returned search result blocks.
- An invalid key gets HTTP 401 with `type: invalid_request_error` and
  `code: invalid_api_key`. With the order `api-key,chatgpt` the request moved to
  `chatgpt`, and the monitor showed
  `chatgpt (from api key: rejected credential)`.

## Unverified behavior

- The API's error codes and statuses for spent credit and rate limiting.
- End to end: `service_tier: "priority"`
  ([priority processing](https://developers.openai.com/api/docs/guides/priority-processing))
  and `filters.blocked_domains` for `web_search` are documented but not yet
  tried live.
