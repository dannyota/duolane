# Changelog

## 0.2.0

### Added

- `terra`, `sol`, and `astra` aliases and their `anthropic-` equivalents select
  the newest listed model in each Codex family. The three short names appear in
  the intercept model picker.
- `luna` and `anthropic-luna` select the newest listed Luna model.

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
