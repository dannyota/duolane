# Security

A web page or a network host must not be able to use the owner's logins through
duolane. duolane attaches the Codex login to requests it accepts, so reaching
its port from a browser or the network must not be enough.

## Trust boundaries

| Actor                                  | Treatment                                         |
| -------------------------------------- | ------------------------------------------------- |
| Claude Code on the same machine        | Trusted client                                    |
| A web page in the owner's browser      | Untrusted, and able to send requests to loopback  |
| A process running as the owner         | Accepted risk: it can already read the Codex file |
| Another OS user or a sandboxed process | Blocked only when an inbound token is set         |
| A host on the network                  | Untrusted, and reaches duolane only on a set bind |
| Anthropic and OpenAI endpoints         | Trusted for auth, not for response size or shape  |
| Model output and tool results          | Untrusted text                                    |

## Inbound requests

A bind address is loopback when its canonical IP is a loopback address. This
includes `127.0.0.0/8`, `::1`, and `::ffff:127.0.0.1`.

1. The default bind address is `127.0.0.1`.
2. Any configured inbound token has at least 32 characters. A configuration file
   that exists but is not valid JSON of the expected shape stops startup, so a
   syntax or type error cannot silently drop the token. duolane refuses to start
   on a non-loopback address without a token. Inbound traffic is plain HTTP,
   including the token and the Claude login, so such a bind is supported only on
   an encrypted path such as an SSH tunnel, WireGuard, or a TLS-terminating
   proxy. duolane prints this warning at startup.
3. On a loopback bind, the `Host` header must be present once and its host must
   exactly match `localhost`, `127.0.0.1`, `[::1]`, or the literal bind address,
   ignoring case. Any port is allowed. Prefix and suffix matches are forbidden.
4. Additional exact hosts can be configured for a local tunnel or reverse proxy,
   through `DUOLANE_ALLOWED_HOSTS` or `allowedHosts`. Each entry is a bare host
   name or a bracketed IPv6 address, with no port, scheme, path, or trailing
   dot. Configuring one requires an inbound token. A reverse proxy that rewrites
   `Host` to a loopback name looks local to these rules, so it must sit behind
   the token too.
5. Any request with an `Origin` header is rejected. duolane sends no CORS
   headers and answers no preflight.
6. `POST` routes that take JSON require the media type `application/json`, and
   reject a missing type. Parameters such as `charset` are allowed.
   `/v1/audio/transcriptions` accepts only `multipart/form-data`, and
   `/v1/images/edits` accepts either type. Multipart is a type browsers can send
   cross-site, so rule 5 is its defense. `GET` and `HEAD` are exempt.
7. When a token is set, every route except `/healthz` requires it. The token
   travels in the `x-duolane-key` header, because `Authorization` carries the
   Claude login. A bearer token is also accepted for OpenAI-compatible clients;
   when the token arrives in `x-duolane-key`, a bearer value is the Claude login
   and is forwarded. `duolane monitor` sends the configured token.
8. `/monitor` also accepts only loopback peers, and only the loopback `Host`
   names or the literal bind address, never a configured extra host. Remote
   monitoring is unsupported. `duolane monitor` sends the token only to a
   loopback or HTTPS URL.
9. Token comparison reveals neither the token length nor the position of a
   mismatch. The inbound token is removed before a request goes upstream.
10. These checks run before the body is read, on every route except `/healthz`.
    `/healthz` returns no account or request data.

Rejections use the Anthropic error shape and never echo request values:

| Condition                                      | Status |
| ---------------------------------------------- | ------ |
| Bad, missing, or repeated `Host`; any `Origin` | 403    |
| Repeated `Authorization` or `x-duolane-key`    | 400    |
| Missing or wrong token                         | 401    |
| Wrong content type                             | 415    |

## Credentials

- The Claude login passes through in transit and is never stored or logged,
  unless Claude accounts are configured, in which case the account store below
  owns it.
- The Codex login is read from `$CODEX_HOME/auth.json`, by default
  `~/.codex/auth.json`, and is never deleted, even after a rejected refresh.
- Writes to that file are atomic, flushed to disk, readable only by the user,
  and keep every field duolane does not own. duolane re-reads the file before
  each write for that reason.
- duolane takes the access token's expiry from its JWT `exp` claim. When the
  claim cannot be read, it assumes one hour, so a stale token surfaces as a 401
  and a forced refresh rather than being trusted indefinitely.
