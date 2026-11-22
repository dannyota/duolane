# CLI

| Command                         | What it does                                                          |
| ------------------------------- | --------------------------------------------------------------------- |
| `duolane serve`                 | Start the proxy and its monitor; omit `serve` for the same behavior   |
| `duolane serve --intercept`     | Also start the local intercept listener                               |
| `duolane claude [args]`         | Pass arguments to Claude Code and route this launch through intercept |
| `duolane models`                | List known model IDs, aliases, and sign-in hints                      |
| `duolane monitor`               | Attach a read-only monitor to a running proxy                         |
| `duolane codex auth status`     | Check the Codex CLI login                                             |
| `duolane accounts login [name]` | Sign in to a named Claude Code profile                                |
| `duolane accounts use [name]`   | Select the profile for the next `duolane claude` launch               |
| `duolane agents setup`          | Install the Codex-family Claude Code subagents                        |
| `duolane agents remove`         | Remove only the unchanged agents installed by setup                   |
| `duolane intercept setup`       | Opt in to persistent intercept settings                               |
| `duolane intercept cleanup`     | Remove the settings and files setup installed                         |
| `duolane intercept renew`       | Replace the intercept certificate while the proxy is stopped          |

Use `duolane --help` or `duolane serve --help` to see available flags.

## Models

Claude model IDs such as `claude-opus-5-5` normally route to the Claude login.
Codex IDs and the aliases `terra`, `sol`, `astra`, and `luna` route to Codex.
Each alias selects the latest listed, API-capable version in its family using
the local Codex catalog, falling back to built-in models. Use a full model ID to
pin a version. `anthropic-terra`, `anthropic-sol`, `anthropic-astra`, and
`anthropic-luna` are equivalent aliases.

Switch in Claude Code with `/model`. Claude Code may assume a larger context
window than the one you want for a Codex model. Use `/autocompact` to set the
model-specific threshold, for example `/autocompact 500k` for Sol or
`/autocompact 272k` for Astra to match Codex's defaults
([Codex models](https://learn.chatgpt.com/docs/models)). Claude Code saves the
value for that model. `/model <codex model>` also becomes the default for new
sessions; pick a Claude model before you run Claude Code without duolane.

## Claude Code agents

Run `duolane agents setup` to create `terra.md`, `sol.md`, `astra.md`, and
`luna.md`. They live under `$CLAUDE_CONFIG_DIR/agents` or `~/.claude/agents`.
Setup leaves custom files alone unless you give `--force`; removal leaves
customized files intact. The agents reach Codex only in a Claude Code session
configured to use duolane. Set effort per Agent call or add an `effort:` field
to the agent definition.

See [Connections](Connections.md) for launcher setup and
[Configuration](Configuration.md) for provider settings.
