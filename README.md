# duolane

[![CI](https://github.com/dannyota/duolane/actions/workflows/ci.yml/badge.svg)](https://github.com/dannyota/duolane/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/dannyota/duolane)](LICENSE)

A local proxy that lets Claude Code use your Claude subscription and your
ChatGPT subscription in the same session. It picks the backend for each request
from the model name, so you can keep one model on each plan and switch between
them mid-conversation.

This is an unofficial open-source project, not affiliated with or endorsed by
Anthropic or OpenAI. Run it only on a machine you trust. Expect breaking changes
until `1.0.0`.

Bug reports, documentation improvements, and focused pull requests are welcome.
See [Contributing](CONTRIBUTING.md) to get started.

## Requirements

- Claude Code, signed in with a Claude Pro or Max plan
- The Codex CLI, signed in with a ChatGPT Plus, Pro, or Team plan
- A stable Rust toolchain

## Install

Install the published version from
[crates.io](https://crates.io/crates/duolane):

```bash
cargo install duolane --locked
```

To build from the latest source instead:

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
  },
  "modelPicker": {
    "options": [
      { "model": "terra", "label": "Terra (Codex)" },
      { "model": "sol", "label": "Sol (Codex)" },
      { "model": "astra", "label": "Astra (Codex)" }
    ]
  }
}
```

`modelPicker` adds Codex models to the `/model` picker
([model picker](https://code.claude.com/docs/en/settings-reference#modelpicker)).
Claude Code's own gateway model discovery needs a gateway credential, which
would replace your Claude login.

Start the proxy and leave it running:

```bash
duolane serve
```

Restart Claude Code, then choose a model:

```text
/model claude-opus-5-5
/model sol
```

`terra`, `sol`, `astra`, and `luna` select the newest listed, API-capable model
in each family in the local Codex catalog. Their `anthropic-` forms, such as
`anthropic-luna`, are equivalent. Use a full ID such as `gpt-6.2-sol` to pin a
version. Without a matching catalog entry, the aliases use built-in models.

Claude Code does not know Codex models and assumes a 1M-token context window.
GPT-6.2 Sol and Astra support up to 1M tokens, while Codex itself defaults to
500k for Sol and 272k for Astra
([Codex models](https://learn.chatgpt.com/docs/models)). While a Codex model is
selected, run `/autocompact` once to choose where Claude Code compacts it, such
as `/autocompact 500k` for Sol or `/autocompact 272k` for Astra to match Codex.
Claude Code saves the value for that model
([auto-compact window](https://code.claude.com/docs/en/model-config#set-the-auto-compact-window)).

## Intercept mode

Intercept mode keeps Claude Code talking to `api.anthropic.com`, so
[Remote Control](https://code.claude.com/docs/en/remote-control) stays on.
Claude Code connects through a local HTTPS proxy instead of
`ANTHROPIC_BASE_URL`. duolane reads only the requests to `api.anthropic.com`,
with a certificate authority that can sign for that one name, and tunnels every
other host unread.

Start the proxy with intercept mode on and leave it running:

```bash
duolane serve --intercept
```

Start Claude Code through it, with any `claude` arguments:

```bash
duolane claude --remote-control
```

`duolane claude` adds the Codex family aliases and catalog models to the
`/model` picker for that launch without changing Claude Code settings. To route
every Claude Code session, including background agents, write the same settings
to the Claude Code user settings once. Every session then needs the proxy
running:

```bash
duolane intercept setup
duolane intercept cleanup
```

`cleanup` removes the settings and the intercept files.

The intercept certificate lasts 90 days. When fewer than 14 days remain,
`duolane serve` and `duolane claude` warn. Stop the proxy, run
`duolane intercept renew`, start it again, and restart Claude Code sessions.

`/model <codex model>` also saves that model as the default for new sessions.
Pick a Claude model before you run Claude Code without duolane.

## Claude accounts

On macOS and Linux, use Claude Code's own sign-in to add a profile and select it
for the next `duolane claude` launch:

```bash
duolane accounts login
duolane accounts use
duolane claude --resume <session-id>
```

Choose `default` in `accounts use` to return to your original profile. Plain
`claude` and an explicit `CLAUDE_CONFIG_DIR` ignore the selection. This does not
switch an open session or copy credentials.

A named profile asks before sharing a project's default transcript folder for
the first time. Resuming then writes to the original transcript without a fork.
This shares the whole displayed folder, including project auto memory, and can
send that history under another account. Close the previous session before
resuming it. See [account selection](docs/design/claude-accounts.md) for path
restrictions and how Claude Code settings affect authentication.

## Claude Code agents

Run `duolane agents setup` to install Codex family agents, or
`duolane agents remove` to remove unmodified agents. Use `--force` with setup to
replace modified files. Files live in `$CLAUDE_CONFIG_DIR/agents`, or
`~/.claude/agents` when `CLAUDE_CONFIG_DIR` is unset.

Agents reach Codex only when Claude Code routes through duolane via
`ANTHROPIC_BASE_URL` or intercept mode (`duolane intercept setup` or
`duolane claude`). Set effort per Agent call or add an `effort:` line to an
agent file.

## Commands

| Command                     | Purpose                                       |
| --------------------------- | --------------------------------------------- |
| `duolane serve`             | Start the proxy and its terminal dashboard    |
| `duolane monitor`           | Attach a dashboard to a running proxy         |
| `duolane models`            | List the models you can use                   |
| `duolane codex auth status` | Check the Codex login                         |
| `duolane claude [args]`     | Start Claude Code through the intercept proxy |
| `duolane accounts login`    | Sign in to a named Claude Code profile        |
| `duolane accounts use`      | Select the profile for the next Claude launch |
| `duolane intercept setup`   | Route every Claude Code session through it    |
| `duolane intercept cleanup` | Remove the intercept settings and files       |
| `duolane intercept renew`   | Replace the intercept certificate             |

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
| `DUOLANE_AUTH_TOKEN`             | Proxy token, at least 32 characters       |
| `DUOLANE_ALLOWED_HOSTS`          | Extra `Host` names, comma-separated       |
| `DUOLANE_ALIAS_PROVIDER`         | Backend for the Claude aliases            |
| `DUOLANE_INTERCEPT`              | Set to `true` to open the intercept proxy |
| `DUOLANE_INTERCEPT_PORT`         | Intercept proxy port, `18766` by default  |
| `DUOLANE_AUTO_REVIEW_MODEL`      | Model for Claude Code's auto-review       |
| `DUOLANE_AUTO_REVIEW_EFFORT`     | Codex auto-review reasoning effort        |
| `ANTHROPIC_DEFAULT_OPUS_MODEL`   | Claude Code's model for the Opus alias    |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | Claude Code's model for the Sonnet alias  |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL`  | Claude Code's small background model      |
| `ENABLE_TOOL_SEARCH`             | Set to `true` to defer MCP tool schemas   |

File settings live in `~/.config/duolane/config.json`, and logs in
`~/.local/state/duolane/`. On Windows they live in `%APPDATA%\duolane` and
`%LOCALAPPDATA%\duolane`. `DUOLANE_CONFIG_DIR` and `DUOLANE_STATE_DIR` move
them.

Set `DUOLANE_AUTO_REVIEW_EFFORT` or `autoReviewEffort` to `none`, `low`,
`medium`, `high`, `xhigh`, or `max` to override Codex classifier effort. The
environment takes precedence over the file. Invalid values are ignored; without
a valid override, normal effort selection applies.

Set `DUOLANE_CODEX_REQUEST_COMPRESSION=gzip` or set `codex.requestCompression`
to `"gzip"` in the config file to compress Codex Responses and search requests
sent over HTTP. The default is `off`. With gzip enabled, Auto sends full-context
requests over 4 MiB via HTTP before attempting WebSocket. Set
`DUOLANE_CODEX_AUTO_WS_MAX_BYTES` or `codex.autoWebSocketMaxBytes` to change the
byte limit, or set it to `0` to disable this routing.

### Reaching the proxy from another machine

The proxy speaks plain HTTP, including your token and Claude login. Bind beyond
loopback only over an encrypted path, such as an SSH tunnel or WireGuard.
duolane refuses to start beyond loopback, or with `DUOLANE_ALLOWED_HOSTS`,
without `DUOLANE_AUTH_TOKEN`. Claude Code sends the token in its own header, and
`duolane monitor` sends the configured token itself, to a loopback or HTTPS URL
only:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://proxy-host:18765",
    "ANTHROPIC_CUSTOM_HEADERS": "x-duolane-key: <token>"
  }
}
```

## Limitations

- Switching backends while a tool call is pending can fail. Start the next step
  fresh if it does.
- Claude Code turns off Remote Control whenever `ANTHROPIC_BASE_URL` points
  anywhere other than `api.anthropic.com`. Use
  [intercept mode](#intercept-mode), or set the variable for a single launch,
  such as `ANTHROPIC_BASE_URL=http://localhost:18765 claude`.

## Credits

duolane is built on
[`raine/claude-code-proxy`](https://github.com/raine/claude-code-proxy) and
[`fcakyon/claude-code-with-codex`](https://github.com/fcakyon/claude-code-with-codex).

## License

[MIT](LICENSE)
