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

## Unsafe directory permissions

On Unix, `duolane serve --intercept` and `duolane claude` reject config paths
that other users can write to, except safe sticky parents such as `/tmp`.
Version 0.6.1 could create its own config directory with mode `0775` under
`umask 0002`.

Remove group and other write permissions from the reported directory only if you
own it and it is meant to be private: `chmod go-w -- <directory>`. If it is
shared, set `DUOLANE_CONFIG_DIR` to a private location with safe parent
directories and use that same setting for the server and launcher. Do not change
permissions recursively. On Linux, `namei -l` shows each component's permissions
if an older release does not name the rejected path.

See [Connections](Connections.md) for mode setup and
[Configuration](Configuration.md) for paths and ports. Attach the monitor with
`duolane monitor`; logs live in duolane's state directory.

When reporting a problem, include the duolane and Claude Code versions, the
mode, requested model alias, and error code. Remove credentials, prompts,
transcripts, and personal paths. Report suspected vulnerabilities privately
through the
[security policy](https://github.com/dannyota/duolane/blob/HEAD/SECURITY.md).
