# Intercept mode

Intercept mode lets a Claude Code session use Codex models while Claude Code
still talks to `api.anthropic.com`, so first-party features such as Remote
Control stay on. [ADR 0004](../adr/0004-intercept-mode.md) records the decision.
This design describes intended behavior; none of it is built.

## Flow

1. `duolane claude [args]` checks that the proxy listener presents duolane's own
   leaf certificate, then starts `claude` with the [environment](#environment)
   below. Nothing persistent changes, so there is nothing to clean up afterward.
2. Claude Code sends `CONNECT host:port` to duolane's proxy listener for each
   host, with the proxy password.
3. For `api.anthropic.com:443`, duolane answers the TLS handshake with its own
   leaf certificate, offering HTTP/1.1 only, and reads the requests.
4. For any other target, duolane opens a TCP connection and copies bytes both
   ways without reading them.

## Environment

| Variable                              | Value                                        |
| ------------------------------------- | -------------------------------------------- |
| `HTTPS_PROXY` and `https_proxy`       | `http://duolane:<password>@127.0.0.1:<port>` |
| `NODE_EXTRA_CA_CERTS`                 | Path of `ca.pem`                             |
| `NO_PROXY` and `no_proxy`             | Loopback names and addresses                 |
| `CLAUDE_CODE_GZIP_REQUEST_BODIES`     | `0`                                          |
| `CLAUDE_STREAM_FIRST_BYTE_TIMEOUT_MS` | Above the Codex response header wait         |
| `CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS`  | The stream idle limit of the security design |

- `ANTHROPIC_BASE_URL` is removed from the environment.
- The wrapper refuses to start when a proxy variable already names another
  proxy, or when `NO_PROXY` would bypass `api.anthropic.com`.
- Programs that Claude Code starts inherit these variables. Their HTTPS traffic
  is tunneled, and Node programs among them also trust the certificate.

For background agents that the wrapper cannot reach, `duolane intercept setup`
writes the same variables to the `env` block of the user settings file instead.
It is opt-in, because every Claude Code surface then depends on duolane running.

## Requests on the intercepted host

| Request                                                                 | Handling                                 |
| ----------------------------------------------------------------------- | ---------------------------------------- |
| `POST /v1/messages` or `/v1/messages/count_tokens` naming a Codex model | The Codex route                          |
| The same paths naming any other model                                   | The Claude route, with its conversions   |
| Any other method or path, and any protocol upgrade                      | Relayed to `api.anthropic.com` unchanged |

- duolane reads a request body only on the two message paths, and refuses a body
  there that carries a `Content-Encoding`, because duolane never decompresses
  and an unread body could carry Codex reasoning to Anthropic.
- The relay keeps the method, path, query, and body bytes. In both directions it
  removes `Proxy-Authorization` and the hop-by-hop and framing headers
  (`Connection` and the fields it names, `Keep-Alive`, `TE`,
  `Transfer-Encoding`, `Content-Length`), and keeps every other header,
  including `Accept-Encoding` and `Content-Encoding`. On a protocol upgrade it
  forwards `Connection: upgrade` and `Upgrade`, then copies bytes both ways.
- Architecture [invariant 3](architecture.md#invariants) holds for traffic that
  reaches duolane. A session that bypasses the proxy is outside it.

## Certificates

- duolane creates a CA certificate with a critical name constraint that permits
  only the DNS subtree `api.anthropic.com` and excludes every IP address, and a
  leaf for `api.anthropic.com` with a subject alternative name, key identifiers,
  and the server authentication usage.
- The CA private key exists only in memory while the pair is created.
- The pair lasts 90 days. When fewer than 14 days remain, `duolane serve` adds a
  new CA certificate to `ca.pem` beside the old one, and serves the new leaf
  only once the old one expires, so sessions started in between keep working.
  The pending pair is stored beside the current one until then.
- duolane verifies the real `api.anthropic.com` certificate as in every other
  mode.

## Files

Everything lives in `intercept/` under the duolane config directory, readable
only by the owner:

| File             | Contents                                      |
| ---------------- | --------------------------------------------- |
| `ca.pem`         | CA certificate that Claude Code trusts        |
| `leaf.pem`       | Leaf certificate                              |
| `leaf.key`       | Leaf private key                              |
| `proxy-password` | Random proxy password                         |
| `manifest.json`  | Each file and setting duolane created, hashed |

## Setup and cleanup

- `duolane intercept setup` edits the user settings file, honoring
  `CLAUDE_CONFIG_DIR`. It creates the file if needed, refuses a symlink, refuses
  a file that changed while it worked, writes atomically, sets the file to
  owner-only because it now holds the proxy password, records each key and value
  in the manifest, and can run again safely. It tells the owner to restart
  running sessions. It refuses when a variable it sets is already set to
  something else in the shell or in user, project, or managed settings.
- `duolane intercept cleanup` removes each setting whose value still matches the
  manifest, reports any that changed, deletes the files, and reminds the owner
  to restart Claude Code. Without a manifest it finds the files by their fixed
  paths and the settings by their duolane values.
- Neither command needs `sudo`. Both print what they changed.

## Security rules

1. The proxy listener binds to loopback only.
2. Every `CONNECT` needs the proxy password, compared like the inbound token.
   Without it, another local user could spend the Codex login through the proxy.
3. Only `api.anthropic.com:443` is intercepted. The list is not configurable.
4. `CONNECT` is refused when the target resolves to a loopback or unspecified
   address, including IPv4-mapped forms. duolane connects to the address it
   checked, never to a second lookup. Other targets and ports are tunneled.
5. Inside the intercepted connection, duolane checks that `Host` is
   `api.anthropic.com` and applies the request limits. The `Origin` and content
   type rules do not apply there, because no browser can reach it.
6. In this mode duolane ignores proxy variables for its own upstream connections
   and for tunnels, and uses only an explicitly configured upstream proxy. This
   prevents a loop through its own listener.
7. `leaf.key` and `proxy-password` are protected like the Codex login: owner
   only, never logged, captured, or printed. Whoever reads `leaf.key` and takes
   the proxy port can read the Claude login and forge model replies until the
   certificate expires.
8. Tunneled and relayed bytes are never logged or captured. Tunnels have their
   own connection limit and no idle timeout.

## Unverified behavior

A spike with an existing intercepting proxy, before any duolane code, must
confirm:

- Remote Control works through a proxy that intercepts `api.anthropic.com` and
  tunnels other hosts, including model changes made from the remote device.
- Claude Code's runtime loads `NODE_EXTRA_CA_CERTS`, accepts the leaf, and
  rejects a leaf signed by a CA constrained to another name.
- `/model`, `--model`, and `ANTHROPIC_MODEL` accept a Codex model id without a
  gateway, and what the request for it contains.
- Which protocol Claude Code negotiates, whether it compresses request bodies
  behind a proxy, and which paths it calls on `api.anthropic.com`.
- The first-byte timeout variable exists and behaves as documented.
