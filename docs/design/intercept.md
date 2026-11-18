# Intercept mode

Intercept mode lets a Claude Code session use Codex models while Claude Code
still talks to `api.anthropic.com`, so first-party features such as Remote
Control stay on. [ADR 0004](../adr/0004-intercept-mode.md) records the decision.
This design describes intended behavior; none of it is built.

## Flow

1. Claude Code runs with `HTTPS_PROXY` set to duolane's proxy listener and
   `NODE_EXTRA_CA_CERTS` set to duolane's CA certificate. `ANTHROPIC_BASE_URL`
   stays unset.
2. Claude Code sends `CONNECT host:443` to the proxy listener for each host.
3. For `api.anthropic.com`, duolane answers the TLS handshake with its own leaf
   certificate, offering HTTP/2 and HTTP/1.1, and reads the requests.
4. For any other host, duolane opens a TCP connection and copies bytes both ways
   without reading them.

## Requests on the intercepted host

| Request                                                                 | Handling                                 |
| ----------------------------------------------------------------------- | ---------------------------------------- |
| `POST /v1/messages` or `/v1/messages/count_tokens` naming a Codex model | The Codex route                          |
| The same paths naming any other model                                   | The Claude route, with its conversions   |
| Any other method or path, and any protocol upgrade                      | Relayed to `api.anthropic.com` unchanged |

- duolane reads a request body only on the two message paths. A body with a
  `Content-Encoding` is relayed unchanged, because duolane never decompresses.
  Setup therefore sets `CLAUDE_CODE_GZIP_REQUEST_BODIES=0`.
- The relay keeps the method, path, query, headers, and body bytes, and applies
  the architecture [invariants](architecture.md#invariants).
- Claude Code sends a model id as written, so a Codex id set through
  `ANTHROPIC_MODEL`, `--model`, or an alias variable reaches duolane
  ([model configuration](https://code.claude.com/docs/en/model-config)).

## Certificates

- Setup creates a CA certificate whose critical name constraint permits only the
  DNS name `api.anthropic.com`, and a leaf certificate for that name with the
  server authentication usage.
- The CA private key exists only in memory during setup and is never written.
- Both certificates last one year. `duolane serve` replaces them when fewer than
  30 days remain, and tells the owner to restart Claude Code.
- duolane verifies the real `api.anthropic.com` certificate as in every other
  mode. Interception never weakens upstream verification.

## Files

Everything lives in `intercept/` under the duolane config directory, readable
only by the owner:

| File            | Contents                                         |
| --------------- | ------------------------------------------------ |
| `ca.pem`        | CA certificate that Claude Code trusts           |
| `leaf.pem`      | Leaf certificate                                 |
| `leaf.key`      | Leaf private key                                 |
| `manifest.json` | Each file and setting setup created, with hashes |

## Setup and cleanup

- `duolane intercept setup` creates the files, then adds `HTTPS_PROXY`,
  `NODE_EXTRA_CA_CERTS`, and `CLAUDE_CODE_GZIP_REQUEST_BODIES` to the `env`
  block of `~/.claude/settings.json`, and records each key and value in the
  manifest. It refuses to run while `ANTHROPIC_BASE_URL`, `HTTPS_PROXY`, or
  `NODE_EXTRA_CA_CERTS` is already set there to something else.
- `duolane intercept cleanup` removes each setting whose value still matches the
  manifest, reports any that changed, and deletes the files. Without a manifest
  it finds the files by their fixed paths and the settings by their duolane
  values.
- Both commands print what they changed. Neither needs `sudo`.

## Security rules

1. The proxy listener binds to loopback only. A non-loopback bind is refused in
   this mode.
2. The proxy accepts only `CONNECT` to port 443.
3. Only `api.anthropic.com` is ever intercepted. The list is not configurable.
4. Intercepted requests pass the [inbound rules](security.md#inbound-requests),
   with `api.anthropic.com` as the only accepted `Host`.
5. duolane's own upstream connections never use a proxy that points at duolane.
6. `leaf.key` is handled like a credential: never logged, captured, or printed.
7. The limits, redaction, and timeouts of the [security design](security.md)
   apply to intercepted requests and to tunnels.

## Unverified behavior

A spike must confirm these before the mode is built:

- Remote Control works through an HTTP proxy that intercepts `api.anthropic.com`
  and tunnels other hosts, including any WebSocket it opens to that host.
- Claude Code's runtime honors the name constraint and accepts the leaf.
- `/model` accepts a Codex model id when no gateway is configured.
- Claude Code does not pin the `api.anthropic.com` certificate.
