# duolane

[![CI](https://github.com/dannyota/duolane/actions/workflows/ci.yml/badge.svg)](https://github.com/dannyota/duolane/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/duolane)](https://crates.io/crates/duolane)
[![Release](https://img.shields.io/github/v/release/dannyota/duolane)](https://github.com/dannyota/duolane/releases)
[![License](https://img.shields.io/github/license/dannyota/duolane)](LICENSE)

Use Claude and Codex models in one Claude Code session. duolane runs locally,
chooses the backend from the requested model, and shows each request in a
terminal monitor. Claude Code and Codex CLI manage their own sign-ins.

This is an unofficial project, not affiliated with Anthropic or OpenAI. Run it
only on a machine you trust. Expect breaking changes before `1.0.0`.

## Install

```bash
cargo install duolane --locked
duolane --version
```

No Rust toolchain? Download a binary and its checksum from
[GitHub Releases](https://github.com/dannyota/duolane/releases). See [Getting
Started][start] for platforms and source builds.

## Quick start

Sign in to Claude Code and the Codex CLI first. In one terminal:

```bash
duolane codex auth status
duolane serve --intercept
```

In a second terminal, from your project:

```bash
duolane claude --model sol
```

Use `/model sol` for Codex or pick a Claude model to go back to Claude. The
`terra`, `sol`, `astra`, and `luna` aliases follow the latest supported model of
each family. Pin a full model ID when you need a specific version.

This launch keeps Claude Code using its first-party API hostname, so Remote
Control can stay on. The wrapper does not install a system CA or edit Claude
Code proxy settings. For gateway mode or background agents, see
[Connections][connections].

## Everyday commands

| Command                          | Purpose                                                 |
| -------------------------------- | ------------------------------------------------------- |
| `duolane models`                 | Discover model IDs and aliases                          |
| `duolane monitor`                | Attach a dashboard to the running proxy                 |
| `duolane agents setup`           | Install opt-in Claude Code subagents for Codex families |
| `duolane accounts list`          | Show Claude profiles and which one is selected          |
| `duolane accounts use [name]`    | Pick which Claude login the next `duolane claude` uses  |
| `duolane accounts remove <name>` | Sign out of a named Claude profile and delete it        |
| `duolane --help`                 | Discover commands and options                           |

## Documentation

- [Getting Started][start]: installation, login, and the first request.
- [Connections][connections]: intercept, gateway, Remote Control, and cleanup.
- [CLI][cli]: model selection, agents, and command reference.
- [Claude Accounts][accounts]: several Claude logins, one transcript.
- [Configuration][configuration]: ports, Codex transport, and state paths.
- [Troubleshooting][troubleshooting]: common failures and safe recovery.

See [Contributing](CONTRIBUTING.md) for development and [Security](SECURITY.md)
to report issues privately. Architecture and decisions live in [docs/](docs/).

## Credits

Built on [`raine/claude-code-proxy`](https://github.com/raine/claude-code-proxy)
and
[`fcakyon/claude-code-with-codex`](https://github.com/fcakyon/claude-code-with-codex).
Licensed under [MIT](LICENSE).

[start]: https://github.com/dannyota/duolane/wiki/Getting-Started
[connections]: https://github.com/dannyota/duolane/wiki/Connections
[cli]: https://github.com/dannyota/duolane/wiki/CLI
[accounts]: https://github.com/dannyota/duolane/wiki/Claude-Accounts
[configuration]: https://github.com/dannyota/duolane/wiki/Configuration
[troubleshooting]: https://github.com/dannyota/duolane/wiki/Troubleshooting

### Codex API key

`duolane codex auth key set` reads a key from a hidden prompt or standard input
and verifies it before saving. Use `--no-verify` for offline setup. Stored keys
are supported on macOS and Linux; Windows uses `DUOLANE_OPENAI_API_KEY` only.
`duolane codex auth key remove` removes the stored key. The environment key wins
over a stored key.

A key alone does not enable API billing. Choose sources explicitly with
`duolane codex auth order api-key` or
`duolane codex auth order chatgpt api-key`. API use is billed at OpenAI API
rates. `duolane codex auth order --reset` restores the default ChatGPT order;
`DUOLANE_CODEX_AUTH_ORDER` overrides the saved order. Restart a running proxy
when credentials or order change.
