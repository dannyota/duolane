# duolane

[![CI](https://github.com/dannyota/duolane/actions/workflows/ci.yml/badge.svg)](https://github.com/dannyota/duolane/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/dannyota/duolane)](LICENSE)

A local proxy that lets Claude Code use your Claude subscription and your
ChatGPT subscription in the same session. It picks the backend for each request
from the model name, so you can keep one model on each plan and switch between
them mid-conversation.

This is a personal, unofficial project, not affiliated with or endorsed by
Anthropic or OpenAI. It has no release yet, and security and reasoning work is
in progress, so run it only on a machine you trust. Expect breaking changes
until `1.0.0`.

## Requirements

- Claude Code, signed in with a Claude Pro or Max plan
- The Codex CLI, signed in with a ChatGPT Plus, Pro, or Team plan
- A stable Rust toolchain

## Install

```bash
cargo install --git https://github.com/dannyota/duolane --locked
```

## Quick start

Check your Codex login. Run `codex login` first if no account is found.

```bash
duolane codex auth status
```

Point Claude Code at the proxy in `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:18765"
  }
}
```

Start the proxy and leave it running:

```bash
duolane serve
```

Restart Claude Code, then choose a model:

```text
/model claude-opus-5-5
/model gpt-6-astra[1m]
```

The `[1m]` suffix turns on Claude Code's larger context mode. duolane removes it
before calling the backend.

## Commands

| Command                     | Purpose                                    |
| --------------------------- | ------------------------------------------ |
| `duolane serve`             | Start the proxy and its terminal dashboard |
| `duolane monitor`           | Attach a dashboard to a running proxy      |
| `duolane models`            | List the models you can use                |
| `duolane codex auth status` | Check the Codex login                      |

## How it works

- **Claude models** go to `api.anthropic.com` unchanged, using the login Claude
  Code already sends. duolane stores no Claude credential.
- **Codex models** are translated to the OpenAI Responses API and use the login
  in `~/.codex/auth.json`. duolane writes refreshed tokens back so the Codex CLI
  keeps working.
- **Other model names** get an HTTP 400 that lists the models you can use.

The [architecture](docs/design/architecture.md) and
[security design](docs/design/security.md) describe the intended behavior.

## Configuration

Do not set `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY`. Either one replaces
your Claude login, and the Claude route then fails.

| Setting                          | Purpose                                   |
| -------------------------------- | ----------------------------------------- |
| `PORT`                           | Listening port, `18765` by default        |
| `DUOLANE_BIND_ADDRESS`           | Listening address, `127.0.0.1` by default |
| `DUOLANE_AUTH_TOKEN`             | Token required on the proxy's routes      |
| `DUOLANE_ALIAS_PROVIDER`         | Backend for the Claude aliases            |
| `DUOLANE_AUTO_REVIEW_MODEL`      | Model for Claude Code's auto-review       |
| `ANTHROPIC_DEFAULT_OPUS_MODEL`   | Claude Code's model for the Opus alias    |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | Claude Code's model for the Sonnet alias  |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL`  | Claude Code's small background model      |
| `ENABLE_TOOL_SEARCH`             | Set to `true` to keep MCP tool search on  |

File settings live in `~/.config/duolane/config.json`.

### Reaching the proxy from another machine

The proxy speaks plain HTTP, including your token and Claude login. Bind beyond
loopback only over an encrypted path, such as an SSH tunnel or WireGuard, and
always set `DUOLANE_AUTH_TOKEN`. Claude Code sends the token in its own header:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://proxy-host:18765",
    "ANTHROPIC_CUSTOM_HEADERS": "x-duolane-key: <token>"
  }
}
```

## Limitations

Switching backends while a tool call is pending can fail. Start the next step
fresh if it does.

## Credits

duolane is built on
[`raine/claude-code-proxy`](https://github.com/raine/claude-code-proxy) and
[`fcakyon/claude-code-with-codex`](https://github.com/fcakyon/claude-code-with-codex).

## License

[MIT](LICENSE)