- duolane processes serialize a refresh with an exclusive lock on
  `auth.json.duolane.lock` beside the auth file, with a 10 second timeout. Under
  the lock, duolane re-reads the file before spending a refresh token.
- duolane writes rotated tokens only if the file still holds the refresh token
  it spent, so a login the Codex CLI changed during the exchange wins. The
  exchange and the write finish even if the request that started them is
  cancelled. A failed write is tried up to three times.
- If the lock cannot be taken, duolane keeps using an unexpired token. After a
  rejected refresh, duolane re-reads the file and uses a token that changed.
  Otherwise it reports that a new `codex login` is needed.
- Connection reuse, continuation, and compaction state are bound to the account
  that created them, and an account change is a cache miss. A request is sent
  only with a login for the account it was translated for. If the login changed,
  duolane translates the request again once for the new account, and answers
  HTTP 409 if the login changes again during that retry.
- The `default` Codex profile keeps the lookup and file handling above,
  including a symlinked `auth.json`.
- Named profiles in `codex-profiles.json` must name absolute paths without `..`
  components. duolane opens the profile file, each named Codex home, and its
  `auth.json` through file descriptors, without following symlinks, accepts only
  regular files and directories, and checks ownership and mode on the opened
  descriptors. The profile file and each home must be owned by the user and not
  writable by group or others; a named `auth.json` must also not be readable by
  them. Reads of a named `auth.json` stop at 1 MiB. Reading, the refresh lock,
  and the atomic rename happen relative to the same opened home directory, so
  replacing a path component cannot redirect them.
- The profile selection in the state directory is written atomically, flushed,
  and readable only by the user. duolane reads and writes it through a
  descriptor of the state directory after checking that the directory is owned
  by the user and not writable by group or others. An unsafe state directory is
  an error for Codex requests, never the same as a missing selection.
- For the profile file, the state directory, and each named home, every
  directory from the resolved path up to `/` must be owned by root or the user
  and not writable by group or others unless it is sticky, as OpenSSH
  `StrictModes` requires. duolane creates the state directory only when it
  writes a selection, with mode `0700`. Files are opened with `O_NOFOLLOW`,
  `O_NOCTTY`, and `O_NONBLOCK`. Access control lists are not checked.
- Named profiles and profile selection exist only on Unix. Elsewhere duolane
  ignores both and uses `default`, and `duolane codex auth use` reports that
  profiles are not supported.
- The Claude account store `claude-accounts.json` maps an account name to an
  absolute Claude Code config directory, the directory `CLAUDE_CONFIG_DIR`
  names. duolane opens the file through a descriptor of the config directory,
  without following symlinks, accepts only a regular file, and checks ownership
  and mode on the opened descriptor: owned by the user and not readable or
  writable by group or others. Reads stop at 16 KiB, and the file holds at most
  16 accounts. Each named directory must be an absolute path without `..`
  components, opened and read the same way as a Codex profile home, under the
  same ownership and `StrictModes` ancestor rules. The owner edits the store by
  hand, so duolane only reads it.
