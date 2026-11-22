# Getting Started

## Install

Install [Claude Code](https://code.claude.com/docs/en/setup), signed in with a
Claude Pro or Max plan, and the
[Codex CLI](https://developers.openai.com/codex/cli), signed in with a ChatGPT
Plus, Pro, or Team plan. Then install duolane:

```bash
cargo install duolane --locked
duolane --version
```

[Release archives](https://github.com/dannyota/duolane/releases) are available
for macOS on Apple silicon, Linux on x86-64 and Arm64, and Windows on x86-64.
Check the archive against its published SHA-256 checksum and put the binary on
`PATH`. Use Cargo to build from source on platforms without an archive:

```bash
cargo install --git https://github.com/dannyota/duolane --locked
```

## Send the first request

If `duolane codex auth status` reports no account, finish `codex login` first.
Start the proxy and leave it running:

```bash
duolane codex auth status
duolane serve --intercept
```

In another terminal, from your project directory:

```bash
duolane claude --model sol
```

Send a short request and look for a Codex row in the monitor. Pick an installed
Claude model with `/model` and send another request to see a Claude row. The
aliases `terra`, `sol`, `astra`, and `luna` choose the latest supported model of
each Codex family. See [CLI](CLI.md) for model selection and agent setup.

Intercept is the recommended local launch. See [Connections](Connections.md) for
persistent setup, gateway mode, and cleanup.
