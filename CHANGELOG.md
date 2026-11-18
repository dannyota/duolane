# Changelog

## Unreleased

### Added

- Design documents, decision records, and security policy.
- `DUOLANE_STATE_DIR` moves logs and captures.

### Changed

- The product is now `duolane`, with new command, crate, environment variable,
  directory, and header names.
- The OpenAI-compatible endpoints return HTTP 400 for non-Codex models.
- On Windows, config and logs move to `%APPDATA%` and `%LOCALAPPDATA%`.
- Dependencies are refreshed to close known vulnerabilities.

### Removed

- The Kimi, Grok, and Cursor backends.

Earlier work is recorded in the commit history of this repository.