- duolane reads the selected account's session key from the login store the
  [accounts design](accounts.md#claude-login-location) names and attaches it to
  an upstream request as `Authorization: Bearer <token>`. The key is never
  logged or captured; the shared credential matcher redacts the header before it
  reaches logs and traffic captures.
- The macOS Keychain login is read by executing `/usr/bin/security` directly,
  without a shell, with the fixed arguments
  `find-generic-password -s <service> -w`, an environment cleared except
  `PATH=/usr/bin:/bin`, stdin closed, and stderr discarded. Standard output is
  read up to 64 KiB, and more is an error. The child gets 30 seconds, after
  which it is killed and reaped and the account reports a timeout. A non-zero
  exit, including a denied or dismissed access prompt, fails only that account.
  duolane does not require or ask for "Always Allow".
- On other Unix systems `.credentials.json` is opened with `openat` on the
  descriptor of the account directory that passed the ownership and ancestor
  checks, with `O_NOFOLLOW`, `O_NOCTTY`, and `O_NONBLOCK`. It must be a regular
  file owned by the user and not readable or writable by group or others, and
  more than 64 KiB is an error. duolane never writes either login store.
- The usage query sends a bearer key only to the fixed
  `https://api.anthropic.com/api/oauth/usage`, with redirects disabled, a
  15-second connect timeout, and a 30-second overall timeout. It honors
  `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY`, except that a proxy whose host is
  loopback and whose port is duolane's intercept port is skipped, so a run
  inside an intercepted Claude Code session never routes a key through duolane's
  own intercept listener. Other proxies are used as configured, and a proxy
  failure is never retried directly. The response body is read up to 64 KiB and
  more is an error. A token is held in a type whose debug form is redacted.
  Messages use the fixed per-account reasons the
  [accounts design](accounts.md#usage) lists, with only the numeric HTTP status
  added; keys, login contents, response bodies, child output, and library error
  text are never printed or logged.
- The `claude-account` selection in the state directory is written atomically,
  flushed, and readable only by the user, exactly like the `codex-profile`
  selection.

## Upstream connections

- Every client that sends a credential disables redirects and sets a connect
  timeout.
- Requests to a loopback host never go through a proxy.
- Upstream URLs use HTTPS, and WebSocket URLs use WSS. Plain HTTP and WS are
  allowed only for a loopback host, for tests and local development. The Codex,
  Codex images, and Anthropic base URL settings are checked at startup and may
  not carry user info, a query, or a fragment.
- TLS uses rustls with the ring provider. HTTP clients verify certificates
  through the operating system: the Keychain on macOS, and the system CA bundle
  on Linux, where `SSL_CERT_FILE` and `SSL_CERT_DIR` apply and a missing bundle
  stops startup. The WebSocket client uses the system roots, honoring those
  variables, plus the bundled Mozilla roots. A private CA for a TLS-inspecting
  proxy therefore belongs in the Keychain on macOS.
- duolane never decompresses upstream data.

## Limits

| Resource                            | Limit                     |
| ----------------------------------- | ------------------------- |
| Anthropic Messages request body     | 64 MiB                    |
| `/v1/responses` and chat request    | 16 MiB                    |
| Image generation request            | 256 KiB                   |
| Image edit request                  | 64 MiB                    |
| Transcription request               | 26 MiB                    |
| Inbound header read timeout         | 30 seconds                |
| Open inbound connections            | 128                       |
| Buffered upstream response          | 64 MiB                    |
| Image response                      | 128 MiB                   |
| Transcription response              | 1 MiB                     |
| One upstream SSE or WebSocket frame | 8 MiB                     |
| Pending Codex HTTP function calls   | 64 calls, 8 MiB total     |
| Committed Codex HTTP call IDs       | 1,024 IDs, 1 MiB total    |
| Codex HTTP arguments per function   | 5,000,000 bytes           |
| Upstream connect timeout            | 15 seconds                |
| Wait for Claude response headers    | 15 minutes                |
| Wait for Codex response headers     | 5 minutes                 |
| Idle time between streamed bytes    | 5 minutes                 |
| Monitor text per field              | 1 KiB                     |
| Monitor recent requests             | 200                       |
| Monitor per-session usage entries   | 1,000, for 24 hours       |
| Session state entries               | 10,000                    |
| Logged string                       | 4,000 bytes               |
| Log file before rotation            | 20 MiB, keeping 5 rotated |
| Error capture                       | 256 KiB per file          |
| Error captures kept                 | Newest 200, up to 7 days  |

An error capture larger than its limit keeps valid JSON: an oversized field
becomes its byte count and a prefix. `DUOLANE_LOG_VERBOSE` lifts the logged
string limit, and opt-in traffic captures have no total size limit, because both
are debugging aids the owner turns on deliberately.

A streamed Claude response has no total size limit, because duolane forwards it
without buffering. The limit on idle time between bytes still applies. Text is
truncated on a character boundary.

## Stored data

- Traffic capture is off by default and warns when enabled, because it stores
  conversation content. It stores only redacted JSON events, never raw upstream
  bodies.
- Logs and captures are readable only by the user.
- Traffic captures replace image payloads, `encrypted_content`, and duolane
  signatures with a length marker, and keep Anthropic signatures. An image
  source without a `media_type` is still redacted, because capture runs before
  validation. An image field keeps its object shape, with each data URL inside
  it replaced.
- Under a credential key name, the whole value is replaced whatever its type.
  Header names, body keys, and query parameters share one matcher, which also
  covers names ending in `token`, `secret`, or `password`.

## Terminal output

Text from requests, models, and upstream errors has control characters removed
before it reaches the terminal.
