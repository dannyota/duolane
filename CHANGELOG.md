# Changelog

## Unreleased

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

### Removed

- The Kimi, Grok, and Cursor backends.

Earlier work is recorded in the commit history of this repository.
