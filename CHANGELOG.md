# Changelog

## 0.6.2

### Fixed

- `duolane serve --intercept` creates private configuration directories even
  with `umask 0002`, so `duolane claude` no longer refuses a fresh install.
  Permission errors name the directory and explain how to fix it. Existing
  directories are not changed.

### Changed

- `claude-terra`, `claude-sol`, `claude-astra`, and `claude-luna` replace the
  old `anthropic-` aliases and still route to Codex. Add `-fast` to a family
  alias to request the priority tier on its latest model.

## 0.6.1

### Changed

- In intercept mode the proxy holds up to 2048 tunnels and 512 intercepted
  connections, up from 256 each, because programs that Claude Code starts send
  their HTTPS traffic through it. `duolane serve` raises its open-file limit up
  to 16384 and sizes both pools from the limit it gets, so a low limit gives
  smaller pools.

## 0.6.0

### Added

- An OpenAI API key can serve Codex models next to, or instead of, the ChatGPT
  login. `duolane codex auth key set` stores the key, `duolane codex auth order`
  chooses which source a request tries first, and `duolane codex auth status`
  shows both. The key is never used until the order names `api-key`, so a key
  alone does not turn API billing on. `OPENAI_API_KEY` is ignored; use
  `DUOLANE_OPENAI_API_KEY` or the stored key.
- With both sources in the order, `/v1/messages` moves to the next source when
  the first has spent its limit, rejects its credential, or runs out of retries,
  and only before any output reached the client. A request whose hosted web
  search may already have run is never sent again.
- The monitor shows the source of each Codex request, `chatgpt` or `api key`,
  and why a request moved on.
- `duolane claude` offers `[1m]` model names for Codex models that have a
  1M-token context window, so Claude Code no longer warns that the model is not
  recognized.

### Changed

- `/v1/chat/completions` uses the API key only when the order is `api-key`
  alone. With both sources in the order it uses the ChatGPT login.
- `/v1/responses`, images, and transcription need the ChatGPT login and answer
  HTTP 400 when the order leaves `chatgpt` out.
- `duolane claude` does not pass `DUOLANE_OPENAI_API_KEY` to Claude Code.

## 0.5.1

### Fixed

- A non-streaming Codex request is not sent again after the response confirmed
  that a hosted web search ran. A cut-off response after a hosted search now
  returns an error instead of running the search twice.

## 0.5.0

### Added

- `duolane accounts login [name]` signs in to a named Claude Code profile with
  Claude Code's own login. `duolane accounts use [name]` picks the profile for
  the next `duolane claude` launch; `default` returns to the original login. A
  named launch can resume a session from another profile in the same transcript
  file after you confirm sharing that project's history folder. macOS and Linux
  only. duolane never reads or copies Claude credentials.

## 0.4.1

### Added

- `duolane agents setup` writes Claude Code subagents `terra`, `sol`, `astra`,
  and `luna` that run on the newest Codex model of each family.
  `duolane agents remove` deletes the ones it wrote.

### Fixed

- The monitor shows a session's model and effort from its main conversation, not
  from Claude Code's background Haiku calls.

## 0.4.0

### Added

- `DUOLANE_AUTO_REVIEW_EFFORT` or `autoReviewEffort` sets the reasoning effort
  for Claude Code auto-review requests that go to Codex.

### Changed

- A Codex request makes at most three recovery retries, within 60 seconds of the
  first retry. When they run out, duolane answers 503 with
  `x-should-retry: false`.
- With `Auto` transport, Claude Code's compaction summary goes to Codex over
  HTTP.
- New Codex WebSocket connections no longer wait behind one another. Spacing
  grows after a Codex 403 on upgrade and shrinks again after successes.
- The monitor sizes Model, Endpoint, and Project to their content on wide
  terminals, so the numbers sit next to the text.
- The default branch is now `master`.

### Fixed

- With gzip on, an oversized streaming request whose WebSocket was lost goes
  over HTTP, as buffered requests already did.
- The monitor no longer shows `name)` as the project after Claude Code reports a
  changed working directory.

## 0.3.0

### Added

- `DUOLANE_CODEX_REQUEST_COMPRESSION=gzip` compresses Codex HTTP request bodies.
- With gzip on and `Auto` transport, full-context requests larger than
  `DUOLANE_CODEX_AUTO_WS_MAX_BYTES` (4 MiB by default) go over HTTP instead of
  the WebSocket.

### Fixed

- Codex requests are retried after a TLS connection closes before any output,
  but never after a hosted web search has run.
- A Codex HTTP stream that drops mid tool arguments is retried, and Claude Code
  sees only one complete tool call.
- Transient WebSocket drops after a full-context retry use the normal retry
  budget, and a client disconnect cancels a stalled WebSocket send.
- Compaction requests are detected when hook messages follow them.
- Each Claude Code agent gets its own Codex standalone search identity.
- Native Codex requests are not resent with another account's token after a
  login change.
- The monitor reads Claude usage despite a leading BOM or escaped keys, and
  ignores absurd token counts.
- Hosted web search on a Luna alias keeps the Lite decision made at resolution.

### Changed

- Releases no longer include prebuilt binaries for Intel Macs or Windows on Arm.
  Install on those platforms with `cargo install duolane --locked`.

## 0.2.1

### Added

- `luna` and `anthropic-luna` select the newest listed Luna model.

### Changed

- The monitor shows rate, input, and output tokens for Claude requests.
- The monitor widens the Project column on wide terminals and labels the Claude
  provider `claude`.

## 0.2.0

### Added

- `terra`, `sol`, and `astra` aliases and their `anthropic-` equivalents select
  the newest listed model in each Codex family. The three short names appear in
  the intercept model picker.

## 0.1.1

### Added

- Click a session, recent request, or event to open its details in the monitor.
- Publish crates.io versions from CI using Trusted Publishing and owner
  approval.

### Changed

- Events counts expected thread retries separately so they do not displace
  failures. Their HTTP 400 and details remain in Recent requests.
- The contribution guide explains how to submit a pull request from a fork.

## 0.1.0 - 2026-11-20

### Added

- Design documents, decision records, and security policy.
- `DUOLANE_STATE_DIR` moves logs and captures.
- `DUOLANE_ALLOWED_HOSTS` and `allowedHosts` accept extra `Host` names when an
  inbound token is set.
- `duolane serve --intercept` opens a loopback proxy that intercepts
  `api.anthropic.com` and tunnels other hosts.
- `duolane claude`, `duolane intercept setup`, and `duolane intercept cleanup`
  route Claude Code through that proxy, and `duolane intercept renew` replaces
  its certificate before it expires.
- `duolane claude` and `duolane intercept setup` list the Codex models in Claude
  Code's `/model` picker.

### Changed

- The product is now `duolane`, with new command, crate, environment variable,
  directory, and header names.
- The OpenAI-compatible endpoints return HTTP 400 for non-Codex models.
- Codex streams that end early or with an open tool call are reported as errors,
  a length stop is reported as `max_tokens`, and content the Codex route cannot
  carry is replaced with a visible marker.
- Inbound requests must name a local `Host`, carry no `Origin`, and use the
  route's content type. The inbound token needs 32 characters, is required
  beyond loopback, and now also guards `/monitor`.
- On Windows, config and logs move to `%APPDATA%` and `%LOCALAPPDATA%`.
- Dependencies are refreshed to close known vulnerabilities.
- A Codex context window overflow is reported as `prompt is too long`, so Claude
  Code compacts the conversation.

### Removed

- The Kimi, Grok, and Cursor backends.

Earlier work is recorded in the commit history of this repository.
