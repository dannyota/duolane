# Intercept mode

Intercept mode lets a Claude Code session use Codex models while Claude Code
still talks to `api.anthropic.com`, so first-party features such as Remote
Control stay on. [ADR 0004](../adr/0004-intercept-mode.md) records the decision.
A [spike](#spike-results) confirmed the behavior this design depends on. The
[foundation plan](../plans/foundation.md) tracks what is built.

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

## Listener

- `duolane serve` opens the proxy listener only when intercept mode is on:
  `intercept` in the config file, `DUOLANE_INTERCEPT`, or `serve --intercept`.
  It creates the [files](#files) on first start.
- The listener binds to `127.0.0.1` on `interceptPort` or
  `DUOLANE_INTERCEPT_PORT`, by default 18766. The bind address setting does not
  apply to it.
- It accepts only `CONNECT`. Any other method gets 405, and a `CONNECT` without
  the proxy password gets 407.
- At shutdown it stops accepting, gives open connections 5 seconds, then closes
  them, because tunnels and long polls would otherwise never end.
- `duolane serve` raises its soft open-file limit to the hard limit, up to 4096,
  because the default of 256 on macOS is below what the listener may hold.

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

| Request                                                                 | Handling                                       |
| ----------------------------------------------------------------------- | ---------------------------------------------- |
| `POST /v1/messages` or `/v1/messages/count_tokens` naming a Codex model | The Codex route                                |
| The same paths naming any other model                                   | The Claude route, with its conversions         |
| A request that asks for a protocol upgrade                              | Refused with 501                               |
| Any other method or path                                                | Relayed to the Anthropic upstream URL, as sent |

- The two message paths match exactly, whatever the query string is, because
  Claude Code sends `?beta=true`. A request is refused with 400 when its target
  is not in origin form, or when its path is not a message path but becomes one
  after percent-decoding, removing repeated slashes, dot segments, and a
  trailing slash, treating a backslash as a slash, and ignoring case. No
  spelling of a message path reaches the relay, and a `CONNECT` inside the
  connection is refused because its target is not in origin form.
- duolane reads a request body only on the two message paths, and refuses a body
  there that carries a `Content-Encoding`, because duolane never decompresses
  and an unread body could carry Codex reasoning to Anthropic.
- The relay keeps the method, query, and body bytes, and the path as the URL
  parser normalizes it: dot segments resolved, a backslash turned into a slash,
  and characters a URL may not carry percent-encoded. In both directions it
  removes `Proxy-Authorization` and the hop-by-hop and framing headers
  (`Connection` and the fields it names, `Keep-Alive`, `TE`,
  `Transfer-Encoding`, `Content-Length`, `Upgrade`), removes `x-duolane-key`
  from requests, and keeps every other header, including `Accept-Encoding` and
  `Content-Encoding`.
- The relay streams bodies in both directions as they arrive, follows no
  redirect, and applies no response or idle timeout. Remote Control holds long
  polls and an event stream open on this host, and it stops receiving messages
  when a proxy buffers them. TCP keepalive on both sides of a relay or tunnel
  ends one whose peer vanished, for example after the laptop slept.
- Protocol upgrades are refused because the spike saw none on this host. Relay
  them once Claude Code is seen to need one.
- Architecture [invariant 3](architecture.md#invariants) holds on the message
  paths. Remote Control mirrors the session transcript to Anthropic through
  relayed requests, including each thinking block and its signature, so a Codex
  signature travels there unread. The owner accepted this on 2026-11-19: the
  transcript is what Remote Control exists to send, and the encrypted reasoning
  in the signature is readable only by OpenAI.

## Certificates

- duolane creates a CA certificate with a critical name constraint that permits
  only the DNS subtree `api.anthropic.com` and excludes every IP address, and a
  leaf for `api.anthropic.com` with a subject alternative name, key identifiers,
  and the server authentication usage.
- The CA private key exists only in memory while the pair is created.
- The pair lasts 90 days. duolane never replaces a pair while `duolane serve`
  runs, because each running Claude Code session trusts only the CA it read at
  start.
- When fewer than 14 days remain, `duolane serve` warns at start and every 24
  hours while it runs, and `duolane claude` warns at each launch. The warning
  says to stop `duolane serve`, run `duolane intercept renew`, start
  `duolane serve --intercept` again, and restart Claude Code sessions.
- When the pair has expired, `duolane claude` refuses before it connects and
  says to restart `duolane serve --intercept`. At start, `duolane serve`
  replaces an expired or unreadable pair as on first start, and says that Claude
  Code sessions need a restart. A running `duolane serve` keeps serving the
  expired leaf, so every session fails until it restarts.
- `duolane intercept renew` replaces the CA and the leaf, keeps the proxy
  password, updates the manifest when there is one, and never edits a settings
  file, because the CA path and the password stay the same. It fails when there
  are no intercept files to renew. It holds the config directory lock from its
  port check to its last write, and `duolane serve` binds its port before it
  takes that lock to read the pair, so the two never miss each other. The port
  check is a plain TCP connect that sends nothing; when something accepts the
  connection, `renew` refuses and names the port. It checks only the configured
  port. A crash between its writes leaves a pair that the next `duolane serve`
  start replaces.
- duolane verifies the real `api.anthropic.com` certificate as in every other
  mode.
- The certificates are created with `rcgen`, which the rustls project maintains
  and which signs with `ring`, the provider duolane already uses.

The mode adds three more direct dependencies, all of which other crates already
brought into the lockfile: `hyper` serves the `CONNECT` upgrade, `socket2` sets
TCP keepalive, and `rustix` raises the open-file limit without unsafe code.

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

On Unix, `duolane serve` and the `duolane intercept` commands hold an exclusive
lock on the duolane config directory while they create, renew, or delete these
files, so two of them never interleave.

## Setup and cleanup

- `duolane intercept setup` edits the user settings file, honoring
  `CLAUDE_CONFIG_DIR`. It creates the file if needed, refuses a symlink, refuses
  a file that changed while it worked, writes atomically, sets the file to
  owner-only because it now holds the proxy password, records each key and value
  in the manifest, and can run again safely. It tells the owner to restart
  running sessions. It refuses when a variable it sets is already set to
  something else in the shell or in user, project, or managed settings, and when
  the manifest names another settings file.
- Setup records a setting in the manifest when it wrote the value, when an
  earlier setup recorded it, or when it already holds this install's proxy URL
  or `ca.pem` path. Cleanup therefore never removes another equal value the
  owner set first, and never leaves the proxy password behind. Setup keeps the
  file's indentation.
- `duolane intercept cleanup` removes each setting whose value still matches the
  manifest, reports any that changed, deletes the files, and reminds the owner
  to restart Claude Code. Without a manifest it finds the files by their fixed
  paths, and removes the proxy and CA settings that hold duolane values; when it
  finds one of those, it also removes the other variables setup writes where
  they hold exactly setup's values.
- Cleanup deletes the files even when it cannot edit the settings file, then
  fails and names that file.
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
   `api.anthropic.com` and applies the request limits. The inbound token and the
   `Origin` and content type rules do not apply there, because the proxy
   password already authenticated the connection and no browser can reach it.
6. `duolane serve` refuses to start in this mode when `HTTPS_PROXY`,
   `ALL_PROXY`, or their lowercase forms in its own environment name `localhost`
   or a loopback or unspecified address on the proxy port, because its upstream
   connections would loop through the listener. Tunnels always connect directly.
7. `leaf.key` and `proxy-password` are protected like the Codex login: owner
   only, never logged, captured, or printed. Whoever reads `leaf.key` and takes
   the proxy port can read the Claude login and forge model replies until the
   certificate expires. After a leak, `duolane intercept renew` and a restart of
   every Claude Code session end that trust; a session that is not restarted
   trusts the old CA until it expires.
8. Tunneled and relayed bytes are never logged or captured. Tunnels have no idle
   timeout.
9. The listener holds at most 64 connections that have not yet sent the
   password, 256 intercepted connections, and 256 tunnels. A connection counts
   as intercepted or tunneled only after the password check, and gets 503 when
   its pool is full. A connection that sends no complete `CONNECT` or TLS
   handshake within 30 seconds is closed.
10. A client sends the proxy password in `CONNECT`, before it can check the
    leaf, because the leaf arrives inside the tunnel. Another local user who
    takes the proxy port while `duolane serve` is down receives the password.
    This risk is accepted. When the listener check of `duolane claude` fails
    after it began sending the password, it says the password was exposed and
    that `duolane intercept cleanup` replaces it.

## Spike results

Checked on 2026-11-19 with Claude Code 2.1.327 behind an existing intercepting
proxy, before any duolane code:

- Claude Code loads `NODE_EXTRA_CA_CERTS`, accepts a leaf from the
  name-constrained CA, rejects a leaf from a CA constrained to another name, and
  sends the password in the `HTTPS_PROXY` URL on `CONNECT`.
- Remote Control works through a proxy that intercepts `api.anthropic.com` and
  tunnels other hosts, both for `claude --remote-control` and for the
  `claude remote-control` server: the phone sees the session, sends messages,
  gets replies, and changes the model. It uses long polls and one event stream
  under `/v1/environments/`, `/v1/code/sessions/`, and `/v2/ccr-sessions/`,
  without a protocol upgrade.
- `--model`, `ANTHROPIC_MODEL`, and `/model <id>`, typed locally or sent from
  the phone, send a Codex model id unchanged in `POST /v1/messages?beta=true`.
  Claude Code warns that it does not recognize the id and assumes a window of
  1,000,000 tokens. The phone's model list offers only Claude models.
- `/model <id>` also saves the id as the default for new sessions, so a session
  started later without duolane fails until the owner picks a Claude model.
- Claude Code speaks HTTP/1.1 when offered only that, sends request bodies
  uncompressed, and authenticates with `Authorization`. `api.anthropic.com` is
  the only host it needs intercepted.

## Unverified behavior

[Live verification](../plans/foundation.md) checks these with the built mode:

- No first-party feature needs a protocol upgrade on the intercepted host.
- The first-byte timeout variable exists and behaves as documented.
- Whether the wrapper must set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` to correct the
  window Claude Code assumes for a Codex model
  ([model configuration](https://code.claude.com/docs/en/model-config)).
