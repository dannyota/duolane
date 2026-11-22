# Connections

| Mode      | Use it for                                                       | Start Claude Code                                  |
| --------- | ---------------------------------------------------------------- | -------------------------------------------------- |
| Intercept | Local Claude Code with Remote Control and the Codex model picker | `duolane claude`                                   |
| Gateway   | Clients that support a custom Anthropic base URL                 | `ANTHROPIC_BASE_URL=http://127.0.0.1:18765 claude` |

## Intercept

```bash
duolane serve --intercept
```

In another terminal, launch `duolane claude` with any Claude Code flags you
need. Add `--remote-control` to open Remote Control at startup. The launcher
sets up a local HTTPS proxy and process-scoped trust. duolane intercepts only
`api.anthropic.com` and tunnels other hosts. It does not install a system CA.

For Claude Code background agents and other sessions the wrapper cannot reach,
opt in to persistent user settings:

```bash
duolane intercept setup
```

The proxy must stay running for those sessions. To restore the previous setup,
run `duolane intercept cleanup`. To replace an expiring certificate, stop the
proxy, run `duolane intercept renew`, then restart the proxy and Claude Code.

## Gateway

Start `duolane serve`, then in another terminal:

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 claude --model sol
```

For a persistent gateway URL, merge this into your Claude Code user settings.
The optional `modelPicker` entries make the Codex aliases selectable:

```json
{
  "env": { "ANTHROPIC_BASE_URL": "http://127.0.0.1:18765" },
  "modelPicker": {
    "options": [
      { "model": "terra", "label": "Terra (Codex)" },
      { "model": "sol", "label": "Sol (Codex)" },
      { "model": "astra", "label": "Astra (Codex)" },
      { "model": "luna", "label": "Luna (Codex)" }
    ]
  }
}
```

To use your Claude subscription, leave `ANTHROPIC_API_KEY` and
`ANTHROPIC_AUTH_TOKEN` unset. Claude Code disables Remote Control when
`ANTHROPIC_BASE_URL` points at a gateway.

The gateway speaks plain HTTP. Keep it bound to loopback; use an encrypted path
such as an SSH tunnel when you need to reach it from another machine. See
[Configuration](Configuration.md) for the required proxy token when binding
beyond loopback.
