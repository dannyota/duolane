# Troubleshooting

| Symptom                                            | Check                                                                                                                                                                                  |
| -------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `duolane` is not found, or the version is old      | Run `command -v duolane` and `duolane --version`. Make sure your new binary is first on `PATH`, then restart the running proxy.                                                        |
| Codex does not have a login                        | Run `codex login`, then `duolane codex auth status`. If you use `CODEX_HOME` or `DUOLANE_CODEX_AUTH_FILE`, confirm the proxy received the intended setting.                            |
| `duolane claude` cannot reach intercept            | Start `duolane serve --intercept`. The wrapper rejects an incompatible proxy or extra CA environment. Clear the conflicting setting for this launch rather than changing system trust. |
| The Codex alias is absent from `/model`            | Start with `duolane claude --model sol`. Automatic picker injection is skipped when `--settings` or a user or managed lineup takes precedence.                                         |
| Remote Control is unavailable                      | Don't set a gateway `ANTHROPIC_BASE_URL`. Start with intercept mode instead.                                                                                                           |
| A tool call cannot continue after a backend switch | Finish or restart the pending step before switching providers.                                                                                                                         |
| The intercept certificate is expired               | Stop the proxy, run `duolane intercept renew`, then restart the proxy and Claude Code.                                                                                                 |

See [Connections](Connections.md) for mode setup and
[Configuration](Configuration.md) for paths and ports. Attach the monitor with
`duolane monitor`; logs live in duolane's state directory.

When reporting a problem, include the duolane and Claude Code versions, the
mode, requested model alias, and error code. Remove credentials, prompts,
transcripts, and personal paths. Report suspected vulnerabilities privately
through the
[security policy](https://github.com/dannyota/duolane/blob/HEAD/SECURITY.md).
