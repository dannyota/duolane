# Configuration

Environment variables override corresponding settings in `config.json`. The
usual Unix locations are `~/.config/duolane/config.json` for configuration and
`~/.local/state/duolane/` for logs and runtime state. On Windows, use
`%APPDATA%\duolane` and `%LOCALAPPDATA%\duolane`. `DUOLANE_CONFIG_DIR` and
`DUOLANE_STATE_DIR` select other paths.

## Listening addresses

| Setting                  | Default or effect                              |
| ------------------------ | ---------------------------------------------- |
| `PORT`                   | Gateway and monitor port: `18765`              |
| `DUOLANE_BIND_ADDRESS`   | Gateway bind address: `127.0.0.1`              |
| `DUOLANE_INTERCEPT`      | Set to `true` to start the intercept listener  |
| `DUOLANE_INTERCEPT_PORT` | Loopback intercept port: `18766`               |
| `DUOLANE_AUTH_TOKEN`     | Proxy key, at least 32 characters              |
| `DUOLANE_ALLOWED_HOSTS`  | Additional allowed Host names, comma-separated |

Binding beyond loopback or configuring `DUOLANE_ALLOWED_HOSTS` requires
`DUOLANE_AUTH_TOKEN`. Use an encrypted path for remote access; the gateway
itself is HTTP. Set `env.ANTHROPIC_CUSTOM_HEADERS` in Claude Code to
`x-duolane-key: <token>` with your proxy token in place of `<token>`.
`duolane monitor` only sends the configured token to a loopback or HTTPS URL. Do
not put the token in a URL, screenshot, or bug report.

## Codex transport and effort

| Environment variable                | Config key                    | Values or effect                                                                                                      |
| ----------------------------------- | ----------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `DUOLANE_CODEX_TRANSPORT`           | `codex.transport`             | `auto`, `http`, or `websocket`                                                                                        |
| `DUOLANE_CODEX_REQUEST_COMPRESSION` | `codex.requestCompression`    | `off` (default) or `gzip` for HTTP Responses and search requests                                                      |
| `DUOLANE_CODEX_AUTO_WS_MAX_BYTES`   | `codex.autoWebSocketMaxBytes` | Route eligible full-context requests above this byte limit to HTTP in Auto mode; `4194304` by default, `0` to disable |
| `DUOLANE_CODEX_BASE_INSTRUCTIONS`   | `codex.baseInstructions`      | `on` (default) or `off`: send the Codex catalog's base instructions before Claude Code's system prompt on agent turns |
| `DUOLANE_AUTO_REVIEW_MODEL`         | `autoReviewModel`             | Override Claude Code's classifier model                                                                               |
| `DUOLANE_AUTO_REVIEW_EFFORT`        | `autoReviewEffort`            | `none`, `low`, `medium`, `high`, `xhigh`, or `max` for classifiers routed to Codex                                    |

The Auto byte limit applies independently of HTTP request compression. See the
[Codex route design](https://github.com/dannyota/duolane/blob/HEAD/docs/design/codex.md)
for routing, retry, and compaction behavior.

## Model selection

`DUOLANE_ALIAS_PROVIDER` changes the backend used for the Claude aliases.
`ANTHROPIC_DEFAULT_OPUS_MODEL` and `ANTHROPIC_DEFAULT_SONNET_MODEL` choose
Claude Code's models for those aliases. `ANTHROPIC_DEFAULT_HAIKU_MODEL` sets its
small background model. Set `ENABLE_TOOL_SEARCH=true` in Claude Code to defer
MCP tool schemas when supported by the selected model.

## Sign-ins

Claude Code manages the Claude login. Do not set `ANTHROPIC_AUTH_TOKEN` or
`ANTHROPIC_API_KEY`: either replaces your Claude login and the Claude route then
fails. See [Claude Accounts](Claude-Accounts.md) for several logins. duolane
uses the Codex CLI's login selected by `DUOLANE_CODEX_AUTH_FILE`, then
`$CODEX_HOME/auth.json`, then `~/.codex/auth.json`. Use `codex login` to create
it. Do not open or share credential files for troubleshooting.

## Codex API key

`DUOLANE_OPENAI_API_KEY` overrides the stored key. Stored keys are available on
macOS and Linux only. `DUOLANE_CODEX_AUTH_ORDER` overrides `codex.authOrder` in
`config.json` and accepts a comma-separated list of `chatgpt` and `api-key`. The
default is `chatgpt`. A key does not opt in to API use by itself; API use is
billed at OpenAI API rates. Restart the proxy after changing credentials or
order. Use `duolane codex auth order --reset` to remove the saved order.
