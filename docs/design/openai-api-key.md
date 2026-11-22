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
- Without an order, a lone credential is used alone. When both exist, duolane
  uses `chatgpt` only and says at startup and in `codex auth status` that the
  key is unused until an order is chosen. duolane never adds API billing on its
  own.
- A source in the order without a credential is skipped, with a startup warning.
  When no source is usable, a Codex request fails as it does today, and the
  message names the login and key commands.
- duolane reads the order and the key at startup. The commands below say that a
  running proxy must restart to apply a change.

## Commands

- `duolane codex auth key set` reads the key from a hidden terminal prompt, or
  from standard input when that is not a terminal. It never accepts the key as
  an argument, which would leave it in shell history and process lists.
- `key set` trims surrounding whitespace and rejects an empty value, a value
  over 512 bytes, or any character outside printable ASCII. It then checks the
  key with `GET /v1/models`; a 401 is refused and nothing is saved.
  `--no-verify` skips the check for offline setup.
- When `key set` runs in a terminal, a ChatGPT login exists, and no order is
  set, it asks which order to save: ChatGPT then API key, API key then ChatGPT,
  API key only, or ChatGPT only.
- `duolane codex auth key remove` deletes the stored key. It does not touch the
  environment or the order; a listed `api-key` source is then skipped.
- `duolane codex auth order` prints the effective order and where it came from.
  `duolane codex auth order <source>...` saves `codex.authOrder`, and `--reset`
  removes it. Saving keeps every other key in `config.json`, writes atomically,
  and warns when `DUOLANE_CODEX_AUTH_ORDER` overrides the file.
- `duolane codex auth status` adds the key's origin (`stored`, `environment`, or
  `none`) and the effective order. It never prints any part of the key.

## Key storage

- `key set` writes the key, alone on one line, to
  `<config dir>/credentials/openai-api-key`. The `credentials` directory is mode
  `0700` and the file `0600`, written with the atomic private-file writer the
  Codex login already uses.
- Before reading the file, duolane refuses a symlink, a file another user owns,
  or a file readable by group or others, and says how to fix it. On Windows the
  file lives under `%APPDATA%\duolane` with the profile's default access rules.
- `DUOLANE_OPENAI_API_KEY` overrides the stored key. duolane ignores
  `OPENAI_API_KEY` on purpose: a shell may export it for another tool or
  organization, and duolane must not bill a key the user did not give it.
- The key goes only to the configured API base URL, in the `Authorization`
  header. It never appears in logs, captures, the monitor, error messages, or
  terminal output. Existing redaction already covers the header.

## Requests with the API key

- `DUOLANE_OPENAI_BASE_URL` or `codex.apiBaseUrl` sets the base URL, default
  `https://api.openai.com/v1`, with the same checks as the Codex base URL.
- Requests send `Authorization: Bearer <key>` and none of the ChatGPT-only
  headers (`ChatGPT-Account-Id`, `originator`, and the Codex session and window
  headers). `store` stays `false`; the API then returns encrypted reasoning
  without asking
  ([reasoning](https://developers.openai.com/api/docs/guides/reasoning#preserve-reasoning-without-stored-responses)).
- The API has one Responses endpoint, so the Lite lane rules do not apply:
  parallel tool calls stay as requested, deferred tools stay in the tool list,
  and a Luna search request keeps its Luna model.
- Transport is HTTP. WebSocket mode and `previous_response_id` continuation stay
  with the `chatgpt` source until a live check confirms them on the API.
- Codex family aliases resolve against `GET /v1/models` for the key, fetched
  once at startup and at most once per hour after that. A failed fetch keeps the
  last list, then the built-in versions. The API decides access for exact ids.
- A `-fast` suffix sends `service_tier: "priority"`. `-ultrafast` narrows to
  priority.
- Hosted web search uses the API's `web_search` tool in the same Responses
  request. Claude Code's standalone search request becomes a Responses request
  with that tool, and its result is translated into the same Anthropic search
  response the `chatgpt` source returns.
- The images and transcription routes stay with the `chatgpt` source in this
  version. With only `api-key` in the order they answer an error that says so.
- Token counting is local and does not change.

## Moving to the next source

A request tries the sources in order. Within a source, the
[Codex retry rules](codex.md#transport-and-retries) apply unchanged. duolane
moves to the next source only before any client output and before the source
reports hosted-tool execution, and only for these failures:

- a spent plan window (`usage_limit_reached`) on `chatgpt`, or exhausted credit
  (`insufficient_quota`) on `api-key`;
- a credential the source rejects: a ChatGPT login a refresh cannot repair, or
  an API key answered with 401;
- transient failures (HTTP 5xx, connection errors, API rate limiting) after the
  source spent its retry budget.

A request does not move on for an invalid request, a context window overflow, a
header timeout, or any failure after output, because the next source would fail
the same way or repeat work the client already saw.

- The request is translated again for the next source, as for an account change.
  Each source has its own retry budget, so one request makes at most two budgets
  of attempts.
- A spent window or exhausted credit marks the source as skipped until its reset
  time, or for one hour when no reset time is known. A rejected credential is
  skipped until restart. Transient failures leave no mark.
- When every source fails, the client receives the error of the last source
  tried.

## Reasoning

The `api-key` source is its own account for
[reasoning portability](architecture.md#reasoning-across-backends). Its account
tag is the first 16 hexadecimal characters of the SHA-256 of `openai-api-key:`
followed by the key. The signature format does not change. Encrypted reasoning
from one source reaches the other as reasoning text, and a new key is a new
account. Connection reuse and compaction state stay bound to the account that
created them.

## Monitor

Each Codex request shows its source, `chatgpt` or `api key`. A request that
moved on also shows the source it left and why, for example
`from chatgpt: usage limit`. duolane does not estimate cost.

## Non-goals

- Several keys, OpenAI organization or project headers, and Azure OpenAI.
- A different order per model or per session.
- Spending limits. The OpenAI Platform's own budget settings apply.
- Images and transcription with an API key, WebSocket with an API key, and
  reading the key from the Codex CLI's API-key login.

## Testing

- Hermetic tests use a mock API upstream, toy keys, and a temporary config
  directory. They cover order parsing, the startup rules, the file permission
  checks, each move-on failure and each failure that must not move on, the skip
  marks, and that no test output contains the key.
- Live checks use the owner's key with the owner's approval for each run: one
  Messages request per family alias, hosted search, a standalone search, and a
  move from an invalid `DUOLANE_OPENAI_API_KEY` to `chatgpt` with the order
  `api-key,chatgpt`.

## Unverified behavior

- The API's error codes and statuses for spent credit, a bad key, and rate
  limiting.
- That the API accepts duolane's Responses body unchanged, including
  `prompt_cache_key`, `include`, reasoning summaries, and `text.verbosity`.
- That the API rejects encrypted reasoning from the ChatGPT backend, which the
  separate account tag assumes.
- The API's `service_tier` value for priority processing.
